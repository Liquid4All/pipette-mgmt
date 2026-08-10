use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use aws_sdk_s3::error::ProvideErrorMetadata;
use aws_sdk_s3::operation::get_object::GetObjectError;
use aws_sdk_s3::primitives::ByteStream;
use chrono::{DateTime, Utc};
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use futures::TryStreamExt;
use futures::stream::{self, StreamExt};
use object_store::ObjectStoreExt;
use object_store::aws::AmazonS3Builder;
use object_store::path::Path as ObjPath;
use object_store::{ObjectStore, PutMode, PutPayload, UpdateVersion};

use crate::benchmark::Benchmark;
use crate::client::{Client, parse_client_or_self_heal};
use crate::config::Config;
use crate::eval_sample_result::{self, EvalSampleResult};
use crate::parquet_utils::WriterOpts;
use crate::plan::{PlanManifest, PlanStatus};
use crate::preauth::{
    PreauthConsumeOutcome, PreauthKey, PreauthRejection, PreauthUsage, Secret, validate,
};
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

/// Build an `(object_store, normalized_prefix)` pair from an S3
/// `StorageConfig`. Shared by every entry point that needs to talk to
/// S3 (the per-store builders below and `ModelCatalog::load`) so all
/// paths use the same builder/credentials/region/endpoint resolution.
pub fn build_s3_object_store(
    storage: &crate::config::StorageConfig,
) -> anyhow::Result<(Arc<dyn ObjectStore>, String)> {
    let crate::config::StorageConfig::S3 {
        bucket,
        prefix,
        region,
        endpoint,
        ..
    } = storage
    else {
        anyhow::bail!("build_s3_object_store called with non-S3 storage config");
    };
    let mut builder = AmazonS3Builder::from_env().with_bucket_name(bucket);
    if let Some(region) = region {
        builder = builder.with_region(region);
    }
    if let Some(endpoint) = endpoint {
        builder = builder.with_endpoint(endpoint).with_allow_http(true);
    }
    let store: Arc<dyn ObjectStore> = Arc::new(builder.build()?);
    Ok((store, prefix.trim_end_matches('/').to_string()))
}

pub fn build_s3_auth_store(
    storage: &crate::config::StorageConfig,
) -> anyhow::Result<Arc<dyn AuthStore>> {
    let (store, prefix) = build_s3_object_store(storage)?;
    Ok(Arc::new(S3AuthStore { store, prefix }))
}

/// `todo` is pre-built by the caller: its backend comes from
/// `config.todo_storage()` rather than `[storage]`, and the S3 todo builder
/// resolves AWS credentials via `aws-config`'s async chain (see
/// [`build_s3_todo_store`]).
pub fn build_s3_stores(config: &Config, todo: Arc<dyn TodoStore>) -> anyhow::Result<Stores> {
    let crate::config::StorageConfig::S3 {
        max_concurrent_requests,
        ..
    } = &config.storage
    else {
        anyhow::bail!("build_s3_stores called with non-S3 storage config");
    };
    let (store, prefix) = build_s3_object_store(&config.storage)?;
    let read_days = config.warehouse_read_days;
    let writer_opts = config.writer_opts();
    let tuning = S3Tuning {
        max_concurrent_requests: *max_concurrent_requests,
    };

    Ok(Stores {
        catalog: Arc::new(S3CatalogStore {
            store: store.clone(),
            prefix: prefix.clone(),
        }),
        auth: Arc::new(S3AuthStore {
            store: store.clone(),
            prefix: prefix.clone(),
        }),
        submissions: Arc::new(S3SubmissionStore {
            store: store.clone(),
            prefix: prefix.clone(),
        }),
        warehouse: Arc::new(S3WarehouseStore {
            store: store.clone(),
            prefix: prefix.clone(),
            read_days,
            max_rows_per_part: config.warehouse_max_rows_per_part,
            writer_opts,
            tuning,
        }),
        eval_sample_results: Arc::new(S3EvalSampleResultStore {
            store: store.clone(),
            prefix: prefix.clone(),
            writer_opts,
        }),
        plans: Arc::new(S3PlanStore { store, prefix }),
        todo,
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn obj_path(prefix: &str, suffix: &str) -> ObjPath {
    if prefix.is_empty() {
        ObjPath::from(suffix)
    } else {
        ObjPath::from(format!("{prefix}/{suffix}"))
    }
}

/// Extract the `job_id` from an object path's trailing filename.
/// Accepts both `.json` (incoming) and `.json.gz` (processed) suffixes.
/// Keys are written from validated ids, so a filename that fails `try_new` is
/// corruption/tampering — an anomaly to surface (warn) and skip, not a fatal
/// error.
fn job_id_from_path(path: &ObjPath) -> Option<JobId> {
    let filename = path.as_ref().rsplit('/').next()?;
    // `.json.gz` first because `.json` is a suffix of `.json.gz`.
    let job_id = filename
        .strip_suffix(".json.gz")
        .or_else(|| filename.strip_suffix(".json"))?;
    JobId::try_new(job_id)
        .inspect_err(
            |e| tracing::warn!(path = %path, error = %e, "skipping object with invalid job_id"),
        )
        .ok()
}

/// Whether a submission blob is stored as plain JSON or gzipped JSON.
#[derive(Debug, Clone, Copy)]
enum Encoding {
    Plain,
    Gz,
}

/// S3-specific tuning shared by every store backed by the same bucket.
/// Held by value (Copy) so each store keeps its own snapshot — no
/// cross-store synchronization concern.
#[derive(Debug, Clone, Copy)]
struct S3Tuning {
    max_concurrent_requests: usize,
}

fn parse_json_blob(data: &[u8], encoding: Encoding) -> anyhow::Result<serde_json::Value> {
    match encoding {
        Encoding::Plain => Ok(serde_json::from_slice(data)?),
        Encoding::Gz => Ok(serde_json::from_reader(GzDecoder::new(data))?),
    }
}

async fn get_bytes(
    store: &Arc<dyn ObjectStore>,
    path: &ObjPath,
) -> anyhow::Result<Option<bytes::Bytes>> {
    match store.get(path).await {
        Ok(result) => Ok(Some(result.bytes().await?)),
        Err(object_store::Error::NotFound { .. }) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

async fn get_bytes_for_update(
    store: &Arc<dyn ObjectStore>,
    path: &ObjPath,
) -> anyhow::Result<Option<(bytes::Bytes, UpdateVersion)>> {
    match store.get(path).await {
        Ok(result) => {
            let version = UpdateVersion {
                e_tag: result.meta.e_tag.clone(),
                version: result.meta.version.clone(),
            };
            Ok(Some((result.bytes().await?, version)))
        }
        Err(object_store::Error::NotFound { .. }) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

async fn put_bytes(
    store: &Arc<dyn ObjectStore>,
    path: &ObjPath,
    data: Vec<u8>,
) -> anyhow::Result<()> {
    store.put(path, PutPayload::from(data)).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// CatalogStore
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct S3CatalogStore {
    store: Arc<dyn ObjectStore>,
    prefix: String,
}

#[async_trait]
impl CatalogStore for S3CatalogStore {
    async fn load_catalog(&self) -> anyhow::Result<HashMap<BenchmarkId, Benchmark>> {
        let prefix = obj_path(&self.prefix, "benchmarks/");
        let entries: Vec<_> = self.store.list(Some(&prefix)).try_collect().await?;
        let mut catalog = HashMap::new();
        for meta in entries {
            let path_str = meta.location.as_ref();
            let filename = path_str.rsplit('/').next().unwrap_or(path_str);
            let benchmark_id = match filename.strip_suffix(".toml") {
                Some(id) => id,
                None => continue,
            };
            let data = get_bytes(&self.store, &meta.location)
                .await?
                .ok_or_else(|| anyhow::anyhow!("benchmark vanished: {}", meta.location))?;
            let content = std::str::from_utf8(&data)?;
            let bm = Benchmark::from_toml(benchmark_id, content)?;
            catalog.insert(BenchmarkId::try_new(benchmark_id)?, bm);
        }
        Ok(catalog)
    }
}

// ---------------------------------------------------------------------------
// AuthStore
// ---------------------------------------------------------------------------

/// Which of a tag-marker key's two trailing segments is which.
enum MarkerOrder {
    /// `.../{client_id}/{tag}` (forward tree) — last segment is the tag.
    ClientThenTag,
    /// `.../{tag}/{client_id}` (reverse tree) — last segment is the client id.
    TagThenClient,
}

/// Parse `(ClientId, Tag)` from the last two `/`-segments of a tag-marker key;
/// `None` if a segment is missing or fails validation.
fn parse_marker_pair(key: &str, order: MarkerOrder) -> Option<(ClientId, crate::validated::Tag)> {
    let mut segs = key.rsplit('/');
    let (last, prev) = (segs.next()?, segs.next()?);
    let (id, tag) = match order {
        MarkerOrder::ClientThenTag => (prev, last),
        MarkerOrder::TagThenClient => (last, prev),
    };
    Some((
        ClientId::try_new(id).ok()?,
        crate::validated::Tag::try_new(tag).ok()?,
    ))
}

#[derive(Clone)]
struct S3AuthStore {
    store: Arc<dyn ObjectStore>,
    prefix: String,
}

impl S3AuthStore {
    fn forward_tag_path(&self, client_id: &ClientId, tag: &crate::validated::Tag) -> ObjPath {
        obj_path(
            &self.prefix,
            &format!("tags-index/by-client/{client_id}/{tag}"),
        )
    }

    fn reverse_tag_path(&self, tag: &crate::validated::Tag, client_id: &ClientId) -> ObjPath {
        obj_path(
            &self.prefix,
            &format!("tags-index/by-tag/{tag}/{client_id}"),
        )
    }

    async fn delete_idempotent(&self, path: ObjPath) -> anyhow::Result<()> {
        match self.store.delete(&path).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    async fn persist_self_healed_client(
        &self,
        path: &ObjPath,
        client: &Client,
        version: UpdateVersion,
    ) {
        let data = match serde_json::to_vec_pretty(client) {
            Ok(data) => data,
            Err(e) => {
                tracing::warn!(
                    path = %path,
                    client_id = %client.client_id,
                    error = %e,
                    "failed to serialize self-healed client record"
                );
                return;
            }
        };

        match self
            .store
            .put_opts(
                path,
                PutPayload::from(data),
                PutMode::Update(version).into(),
            )
            .await
        {
            Ok(_) => tracing::warn!(
                path = %path,
                client_id = %client.client_id,
                "self-healed malformed client record"
            ),
            Err(object_store::Error::Precondition { .. }) => tracing::warn!(
                path = %path,
                client_id = %client.client_id,
                "skipped self-heal because client record changed during repair"
            ),
            Err(e) => tracing::warn!(
                path = %path,
                client_id = %client.client_id,
                error = %e,
                "failed to persist self-healed client record"
            ),
        }
    }
}

#[async_trait]
impl AuthStore for S3AuthStore {
    async fn get_client(&self, client_id: &ClientId) -> anyhow::Result<Option<Client>> {
        let path = obj_path(&self.prefix, &format!("clients/{client_id}.json"));
        match get_bytes_for_update(&self.store, &path).await? {
            Some((data, version)) => {
                let (client, repaired) = parse_client_or_self_heal(&data)?;
                if repaired {
                    self.persist_self_healed_client(&path, &client, version)
                        .await;
                }
                Ok(Some(client))
            }
            None => Ok(None),
        }
    }

    async fn put_client(&self, client: &Client) -> anyhow::Result<()> {
        let path = obj_path(&self.prefix, &format!("clients/{}.json", client.client_id));
        put_bytes(&self.store, &path, serde_json::to_vec_pretty(client)?).await
    }

    async fn delete_client(&self, client_id: &ClientId) -> anyhow::Result<()> {
        let path = obj_path(&self.prefix, &format!("clients/{client_id}.json"));
        self.delete_idempotent(path).await?;
        // Delete this client's forward + reverse markers with bounded
        // concurrency — one delete per key, all independent.
        let paths: Vec<ObjPath> = self
            .get_client_tags(client_id)
            .await?
            .iter()
            .flat_map(|tag| {
                [
                    self.forward_tag_path(client_id, tag),
                    self.reverse_tag_path(tag, client_id),
                ]
            })
            .collect();
        stream::iter(paths.into_iter().map(|p| self.delete_idempotent(p)))
            .buffer_unordered(crate::stores::STORAGE_CONCURRENCY)
            .try_collect::<Vec<_>>()
            .await?;
        Ok(())
    }

    async fn list_clients(&self) -> anyhow::Result<Vec<Client>> {
        let prefix = obj_path(&self.prefix, "clients/");
        let entries: Vec<_> = self.store.list(Some(&prefix)).try_collect().await?;
        let mut clients: Vec<Client> = Vec::new();
        for meta in entries {
            if !meta.location.as_ref().ends_with(".json") {
                continue;
            }
            let (data, version) = get_bytes_for_update(&self.store, &meta.location)
                .await?
                .ok_or_else(|| anyhow::anyhow!("client vanished: {}", meta.location))?;
            match parse_client_or_self_heal(&data) {
                Ok((client, repaired)) => {
                    if repaired {
                        self.persist_self_healed_client(&meta.location, &client, version)
                            .await;
                    }
                    clients.push(client);
                }
                Err(e) => {
                    tracing::warn!(
                        path = %meta.location,
                        error = %e,
                        "skipping malformed client record"
                    );
                }
            }
        }
        clients.sort_by_key(|c| std::cmp::Reverse(c.registered_at));
        Ok(clients)
    }

    async fn has_public_key(
        &self,
        public_key: &crate::validated::PublicKeyHex,
    ) -> anyhow::Result<bool> {
        let client_id = crate::client::derive_client_id(public_key)?;
        let path = obj_path(&self.prefix, &format!("clients/{client_id}.json"));
        match self.store.head(&path).await {
            Ok(_) => Ok(true),
            Err(object_store::Error::NotFound { .. }) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    async fn add_client_tag(
        &self,
        client_id: &ClientId,
        tag: &crate::validated::Tag,
    ) -> anyhow::Result<()> {
        put_bytes(
            &self.store,
            &self.forward_tag_path(client_id, tag),
            Vec::new(),
        )
        .await?;
        put_bytes(
            &self.store,
            &self.reverse_tag_path(tag, client_id),
            Vec::new(),
        )
        .await
    }

    async fn remove_client_tag(
        &self,
        client_id: &ClientId,
        tag: &crate::validated::Tag,
    ) -> anyhow::Result<()> {
        self.delete_idempotent(self.forward_tag_path(client_id, tag))
            .await?;
        self.delete_idempotent(self.reverse_tag_path(tag, client_id))
            .await
    }

    async fn get_client_tags(
        &self,
        client_id: &ClientId,
    ) -> anyhow::Result<std::collections::BTreeSet<crate::validated::Tag>> {
        // Forward: list `tags-index/by-client/{client_id}/` and read the tag off
        // each marker key's final segment.
        let list_prefix = obj_path(&self.prefix, &format!("tags-index/by-client/{client_id}/"));
        let entries: Vec<_> = self.store.list(Some(&list_prefix)).try_collect().await?;
        Ok(entries
            .iter()
            .filter_map(|meta| {
                let name = meta.location.as_ref().rsplit('/').next()?;
                crate::validated::Tag::try_new(name).ok()
            })
            .collect())
    }

    async fn list_client_ids_by_tag(
        &self,
        tag: &crate::validated::Tag,
    ) -> anyhow::Result<Vec<ClientId>> {
        // Reverse: list `tags-index/by-tag/{tag}/` and read the client id off
        // each marker key's final segment. Tags are flat, so the key is always
        // `.../tags-index/by-tag/{tag}/{client_id}` — unambiguous to split.
        let list_prefix = obj_path(&self.prefix, &format!("tags-index/by-tag/{tag}/"));
        let entries: Vec<_> = self.store.list(Some(&list_prefix)).try_collect().await?;
        let mut ids: Vec<ClientId> = entries
            .iter()
            .filter_map(|meta| {
                let name = meta.location.as_ref().rsplit('/').next()?;
                ClientId::try_new(name).ok()
            })
            .collect();
        ids.sort();
        Ok(ids)
    }

    async fn list_forward_tag_markers(
        &self,
    ) -> anyhow::Result<Vec<(ClientId, crate::validated::Tag)>> {
        // Keys are `.../tags-index/by-client/{client_id}/{tag}`; the last two
        // segments are the pair.
        let list_prefix = obj_path(&self.prefix, "tags-index/by-client/");
        let entries: Vec<_> = self.store.list(Some(&list_prefix)).try_collect().await?;
        Ok(entries
            .iter()
            .filter_map(|meta| {
                parse_marker_pair(meta.location.as_ref(), MarkerOrder::ClientThenTag)
            })
            .collect())
    }

    async fn list_reverse_tag_markers(
        &self,
    ) -> anyhow::Result<Vec<(ClientId, crate::validated::Tag)>> {
        // Keys are `.../tags-index/by-tag/{tag}/{client_id}`; the last two
        // segments are the pair (reversed relative to the forward tree).
        let list_prefix = obj_path(&self.prefix, "tags-index/by-tag/");
        let entries: Vec<_> = self.store.list(Some(&list_prefix)).try_collect().await?;
        Ok(entries
            .iter()
            .filter_map(|meta| {
                parse_marker_pair(meta.location.as_ref(), MarkerOrder::TagThenClient)
            })
            .collect())
    }

    async fn has_signature_migration(&self, client_id: &ClientId) -> anyhow::Result<bool> {
        let path = obj_path(
            &self.prefix,
            &format!("signature-migration/{client_id}.json"),
        );
        // `head` rather than `get`: the answer is the object's existence, so the
        // body never has to cross the wire.
        match self.store.head(&path).await {
            Ok(_) => Ok(true),
            Err(object_store::Error::NotFound { .. }) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    async fn record_signature_migration(
        &self,
        client_id: &ClientId,
        at: chrono::DateTime<chrono::Utc>,
    ) -> anyhow::Result<crate::client::MigrationRecord> {
        let path = obj_path(
            &self.prefix,
            &format!("signature-migration/{client_id}.json"),
        );
        let record = crate::client::SignatureMigration { first_seen: at };
        match self
            .store
            .put_opts(
                &path,
                PutPayload::from(serde_json::to_vec_pretty(&record)?),
                PutMode::Create.into(),
            )
            .await
        {
            Ok(_) => Ok(crate::client::MigrationRecord::First),
            // The conditional create is what keeps `first_seen` the *first*
            // sighting: a marker already in place means this client migrated on
            // an earlier request, which is the time worth keeping.
            Err(object_store::Error::AlreadyExists { .. }) => {
                Ok(crate::client::MigrationRecord::Existing)
            }
            Err(e) => Err(e.into()),
        }
    }

    async fn list_signature_migrations(
        &self,
    ) -> anyhow::Result<Vec<(ClientId, crate::client::SignatureMigration)>> {
        let prefix = obj_path(&self.prefix, "signature-migration/");
        let entries: Vec<_> = self.store.list(Some(&prefix)).try_collect().await?;
        // Skips unreadable entries per the trait contract.
        Ok(stream::iter(entries.into_iter().map(|meta| async move {
            let name = meta.location.filename()?;
            let client_id = ClientId::try_new(name.strip_suffix(".json")?).ok()?;
            let data = get_bytes(&self.store, &meta.location).await.ok()??;
            Some((client_id, serde_json::from_slice(&data).ok()?))
        }))
        .buffer_unordered(crate::stores::STORAGE_CONCURRENCY)
        .filter_map(|entry| async move { entry })
        .collect::<Vec<_>>()
        .await)
    }

    async fn put_preauth_key(&self, key: &PreauthKey) -> anyhow::Result<()> {
        let path = obj_path(&self.prefix, &format!("preauth/{}.json", key.key_id));
        put_bytes(&self.store, &path, serde_json::to_vec_pretty(key)?).await
    }

    async fn consume_preauth_key(
        &self,
        key_id: &PreauthKeyId,
        secret: &Secret,
    ) -> anyhow::Result<PreauthConsumeOutcome> {
        let path = obj_path(&self.prefix, &format!("preauth/{key_id}.json"));
        let Some(data) = get_bytes(&self.store, &path).await? else {
            return Ok(PreauthConsumeOutcome::Rejected(PreauthRejection::NotFound));
        };
        let now = chrono::Utc::now();
        let key: PreauthKey = serde_json::from_slice(&data)?;
        let grant = match validate(&key, secret, now) {
            Ok(grant) => grant,
            Err(rejection) => return Ok(PreauthConsumeOutcome::Rejected(rejection)),
        };
        // Spending is the exclusive create of the marker, not the delete of the
        // record: `object_store` has no conditional delete, but it does have
        // conditional create (`If-None-Match: *`), so the marker is what makes
        // one winner out of any number of concurrent consumes. The record delete
        // that follows is cleanup — if it never lands, the marker still stands
        // and the next consume loses the create. Multi-use keys are not mutated,
        // so nothing here runs for them.
        if matches!(key.usage, PreauthUsage::SingleUse) {
            let marker = obj_path(&self.prefix, &format!("preauth/{key_id}.spent"));
            match self
                .store
                .put_opts(
                    &marker,
                    PutPayload::from(now.to_rfc3339().into_bytes()),
                    PutMode::Create.into(),
                )
                .await
            {
                Ok(_) => {}
                // Already spent. Reported as unknown, like every other reason a
                // key will not grant, so the endpoint stays uninformative.
                Err(object_store::Error::AlreadyExists { .. }) => {
                    return Ok(PreauthConsumeOutcome::Rejected(PreauthRejection::NotFound));
                }
                Err(e) => return Err(e.into()),
            }
            self.delete_idempotent(path).await?;
        }
        Ok(PreauthConsumeOutcome::Granted(grant))
    }

    async fn list_preauth_keys(&self) -> anyhow::Result<Vec<PreauthKey>> {
        let prefix = obj_path(&self.prefix, "preauth/");
        let entries: Vec<_> = self.store.list(Some(&prefix)).try_collect().await?;
        stream::iter(
            entries
                .into_iter()
                .filter(|meta| meta.location.as_ref().ends_with(".json"))
                .map(|meta| async move {
                    let data = get_bytes(&self.store, &meta.location)
                        .await?
                        .ok_or_else(|| {
                            anyhow::anyhow!("preauth key vanished: {}", meta.location)
                        })?;
                    Ok(serde_json::from_slice(&data)?)
                }),
        )
        .buffer_unordered(crate::stores::STORAGE_CONCURRENCY)
        .try_collect()
        .await
    }

    async fn delete_preauth_key(&self, key_id: &PreauthKeyId) -> anyhow::Result<()> {
        let path = obj_path(&self.prefix, &format!("preauth/{key_id}.json"));
        self.delete_idempotent(path).await
    }

    async fn list_spent_markers(&self) -> anyhow::Result<Vec<PreauthKeyId>> {
        let prefix = obj_path(&self.prefix, "preauth/");
        let entries: Vec<_> = self.store.list(Some(&prefix)).try_collect().await?;
        Ok(entries
            .iter()
            .filter_map(|meta| {
                let name = meta.location.filename()?;
                PreauthKeyId::try_new(name.strip_suffix(".spent")?).ok()
            })
            .collect())
    }

    async fn delete_spent_marker(&self, key_id: &PreauthKeyId) -> anyhow::Result<()> {
        let path = obj_path(&self.prefix, &format!("preauth/{key_id}.spent"));
        self.delete_idempotent(path).await
    }
}

// ---------------------------------------------------------------------------
// PlanStore
// ---------------------------------------------------------------------------

/// Plan manifests as `plans/{plan_id}.json` in the `[storage]` object store.
/// Mirrors the preauth durable-record methods; `list_plans` is the same
/// list-then-read-each fan-out as `list_preauth_keys` (see
/// `docs/plan-ingestion.md` §9).
#[derive(Clone)]
struct S3PlanStore {
    store: Arc<dyn ObjectStore>,
    prefix: String,
}

impl S3PlanStore {
    fn manifest_path(&self, plan_id: &PlanId) -> ObjPath {
        obj_path(&self.prefix, &format!("plans/{}.json", plan_id.as_str()))
    }

    /// Marker key — the bare `plan_id`, no extension, under a keyspace that is a
    /// **sibling** of `plans/` so `list_plans`'s prefix listing never returns
    /// markers.
    fn cancel_marker_path(&self, plan_id: &PlanId) -> ObjPath {
        obj_path(
            &self.prefix,
            &format!("cancelled_plans/{}", plan_id.as_str()),
        )
    }
}

#[async_trait]
impl PlanStore for S3PlanStore {
    async fn put_plan(&self, manifest: &PlanManifest) -> anyhow::Result<()> {
        put_bytes(
            &self.store,
            &self.manifest_path(&manifest.plan_id),
            serde_json::to_vec_pretty(manifest)?,
        )
        .await
    }

    async fn get_plan(&self, plan_id: &PlanId) -> anyhow::Result<Option<PlanManifest>> {
        match get_bytes(&self.store, &self.manifest_path(plan_id)).await? {
            Some(data) => Ok(Some(serde_json::from_slice(&data)?)),
            None => Ok(None),
        }
    }

    async fn list_plans(&self, status: Option<PlanStatus>) -> anyhow::Result<Vec<PlanManifest>> {
        let prefix = obj_path(&self.prefix, "plans/");
        let entries: Vec<_> = self.store.list(Some(&prefix)).try_collect().await?;
        let plans: Vec<PlanManifest> = stream::iter(
            entries
                .into_iter()
                .filter(|meta| meta.location.as_ref().ends_with(".json"))
                .map(|meta| async move {
                    match get_bytes(&self.store, &meta.location).await? {
                        Some(data) => {
                            Ok::<_, anyhow::Error>(Some(serde_json::from_slice::<PlanManifest>(
                                &data,
                            )?))
                        }
                        // Deleted between the listing and this read (e.g. the
                        // retention GC) — equivalent to having listed a moment
                        // later; skip rather than fail the whole listing.
                        None => Ok(None),
                    }
                }),
        )
        .buffer_unordered(crate::stores::STORAGE_CONCURRENCY)
        .try_collect::<Vec<Option<PlanManifest>>>()
        .await?
        .into_iter()
        .flatten()
        .collect();
        Ok(match status {
            Some(s) => plans.into_iter().filter(|p| p.status == s).collect(),
            None => plans,
        })
    }

    async fn delete_plan(&self, plan_id: &PlanId) -> anyhow::Result<()> {
        match self.store.delete(&self.manifest_path(plan_id)).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    async fn write_cancel_marker(&self, plan_id: &PlanId) -> anyhow::Result<()> {
        // Zero-byte object: the key's existence *is* the signal. A plain PUT, so
        // re-cancelling overwrites it harmlessly.
        put_bytes(&self.store, &self.cancel_marker_path(plan_id), Vec::new()).await
    }

    async fn has_cancel_marker(&self, plan_id: &PlanId) -> anyhow::Result<bool> {
        // HEAD rather than GET — there is no body worth fetching. Mirrors
        // `has_public_key`; a non-NotFound error propagates rather than reading
        // as "not cancelled".
        match self.store.head(&self.cancel_marker_path(plan_id)).await {
            Ok(_) => Ok(true),
            Err(object_store::Error::NotFound { .. }) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    async fn list_cancel_markers(&self) -> anyhow::Result<Vec<PlanId>> {
        let prefix = obj_path(&self.prefix, "cancelled_plans/");
        let entries: Vec<_> = self.store.list(Some(&prefix)).try_collect().await?;
        // Markers carry no body, so unlike `list_plans` this needs no per-object
        // read — the ids come straight off the keys.
        Ok(entries
            .into_iter()
            .filter_map(|meta| {
                let name = meta.location.filename().unwrap_or_default();
                match PlanId::try_new(name) {
                    Ok(plan_id) => Some(plan_id),
                    Err(_) => {
                        tracing::warn!(
                            key = %meta.location,
                            "skipping unparseable cancel marker"
                        );
                        None
                    }
                }
            })
            .collect())
    }

    async fn delete_cancel_marker(&self, plan_id: &PlanId) -> anyhow::Result<()> {
        match self.store.delete(&self.cancel_marker_path(plan_id)).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

// ---------------------------------------------------------------------------
// SubmissionStore
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct S3SubmissionStore {
    store: Arc<dyn ObjectStore>,
    prefix: String,
}

impl S3SubmissionStore {
    fn incoming_path(&self, job_id: &JobId) -> ObjPath {
        obj_path(&self.prefix, &format!("submissions/incoming/{job_id}.json"))
    }

    fn processed_path(&self, job_id: &JobId) -> ObjPath {
        obj_path(
            &self.prefix,
            &format!("submissions/processed/{job_id}.json.gz"),
        )
    }

    fn unverified_path(&self, client_id: &ClientId, job_id: &JobId) -> ObjPath {
        obj_path(
            &self.prefix,
            &format!("submissions/unverified/{client_id}/{job_id}.json"),
        )
    }

    fn unverified_client_prefix(&self, client_id: &ClientId) -> ObjPath {
        obj_path(
            &self.prefix,
            &format!("submissions/unverified/{client_id}/"),
        )
    }

    fn stage_path(&self, stage: ScoreQueueStage, job_id: &JobId) -> ObjPath {
        obj_path(
            &self.prefix,
            &format!(
                "submissions/{}/{}/{job_id}.json",
                ScoreQueueStage::ROOT,
                stage.leaf()
            ),
        )
    }

    fn stage_prefix(&self, stage: ScoreQueueStage) -> ObjPath {
        obj_path(
            &self.prefix,
            &format!("submissions/{}/{}/", ScoreQueueStage::ROOT, stage.leaf()),
        )
    }

    /// Delete an object, treating an already-absent object as success. Shared
    /// by `delete_incoming` and `dequeue`, which route a job out of one prefix
    /// after copying it elsewhere and so must be idempotent on retry.
    async fn delete_object(&self, path: &ObjPath) -> anyhow::Result<()> {
        match self.store.delete(path).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// List up to `limit` `.json` job ids under `prefix`. Filtering + parsing
    /// happen during streaming so `take(limit)` short-circuits the paginated
    /// `ListObjectsV2`: per-call LIST cost is `ceil(limit / 1000)` requests,
    /// not `ceil(N_total / 1000)`.
    ///
    /// INVARIANT: each listed prefix contains only `.json` submission bodies.
    /// `try_filter_map` skips anything else, but `take(limit)` counts
    /// post-filter — a stray non-`.json` object would defeat the short-circuit
    /// (we'd keep paging until `limit` `.json` items appear). Keep prefixes clean.
    async fn list_json_job_ids(
        &self,
        prefix: &ObjPath,
        limit: std::num::NonZeroUsize,
    ) -> anyhow::Result<Vec<JobId>> {
        let jobs: Vec<JobId> = self
            .store
            .list(Some(prefix))
            .try_filter_map(|m| async move {
                if !m.location.as_ref().ends_with(".json") {
                    return Ok(None);
                }
                Ok(job_id_from_path(&m.location))
            })
            .take(limit.get())
            .try_collect()
            .await?;
        Ok(jobs)
    }
}

#[async_trait]
impl SubmissionStore for S3SubmissionStore {
    async fn write_incoming(&self, job_id: &JobId, body: &serde_json::Value) -> anyhow::Result<()> {
        let path = self.incoming_path(job_id);
        put_bytes(&self.store, &path, serde_json::to_vec_pretty(body)?).await
    }

    async fn write_processed(
        &self,
        job_id: &JobId,
        body: &serde_json::Value,
    ) -> anyhow::Result<()> {
        let path = self.processed_path(job_id);
        let raw = serde_json::to_vec(body)?;
        let mut encoder = GzEncoder::new(Vec::with_capacity(raw.len()), Compression::default());
        encoder.write_all(&raw)?;
        let compressed = encoder.finish()?;
        put_bytes(&self.store, &path, compressed).await
    }

    async fn get_submission(&self, job_id: &JobId) -> anyhow::Result<Option<SubmissionRecord>> {
        if let Some(data) = get_bytes(&self.store, &self.incoming_path(job_id)).await? {
            return Ok(Some(SubmissionRecord {
                job_id: job_id.clone(),
                state: JobState::Incoming,
                body: parse_json_blob(&data, Encoding::Plain)?,
            }));
        }
        if let Some(data) = get_bytes(&self.store, &self.processed_path(job_id)).await? {
            return Ok(Some(SubmissionRecord {
                job_id: job_id.clone(),
                state: JobState::Processed,
                body: parse_json_blob(&data, Encoding::Gz)?,
            }));
        }
        Ok(None)
    }

    async fn list_incoming(&self, limit: std::num::NonZeroUsize) -> anyhow::Result<Vec<JobId>> {
        let prefix = obj_path(&self.prefix, "submissions/incoming/");
        self.list_json_job_ids(&prefix, limit).await
    }

    async fn mark_processed(&self, job_id: &JobId) -> anyhow::Result<()> {
        // Stream the incoming GET body chunk-by-chunk into a gzip encoder.
        // Peak memory is one read chunk (~64 KB) plus the growing compressed
        // buffer, not raw + compressed simultaneously. The PUT stays a
        // single request — multipart adds 3 API ops per write and is not
        // worth it at typical submission sizes.
        //
        // PUT and DELETE are each atomic on S3; if we crash between them,
        // the incoming is re-processed on the next pass and the second PUT
        // overwrites the existing processed object. At-least-once but
        // idempotent — same shape as local_fs.
        let from = self.incoming_path(job_id);
        let to = self.processed_path(job_id);

        let result = match self.store.get(&from).await {
            Ok(r) => r,
            Err(object_store::Error::NotFound { .. }) => {
                anyhow::bail!("incoming submission vanished: {from}");
            }
            Err(e) => return Err(e.into()),
        };

        let mut stream = result.into_stream();
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        while let Some(chunk) = stream.try_next().await? {
            encoder.write_all(&chunk)?;
        }
        let compressed = encoder.finish()?;

        put_bytes(&self.store, &to, compressed).await?;
        self.store.delete(&from).await?;
        Ok(())
    }

    async fn delete_incoming(&self, job_id: &JobId) -> anyhow::Result<()> {
        self.delete_object(&self.incoming_path(job_id)).await
    }

    async fn enqueue(
        &self,
        stage: ScoreQueueStage,
        job_id: &JobId,
        body: &serde_json::Value,
    ) -> anyhow::Result<()> {
        let path = self.stage_path(stage, job_id);
        put_bytes(&self.store, &path, serde_json::to_vec_pretty(body)?).await
    }

    async fn list_queue(
        &self,
        stage: ScoreQueueStage,
        limit: std::num::NonZeroUsize,
    ) -> anyhow::Result<Vec<JobId>> {
        self.list_json_job_ids(&self.stage_prefix(stage), limit)
            .await
    }

    async fn read_queue(
        &self,
        stage: ScoreQueueStage,
        job_id: &JobId,
    ) -> anyhow::Result<Option<serde_json::Value>> {
        match get_bytes(&self.store, &self.stage_path(stage, job_id)).await? {
            Some(data) => Ok(Some(parse_json_blob(&data, Encoding::Plain)?)),
            None => Ok(None),
        }
    }

    async fn dequeue(&self, stage: ScoreQueueStage, job_id: &JobId) -> anyhow::Result<()> {
        self.delete_object(&self.stage_path(stage, job_id)).await
    }

    async fn find_job(&self, job_id: &JobId) -> anyhow::Result<Option<SubmissionRecord>> {
        if let Some(data) = get_bytes(&self.store, &self.incoming_path(job_id)).await? {
            return Ok(Some(SubmissionRecord {
                job_id: job_id.clone(),
                state: JobState::Incoming,
                body: parse_json_blob(&data, Encoding::Plain)?,
            }));
        }
        if let Some(data) = get_bytes(&self.store, &self.processed_path(job_id)).await? {
            return Ok(Some(SubmissionRecord {
                job_id: job_id.clone(),
                state: JobState::Processed,
                body: parse_json_blob(&data, Encoding::Gz)?,
            }));
        }
        self.find_in_score_queue(job_id).await
    }

    async fn write_unverified(
        &self,
        client_id: &ClientId,
        job_id: &JobId,
        body: &serde_json::Value,
    ) -> anyhow::Result<()> {
        // Same plain-JSON object as `incoming/`; only the prefix differs
        // (`submissions/unverified/{client_id}/`). Nothing lists or reads
        // this prefix except the operator `unverified` subcommands.
        let path = self.unverified_path(client_id, job_id);
        put_bytes(&self.store, &path, serde_json::to_vec_pretty(body)?).await
    }

    async fn list_unverified_client(
        &self,
        client_id: &ClientId,
    ) -> anyhow::Result<Vec<(JobId, serde_json::Value)>> {
        let prefix = self.unverified_client_prefix(client_id);
        let metas: Vec<_> = self.store.list(Some(&prefix)).try_collect().await?;
        let mut out = Vec::with_capacity(metas.len());
        for meta in metas {
            if !meta.location.as_ref().ends_with(".json") {
                continue;
            }
            let Some(job_id) = job_id_from_path(&meta.location) else {
                continue;
            };
            let Some(data) = get_bytes(&self.store, &meta.location).await? else {
                continue;
            };
            out.push((job_id, parse_json_blob(&data, Encoding::Plain)?));
        }
        Ok(out)
    }

    async fn delete_unverified(&self, client_id: &ClientId, job_id: &JobId) -> anyhow::Result<()> {
        let path = self.unverified_path(client_id, job_id);
        match self.store.delete(&path).await {
            Ok(()) => Ok(()),
            // Idempotent — `promote` may race a concurrent prune.
            Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    async fn delete_unverified_client(
        &self,
        client_id: &ClientId,
        dry_run: bool,
    ) -> anyhow::Result<usize> {
        let prefix = self.unverified_client_prefix(client_id);
        let mut deleted = 0usize;
        let mut listing = self.store.list(Some(&prefix));
        while let Some(meta) = listing.try_next().await? {
            if !meta.location.as_ref().ends_with(".json") {
                continue;
            }
            if !dry_run {
                self.store.delete(&meta.location).await?;
            }
            deleted += 1;
        }
        Ok(deleted)
    }

    async fn prune_unverified(
        &self,
        older_than: std::time::Duration,
        dry_run: bool,
    ) -> anyhow::Result<crate::stores::PruneSummary> {
        let prefix = obj_path(&self.prefix, "submissions/unverified/");
        let older_than = chrono::Duration::from_std(older_than)
            .map_err(|e| anyhow::anyhow!("prune age out of range: {e}"))?;
        // Cutoff against the object's `LastModified`, mirroring the
        // filesystem `mtime` semantics on local_fs.
        let cutoff = chrono::Utc::now() - older_than;

        let mut deleted = 0usize;
        let mut kept = 0usize;
        // Recurses across every `{client_id}/` sub-prefix — S3 `list` is
        // a flat key walk, so one prefix covers the whole tree.
        let mut listing = self.store.list(Some(&prefix));
        while let Some(meta) = listing.try_next().await? {
            // Skip anything that isn't a submission object (defensive —
            // the prefix should only ever hold `.json` blobs).
            if !meta.location.as_ref().ends_with(".json") {
                continue;
            }
            if meta.last_modified < cutoff {
                if !dry_run {
                    self.store.delete(&meta.location).await?;
                }
                deleted += 1;
            } else {
                kept += 1;
            }
        }
        Ok(crate::stores::PruneSummary { deleted, kept })
    }
}

// ---------------------------------------------------------------------------
// WarehouseStore
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct S3WarehouseStore {
    store: Arc<dyn ObjectStore>,
    prefix: String,
    read_days: u32,
    max_rows_per_part: usize,
    writer_opts: WriterOpts,
    tuning: S3Tuning,
}

impl S3WarehouseStore {
    fn partition_prefix(
        &self,
        benchmark_id: &BenchmarkId,
        client_id: &ClientId,
        day_key: &str,
    ) -> String {
        obj_path(
            &self.prefix,
            &format!(
                "warehouse/results/benchmark_id={benchmark_id}/client_id={client_id}/day={day_key}"
            ),
        )
        .to_string()
    }

    /// The highest-indexed `part-NNNN.parquet` object under the partition and
    /// its rows, or `None` when the partition is empty.
    async fn tail_part(
        &self,
        part_prefix: &str,
    ) -> anyhow::Result<Option<(usize, Vec<MetricRow>)>> {
        let prefix = ObjPath::from(format!("{part_prefix}/"));
        let entries: Vec<_> = self.store.list(Some(&prefix)).try_collect().await?;
        let best = entries
            .into_iter()
            .filter_map(|meta| {
                let idx = meta
                    .location
                    .as_ref()
                    .rsplit('/')
                    .next()
                    .and_then(warehouse::part_index)?;
                Some((idx, meta.location))
            })
            .max_by_key(|(idx, _)| *idx);

        match best {
            Some((idx, loc)) => {
                let data = get_bytes(&self.store, &loc)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("object vanished: {loc}"))?;
                Ok(Some((idx, warehouse::rows_from_parquet_bytes(&data)?)))
            }
            None => Ok(None),
        }
    }
}

#[async_trait]
impl WarehouseStore for S3WarehouseStore {
    async fn write_partition_metrics(
        &self,
        benchmark_id: &BenchmarkId,
        client_id: &ClientId,
        day_key: &str,
        rows: &[MetricRow],
    ) -> anyhow::Result<()> {
        anyhow::ensure!(!rows.is_empty(), "cannot write empty metric rows");
        let part_prefix = self.partition_prefix(benchmark_id, client_id, day_key);

        // Append-only (no dedup): top up the tail part to capacity, then roll
        // overflow into fresh parts. Earlier parts are never read or rewritten.
        // A PUT overwrites the tail object atomically. See docs/storage.md.
        let (mut next_index, mut carry) = match self.tail_part(&part_prefix).await? {
            Some((idx, tail)) if tail.len() < self.max_rows_per_part => (idx, tail),
            Some((idx, _)) => (idx + 1, Vec::new()),
            None => (1, Vec::new()),
        };

        carry.extend(rows.iter().cloned());
        for chunk in carry.chunks(self.max_rows_per_part) {
            let path = ObjPath::from(format!("{part_prefix}/part-{next_index:04}.parquet"));
            let data = warehouse::rows_to_parquet_bytes(self.writer_opts, chunk)?;
            put_bytes(&self.store, &path, data).await?;
            next_index += 1;
        }

        Ok(())
    }

    async fn read_job_metrics(
        &self,
        benchmark_id: &BenchmarkId,
        client_id: &ClientId,
        job_id: &JobId,
    ) -> anyhow::Result<Option<JobMetrics>> {
        let base = obj_path(
            &self.prefix,
            &format!("warehouse/results/benchmark_id={benchmark_id}/client_id={client_id}/"),
        );

        let list_result = self.store.list_with_delimiter(Some(&base)).await?;
        // Map each partition's Hive key ("day=YYYY-MM-DD" / legacy
        // "month=YYYY-MM") to its prefix, then window + order via the shared
        // selector (newest first).
        let by_key: std::collections::HashMap<String, ObjPath> = list_result
            .common_prefixes
            .into_iter()
            .filter_map(|p| {
                let key = p
                    .as_ref()
                    .trim_end_matches('/')
                    .rsplit('/')
                    .next()?
                    .to_string();
                Some((key, p))
            })
            .collect();
        let today = chrono::Utc::now().date_naive();
        let selected =
            warehouse::select_partitions_to_scan(by_key.keys().cloned(), self.read_days, today);

        // Partitions are scanned newest-first (days before months), so the
        // first one with a match holds the newest copy; stop there.
        // `from_latest_rows` then picks that job's newest scoring run, so an
        // append-only re-score duplicate resolves to its latest copy.
        let prefix = format!("{job_id}_");
        for key in &selected {
            let Some(part_prefix) = by_key.get(key) else {
                continue;
            };
            let matching = search_partition_for_job(&self.store, part_prefix, &prefix).await?;
            if !matching.is_empty() {
                return Ok(JobMetrics::from_latest_rows(&matching));
            }
        }
        Ok(None)
    }

    async fn for_each_metric_row(
        &self,
        f: &mut (dyn for<'a> FnMut(&'a mut MetricRow) -> bool + Send),
    ) -> anyhow::Result<()> {
        let prefix = obj_path(&self.prefix, "warehouse/results/");
        let parquet_keys: Vec<ObjPath> = self
            .store
            .list(Some(&prefix))
            .try_filter_map(|m| async move {
                Ok(m.location
                    .as_ref()
                    .ends_with(".parquet")
                    .then_some(m.location))
            })
            .try_collect()
            .await?;

        // Pipelined GET → apply-`f` → PUT. The closure `f` cannot be
        // cloned into worker tasks (single `&mut`), so we fan out only
        // the I/O and drive `f` serially as each load completes.
        // Writes start as soon as the first dirty file is detected so
        // peak in-flight memory is bounded to `~max * file_size`,
        // independent of how many files end up dirty.
        let max = self.tuning.max_concurrent_requests;
        let mut loads = stream::iter(parquet_keys)
            .map(|key| {
                let store = self.store.clone();
                async move {
                    let bytes = get_bytes(&store, &key)
                        .await?
                        .ok_or_else(|| anyhow::anyhow!("parquet vanished: {key}"))?;
                    let rows = warehouse::rows_from_parquet_bytes(&bytes)?;
                    anyhow::Ok((key, rows))
                }
            })
            .buffer_unordered(max);

        let mut puts = futures::stream::FuturesUnordered::new();
        while let Some(result) = loads.next().await {
            let (key, mut rows) = result?;
            if !rows.is_empty() {
                let mut dirty = false;
                rows.iter_mut().for_each(|row| {
                    if f(row) {
                        dirty = true;
                    }
                });
                if dirty {
                    let bytes = warehouse::rows_to_parquet_bytes(self.writer_opts, &rows)?;
                    let store = self.store.clone();
                    puts.push(async move {
                        store.put(&key, PutPayload::from(bytes)).await?;
                        anyhow::Ok(())
                    });
                }
            }
            // Drain in-flight PUTs whenever they pile up so peak memory
            // tracks `max`, not the total count of dirty files.
            while puts.len() >= max {
                match puts.next().await {
                    Some(r) => r?,
                    None => break,
                }
            }
        }
        while let Some(r) = puts.next().await {
            r?;
        }
        Ok(())
    }
}

/// Read every parquet part under `partition_prefix` and return rows whose
/// `result_id` starts with `job_prefix`. Helper for the partition scan in
/// [`S3WarehouseStore::read_job_metrics`].
async fn search_partition_for_job(
    store: &Arc<dyn ObjectStore>,
    partition_prefix: &ObjPath,
    job_prefix: &str,
) -> anyhow::Result<Vec<MetricRow>> {
    let part_prefix = ObjPath::from(format!(
        "{}/",
        partition_prefix.as_ref().trim_end_matches('/')
    ));
    let entries: Vec<_> = store.list(Some(&part_prefix)).try_collect().await?;

    let mut matching = Vec::new();
    for meta in &entries {
        if !meta.location.as_ref().ends_with(".parquet") {
            continue;
        }
        let data = get_bytes(store, &meta.location)
            .await?
            .ok_or_else(|| anyhow::anyhow!("object vanished: {}", meta.location))?;
        let rows = warehouse::rows_from_parquet_bytes(&data)?;
        matching.extend(
            rows.into_iter()
                .filter(|r| r.result_id.starts_with(job_prefix)),
        );
    }
    Ok(matching)
}

#[cfg(test)]
mod auth_store_tests {
    use super::*;
    use anyhow::Context;
    use chrono::TimeZone;
    use object_store::memory::InMemory;

    fn test_client(
        client_id: &str,
        public_key: &str,
        registered_at_secs: i64,
    ) -> anyhow::Result<Client> {
        Ok(Client {
            client_id: ClientId::try_new(client_id)?,
            public_key: crate::validated::PublicKeyHex::try_new(public_key)?,
            organization: crate::validated::NonEmptyTrimmedString::try_new("test-org")?,
            client_details: crate::validated::NonEmptyTrimmedString::try_new("details")?,
            contact_email: crate::validated::ContactEmail::try_new("a@b.com")?,
            status: crate::client::ClientStatus::Pending,
            registered_at: chrono::Utc
                .timestamp_opt(registered_at_secs, 0)
                .single()
                .context("valid timestamp")?,
            device_profile: Default::default(),
            capabilities: Default::default(),
        })
    }

    /// Here the first write wins by conditional create, i.e. `If-None-Match: *`.
    #[tokio::test]
    async fn s3_signature_migration_keeps_the_first_sighting() -> anyhow::Result<()> {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let auth_store = S3AuthStore {
            store: store.clone(),
            prefix: "test-prefix".to_string(),
        };
        crate::stores::assert_signature_migration_keeps_first_sighting(&auth_store).await
    }

    #[tokio::test]
    async fn s3_auth_store_list_clients_self_heals_legacy_records() -> anyhow::Result<()> {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let auth_store = S3AuthStore {
            store: store.clone(),
            prefix: "test-prefix".to_string(),
        };

        auth_store
            .put_client(&test_client(
                "ev1_valid",
                "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
                1_767_225_600,
            )?)
            .await?;

        put_bytes(
            &store,
            &obj_path("test-prefix", "clients/ev1_bad.json"),
            br#"{
  "client_id": "ev1_bad",
  "public_key": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
  "client_details": "",
  "contact_email": "bad@example.com",
  "status": "pending",
  "registered_at": "2026-01-01T00:00:00Z"
}"#
            .to_vec(),
        )
        .await?;

        let clients = auth_store.list_clients().await?;

        assert_eq!(clients.len(), 2);

        let repaired = get_bytes(&store, &obj_path("test-prefix", "clients/ev1_bad.json"))
            .await?
            .context("expected repaired record")?;
        let repaired: serde_json::Value = serde_json::from_slice(&repaired)?;
        assert_eq!(repaired["organization"], "<unset>");
        assert_eq!(repaired["client_details"], "legacy client");
        Ok(())
    }

    #[tokio::test]
    async fn s3_auth_store_get_client_self_heals_legacy_record() -> anyhow::Result<()> {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let auth_store = S3AuthStore {
            store: store.clone(),
            prefix: "test-prefix".to_string(),
        };
        let path = obj_path("test-prefix", "clients/ev1_bad.json");

        put_bytes(
            &store,
            &path,
            br#"{
  "client_id": "ev1_bad",
  "public_key": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
  "client_details": "",
  "contact_email": "bad@example.com",
  "status": "pending",
  "registered_at": "2026-01-01T00:00:00Z"
}"#
            .to_vec(),
        )
        .await?;

        let client = auth_store
            .get_client(&ClientId::try_new("ev1_bad")?)
            .await?
            .context("expected repaired client")?;
        assert_eq!(client.organization.as_str(), "<unset>");
        assert_eq!(client.client_details.as_str(), "legacy client");

        let repaired = get_bytes(&store, &path)
            .await?
            .context("expected repaired record")?;
        let repaired: serde_json::Value = serde_json::from_slice(&repaired)?;
        assert_eq!(repaired["organization"], "<unset>");
        assert_eq!(repaired["client_details"], "legacy client");
        Ok(())
    }

    #[tokio::test]
    async fn s3_auth_store_tag_markers() -> anyhow::Result<()> {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let auth_store = S3AuthStore {
            store: store.clone(),
            prefix: "test-prefix".to_string(),
        };

        let a = test_client(
            "ev1_a",
            "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456700",
            1,
        )?;
        let b = test_client(
            "ev1_b",
            "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456711",
            2,
        )?;
        auth_store.put_client(&a).await?;
        auth_store.put_client(&b).await?;

        crate::stores::assert_tag_store_roundtrip(&auth_store, &a.client_id, &b.client_id).await
    }
}

// ---------------------------------------------------------------------------
// EvalSampleResultStore
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct S3EvalSampleResultStore {
    store: Arc<dyn ObjectStore>,
    prefix: String,
    writer_opts: WriterOpts,
}

impl S3EvalSampleResultStore {
    fn esr_path(&self, job_id: &JobId) -> ObjPath {
        obj_path(
            &self.prefix,
            &format!("warehouse/eval_sample_results/{job_id}.parquet"),
        )
    }
}

#[async_trait]
impl EvalSampleResultStore for S3EvalSampleResultStore {
    async fn write(&self, job_id: &JobId, rows: &[EvalSampleResult]) -> anyhow::Result<()> {
        anyhow::ensure!(
            !rows.is_empty(),
            "refusing to write eval sample results with no rows"
        );
        let path = self.esr_path(job_id);
        let data = eval_sample_result::rows_to_parquet_bytes(self.writer_opts, rows)?;
        put_bytes(&self.store, &path, data).await
    }

    async fn read(&self, job_id: &JobId) -> anyhow::Result<Option<Vec<EvalSampleResult>>> {
        let path = self.esr_path(job_id);
        match get_bytes(&self.store, &path).await? {
            Some(data) => Ok(Some(eval_sample_result::rows_from_parquet_bytes(&data)?)),
            None => Ok(None),
        }
    }

    async fn list_job_ids(&self) -> anyhow::Result<Vec<JobId>> {
        let prefix = obj_path(&self.prefix, "warehouse/eval_sample_results/");
        let keys: Vec<ObjPath> = self
            .store
            .list(Some(&prefix))
            .try_filter_map(|m| async move {
                Ok(m.location
                    .as_ref()
                    .ends_with(".parquet")
                    .then_some(m.location))
            })
            .try_collect()
            .await?;
        // `filename()` includes the extension; strip `.parquet` for the job id.
        // Skip `.tmp-*` leftovers and any stem that isn't a valid job id.
        let job_ids = keys
            .into_iter()
            .filter_map(|key| {
                let name = key.filename().and_then(|n| n.strip_suffix(".parquet"))?;
                if name.starts_with(".tmp-") {
                    return None;
                }
                match JobId::try_new(name) {
                    Ok(job_id) => Some(job_id),
                    Err(e) => {
                        tracing::warn!(
                            key = %key,
                            error = %e,
                            "skipping eval sample results object with non-job-id name"
                        );
                        None
                    }
                }
            })
            .collect();
        Ok(job_ids)
    }
}

// ---------------------------------------------------------------------------
// TodoStore (all `aws-sdk-s3` — the atomic `RenameObject` claim path requires
// it, and one client for the whole store keeps auth/session resolution single-
// sourced and lets `aws-smithy-mocks` mock every op, renames included.)
// ---------------------------------------------------------------------------

/// Build the S3 `TodoStore`. Uses `aws-config`'s async credential chain (IAM
/// role / profile / env / IMDS), so this is `async` — unlike the `object_store`
/// builders. `endpoint` override forces path-style addressing for dev/mock
/// backends; real S3 Express uses the default virtual-hosted style.
pub async fn build_s3_todo_store(
    storage: &crate::config::StorageConfig,
) -> anyhow::Result<Arc<dyn TodoStore>> {
    let crate::config::StorageConfig::S3 {
        bucket,
        prefix,
        region,
        endpoint,
        ..
    } = storage
    else {
        anyhow::bail!("build_s3_todo_store called with non-S3 storage config");
    };

    let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest());
    if let Some(region) = region {
        loader = loader.region(aws_sdk_s3::config::Region::new(region.clone()));
    }
    let sdk_config = loader.load().await;

    let mut s3_conf = aws_sdk_s3::config::Builder::from(&sdk_config);
    if let Some(endpoint) = endpoint {
        s3_conf = s3_conf.endpoint_url(endpoint).force_path_style(true);
    }
    let client = aws_sdk_s3::Client::from_conf(s3_conf.build());

    Ok(Arc::new(S3TodoStore {
        client,
        bucket: bucket.clone(),
        prefix: prefix.trim_end_matches('/').to_string(),
    }))
}

struct S3TodoStore {
    client: aws_sdk_s3::Client,
    bucket: String,
    prefix: String,
}

impl S3TodoStore {
    /// Absolute object key: `{prefix}/todo/{suffix}` (or `todo/{suffix}` when the
    /// configured prefix is empty).
    fn key(&self, suffix: &str) -> String {
        if self.prefix.is_empty() {
            format!("todo/{suffix}")
        } else {
            format!("{}/todo/{suffix}", self.prefix)
        }
    }

    /// The listing prefix for a `todo/` subtree, e.g. `avail/` → the full key
    /// prefix whose stripped remainder is the relative key callers expect.
    fn list_prefix(&self, suffix: &str) -> String {
        self.key(suffix)
    }

    /// The `RenameObject` source value (`x-amz-rename-source`): `{bucket}/{key}`.
    /// The SDK wants it URL-encoded, but every key we generate is URL-safe
    /// (`[A-Za-z0-9_./-]`), so no percent-encoding is needed. **Not exercised by
    /// the mock tests** (they match on whatever we pass) — the exact on-wire
    /// format is validated only against a real S3 Express bucket.
    fn rename_source(&self, source_key: &str) -> String {
        format!("{}/{source_key}", self.bucket)
    }

    async fn get_bytes(&self, key: &str) -> anyhow::Result<Option<Vec<u8>>> {
        match self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
        {
            Ok(out) => Ok(Some(out.body.collect().await?.into_bytes().to_vec())),
            // `NoSuchKey` is a modeled variant on `GetObjectError` → match it
            // directly; anything else is a real failure.
            Err(e) => match e.into_service_error() {
                GetObjectError::NoSuchKey(_) => Ok(None),
                other => Err(other.into()),
            },
        }
    }

    async fn get_json(&self, key: &str) -> anyhow::Result<Option<serde_json::Value>> {
        match self.get_bytes(key).await? {
            Some(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            None => Ok(None),
        }
    }

    async fn put_bytes(&self, key: &str, bytes: Vec<u8>) -> anyhow::Result<()> {
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(ByteStream::from(bytes))
            .send()
            .await?;
        Ok(())
    }

    async fn put_empty(&self, key: &str) -> anyhow::Result<()> {
        self.put_bytes(key, Vec::new()).await
    }

    /// `DeleteObject` is idempotent on S3 — a missing key still returns success.
    async fn delete_key(&self, key: &str) -> anyhow::Result<()> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await?;
        Ok(())
    }

    /// List every object under `list_prefix`, returning `(relative_key,
    /// last_modified)` with `list_prefix` stripped. Walks continuation tokens —
    /// S3 Express supports neither `start-after` nor delimiters, so any offset
    /// filtering is applied by callers on the returned relative keys.
    async fn list_objects(
        &self,
        list_prefix: &str,
    ) -> anyhow::Result<Vec<(String, Option<DateTime<Utc>>)>> {
        let mut out = Vec::new();
        let mut continuation: Option<String> = None;
        loop {
            let mut req = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(list_prefix);
            if let Some(token) = &continuation {
                req = req.continuation_token(token);
            }
            let resp = req.send().await?;
            out.extend(resp.contents().iter().filter_map(|obj| {
                let rel = obj.key()?.strip_prefix(list_prefix)?;
                let modified = obj
                    .last_modified()
                    .and_then(|dt| DateTime::from_timestamp(dt.secs(), dt.subsec_nanos()));
                Some((rel.to_string(), modified))
            }));
            match resp.next_continuation_token() {
                Some(token) if resp.is_truncated() == Some(true) => {
                    continuation = Some(token.to_string())
                }
                _ => break,
            }
        }
        Ok(out)
    }

    /// Relative keys under `list_prefix` (last-modified dropped).
    async fn list_keys(&self, list_prefix: &str) -> anyhow::Result<Vec<String>> {
        Ok(self
            .list_objects(list_prefix)
            .await?
            .into_iter()
            .map(|(key, _)| key)
            .collect())
    }
}

/// Whether an operation error carries S3's `NoSuchKey` code. Used on the rename
/// path, where `RenameObjectError` has **no** modeled `NoSuchKey` variant — a
/// vanished source (lost claim race) surfaces only via the error code.
fn is_no_such_key<E: ProvideErrorMetadata>(err: &E) -> bool {
    err.code() == Some("NoSuchKey")
}

/// Read the recycle target's `expires_at` from a job body. `expires_at` is
/// optional in a job body (`planner.md`): absent or null means the job never
/// auto-expires, so recycle it to `avail/{job_id}.never.json`. Only a *present*
/// value that isn't a parseable timestamp/`never` is a corrupt body worth
/// failing on. Mirrors the local_fs `recycle_lease`.
fn expires_at_from_body(body: &serde_json::Value) -> anyhow::Result<ExpiresAt> {
    match body.get("expires_at") {
        None | Some(serde_json::Value::Null) => Ok(ExpiresAt::Never),
        Some(v) => v
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("`expires_at` in job body is not a string"))?
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid expires_at in job body: {e}")),
    }
}

#[async_trait]
impl TodoStore for S3TodoStore {
    async fn list_avail(
        &self,
        start_after: Option<&str>,
        limit: NonZeroUsize,
    ) -> anyhow::Result<Vec<String>> {
        let mut names: Vec<String> = self
            .list_keys(&self.list_prefix("avail/"))
            .await?
            .into_iter()
            .filter(|n| n.ends_with(".json"))
            .collect();
        names.sort_unstable();
        // Express has no server-side `start-after`; filter client-side.
        if let Some(after) = start_after {
            names.retain(|n| n.as_str() > after);
        }
        names.truncate(limit.get());
        Ok(names)
    }

    async fn get_avail(
        &self,
        job_id: &JobId,
        expires_at: ExpiresAt,
    ) -> anyhow::Result<Option<serde_json::Value>> {
        self.get_json(&self.key(&format!("avail/{}", avail_filename(job_id, expires_at))))
            .await
    }

    async fn delete_avail(&self, job_id: &JobId, expires_at: ExpiresAt) -> anyhow::Result<()> {
        self.delete_key(&self.key(&format!("avail/{}", avail_filename(job_id, expires_at))))
            .await
    }

    async fn delete_avail_by_job(&self, job_id: &JobId) -> anyhow::Result<()> {
        let names = self.list_keys(&self.list_prefix("avail/")).await?;
        let job_prefix = format!("{job_id}.");
        for name in names
            .into_iter()
            .filter(|n| n.starts_with(&job_prefix) && n.ends_with(".json"))
        {
            self.delete_key(&self.key(&format!("avail/{name}"))).await?;
        }
        Ok(())
    }

    async fn get_avail_by_job(&self, job_id: &JobId) -> anyhow::Result<Option<serde_json::Value>> {
        let names = self.list_keys(&self.list_prefix("avail/")).await?;
        let job_prefix = format!("{job_id}.");
        let Some(name) = names
            .into_iter()
            .find(|n| n.starts_with(&job_prefix) && n.ends_with(".json"))
        else {
            return Ok(None);
        };
        self.get_json(&self.key(&format!("avail/{name}"))).await
    }

    async fn list_leased(&self) -> anyhow::Result<Vec<String>> {
        // Keys under `leased/` are `{client_id}/{job_id}.{expiry}.json`; the flat
        // S3 listing already returns exactly those relative keys.
        Ok(self
            .list_keys(&self.list_prefix("leased/"))
            .await?
            .into_iter()
            .filter(|n| n.ends_with(".json"))
            .collect())
    }

    async fn list_leased_for_client(&self, client_id: &ClientId) -> anyhow::Result<Vec<String>> {
        let client = client_id.as_str();
        Ok(self
            .list_keys(&self.list_prefix(&format!("leased/{client}/")))
            .await?
            .into_iter()
            .filter(|leaf| leaf.ends_with(".json"))
            .map(|leaf| format!("{client}/{leaf}"))
            .collect())
    }

    async fn get_leased(
        &self,
        job_id: &JobId,
        client_id: &ClientId,
        lease_expiry: DateTime<Utc>,
    ) -> anyhow::Result<Option<serde_json::Value>> {
        self.get_json(&self.key(&format!(
            "leased/{}",
            leased_key(job_id, client_id, lease_expiry)
        )))
        .await
    }

    async fn renew_lease(
        &self,
        job_id: &JobId,
        client_id: &ClientId,
        new_expiry: DateTime<Utc>,
    ) -> anyhow::Result<RenewLeaseResult> {
        let job_prefix = format!("{job_id}.");

        // 1. Locate this client's current lease (own partition only). The
        //    relative keys are `{client}/{leaf}`; match the leaf on the job
        //    prefix. Renewal predicate is lease *existence*, not expiry — an
        //    expired-but-not-yet-recycled lease still renews (recycling is the
        //    only thing that ends a lease, and step 2 tolerates losing that race).
        let own = self
            .list_leased_for_client(client_id)
            .await?
            .into_iter()
            .find(|rel| {
                rel.rsplit('/')
                    .next()
                    .is_some_and(|leaf| leaf.starts_with(&job_prefix))
            });
        if let Some(rel) = own {
            let src = self.key(&format!("leased/{rel}"));
            let dst = self.key(&format!(
                "leased/{}",
                leased_key(job_id, client_id, new_expiry)
            ));
            match self
                .client
                .rename_object()
                .bucket(&self.bucket)
                .key(&dst)
                .rename_source(self.rename_source(&src))
                .send()
                .await
            {
                Ok(_) => return Ok(RenewLeaseResult::Renewed),
                Err(e) => {
                    let err = e.into_service_error();
                    // Raced a recycle between the list and the rename; fall
                    // through to the cross-partition check. Anything else fails.
                    if !is_no_such_key(&err) {
                        return Err(err.into());
                    }
                }
            }
        }

        // 2. This client holds no lease for the job. Distinguish `WrongClient`
        //    (held elsewhere) from `NotFound` (gone) with a full sweep — only on
        //    this miss path. `list_leased` is a single listing that either
        //    returns every key or errors (propagated), so a read failure can
        //    never be mistaken for `NotFound` — the sound direction for this
        //    asymmetric negative (planner.md, and the soundness note in the
        //    local_fs `renew_lease`).
        let held_elsewhere = self.list_leased().await?.into_iter().any(|rel| {
            matches!(
                rel.split_once('/'),
                Some((owner, leaf)) if owner != client_id.as_str() && leaf.starts_with(&job_prefix)
            )
        });

        Ok(if held_elsewhere {
            RenewLeaseResult::WrongClient
        } else {
            RenewLeaseResult::NotFound
        })
    }

    async fn delete_lease(
        &self,
        job_id: &JobId,
        client_id: &ClientId,
        expiry: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        self.delete_key(&self.key(&format!("leased/{}", leased_key(job_id, client_id, expiry))))
            .await
    }

    async fn claim_job(
        &self,
        job_id: &JobId,
        expires_at: ExpiresAt,
        client_id: &ClientId,
        lease_expiry: DateTime<Utc>,
    ) -> anyhow::Result<ClaimResult> {
        let src = self.key(&format!("avail/{}", avail_filename(job_id, expires_at)));
        let dst = self.key(&format!(
            "leased/{}",
            leased_key(job_id, client_id, lease_expiry)
        ));

        // Atomic move: the *source* is the contention point — a second claimant's
        // rename of the now-gone source fails with `NoSuchKey` → `Gone`.
        match self
            .client
            .rename_object()
            .bucket(&self.bucket)
            .key(&dst)
            .rename_source(self.rename_source(&src))
            .send()
            .await
        {
            Ok(_) => {
                // `RenameObject` returns no body, but the handler needs the job
                // JSON. Read the now-leased object. This second GET is safe: the
                // lease is already ours, so nothing else can move it out from
                // under us before we read it.
                let body = self.get_json(&dst).await?.ok_or_else(|| {
                    anyhow::anyhow!("claimed job body missing after rename: {dst}")
                })?;
                Ok(ClaimResult::Claimed(body))
            }
            Err(e) => {
                let err = e.into_service_error();
                if is_no_such_key(&err) {
                    Ok(ClaimResult::Gone)
                } else {
                    Err(err.into())
                }
            }
        }
    }

    async fn recycle_lease(
        &self,
        job_id: &JobId,
        client_id: &ClientId,
        lease_expiry: DateTime<Utc>,
    ) -> anyhow::Result<RecycleResult> {
        // Retry-safe, not atomic (planner.md): GET the body for its `expires_at`,
        // then rename `leased/ → avail/`. A failure at either step leaves no
        // partial state — the lease is untouched or fully moved — so a retry is
        // clean. A source missing at either step is `Gone` (someone else
        // resolved the lease between the two), not an error.
        let src = self.key(&format!(
            "leased/{}",
            leased_key(job_id, client_id, lease_expiry)
        ));
        let Some(body) = self.get_json(&src).await? else {
            return Ok(RecycleResult::Gone);
        };
        let expires_at = expires_at_from_body(&body)?;
        let dst = self.key(&format!("avail/{}", avail_filename(job_id, expires_at)));
        match self
            .client
            .rename_object()
            .bucket(&self.bucket)
            .key(&dst)
            .rename_source(self.rename_source(&src))
            .send()
            .await
        {
            Ok(_) => Ok(RecycleResult::Recycled),
            Err(e) => {
                let err = e.into_service_error();
                if is_no_such_key(&err) {
                    Ok(RecycleResult::Gone)
                } else {
                    Err(err.into())
                }
            }
        }
    }

    async fn write_denied(&self, job_id: &JobId, client_id: &ClientId) -> anyhow::Result<()> {
        self.put_empty(&self.key(&format!("denied/{job_id}.{client_id}")))
            .await
    }

    async fn list_denied_for_job(&self, job_id: &JobId) -> anyhow::Result<Vec<ClientId>> {
        let prefix = format!("{job_id}.");
        self.list_keys(&self.list_prefix("denied/"))
            .await?
            .into_iter()
            .filter_map(|name| name.strip_prefix(&prefix).map(str::to_owned))
            .map(|rest| Ok(ClientId::try_new(rest)?))
            .collect()
    }

    async fn delete_denied_for_job(&self, job_id: &JobId) -> anyhow::Result<()> {
        let prefix = format!("{job_id}.");
        let names = self.list_keys(&self.list_prefix("denied/")).await?;
        for name in names.into_iter().filter(|n| n.starts_with(&prefix)) {
            self.delete_key(&self.key(&format!("denied/{name}")))
                .await?;
        }
        Ok(())
    }

    async fn delete_denied(&self, job_id: &JobId, client_id: &ClientId) -> anyhow::Result<()> {
        self.delete_key(&self.key(&format!("denied/{job_id}.{client_id}")))
            .await
    }

    async fn list_all_denied(&self) -> anyhow::Result<Vec<(JobId, ClientId)>> {
        // Log and skip malformed names — an anomaly to surface, not a fatal
        // error.
        let names = self.list_keys(&self.list_prefix("denied/")).await?;
        let markers = names
            .into_iter()
            .filter_map(|name| {
                parse_denied_marker(&name)
                    .inspect_err(|e| {
                        tracing::warn!(marker = %name, error = %e, "skipping malformed denied marker");
                    })
                    .ok()
            })
            .collect();
        Ok(markers)
    }

    async fn list_eligible_for_client(
        &self,
        client_id: &ClientId,
    ) -> anyhow::Result<Vec<(JobId, ExpiresAt)>> {
        let names = self
            .list_keys(&self.list_prefix(&format!("eligible/clients/{}/", client_id.as_str())))
            .await?;
        Ok(names
            .into_iter()
            .filter_map(|name| match parse_eligible_filename(&name) {
                Ok(parsed) => Some(parsed),
                Err(e) => {
                    tracing::warn!(marker = %name, client_id = %client_id, error = %e, "skipping malformed eligible marker");
                    None
                }
            })
            .collect())
    }

    async fn write_eligible(
        &self,
        client_id: &ClientId,
        job_id: &JobId,
        expires_at: ExpiresAt,
    ) -> anyhow::Result<()> {
        self.put_empty(&self.key(&format!(
            "eligible/clients/{}/{}",
            client_id.as_str(),
            eligible_filename(job_id, expires_at)
        )))
        .await
    }

    async fn delete_eligible_for_client(&self, client_id: &ClientId) -> anyhow::Result<()> {
        let list_prefix = self.list_prefix(&format!("eligible/clients/{}/", client_id.as_str()));
        let names = self.list_keys(&list_prefix).await?;
        for name in names {
            self.delete_key(&format!("{list_prefix}{name}")).await?;
        }
        Ok(())
    }

    async fn delete_eligible(
        &self,
        client_id: &ClientId,
        job_id: &JobId,
        expires_at: ExpiresAt,
    ) -> anyhow::Result<()> {
        self.delete_key(&self.key(&format!(
            "eligible/clients/{}/{}",
            client_id.as_str(),
            eligible_filename(job_id, expires_at)
        )))
        .await
    }

    async fn list_all_eligible(&self) -> anyhow::Result<Vec<(ClientId, JobId, ExpiresAt)>> {
        // Keys are `{client_id}/{eligible_filename}`; split on the first `/`
        // (client_id has none). Log and skip malformed names.
        let names = self
            .list_keys(&self.list_prefix("eligible/clients/"))
            .await?;
        let markers = names
            .into_iter()
            .filter_map(|name| {
                let Some((client_id, filename)) = name.split_once('/') else {
                    tracing::warn!(marker = %name, "skipping malformed eligible marker (no '/')");
                    return None;
                };
                let client_id = match ClientId::try_new(client_id) {
                    Ok(id) => id,
                    Err(e) => {
                        tracing::warn!(marker = %name, error = %e, "skipping eligible marker with invalid client_id");
                        return None;
                    }
                };
                match parse_eligible_filename(filename) {
                    Ok((job_id, expires_at)) => Some((client_id, job_id, expires_at)),
                    Err(e) => {
                        tracing::warn!(marker = %name, error = %e, "skipping malformed eligible marker");
                        None
                    }
                }
            })
            .collect();
        Ok(markers)
    }

    async fn write_pending_reindex(&self, client_id: &ClientId) -> anyhow::Result<()> {
        self.put_empty(&self.key(&format!(
            "pending-reindex/{}",
            crate::todo_filename::pending_reindex_filename(client_id)
        )))
        .await
    }

    async fn list_pending_reindex(&self) -> anyhow::Result<Vec<(ClientId, String)>> {
        // Listing errors propagate (the gate must not lose a flag silently);
        // an unparseable *name* is foreign cruft — warned and skipped, per
        // the trait contract.
        Ok(self
            .list_keys(&self.list_prefix("pending-reindex/"))
            .await?
            .into_iter()
            .filter_map(
                |name| match crate::todo_filename::parse_pending_reindex_filename(&name) {
                    Ok(client_id) => Some((client_id, name)),
                    Err(_) => {
                        tracing::warn!(key = %name, "skipping unparseable pending-reindex flag");
                        None
                    }
                },
            )
            .collect())
    }

    async fn delete_pending_reindex(&self, key: &str) -> anyhow::Result<()> {
        self.delete_key(&self.key(&format!("pending-reindex/{key}")))
            .await
    }

    async fn has_pending_reindex(&self, client_id: &ClientId) -> anyhow::Result<bool> {
        // A full-prefix list, then parse-and-compare: a `pending-reindex/
        // {client_id}` LIST prefix alone would also match another client
        // whose id merely extends this one (ids share a charset with the
        // separator). Flags are bounded by the PATCH rate per cron interval,
        // so the listing stays small.
        Ok(self
            .list_pending_reindex()
            .await?
            .iter()
            .any(|(flagged, _)| flagged == client_id))
    }

    async fn write_pending_reindex_job(&self, job_id: &JobId) -> anyhow::Result<()> {
        self.put_empty(&self.key(&format!("pending-reindex-jobs/{}", job_id.as_str())))
            .await
    }

    async fn list_pending_reindex_jobs(&self) -> anyhow::Result<Vec<JobId>> {
        // Listing errors propagate (a lost flag is lost reindex debt); an
        // unparseable *name* is foreign cruft, not a flag — every
        // system-written flag is a valid `JobId` by construction — so it is
        // warned and skipped rather than wedging every maintenance run until
        // an operator deletes the object.
        Ok(self
            .list_keys(&self.list_prefix("pending-reindex-jobs/"))
            .await?
            .into_iter()
            .filter_map(|name| match JobId::try_new(&name) {
                Ok(job_id) => Some(job_id),
                Err(_) => {
                    tracing::warn!(key = %name, "skipping unparseable pending-reindex-jobs entry");
                    None
                }
            })
            .collect())
    }

    async fn delete_pending_reindex_job(&self, job_id: &JobId) -> anyhow::Result<()> {
        self.delete_key(&self.key(&format!("pending-reindex-jobs/{}", job_id.as_str())))
            .await
    }

    async fn write_tmp(&self, job_id: &JobId, body: &serde_json::Value) -> anyhow::Result<()> {
        let key = self.key(&format!("tmp/{}", tmp_filename(job_id)));
        self.put_bytes(&key, serde_json::to_vec(body)?).await
    }

    async fn promote_avail(&self, job_id: &JobId, expires_at: ExpiresAt) -> anyhow::Result<()> {
        // Atomic `RenameObject` (S3 Express One Zone), exactly as `claim_job`:
        // the staged body appears in `avail/` whole or not at all, so a partial
        // write is never claimable. A vanished source (a bug: the caller staged
        // it immediately before) surfaces as an error, not a silent no-op.
        let src = self.key(&format!("tmp/{}", tmp_filename(job_id)));
        let dst = self.key(&format!("avail/{}", avail_filename(job_id, expires_at)));
        self.client
            .rename_object()
            .bucket(&self.bucket)
            .key(&dst)
            .rename_source(self.rename_source(&src))
            .send()
            .await?;
        Ok(())
    }

    async fn list_stale_tmp(&self, age: Duration) -> anyhow::Result<Vec<String>> {
        let age = chrono::Duration::from_std(age)
            .map_err(|e| anyhow::anyhow!("stale age out of range: {e}"))?;
        let cutoff = Utc::now() - age;
        Ok(self
            .list_objects(&self.list_prefix("tmp/"))
            .await?
            .into_iter()
            .filter(|(_, modified)| modified.is_some_and(|m| m < cutoff))
            .map(|(name, _)| name)
            .collect())
    }

    async fn delete_tmp_object(&self, key: &str) -> anyhow::Result<()> {
        self.delete_key(&self.key(&format!("tmp/{key}"))).await
    }

    async fn read_eligible_cursor(&self) -> anyhow::Result<Option<String>> {
        match self.get_bytes(&self.key(".eligible-cursor")).await? {
            Some(bytes) => Ok(Some(String::from_utf8(bytes)?.trim().to_owned())),
            None => Ok(None),
        }
    }

    async fn write_eligible_cursor(&self, key: &str) -> anyhow::Result<()> {
        self.put_bytes(&self.key(".eligible-cursor"), key.as_bytes().to_vec())
            .await
    }

    async fn read_gc_candidates(&self) -> anyhow::Result<HashSet<String>> {
        match self.get_bytes(&self.key(".gc-candidates")).await? {
            Some(bytes) => Ok(crate::stores::parse_gc_candidates(&bytes)),
            None => Ok(HashSet::new()),
        }
    }

    async fn write_gc_candidates(&self, candidates: &HashSet<String>) -> anyhow::Result<()> {
        self.put_bytes(&self.key(".gc-candidates"), serde_json::to_vec(candidates)?)
            .await
    }

    async fn write_suspension(
        &self,
        client_id: &ClientId,
        suspended_at: DateTime<Utc>,
        conflicting_job_id: &JobId,
    ) -> anyhow::Result<()> {
        let record = SuspensionRecord {
            suspended_at,
            conflicting_job_id: conflicting_job_id.clone(),
        };
        let body = serde_json::to_vec(&record)?;
        self.put_bytes(
            &self.key(&format!("suspended/{}.json", client_id.as_str())),
            body,
        )
        .await
    }

    async fn read_suspension(
        &self,
        client_id: &ClientId,
    ) -> anyhow::Result<Option<SuspensionRecord>> {
        match self
            .get_bytes(&self.key(&format!("suspended/{}.json", client_id.as_str())))
            .await?
        {
            Some(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            None => Ok(None),
        }
    }

    async fn delete_suspension(&self, client_id: &ClientId) -> anyhow::Result<()> {
        self.delete_key(&self.key(&format!("suspended/{}.json", client_id.as_str())))
            .await
    }

    async fn list_suspensions(&self) -> anyhow::Result<Vec<(ClientId, SuspensionRecord)>> {
        let names = self.list_keys(&self.list_prefix("suspended/")).await?;
        let mut result = Vec::new();
        for name in names {
            let Some(id_str) = name.strip_suffix(".json") else {
                continue;
            };
            let client_id = ClientId::try_new(id_str)?;
            let Some(bytes) = self
                .get_bytes(&self.key(&format!("suspended/{name}")))
                .await?
            else {
                continue;
            };
            result.push((client_id, serde_json::from_slice(&bytes)?));
        }
        Ok(result)
    }

    async fn validate_backend(&self) -> anyhow::Result<()> {
        // Probe `RenameObject` against a source that does not exist. The source
        // key is randomized per probe: `tmp/` is a live staging namespace (the
        // planner writes job files there), and a fixed key could collide with a
        // real object — which the probe would then move. On an S3 Express One
        // Zone directory bucket the operation is supported and the absent source
        // yields `NoSuchKey` — nothing is moved, so the probe is a no-op. A
        // general-purpose bucket does not implement the API and returns
        // `NotImplemented`; accepting one would make `claim_job` a non-atomic
        // copy-then-delete and let two clients win the same job, so it is fatal.
        // Any other error (auth, missing bucket, network) is a real operational
        // fault and propagates unchanged rather than masquerading as a bucket-type
        // problem.
        //
        // The probe proves the endpoint *answers* `RenameObject`, not that its
        // rename is atomic; on AWS the two coincide (only Express One Zone
        // implements the API). A *success* for the absent source is therefore
        // also fatal: a conforming backend must return `NoSuchKey`, so a 200
        // here marks an S3-compatible endpoint whose rename semantics can't be
        // trusted for atomic claims.
        let probe = format!("tmp/.rename-probe-{}", uuid::Uuid::new_v4());
        let src = self.key(&probe);
        let dst = self.key(&format!("{probe}.moved"));
        match self
            .client
            .rename_object()
            .bucket(&self.bucket)
            .key(&dst)
            .rename_source(self.rename_source(&src))
            .send()
            .await
        {
            Ok(_) => Err(anyhow::anyhow!(
                "todo_storage bucket `{}`: RenameObject of a nonexistent source \
                 unexpectedly succeeded, so this endpoint's rename semantics can't \
                 be trusted for atomic claims; it must be an S3 Express One Zone \
                 directory bucket (which returns NoSuchKey here) — see \
                 docs/operations.md",
                self.bucket,
            )),
            Err(e) => {
                let err = e.into_service_error();
                match err.code() {
                    Some("NoSuchKey") => {
                        tracing::info!(
                            bucket = %self.bucket,
                            "todo storage validated: RenameObject supported (S3 Express One Zone)"
                        );
                        Ok(())
                    }
                    Some("NotImplemented") => Err(anyhow::anyhow!(
                        "todo_storage bucket `{}` does not support the atomic RenameObject \
                         operation (returned NotImplemented); it must be an S3 Express One \
                         Zone directory bucket — see docs/operations.md",
                        self.bucket,
                    )),
                    _ => Err(err.into()),
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests (using in-memory ObjectStore)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use crate::TEST_LIST_LIMIT;
    use crate::stores::s3::*;
    use crate::types::{BenchmarkId, ClientId, JobId};
    use anyhow::Context;

    fn make_test_stores(prefix: &str) -> Stores {
        let store: Arc<dyn ObjectStore> = Arc::new(object_store::memory::InMemory::new());
        let prefix = prefix.to_string();
        Stores {
            catalog: Arc::new(S3CatalogStore {
                store: store.clone(),
                prefix: prefix.clone(),
            }),
            auth: Arc::new(S3AuthStore {
                store: store.clone(),
                prefix: prefix.clone(),
            }),
            submissions: Arc::new(S3SubmissionStore {
                store: store.clone(),
                prefix: prefix.clone(),
            }),
            warehouse: Arc::new(S3WarehouseStore {
                store: store.clone(),
                prefix: prefix.clone(),
                read_days: 36_500,
                max_rows_per_part: 10_000,
                writer_opts: WriterOpts::default(),
                tuning: S3Tuning {
                    max_concurrent_requests: 32,
                },
            }),
            eval_sample_results: Arc::new(S3EvalSampleResultStore {
                store,
                prefix,
                writer_opts: WriterOpts::default(),
            }),
            todo: Arc::new(crate::stores::TodoStoreUnimplemented),
            plans: Arc::new(crate::stores::PlanStoreUnimplemented),
        }
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
    async fn test_s3_submission_write_list_find_and_mark_processed() -> anyhow::Result<()> {
        let stores = make_test_stores("");
        let sub = stores.submissions.as_ref();
        let job_id = JobId::new_unchecked("job1");
        let body = sample_submission(job_id.as_str(), "ev1_client1");

        sub.write_incoming(&job_id, &body).await?;

        let incoming = sub.list_incoming(TEST_LIST_LIMIT).await?;
        assert_eq!(incoming.len(), 1);
        assert_eq!(incoming[0], job_id);

        let record = sub
            .get_submission(&job_id)
            .await?
            .context("expected submission")?;
        assert_eq!(record.state, JobState::Incoming);

        let found = sub.find_job(&job_id).await?.context("expected job1")?;
        assert_eq!(found.state, JobState::Incoming);

        sub.mark_processed(&job_id).await?;

        assert!(sub.list_incoming(TEST_LIST_LIMIT).await?.is_empty());
        let processed = sub
            .get_submission(&job_id)
            .await?
            .context("expected processed")?;
        assert_eq!(processed.state, JobState::Processed);

        Ok(())
    }

    #[tokio::test]
    async fn test_s3_mark_processed_streams_large_payload() -> anyhow::Result<()> {
        // ~1 MB pretty-printed JSON exercises the chunk-by-chunk gzip
        // streaming in mark_processed and catches encoder-state
        // regressions on a non-trivial payload.
        let stores = make_test_stores("");
        let sub = stores.submissions.as_ref();
        let job_id = JobId::new_unchecked("jobL");
        let mut body = sample_submission(job_id.as_str(), "ev1_clientL");
        body["padding"] = serde_json::Value::String("x".repeat(1_000_000));

        sub.write_incoming(&job_id, &body).await?;
        sub.mark_processed(&job_id).await?;

        let processed = sub
            .get_submission(&job_id)
            .await?
            .context("expected processed")?;
        assert_eq!(processed.state, JobState::Processed);
        assert_eq!(processed.body, body);
        Ok(())
    }

    #[tokio::test]
    async fn test_s3_submission_get_nonexistent() -> anyhow::Result<()> {
        let stores = make_test_stores("");
        assert!(
            stores
                .submissions
                .get_submission(&JobId::new_unchecked("z"))
                .await?
                .is_none()
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_s3_submission_find_nonexistent() -> anyhow::Result<()> {
        let stores = make_test_stores("");
        assert!(
            stores
                .submissions
                .find_job(&JobId::new_unchecked("j"))
                .await?
                .is_none()
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_s3_warehouse_write_and_read() -> anyhow::Result<()> {
        let stores = make_test_stores("");
        let wh = stores.warehouse.as_ref();
        let rows = vec![
            make_metric_row("job1_0", "ttft", 34.7, Some(1.2), "ms")?,
            make_metric_row("job1_1", "prefill_throughput", 7377.5, None, "tokens/sec")?,
        ];
        wh.write_partition_metrics(
            &BenchmarkId::try_new("prefill_throughput_256")?,
            &ClientId::try_new("ev1_client1")?,
            "2025-03-10",
            &rows,
        )
        .await?;

        let metrics = wh
            .read_job_metrics(
                &BenchmarkId::try_new("prefill_throughput_256")?,
                &ClientId::try_new("ev1_client1")?,
                &JobId::new_unchecked("job1"),
            )
            .await?
            .context("expected metrics")?;
        assert_eq!(metrics.metrics.len(), 2);
        assert_eq!(metrics.metrics[0].metric, "ttft");
        Ok(())
    }

    #[tokio::test]
    async fn test_s3_warehouse_read_keeps_latest_on_rescore() -> anyhow::Result<()> {
        // S3 append + read must resolve a re-scored job to its newest copy
        // (by scored_at), not return duplicate/stale rows.
        let stores = make_test_stores("");
        let wh = stores.warehouse.as_ref();
        let bid = BenchmarkId::try_new("prefill_throughput_256")?;
        let cid = ClientId::try_new("ev1_client1")?;

        let mut c1 = make_metric_row("job1_0", "ttft", 34.7, Some(1.2), "ms")?;
        c1.scored_at = 1_000_000_000;
        wh.write_partition_metrics(&bid, &cid, "2025-03-10", &[c1])
            .await?;

        let mut c2 = make_metric_row("job1_0", "ttft", 40.0, Some(2.5), "ms")?;
        c2.scored_at = 2_000_000_000;
        wh.write_partition_metrics(&bid, &cid, "2025-03-10", &[c2])
            .await?;

        let metrics = wh
            .read_job_metrics(&bid, &cid, &JobId::new_unchecked("job1"))
            .await?
            .context("expected metrics")?;
        assert_eq!(
            metrics.metrics.len(),
            1,
            "duplicate copy must not be returned"
        );
        assert_eq!(metrics.metrics[0].value, 40.0, "newest copy wins");
        Ok(())
    }

    #[tokio::test]
    async fn test_s3_warehouse_read_nonexistent() -> anyhow::Result<()> {
        let stores = make_test_stores("");
        assert!(
            stores
                .warehouse
                .read_job_metrics(
                    &BenchmarkId::try_new("x")?,
                    &ClientId::try_new("y")?,
                    &JobId::new_unchecked("z"),
                )
                .await?
                .is_none()
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_s3_warehouse_write_empty_rows_errors() -> anyhow::Result<()> {
        let stores = make_test_stores("");
        let result = stores
            .warehouse
            .write_partition_metrics(
                &BenchmarkId::try_new("x")?,
                &ClientId::try_new("y")?,
                "2025-03-10",
                &[],
            )
            .await;
        assert!(result.is_err());
        Ok(())
    }

    /// Regression test: `read_job_metrics` must aggregate matching rows
    /// across **all** part files in a month. The pre-parallelization S3
    /// implementation returned on the first part file with any matches,
    /// silently dropping rows that lived in later parts.
    #[tokio::test]
    async fn test_s3_read_job_metrics_aggregates_across_part_files() -> anyhow::Result<()> {
        // Force a multi-part layout by setting max_rows_per_part = 1 so each
        // row lands in its own part-NNNN.parquet.
        let store: Arc<dyn ObjectStore> = Arc::new(object_store::memory::InMemory::new());
        let wh = S3WarehouseStore {
            store: store.clone(),
            prefix: String::new(),
            read_days: 36_500,
            max_rows_per_part: 1,
            writer_opts: WriterOpts::default(),
            tuning: S3Tuning {
                max_concurrent_requests: 32,
            },
        };

        // Two metrics for the same job_id; with max_rows_per_part=1 they
        // land in part-0001.parquet and part-0002.parquet respectively.
        let rows = vec![
            make_metric_row("job1_0", "ttft", 30.0, None, "ms")?,
            make_metric_row("job1_1", "decode_throughput", 100.0, None, "tokens/sec")?,
        ];
        wh.write_partition_metrics(
            &BenchmarkId::try_new("bench1")?,
            &ClientId::try_new("ev1_client1")?,
            "2025-03-10",
            &rows,
        )
        .await?;

        // Confirm the layout actually has two part files (otherwise this
        // test would silently pass for the wrong reason).
        let part_prefix = ObjPath::from(
            "warehouse/results/benchmark_id=bench1/client_id=ev1_client1/day=2025-03-10/",
        );
        let entries: Vec<_> = store.list(Some(&part_prefix)).try_collect().await?;
        assert_eq!(
            entries.len(),
            2,
            "expected 2 part files for max_rows_per_part=1 + 2 rows"
        );

        // The bug being locked in: must return BOTH metrics, not just one.
        let metrics = wh
            .read_job_metrics(
                &BenchmarkId::try_new("bench1")?,
                &ClientId::try_new("ev1_client1")?,
                &JobId::new_unchecked("job1"),
            )
            .await?
            .context("expected metrics for job1")?;

        assert_eq!(
            metrics.metrics.len(),
            2,
            "should aggregate across all part files, got {} metric(s)",
            metrics.metrics.len()
        );
        let names: Vec<&str> = metrics.metrics.iter().map(|m| m.metric.as_str()).collect();
        assert!(names.contains(&"ttft"), "missing ttft from {names:?}");
        assert!(
            names.contains(&"decode_throughput"),
            "missing decode_throughput from {names:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_s3_eval_sample_result_write_and_read() -> anyhow::Result<()> {
        let stores = make_test_stores("");
        let esr = stores.eval_sample_results.as_ref();
        let rows = vec![
            EvalSampleResult {
                id: "s1".to_string(),
                messages: r#"[{"role":"user","content":"Q1"}]"#.to_string(),
                completion: "A".to_string(),
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
                messages: r#"[{"role":"user","content":"Q2"}]"#.to_string(),
                completion: "B".to_string(),
                is_correct: false,
                failed: false,
                failed_reason: None,
                stop_reason: None,
                stop_reason_source: None,
                stop_detail: None,
                completion_tokens: None,
            },
        ];
        esr.write(&JobId::new_unchecked("job1"), &rows).await?;

        let result = esr
            .read(&JobId::new_unchecked("job1"))
            .await?
            .context("expected results")?;
        assert_eq!(result.len(), 2);
        assert!(result[0].is_correct);
        assert!(!result[1].is_correct);
        Ok(())
    }

    #[tokio::test]
    async fn test_s3_eval_sample_result_read_nonexistent() -> anyhow::Result<()> {
        let stores = make_test_stores("");
        assert!(
            stores
                .eval_sample_results
                .read(&JobId::new_unchecked("z"))
                .await?
                .is_none()
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_s3_with_prefix() -> anyhow::Result<()> {
        let stores = make_test_stores("v1");
        let sub = stores.submissions.as_ref();
        let job_id = JobId::new_unchecked("j");
        sub.write_incoming(&job_id, &sample_submission("j", "c"))
            .await?;

        let incoming = sub.list_incoming(TEST_LIST_LIMIT).await?;
        assert_eq!(incoming.len(), 1);
        assert_eq!(incoming[0], job_id);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// S3TodoStore tests (aws-smithy-mocks — mocks every op, renames included, so
// no real bucket or InMemory store is needed)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod todo_mock_tests {
    use super::*;
    use aws_sdk_s3::error::ErrorMetadata;
    use aws_sdk_s3::operation::get_object::{GetObjectError, GetObjectOutput};
    use aws_sdk_s3::operation::list_objects_v2::ListObjectsV2Output;
    use aws_sdk_s3::operation::put_object::PutObjectOutput;
    use aws_sdk_s3::operation::rename_object::{RenameObjectError, RenameObjectOutput};
    use aws_sdk_s3::primitives::DateTime as AwsDateTime;
    use aws_sdk_s3::types::Object;
    use aws_sdk_s3::types::error::NoSuchKey;
    use aws_smithy_mocks::{Rule, RuleMode, mock};
    use rstest::rstest;

    fn store(client: aws_sdk_s3::Client) -> S3TodoStore {
        S3TodoStore {
            client,
            bucket: "test-bucket".to_string(),
            prefix: String::new(),
        }
    }

    fn job(s: &str) -> JobId {
        JobId::new_unchecked(s)
    }

    fn cid(s: &str) -> ClientId {
        ClientId::try_new(s).unwrap()
    }

    fn ts() -> DateTime<Utc> {
        "2026-01-01T00:00:00Z".parse().unwrap()
    }

    /// `GetObjectError` for an absent object — `NoSuchKey` is a modeled
    /// variant on GET.
    fn get_no_such_key() -> GetObjectError {
        GetObjectError::NoSuchKey(NoSuchKey::builder().build())
    }

    /// `RenameObjectError` for an absent rename source. `RenameObjectError`
    /// has no modeled `NoSuchKey` variant, so S3 surfaces it via the error
    /// code and stores must sniff it (`is_no_such_key`).
    fn rename_no_such_key() -> RenameObjectError {
        RenameObjectError::generic(
            ErrorMetadata::builder()
                .code("NoSuchKey")
                .message("source gone")
                .build(),
        )
    }

    /// A single-page `list_objects_v2` rule matching `prefix` and returning
    /// `keys` (already absolute) as its contents.
    fn list_rule(prefix: &'static str, keys: Vec<String>) -> Rule {
        let contents: Vec<Object> = keys
            .iter()
            .map(|k| Object::builder().key(k.clone()).build())
            .collect();
        mock!(aws_sdk_s3::Client::list_objects_v2)
            .match_requests(move |r| r.prefix() == Some(prefix))
            .then_output(move || {
                ListObjectsV2Output::builder()
                    .set_contents(Some(contents.clone()))
                    .is_truncated(false)
                    .build()
            })
    }

    // ── reads ────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn get_avail_returns_body() -> anyhow::Result<()> {
        let present = mock!(aws_sdk_s3::Client::get_object)
            .match_requests(|r| r.key() == Some("todo/avail/job1.never.json"))
            .then_output(|| {
                GetObjectOutput::builder()
                    .body(ByteStream::from_static(br#"{"clients":["ev1_a"]}"#))
                    .build()
            });
        let s = store(aws_smithy_mocks::mock_client!(
            aws_sdk_s3,
            RuleMode::Sequential,
            &[&present]
        ));
        let body = s
            .get_avail(&job("job1"), ExpiresAt::Never)
            .await?
            .ok_or_else(|| anyhow::anyhow!("expected avail body"))?;
        assert_eq!(body["clients"][0], "ev1_a");
        Ok(())
    }

    #[tokio::test]
    async fn get_avail_missing_is_none() -> anyhow::Result<()> {
        let missing = mock!(aws_sdk_s3::Client::get_object).then_error(get_no_such_key);
        let s = store(aws_smithy_mocks::mock_client!(
            aws_sdk_s3,
            RuleMode::Sequential,
            &[&missing]
        ));
        assert!(s.get_avail(&job("job1"), ExpiresAt::Never).await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn list_avail_sorts_filters_and_limits() -> anyhow::Result<()> {
        // Express has no server-side start-after, so the store sorts + filters +
        // truncates client-side. Return one unsorted page.
        let page = mock!(aws_sdk_s3::Client::list_objects_v2)
            .match_requests(|r| r.prefix() == Some("todo/avail/"))
            .then_output(|| {
                ListObjectsV2Output::builder()
                    .set_contents(Some(vec![
                        Object::builder().key("todo/avail/b.never.json").build(),
                        Object::builder().key("todo/avail/a.never.json").build(),
                        Object::builder().key("todo/avail/c.never.json").build(),
                    ]))
                    .is_truncated(false)
                    .build()
            });
        let s = store(aws_smithy_mocks::mock_client!(
            aws_sdk_s3,
            RuleMode::Sequential,
            &[&page]
        ));
        let names = s
            .list_avail(Some("a.never.json"), NonZeroUsize::MIN)
            .await?;
        // sorted a,b,c → after "a.never.json" → [b,c] → limit 1 → [b]
        assert_eq!(names, vec!["b.never.json".to_string()]);
        Ok(())
    }

    // ── claim (rename → get) ───────────────────────────────────────────────────

    #[tokio::test]
    async fn claim_renames_then_returns_body() -> anyhow::Result<()> {
        let rename = mock!(aws_sdk_s3::Client::rename_object)
            .then_output(|| RenameObjectOutput::builder().build());
        let get = mock!(aws_sdk_s3::Client::get_object).then_output(|| {
            GetObjectOutput::builder()
                .body(ByteStream::from_static(br#"{"requires":[]}"#))
                .build()
        });
        let s = store(aws_smithy_mocks::mock_client!(
            aws_sdk_s3,
            RuleMode::Sequential,
            &[&rename, &get]
        ));
        let result = s
            .claim_job(&job("job1"), ExpiresAt::Never, &cid("ev1_me"), ts())
            .await?;
        assert!(matches!(result, ClaimResult::Claimed(_)));
        Ok(())
    }

    #[tokio::test]
    async fn claim_gone_when_source_missing() -> anyhow::Result<()> {
        let rename = mock!(aws_sdk_s3::Client::rename_object).then_error(rename_no_such_key);
        let s = store(aws_smithy_mocks::mock_client!(
            aws_sdk_s3,
            RuleMode::Sequential,
            &[&rename]
        ));
        let result = s
            .claim_job(&job("job1"), ExpiresAt::Never, &cid("ev1_me"), ts())
            .await?;
        assert!(matches!(result, ClaimResult::Gone));
        Ok(())
    }

    // ── tmp staging + promotion ────────────────────────────────────────────────

    #[tokio::test]
    async fn write_tmp_puts_body_at_tmp_key() -> anyhow::Result<()> {
        // Staging is a plain PUT keyed by job_id alone (no expires_at).
        let put = mock!(aws_sdk_s3::Client::put_object)
            .match_requests(|r| r.key() == Some("todo/tmp/job1.json"))
            .then_output(|| PutObjectOutput::builder().build());
        let s = store(aws_smithy_mocks::mock_client!(
            aws_sdk_s3,
            RuleMode::Sequential,
            &[&put]
        ));
        s.write_tmp(&job("job1"), &serde_json::json!({ "requires": [] }))
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn promote_avail_renames_tmp_to_avail() -> anyhow::Result<()> {
        // Atomic RenameObject from the tmp slot to the expires_at-encoded avail
        // name — the same mechanism claim_job uses, so a partial write is never
        // claimable. Assert both the destination key and the tmp rename source.
        let rename = mock!(aws_sdk_s3::Client::rename_object)
            .match_requests(|r| {
                r.key() == Some("todo/avail/job1.never.json")
                    && r.rename_source() == Some("test-bucket/todo/tmp/job1.json")
            })
            .then_output(|| RenameObjectOutput::builder().build());
        let s = store(aws_smithy_mocks::mock_client!(
            aws_sdk_s3,
            RuleMode::Sequential,
            &[&rename]
        ));
        s.promote_avail(&job("job1"), ExpiresAt::Never).await?;
        Ok(())
    }

    // ── backend validation ─────────────────────────────────────────────────────

    /// `probe_code` is the error code the rename probe returns (`None` = the
    /// rename reports success). `err_names_bucket_type`: `None` = validation
    /// passes; `Some(named)` = fatal, where `named` is whether the error carries
    /// the bucket-type diagnosis rather than propagating unchanged.
    #[rstest]
    #[case::rename_supported_passes(Some("NoSuchKey"), None)]
    #[case::general_purpose_bucket_is_fatal(Some("NotImplemented"), Some(true))]
    #[case::phantom_rename_success_is_fatal(None, Some(true))]
    #[case::unrelated_errors_propagate(Some("AccessDenied"), Some(false))]
    #[tokio::test]
    async fn validate_backend_verdicts(
        #[case] probe_code: Option<&'static str>,
        #[case] err_names_bucket_type: Option<bool>,
    ) -> anyhow::Result<()> {
        let rename = match probe_code {
            Some(code) => mock!(aws_sdk_s3::Client::rename_object).then_error(move || {
                RenameObjectError::generic(ErrorMetadata::builder().code(code).build())
            }),
            None => mock!(aws_sdk_s3::Client::rename_object)
                .then_output(|| RenameObjectOutput::builder().build()),
        };
        let s = store(aws_smithy_mocks::mock_client!(
            aws_sdk_s3,
            RuleMode::Sequential,
            &[&rename]
        ));
        match err_names_bucket_type {
            None => s.validate_backend().await?,
            Some(named) => {
                let Err(err) = s.validate_backend().await else {
                    anyhow::bail!("probe must be fatal");
                };
                assert_eq!(err.to_string().contains("Express One Zone"), named);
            }
        }
        Ok(())
    }

    // ── renew ──────────────────────────────────────────────────────────────────

    #[rstest]
    #[case::renews_when_client_holds_lease(true, false, RenewLeaseResult::Renewed)]
    #[case::not_found_when_no_lease_anywhere(false, false, RenewLeaseResult::NotFound)]
    #[case::wrong_client_when_held_elsewhere(false, true, RenewLeaseResult::WrongClient)]
    #[tokio::test]
    async fn renew_lease_outcomes(
        #[case] own_holds: bool,
        #[case] other_holds: bool,
        #[case] expected: RenewLeaseResult,
    ) -> anyhow::Result<()> {
        // renew_lease first lists the caller's own partition. If it holds the
        // lease it renames in place (Renewed); otherwise it sweeps the whole
        // `leased/` tree to tell WrongClient (held elsewhere) from NotFound.
        let own_key = format!(
            "todo/leased/{}",
            leased_key(&job("job1"), &cid("ev1_me"), ts())
        );
        let other_key = format!(
            "todo/leased/{}",
            leased_key(&job("job1"), &cid("ev1_other"), ts())
        );
        let own_list = list_rule(
            "todo/leased/ev1_me/",
            own_holds.then_some(own_key).into_iter().collect(),
        );
        let all_list = list_rule(
            "todo/leased/",
            other_holds.then_some(other_key).into_iter().collect(),
        );
        let rename = mock!(aws_sdk_s3::Client::rename_object)
            .then_output(|| RenameObjectOutput::builder().build());

        let mut rules: Vec<&Rule> = vec![&own_list];
        rules.push(if own_holds { &rename } else { &all_list });

        let s = store(aws_smithy_mocks::mock_client!(
            aws_sdk_s3,
            RuleMode::Sequential,
            &rules
        ));
        let result = s.renew_lease(&job("job1"), &cid("ev1_me"), ts()).await?;
        assert_eq!(result, expected);
        Ok(())
    }

    // ── recycle (get → rename), verifying the expires_at→avail-key mapping ──────

    #[tokio::test]
    async fn recycle_reads_expires_and_renames_to_avail() -> anyhow::Result<()> {
        // Body has no `expires_at` → Never → recycle target is `avail/….never.json`.
        let get = mock!(aws_sdk_s3::Client::get_object).then_output(|| {
            GetObjectOutput::builder()
                .body(ByteStream::from_static(b"{}"))
                .build()
        });
        let rename = mock!(aws_sdk_s3::Client::rename_object)
            .match_requests(|r| r.key() == Some("todo/avail/job1.never.json"))
            .then_output(|| RenameObjectOutput::builder().build());
        let s = store(aws_smithy_mocks::mock_client!(
            aws_sdk_s3,
            RuleMode::Sequential,
            &[&get, &rename]
        ));
        assert_eq!(
            s.recycle_lease(&job("job1"), &cid("ev1_me"), ts()).await?,
            RecycleResult::Recycled
        );
        Ok(())
    }

    /// The lease can vanish at either step of the recycle (the holder's
    /// terminal submission or another recycler beat us): `NoSuchKey` on the
    /// body GET, or on the rename (unmodeled on `RenameObjectError`, surfaced
    /// via code). Both resolve to `Gone`, not an error.
    #[rstest]
    #[case::missing_at_get(true)]
    #[case::missing_at_rename(false)]
    #[tokio::test]
    async fn recycle_gone_when_lease_missing(#[case] fails_at_get: bool) -> anyhow::Result<()> {
        let get_err = mock!(aws_sdk_s3::Client::get_object).then_error(get_no_such_key);
        let get_ok = mock!(aws_sdk_s3::Client::get_object).then_output(|| {
            GetObjectOutput::builder()
                .body(ByteStream::from_static(b"{}"))
                .build()
        });
        let rename = mock!(aws_sdk_s3::Client::rename_object).then_error(rename_no_such_key);
        // `fails_at_get`: the GET itself 404s. Otherwise the GET succeeds and
        // the rename 404s (the lease vanished between the two calls).
        let s = if fails_at_get {
            store(aws_smithy_mocks::mock_client!(
                aws_sdk_s3,
                RuleMode::Sequential,
                &[&get_err]
            ))
        } else {
            store(aws_smithy_mocks::mock_client!(
                aws_sdk_s3,
                RuleMode::Sequential,
                &[&get_ok, &rename]
            ))
        };
        assert_eq!(
            s.recycle_lease(&job("job1"), &cid("ev1_me"), ts()).await?,
            RecycleResult::Gone
        );
        Ok(())
    }

    // ── marker listing / parsing ───────────────────────────────────────────────

    #[tokio::test]
    async fn list_all_denied_parses_markers() -> anyhow::Result<()> {
        // `{job_id}.{client_id}` split on the `.`; client_id itself contains `_`.
        let list = mock!(aws_sdk_s3::Client::list_objects_v2)
            .match_requests(|r| r.prefix() == Some("todo/denied/"))
            .then_output(|| {
                ListObjectsV2Output::builder()
                    .set_contents(Some(vec![
                        Object::builder().key("todo/denied/job1.ev1_abc").build(),
                    ]))
                    .is_truncated(false)
                    .build()
            });
        let s = store(aws_smithy_mocks::mock_client!(
            aws_sdk_s3,
            RuleMode::Sequential,
            &[&list]
        ));
        let denied = s.list_all_denied().await?;
        assert_eq!(denied, vec![(job("job1"), cid("ev1_abc"))]);
        Ok(())
    }

    #[tokio::test]
    async fn list_stale_tmp_returns_old_objects() -> anyhow::Result<()> {
        let list = mock!(aws_sdk_s3::Client::list_objects_v2)
            .match_requests(|r| r.prefix() == Some("todo/tmp/"))
            .then_output(|| {
                ListObjectsV2Output::builder()
                    .set_contents(Some(vec![
                        Object::builder()
                            .key("todo/tmp/stale.tmp")
                            .last_modified(AwsDateTime::from_secs(0)) // 1970 → stale
                            .build(),
                    ]))
                    .is_truncated(false)
                    .build()
            });
        let s = store(aws_smithy_mocks::mock_client!(
            aws_sdk_s3,
            RuleMode::Sequential,
            &[&list]
        ));
        let stale = s.list_stale_tmp(Duration::from_secs(60)).await?;
        assert_eq!(stale, vec!["stale.tmp".to_string()]);
        Ok(())
    }

    /// A missing `.gc-candidates` key reads as the empty set (first run), not
    /// an error.
    #[tokio::test]
    async fn read_gc_candidates_missing_key_is_empty() -> anyhow::Result<()> {
        let missing = mock!(aws_sdk_s3::Client::get_object).then_error(get_no_such_key);
        let s = store(aws_smithy_mocks::mock_client!(
            aws_sdk_s3,
            RuleMode::Sequential,
            &[&missing]
        ));
        assert!(s.read_gc_candidates().await?.is_empty());
        Ok(())
    }

    /// A canonical hyphenated v7 uuid, as `pending_reindex_filename` mints.
    const NONCE: &str = "01890a5d-ac96-774b-bcce-b302099a8057";

    /// One listing rule for `pending-reindex/` carrying a nonce key for
    /// `ev1_abc` (the `_`-bearing id exercises the split on the `.` separator)
    /// and a foreign name that must be skipped, not an error.
    fn pending_reindex_listing() -> aws_smithy_mocks::Rule {
        mock!(aws_sdk_s3::Client::list_objects_v2)
            .match_requests(|r| r.prefix() == Some("todo/pending-reindex/"))
            .then_output(|| {
                ListObjectsV2Output::builder()
                    .set_contents(Some(vec![
                        Object::builder()
                            .key(format!("todo/pending-reindex/ev1_abc.{NONCE}"))
                            .build(),
                        Object::builder()
                            .key("todo/pending-reindex/.DS_Store")
                            .build(),
                    ]))
                    .is_truncated(false)
                    .build()
            })
    }

    #[tokio::test]
    async fn list_pending_reindex_parses_nonce_keys_and_skips_cruft() -> anyhow::Result<()> {
        let list = pending_reindex_listing();
        let s = store(aws_smithy_mocks::mock_client!(
            aws_sdk_s3,
            RuleMode::Sequential,
            &[&list]
        ));
        assert_eq!(
            s.list_pending_reindex().await?,
            vec![(cid("ev1_abc"), format!("ev1_abc.{NONCE}"))]
        );
        Ok(())
    }

    #[rstest]
    #[case::flagged("ev1_abc", true)]
    #[case::not_flagged("ev1_other", false)]
    #[tokio::test]
    async fn has_pending_reindex_compares_parsed_ids(
        #[case] client: &str,
        #[case] expected: bool,
    ) -> anyhow::Result<()> {
        let list = pending_reindex_listing();
        let s = store(aws_smithy_mocks::mock_client!(
            aws_sdk_s3,
            RuleMode::Sequential,
            &[&list]
        ));
        assert_eq!(s.has_pending_reindex(&cid(client)).await?, expected);
        Ok(())
    }

    /// Foreign names under `pending-reindex-jobs/` are skipped, not an error —
    /// one stray object must not wedge the maintenance run.
    #[tokio::test]
    async fn list_pending_reindex_jobs_skips_cruft() -> anyhow::Result<()> {
        let list = mock!(aws_sdk_s3::Client::list_objects_v2)
            .match_requests(|r| r.prefix() == Some("todo/pending-reindex-jobs/"))
            .then_output(|| {
                ListObjectsV2Output::builder()
                    .set_contents(Some(vec![
                        Object::builder()
                            .key("todo/pending-reindex-jobs/job1")
                            .build(),
                        Object::builder()
                            .key("todo/pending-reindex-jobs/.DS_Store")
                            .build(),
                    ]))
                    .is_truncated(false)
                    .build()
            });
        let s = store(aws_smithy_mocks::mock_client!(
            aws_sdk_s3,
            RuleMode::Sequential,
            &[&list]
        ));
        assert_eq!(s.list_pending_reindex_jobs().await?, vec![job("job1")]);
        Ok(())
    }
}

/// `PlanStore` uses the `object_store` handle (not the `aws-sdk` rename path),
/// so it is exercised against the in-memory `object_store` backend rather than
/// the `aws-smithy-mocks` harness.
#[cfg(test)]
mod plan_store_tests {
    use super::*;
    use crate::plan::{PlanStatus, sample_manifest};
    use rstest::rstest;

    fn plan_store(prefix: &str) -> S3PlanStore {
        S3PlanStore {
            store: Arc::new(object_store::memory::InMemory::new()),
            prefix: prefix.to_string(),
        }
    }

    #[tokio::test]
    async fn roundtrip_list_filter_and_delete() -> anyhow::Result<()> {
        let store = plan_store("root");
        let active_id = PlanId::from_uuid(uuid::Uuid::from_u128(1));
        let done_id = PlanId::from_uuid(uuid::Uuid::from_u128(2));
        // With a progress_snapshot and without one.
        let active = sample_manifest(active_id.clone(), PlanStatus::Active, true);
        let done = sample_manifest(done_id.clone(), PlanStatus::Complete, false);
        store.put_plan(&active).await?;
        store.put_plan(&done).await?;

        assert_eq!(store.get_plan(&active_id).await?.as_ref(), Some(&active));
        assert_eq!(store.get_plan(&done_id).await?.as_ref(), Some(&done));

        let mut all = store.list_plans(None).await?;
        all.sort_by(|a, b| a.plan_id.as_str().cmp(b.plan_id.as_str()));
        assert_eq!(all, vec![active.clone(), done.clone()]);
        assert_eq!(
            store.list_plans(Some(PlanStatus::Complete)).await?,
            vec![done]
        );
        assert!(
            store
                .list_plans(Some(PlanStatus::Cancelled))
                .await?
                .is_empty()
        );

        store.delete_plan(&active_id).await?;
        assert!(store.get_plan(&active_id).await?.is_none());
        store.delete_plan(&active_id).await?; // idempotent
        assert_eq!(store.list_plans(None).await?.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn get_missing_is_none() -> anyhow::Result<()> {
        let store = plan_store("");
        assert!(
            store
                .get_plan(&PlanId::from_uuid(uuid::Uuid::from_u128(7)))
                .await?
                .is_none()
        );
        Ok(())
    }

    /// Cancel markers round-trip, are idempotent in both directions, and are
    /// invisible to `list_plans` — the reason the keyspace is a sibling of
    /// `plans/` rather than a key inside it.
    #[rstest]
    // Both with and without a key prefix: the sibling keyspace has to stay
    // disjoint either way.
    #[case("root")]
    #[case("")]
    #[tokio::test]
    async fn cancel_marker_roundtrip(#[case] prefix: &str) -> anyhow::Result<()> {
        let store = plan_store(prefix);
        let a = PlanId::from_uuid(uuid::Uuid::from_u128(1));
        let b = PlanId::from_uuid(uuid::Uuid::from_u128(2));

        assert!(!store.has_cancel_marker(&a).await?);
        assert!(store.list_cancel_markers().await?.is_empty());

        store.write_cancel_marker(&a).await?;
        assert!(store.has_cancel_marker(&a).await?);
        assert!(!store.has_cancel_marker(&b).await?, "marker is per-plan");
        assert_eq!(store.list_cancel_markers().await?, vec![a.clone()]);

        // Re-cancelling overwrites the same zero-byte key: no error, no duplicate.
        store.write_cancel_marker(&a).await?;
        assert_eq!(store.list_cancel_markers().await?, vec![a.clone()]);

        // A manifest for the *same* plan id coexists, and neither listing sees
        // the other's objects.
        store
            .put_plan(&sample_manifest(a.clone(), PlanStatus::Active, false))
            .await?;
        assert_eq!(store.list_plans(None).await?.len(), 1);
        assert_eq!(store.list_cancel_markers().await?.len(), 1);

        store.delete_cancel_marker(&a).await?;
        assert!(!store.has_cancel_marker(&a).await?);
        store.delete_cancel_marker(&a).await?; // idempotent
        assert!(store.get_plan(&a).await?.is_some(), "manifest survives");
        assert!(store.list_cancel_markers().await?.is_empty());
        Ok(())
    }

    /// A foreign key under the marker prefix is warned about and skipped rather
    /// than failing the whole listing.
    #[tokio::test]
    async fn cancel_marker_listing_skips_unparseable() -> anyhow::Result<()> {
        let store = plan_store("root");
        let good = PlanId::from_uuid(uuid::Uuid::from_u128(1));
        store.write_cancel_marker(&good).await?;
        put_bytes(
            &store.store,
            &obj_path(&store.prefix, "cancelled_plans/not.a.plan.id"),
            Vec::new(),
        )
        .await?;

        assert_eq!(store.list_cancel_markers().await?, vec![good]);
        Ok(())
    }
}
