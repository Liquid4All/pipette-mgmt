//! The S3 storage backend: every [`pipette_store`] trait over a bucket. The
//! job queue claims with `RenameObject`, which only S3 Express One Zone
//! supports; `validate_backend` checks for it at startup.

mod auth;
mod campaign_store;
mod dataset_cache;
mod http_count;
#[cfg(test)]
mod mock_bucket;
mod model_params_store;
mod org_layout;
mod org_store;
mod tag_layout;
mod tag_store;
mod vault_store;

pub use auth::{S3ClientStore, S3UserStore};
pub use org_store::S3OrgStore;
pub use tag_store::S3TagStore;

// The key index is handed out as `Arc<dyn KeyIndexStore>`, so the concrete type
// stays inside this crate.
pub(crate) use auth::S3KeyIndexStore;
pub(crate) use campaign_store::S3CampaignStore;
pub(crate) use model_params_store::S3ModelParamsStore;

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use aws_sdk_s3::config::timeout::TimeoutConfig;
use aws_sdk_s3::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_s3::operation::get_object::GetObjectError;
use aws_sdk_s3::operation::rename_object::RenameObjectError;
use aws_sdk_s3::operation::{RequestId, RequestIdExt};
use aws_sdk_s3::primitives::ByteStream;
use chrono::{DateTime, Utc};
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use futures::TryStreamExt;
use futures::stream;
use futures::stream::StreamExt;
use object_store::aws::AmazonS3Builder;
use object_store::path::Path as ObjPath;
use object_store::{ObjectStore, ObjectStoreExt, PutMode, PutPayload, UpdateVersion};
use tokio_util::io::{StreamReader, SyncIoBridge};
use tokio_util::sync::CancellationToken;

use pipette_core::benchmark::{Benchmark, CatalogLoad};
use pipette_core::config::Config;
use pipette_core::denial::DenialRecord;
use pipette_core::eval_sample_result;
use pipette_core::eval_sample_result::EvalSampleResult;
use pipette_core::parquet_utils::WriterOpts;
use pipette_core::plan::{PlanManifest, PlanSource, SettledOutcome};
use pipette_core::todo_filename::{
    avail_filename, denied_job_prefix, denied_marker_filename, eligible_filename,
    job_plan_marker_filename, leased_key, lost_lease_filename, parse_denied_marker,
    parse_eligible_filename, parse_job_plan_marker_filename, parse_leased_key,
    parse_lost_lease_filename, tmp_filename,
};
use pipette_core::types::id_from_key;
use pipette_core::warehouse;
use pipette_core::warehouse::{JobMetrics, MetricRow};
use pipette_plan_types::{
    BenchmarkId, ClientId, ExpiresAt, IdError, JobId, OrgId, PlanId, PlanStatus, RuntimeOutput,
};
use pipette_store::decode_stats::S3_DECODE;
use pipette_store::{
    BacklogSnapshot, CampaignBacklogSnapshot, CatalogStore, ClaimResult, ClientStore,
    ContentKeyCas, ContentKeyClaim, ContentKeyError, ContentKeyRecord, ContentKeyVersion,
    CreateOutcome, EvalSampleResultStore, FinalizeProbe, JobPlanMarker, JobState,
    ORG_BENCHMARKS_PREFIX, OutcomeProbe, PLANS_PREFIX, Page, PendingSourceMarker, PlanFilter,
    PlanPage, PlanStore, RecycleResult, RedateResult, RenewLeaseResult, ScoreQueueStage, Stores,
    SubmissionRecord, SubmissionStore, SuspensionRecord, TodoStore, UnreadableObjectError,
    UserStore, WarehouseStore, find_outcomes_per_job, is_content_digest, page_matching_plans,
};

#[expect(
    clippy::case_sensitive_file_extension_comparisons,
    reason = "the crate writes the lowercase suffix itself; any other case is not one of its keys"
)]
pub(crate) fn is_json_key(key: &str) -> bool {
    key.ends_with(".json")
}

/// Shared by every S3 entry point, so all resolve builder, credentials,
/// region and endpoint the same way.
pub fn build_s3_object_store(
    storage: &pipette_core::config::StorageConfig,
) -> anyhow::Result<(Arc<dyn ObjectStore>, String)> {
    let pipette_core::config::StorageConfig::S3 {
        bucket,
        prefix,
        region,
        endpoint,
        read_timeout_secs,
        ..
    } = storage
    else {
        anyhow::bail!("build_s3_object_store called with non-S3 storage config");
    };
    let mut builder = AmazonS3Builder::from_env()
        .with_bucket_name(bucket)
        .with_http_connector(http_count::CountingConnector);
    if let Some(region) = region {
        builder = builder.with_region(region);
    }
    if let Some(endpoint) = endpoint {
        builder = builder.with_endpoint(endpoint).with_allow_http(true);
    }
    // A config key, not `with_client_options`: new client options would replace
    // the ones that `from_env` and `with_allow_http` set.
    if let Some(secs) = read_timeout_secs {
        let (key, value) = read_timeout(*secs);
        builder = builder.with_config(key, value);
    }
    let store: Arc<dyn ObjectStore> = Arc::new(builder.build()?);
    Ok((store, prefix.trim_end_matches('/').to_string()))
}

/// The client setting that fails a request after `secs` seconds with no bytes
/// received.
fn read_timeout(secs: std::num::NonZeroU64) -> (object_store::aws::AmazonS3ConfigKey, String) {
    (
        object_store::aws::AmazonS3ConfigKey::Client(object_store::ClientConfigKey::ReadTimeout),
        format!("{secs}s"),
    )
}

pub fn build_s3_client_store(
    storage: &pipette_core::config::StorageConfig,
) -> anyhow::Result<Arc<dyn ClientStore>> {
    let (store, prefix) = build_s3_object_store(storage)?;
    Ok(Arc::new(S3ClientStore::new(store, prefix)))
}

pub fn build_s3_user_store(
    storage: &pipette_core::config::StorageConfig,
) -> anyhow::Result<Arc<dyn UserStore>> {
    let (store, prefix) = build_s3_object_store(storage)?;
    Ok(Arc::new(S3UserStore::new(store, prefix)))
}

/// For tests in other crates; each decode still counts in [`S3_DECODE`].
#[cfg(feature = "testkit")]
pub fn in_memory_submission_store() -> Arc<dyn SubmissionStore> {
    Arc::new(S3SubmissionStore {
        store: Arc::new(object_store::memory::InMemory::new()),
        prefix: String::new(),
    })
}

/// `todo` is pre-built: its backend comes from `config.todo_storage()`. `async`
/// because the vault's large-blob copy client resolves credentials through
/// `aws-config` (see [`build_s3_sdk_client`]).
pub async fn build_s3_stores(config: &Config, todo: Arc<dyn TodoStore>) -> anyhow::Result<Stores> {
    let pipette_core::config::StorageConfig::S3 {
        bucket,
        max_concurrent_requests,
        ..
    } = &config.storage
    else {
        anyhow::bail!("build_s3_stores called with non-S3 storage config");
    };
    let (store, prefix) = build_s3_object_store(&config.storage)?;
    // A part copy moves up to gigabytes in one request, so it keeps the SDK's
    // timeouts rather than the queue's.
    let copier =
        vault_store::PartCopier::new(build_s3_sdk_client(&config.storage, None).await?, bucket);
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
        clients: Arc::new(S3ClientStore::new(store.clone(), prefix.clone())),
        users: Arc::new(S3UserStore::new(store.clone(), prefix.clone())),
        orgs: Arc::new(S3OrgStore::new(store.clone(), prefix.clone())),
        tags: Arc::new(S3TagStore::new(store.clone(), prefix.clone())),
        key_index: Arc::new(S3KeyIndexStore::new(store.clone(), prefix.clone())),
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
        dataset_cache: Arc::new(dataset_cache::S3DatasetCacheStore {
            store: store.clone(),
            prefix: prefix.clone(),
        }),
        campaigns: Arc::new(S3CampaignStore::new(store.clone(), prefix.clone())),
        model_params: Arc::new(S3ModelParamsStore::new(store.clone(), prefix.clone())),
        audit: None,
        descriptor_catalog: None,
        catalog_import_runs: None,
        vault: Arc::new(vault_store::S3VaultStore::new(
            store.clone(),
            prefix.clone(),
            copier,
        )),
        plans: Arc::new(S3PlanStore { store, prefix }),
        todo,
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// An empty prefix puts the key at the root. `prefix` comes from
/// [`build_s3_object_store`] with any trailing `/` trimmed.
pub fn obj_path(prefix: &str, suffix: &str) -> ObjPath {
    if prefix.is_empty() {
        ObjPath::from(suffix)
    } else {
        ObjPath::from(format!("{prefix}/{suffix}"))
    }
}

/// Accepts `.json` and `.json.gz`. A filename that fails `try_new` is
/// corruption: warned and skipped, not fatal.
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

#[derive(Debug, Clone, Copy)]
enum Encoding {
    Plain,
    Gz,
}

/// Held by value, so each store keeps its own snapshot.
#[derive(Debug, Clone, Copy)]
struct S3Tuning {
    max_concurrent_requests: usize,
}

/// Parse a stored body, and count the work in [`S3_DECODE`].
fn parse_json_blob<T: serde::de::DeserializeOwned>(
    data: &[u8],
    encoding: Encoding,
) -> anyhow::Result<T> {
    let started = std::time::Instant::now();
    let (parsed, decoded_bytes) = match encoding {
        Encoding::Plain => (
            serde_json::from_slice(data),
            u64::try_from(data.len()).unwrap_or(u64::MAX),
        ),
        Encoding::Gz => {
            let mut reader = CountingReader {
                inner: GzDecoder::new(data),
                bytes: 0,
            };
            (serde_json::from_reader(&mut reader), reader.bytes)
        }
    };
    S3_DECODE.record(data.len(), decoded_bytes, started.elapsed());
    Ok(parsed?)
}

struct CountingReader<R> {
    inner: R,
    bytes: u64,
}

impl<R: std::io::Read> std::io::Read for CountingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buf)?;
        self.bytes += u64::try_from(read).unwrap_or(u64::MAX);
        Ok(read)
    }
}

/// `None` if no object is at `path`. Parses on a blocking thread one chunk at a
/// time, so a `T` with few fields reads a large body without holding it. If
/// this future drops, the stream ends at its next chunk and the parse stops.
async fn stream_json_as<T: serde::de::DeserializeOwned + Send + 'static>(
    store: &Arc<dyn ObjectStore>,
    path: &ObjPath,
    encoding: Encoding,
) -> anyhow::Result<Option<T>> {
    let result = match store.get(path).await {
        Ok(result) => result,
        Err(object_store::Error::NotFound { .. }) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let cancel = CancellationToken::new();
    let chunks = result
        .into_stream()
        .take_until(cancel.clone().cancelled_owned());
    let _cancel_on_drop = cancel.drop_guard();
    let body = SyncIoBridge::new(StreamReader::new(Box::pin(
        chunks.map_err(std::io::Error::other),
    )));
    let parsed = tokio::task::spawn_blocking(move || {
        let started = std::time::Instant::now();
        let mut stored = CountingReader {
            inner: body,
            bytes: 0,
        };
        // `serde_json` reads one byte at a time, so the parser gets a buffer.
        let (parsed, decoded_bytes) = match encoding {
            Encoding::Plain => (
                serde_json::from_reader(std::io::BufReader::new(&mut stored)),
                stored.bytes,
            ),
            Encoding::Gz => {
                let mut decoded = CountingReader {
                    inner: GzDecoder::new(&mut stored),
                    bytes: 0,
                };
                let parsed = serde_json::from_reader(std::io::BufReader::new(&mut decoded));
                (parsed, decoded.bytes)
            }
        };
        S3_DECODE.record(
            usize::try_from(stored.bytes).unwrap_or(usize::MAX),
            decoded_bytes,
            started.elapsed(),
        );
        parsed
    })
    .await??;
    Ok(Some(parsed))
}

/// A JSON body gzipped whole, as `processed/` and `runtime-output/` store it.
fn gzip_json(body: &impl serde::Serialize) -> anyhow::Result<Vec<u8>> {
    let raw = serde_json::to_vec(body)?;
    let mut encoder = GzEncoder::new(Vec::with_capacity(raw.len()), Compression::default());
    encoder.write_all(&raw)?;
    Ok(encoder.finish()?)
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

/// `PutMode::Create` is `If-None-Match: *`; startup refuses an endpoint that
/// ignores it (`pipette_store_init::conditional_put`).
async fn put_create(
    store: &Arc<dyn ObjectStore>,
    path: &ObjPath,
    data: Vec<u8>,
) -> anyhow::Result<CreateOutcome> {
    match store
        .put_opts(path, PutPayload::from(data), PutMode::Create.into())
        .await
    {
        Ok(_) => Ok(CreateOutcome::Created),
        Err(object_store::Error::AlreadyExists { .. }) => Ok(CreateOutcome::AlreadyExists),
        Err(e) => Err(e.into()),
    }
}

async fn object_exists(store: &Arc<dyn ObjectStore>, path: &ObjPath) -> anyhow::Result<bool> {
    match store.head(path).await {
        Ok(_) => Ok(true),
        Err(object_store::Error::NotFound { .. }) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

async fn delete_idempotent(store: &Arc<dyn ObjectStore>, path: &ObjPath) -> anyhow::Result<()> {
    match store.delete(path).await {
        Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

// ---------------------------------------------------------------------------
// CatalogStore
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct S3CatalogStore {
    store: Arc<dyn ObjectStore>,
    prefix: String,
}

impl S3CatalogStore {
    async fn load_entry(
        &self,
        location: &ObjPath,
        benchmark_id: &str,
    ) -> anyhow::Result<(BenchmarkId, Benchmark)> {
        let data = get_bytes(&self.store, location)
            .await?
            .ok_or_else(|| anyhow::anyhow!("benchmark vanished: {location}"))?;
        let content =
            std::str::from_utf8(&data).with_context(|| format!("{location} is not valid UTF-8"))?;
        let benchmark = Benchmark::from_toml(benchmark_id, content)?;
        Ok((BenchmarkId::try_new(benchmark_id)?, benchmark))
    }

    /// Every `.toml` under `prefix`, parsed, and how many keys it saw: the public
    /// loader checks its prefix by that count.
    async fn load_prefix(&self, prefix: &ObjPath) -> anyhow::Result<(CatalogLoad, usize)> {
        let entries: Vec<(ObjPath, String)> = self
            .store
            .list(Some(prefix))
            .try_collect::<Vec<_>>()
            .await?
            .into_iter()
            .filter_map(|meta| {
                let path_str = meta.location.as_ref();
                let filename = path_str.rsplit('/').next().unwrap_or(path_str);
                let benchmark_id = filename.strip_suffix(".toml")?.to_owned();
                Some((meta.location, benchmark_id))
            })
            .collect();
        let seen = entries.len();
        // `buffered`, not `buffer_unordered`: two keys can share an id, and listing
        // order keeps the later one.
        let reads: Vec<_> = stream::iter(
            entries
                .into_iter()
                .map(|(location, benchmark_id)| async move {
                    self.load_entry(&location, &benchmark_id).await
                }),
        )
        .buffered(pipette_store::STORAGE_CONCURRENCY)
        .collect()
        .await;
        let mut load = CatalogLoad::default();
        // A bad object is logged and skipped, not fatal: the deployment serves
        // the entries that parsed rather than refusing to start over one.
        for read in reads {
            match read {
                Ok((id, benchmark)) => {
                    load.catalog.insert(id, benchmark);
                }
                Err(e) => load.errors.push(format!("{e:#}")),
            }
        }
        load.errors.sort();
        load.log_errors();
        Ok((load, seen))
    }

    fn org_prefix(&self, org_id: &OrgId) -> ObjPath {
        obj_path(&self.prefix, &format!("{ORG_BENCHMARKS_PREFIX}/{org_id}/"))
    }

    fn org_entry_path(&self, org_id: &OrgId, benchmark_id: &BenchmarkId) -> ObjPath {
        obj_path(
            &self.prefix,
            &format!("{ORG_BENCHMARKS_PREFIX}/{org_id}/{benchmark_id}.toml"),
        )
    }

    /// A deeper key is not an entry; one naming no valid org or id is skipped.
    async fn org_entry_ids(&self, prefix: &ObjPath) -> anyhow::Result<Vec<(OrgId, BenchmarkId)>> {
        let root = obj_path(&self.prefix, &format!("{ORG_BENCHMARKS_PREFIX}/"));
        let entries: Vec<_> = self.store.list(Some(prefix)).try_collect().await?;
        Ok(entries
            .iter()
            .filter_map(|meta| {
                let mut parts = meta.location.prefix_match(&root)?;
                let org = parts.next()?;
                let file = parts.next()?;
                if parts.next().is_some() {
                    return None;
                }
                let id = file.as_ref().strip_suffix(".toml")?;
                let org_id = OrgId::try_new(org.as_ref())
                    .inspect_err(|e| {
                        tracing::warn!(
                            key = %meta.location,
                            error = %e,
                            "skipping org benchmark entry with invalid org_id"
                        );
                    })
                    .ok()?;
                let benchmark_id = BenchmarkId::try_new(id)
                    .inspect_err(|e| {
                        tracing::warn!(
                            key = %meta.location,
                            error = %e,
                            "skipping org benchmark entry with invalid benchmark_id"
                        );
                    })
                    .ok()?;
                Some((org_id, benchmark_id))
            })
            .collect())
    }
}

#[async_trait]
impl CatalogStore for S3CatalogStore {
    async fn load_catalog(&self) -> anyhow::Result<HashMap<BenchmarkId, Benchmark>> {
        let prefix = obj_path(&self.prefix, "benchmarks/");
        let (load, seen) = self.load_prefix(&prefix).await?;
        // Nothing found is a misconfigured prefix that would 404 every lookup; the
        // filesystem catalog refuses an empty directory too.
        anyhow::ensure!(seen > 0, "no benchmark objects found under {prefix}");
        Ok(load.catalog)
    }

    async fn get_org_benchmark(
        &self,
        org_id: &OrgId,
        benchmark_id: &BenchmarkId,
    ) -> anyhow::Result<Option<Benchmark>> {
        let location = self.org_entry_path(org_id, benchmark_id);
        let Some(data) = get_bytes(&self.store, &location).await? else {
            return Ok(None);
        };
        let unreadable = |reason: String| UnreadableObjectError {
            key: location.to_string(),
            reason,
        };
        let content =
            std::str::from_utf8(&data).map_err(|e| unreadable(format!("not valid UTF-8: {e}")))?;
        let benchmark = Benchmark::from_toml(benchmark_id.as_str(), content)
            .map_err(|e| unreadable(format!("{e:#}")))?;
        Ok(Some(benchmark.private_to(org_id.clone())))
    }

    async fn put_org_benchmark(&self, org_id: &OrgId, benchmark: &Benchmark) -> anyhow::Result<()> {
        put_bytes(
            &self.store,
            &self.org_entry_path(org_id, benchmark.benchmark_id()),
            benchmark.to_toml()?.into_bytes(),
        )
        .await
    }

    async fn delete_org_benchmark(
        &self,
        org_id: &OrgId,
        benchmark_id: &BenchmarkId,
    ) -> anyhow::Result<()> {
        delete_idempotent(&self.store, &self.org_entry_path(org_id, benchmark_id)).await
    }

    async fn list_org_benchmark_ids(&self) -> anyhow::Result<Vec<(OrgId, BenchmarkId)>> {
        self.org_entry_ids(&obj_path(
            &self.prefix,
            &format!("{ORG_BENCHMARKS_PREFIX}/"),
        ))
        .await
    }

    /// One listing of the organization's prefix, not the whole keyspace's.
    async fn list_benchmark_ids_of(&self, org_id: &OrgId) -> anyhow::Result<Vec<BenchmarkId>> {
        let mut ids: Vec<BenchmarkId> = self
            .org_entry_ids(&self.org_prefix(org_id))
            .await?
            .into_iter()
            .filter(|(holder, _)| holder == org_id)
            .map(|(_, benchmark_id)| benchmark_id)
            .collect();
        ids.sort();
        Ok(ids)
    }
}

/// One page of a flat `{keyspace}/{id}.json` keyspace in ascending key order,
/// shared by [`PlanStore::list_plans`] and [`CampaignStore::list_campaigns`].
/// `key_of` rebuilds an id's key, which rejects an object nested deeper.
async fn page_flat_keyspace<T, Id, E>(
    store: &Arc<dyn ObjectStore>,
    prefix: &ObjPath,
    key_of: impl Fn(&Id) -> ObjPath + Send,
    after: Option<&Id>,
    limit: NonZeroUsize,
    parse_id: fn(String) -> Result<Id, E>,
    admit: impl Fn(&T) -> bool + Send,
) -> anyhow::Result<Page<T, Id>>
where
    T: serde::de::DeserializeOwned + Send,
    Id: std::fmt::Display + Clone + Send + Sync,
    IdError: From<E>,
{
    // `list_with_offset`, not a client-side skip: it maps to S3's `start-after`,
    // so a later page lists only the tail.
    let entries: Vec<_> = match after {
        Some(after) => {
            store
                .list_with_offset(Some(prefix), &key_of(after))
                .try_collect()
                .await?
        }
        None => store.list(Some(prefix)).try_collect().await?,
    };
    let mut candidates: Vec<(ObjPath, Id)> = entries
        .into_iter()
        .filter_map(|meta| {
            let leaf = meta.location.filename()?;
            let id = pipette_store::flat_keyspace_id(leaf, parse_id)?;
            (meta.location == key_of(&id)).then_some((meta.location, id))
        })
        .collect();
    let total = candidates.len();
    candidates.sort_by(|(a, _), (b, _)| a.cmp(b));

    // A parse failure is reported: dropping it would answer "no such record",
    // failing would hide the rest. A `get` failure is fatal (we do not know
    // whether the record exists), except a delete that raced the listing.
    // `buffered`, not `buffer_unordered`: reads past `limit` stay unconsumed and
    // unreported, so the next page decides them and each key is decided once.
    let mut reads = stream::iter(candidates.into_iter().map(|(location, id)| async move {
        let outcome = get_bytes(store, &location).await?.map(|data| {
            serde_json::from_slice::<T>(&data).map_err(|e| UnreadableObjectError {
                key: location.to_string(),
                reason: e.to_string(),
            })
        });
        Ok::<_, anyhow::Error>((id, outcome))
    }))
    .buffered(pipette_store::STORAGE_CONCURRENCY);

    let mut page = Page::default();
    let mut examined = 0usize;
    let mut last: Option<Id> = None;
    while let Some(read) = reads.next().await {
        let (id, outcome) = read?;
        examined += 1;
        last = Some(id);
        match outcome {
            // The filter is applied here, after the body is read, so a selective
            // one keeps the page reading.
            Some(Ok(record)) if admit(&record) => page.items.push(record),
            Some(Ok(_)) => {}
            Some(Err(bad)) => page.unreadable.push(bad),
            None => {}
        }
        if page.items.len() == limit.get() {
            break;
        }
    }
    // `None` once the candidates ran out: the caller's signal to stop.
    page.resume_after = (examined < total).then_some(last).flatten();
    Ok(page)
}

// ---------------------------------------------------------------------------
// PlanStore
// ---------------------------------------------------------------------------

/// Plan manifests as `plans/{plan_id}.json` in the `[storage]` object store.
#[derive(Clone)]
struct S3PlanStore {
    store: Arc<dyn ObjectStore>,
    prefix: String,
}

impl S3PlanStore {
    async fn list_plans_by_status(
        &self,
        status: Option<PlanStatus>,
        after: Option<&PlanId>,
        limit: NonZeroUsize,
    ) -> anyhow::Result<PlanPage> {
        page_flat_keyspace(
            &self.store,
            &obj_path(&self.prefix, PLANS_PREFIX),
            |plan_id| self.manifest_path(plan_id),
            after,
            limit,
            PlanId::try_new,
            |manifest: &PlanManifest| status.is_none_or(|wanted| manifest.status == wanted),
        )
        .await
    }

    fn manifest_path(&self, plan_id: &PlanId) -> ObjPath {
        obj_path(
            &self.prefix,
            &format!("{PLANS_PREFIX}{}.json", plan_id.as_str()),
        )
    }

    /// The bare `plan_id`, under a sibling of `plans/` so `list_plans` never
    /// returns markers.
    fn cancel_marker_path(&self, plan_id: &PlanId) -> ObjPath {
        obj_path(
            &self.prefix,
            &format!("cancelled_plans/{}", plan_id.as_str()),
        )
    }

    /// Content-key record key. A digest is lowercase hex, so it is a safe key
    /// component; [`is_content_digest`] filters the read side.
    fn content_key_path(&self, digest: &str) -> ObjPath {
        obj_path(&self.prefix, &format!("{CONTENT_KEYS_PREFIX}{digest}"))
    }

    fn job_plan_marker_path(
        &self,
        job_id: &JobId,
        plan_id: &PlanId,
        expires_at: ExpiresAt,
    ) -> ObjPath {
        obj_path(
            &self.prefix,
            &format!(
                "{JOB_PLANS_PREFIX}{job_id}/{}",
                job_plan_marker_filename(plan_id, expires_at)
            ),
        )
    }

    fn source_path(&self, plan_id: &PlanId) -> ObjPath {
        obj_path(
            &self.prefix,
            &format!("{PLAN_SOURCES_PREFIX}{}.toml", plan_id.as_str()),
        )
    }

    fn job_source_path(&self, job_id: &JobId) -> ObjPath {
        obj_path(&self.prefix, &format!("{JOB_SOURCES_PREFIX}{job_id}.json"))
    }

    /// Link key — the bare `plan_id` under the job's own segment.
    fn job_plan_link_path(&self, job_id: &JobId, plan_id: &PlanId) -> ObjPath {
        obj_path(
            &self.prefix,
            &format!("{JOB_PLAN_LINKS_PREFIX}{job_id}/{}", plan_id.as_str()),
        )
    }

    /// Pending marker key — the bare `plan_id`, like a cancel marker.
    fn pending_source_marker_path(&self, plan_id: &PlanId) -> ObjPath {
        obj_path(
            &self.prefix,
            &format!("{PENDING_PLAN_SOURCES_PREFIX}{}", plan_id.as_str()),
        )
    }

    /// `object_store` returns absolute locations; callers want the leaf shape the
    /// filename parsers take.
    async fn list_relative(&self, suffix: &str) -> anyhow::Result<Vec<(String, DateTime<Utc>)>> {
        let prefix = obj_path(&self.prefix, suffix);
        let entries: Vec<_> = self.store.list(Some(&prefix)).try_collect().await?;
        Ok(entries
            .into_iter()
            .filter_map(|meta| {
                let relative = meta
                    .location
                    .as_ref()
                    .strip_prefix(prefix.as_ref())?
                    .trim_start_matches('/')
                    .to_string();
                Some((relative, meta.last_modified))
            })
            .collect())
    }
}

/// Keyspace roots for the plan-domain trees that a path builder and a listing
/// both address, spelled once so the two cannot disagree.
const CONTENT_KEYS_PREFIX: &str = "content-keys/v1/";
const JOB_PLANS_PREFIX: &str = "job-plans/";
const PLAN_SOURCES_PREFIX: &str = "plan-sources/";
const PENDING_PLAN_SOURCES_PREFIX: &str = "plan-sources-pending/";
const JOB_SOURCES_PREFIX: &str = "job-sources/";
const JOB_PLAN_LINKS_PREFIX: &str = "job-plan-links/";

fn content_key_version(version: UpdateVersion) -> ContentKeyVersion {
    ContentKeyVersion::Object {
        e_tag: version.e_tag,
        version: version.version,
    }
}

/// `None` for a token from another backend, which reads as a lost swap.
fn update_version(version: &ContentKeyVersion) -> Option<UpdateVersion> {
    match version {
        ContentKeyVersion::Object { e_tag, version } => Some(UpdateVersion {
            e_tag: e_tag.clone(),
            version: version.clone(),
        }),
        ContentKeyVersion::Digest(_) | ContentKeyVersion::Revision(_) => None,
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
        // Zero-byte object: the key's existence *is* the signal. A plain PUT, so
        // re-cancelling overwrites it harmlessly.
        put_bytes(&self.store, &self.cancel_marker_path(plan_id), Vec::new()).await
    }

    async fn has_cancel_marker(&self, plan_id: &PlanId) -> anyhow::Result<bool> {
        // HEAD: no body worth fetching. An error other than NotFound propagates
        // rather than read as "not cancelled".
        object_exists(&self.store, &self.cancel_marker_path(plan_id)).await
    }

    async fn list_cancel_markers(&self) -> anyhow::Result<Vec<PlanId>> {
        let prefix = obj_path(&self.prefix, "cancelled_plans/");
        let entries: Vec<_> = self.store.list(Some(&prefix)).try_collect().await?;
        // Markers carry no body, so the ids come straight off the keys.
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

    async fn get_content_key(
        &self,
        digest: &str,
    ) -> anyhow::Result<Option<(ContentKeyRecord, ContentKeyVersion)>> {
        let path = self.content_key_path(digest);
        match get_bytes_for_update(&self.store, &path).await? {
            Some((data, version)) => Ok(Some((
                serde_json::from_slice(&data)?,
                content_key_version(version),
            ))),
            None => Ok(None),
        }
    }

    async fn create_content_key(
        &self,
        digest: &str,
        record: &ContentKeyRecord,
    ) -> anyhow::Result<ContentKeyClaim> {
        let path = self.content_key_path(digest);
        let bytes = serde_json::to_vec_pretty(record)?;
        // `PutMode::Create` (`If-None-Match: *`): exactly one concurrent claim creates
        // the record. Twice, because the GC may delete it between the refused create
        // and the read; vanishing twice propagates.
        for _ in 0..2 {
            match self
                .store
                .put_opts(
                    &path,
                    PutPayload::from(bytes.clone()),
                    PutMode::Create.into(),
                )
                .await
            {
                Ok(_) => return Ok(ContentKeyClaim::Created),
                Err(object_store::Error::AlreadyExists { .. }) => {
                    if let Some((data, version)) = get_bytes_for_update(&self.store, &path).await? {
                        return Ok(ContentKeyClaim::AlreadyExists(
                            serde_json::from_slice(&data)?,
                            content_key_version(version),
                        ));
                    }
                    tracing::debug!(key = %path, "content key removed after a refused create");
                }
                Err(e) => return Err(e.into()),
            }
        }
        Err(ContentKeyError::ConcurrentChurn {
            path: path.to_string(),
        }
        .into())
    }

    async fn replace_content_key(
        &self,
        digest: &str,
        version: &ContentKeyVersion,
        record: &ContentKeyRecord,
    ) -> anyhow::Result<ContentKeyCas> {
        let Some(version) = update_version(version) else {
            return Ok(ContentKeyCas::Lost);
        };
        let bytes = serde_json::to_vec_pretty(record)?;
        // `If-Match` on the version read: an unconditional overwrite would destroy a
        // rival's fresh record and orphan its live job from the index.
        match self
            .store
            .put_opts(
                &self.content_key_path(digest),
                PutPayload::from(bytes),
                PutMode::Update(version).into(),
            )
            .await
        {
            Ok(_) => Ok(ContentKeyCas::Applied),
            Err(
                object_store::Error::Precondition { .. } | object_store::Error::NotFound { .. },
            ) => Ok(ContentKeyCas::Lost),
            Err(e) => Err(e.into()),
        }
    }

    async fn delete_content_key(
        &self,
        digest: &str,
        version: &ContentKeyVersion,
    ) -> anyhow::Result<ContentKeyCas> {
        let path = self.content_key_path(digest);
        // No conditional delete in `object_store`: a fresh read checks the version so
        // a replacement written since survives. A replace landing between read and
        // delete is still lost; that costs a duplicate job, never a wrongly shared one.
        let Some((_, current)) = get_bytes_for_update(&self.store, &path).await? else {
            return Ok(ContentKeyCas::Lost);
        };
        if content_key_version(current) != *version {
            return Ok(ContentKeyCas::Lost);
        }
        match self.store.delete(&path).await {
            Ok(()) => Ok(ContentKeyCas::Applied),
            Err(object_store::Error::NotFound { .. }) => Ok(ContentKeyCas::Lost),
            Err(e) => Err(e.into()),
        }
    }

    async fn list_content_keys(
        &self,
    ) -> anyhow::Result<Vec<(String, ContentKeyRecord, ContentKeyVersion)>> {
        let digests: Vec<String> = self
            .list_relative(CONTENT_KEYS_PREFIX)
            .await?
            .into_iter()
            .map(|(name, _)| name)
            .filter(|name| is_content_digest(name))
            .collect();
        // A record gone before the read is skipped, as if listed a moment later.
        Ok(stream::iter(digests.into_iter().map(|digest| async move {
            let path = self.content_key_path(&digest);
            match get_bytes_for_update(&self.store, &path).await? {
                Some((data, version)) => Ok::<_, anyhow::Error>(Some((
                    digest,
                    serde_json::from_slice(&data)?,
                    content_key_version(version),
                ))),
                None => Ok(None),
            }
        }))
        .buffer_unordered(pipette_store::STORAGE_CONCURRENCY)
        .try_collect::<Vec<_>>()
        .await?
        .into_iter()
        .flatten()
        .collect())
    }

    async fn write_job_plan_marker(
        &self,
        job_id: &JobId,
        plan_id: &PlanId,
        expires_at: ExpiresAt,
    ) -> anyhow::Result<()> {
        // Zero-byte object: the key's existence *is* the membership.
        put_bytes(
            &self.store,
            &self.job_plan_marker_path(job_id, plan_id, expires_at),
            Vec::new(),
        )
        .await
    }

    async fn list_job_plan_markers(
        &self,
        job_id: &JobId,
    ) -> anyhow::Result<Vec<(PlanId, ExpiresAt)>> {
        Ok(self
            .list_relative(&format!("{JOB_PLANS_PREFIX}{job_id}/"))
            .await?
            .into_iter()
            .filter_map(|(name, _)| {
                parse_job_plan_marker_filename(&name)
                    .inspect_err(|e| {
                        tracing::warn!(key = %name, error = %e,
                            "skipping unparseable job-plans marker");
                    })
                    .ok()
            })
            .collect())
    }

    async fn list_all_job_plan_markers(&self) -> anyhow::Result<Vec<JobPlanMarker>> {
        Ok(self
            .list_relative(JOB_PLANS_PREFIX)
            .await?
            .into_iter()
            .filter_map(|(relative, written_at)| {
                // `{job_id}/{plan_id}.{expires_at}`; a key of any other shape is cruft,
                // warned and skipped.
                let parsed = relative.split_once('/').and_then(|(job_str, leaf)| {
                    let job_id = JobId::try_new(job_str).ok()?;
                    let (plan_id, expires_at) = parse_job_plan_marker_filename(leaf).ok()?;
                    Some(JobPlanMarker {
                        job_id,
                        plan_id,
                        expires_at,
                        written_at,
                    })
                });
                if parsed.is_none() {
                    tracing::warn!(key = %relative, "skipping unparseable job-plans marker");
                }
                parsed
            })
            .collect())
    }

    async fn delete_job_plan_marker(
        &self,
        job_id: &JobId,
        plan_id: &PlanId,
        expires_at: ExpiresAt,
    ) -> anyhow::Result<()> {
        let path = self.job_plan_marker_path(job_id, plan_id, expires_at);
        match self.store.delete(&path).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(e.into()),
        }
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
        put_create(
            &self.store,
            &self.source_path(plan_id),
            source.text().as_bytes().to_vec(),
        )
        .await
    }

    async fn get_plan_source(&self, plan_id: &PlanId) -> anyhow::Result<Option<PlanSource>> {
        let path = self.source_path(plan_id);
        let Some(bytes) = get_bytes(&self.store, &path).await? else {
            return Ok(None);
        };
        let text = String::from_utf8(bytes.to_vec())
            .with_context(|| format!("plan source {path} is not UTF-8"))?;
        Ok(Some(PlanSource::new(text)?))
    }

    async fn delete_plan_source(&self, plan_id: &PlanId) -> anyhow::Result<()> {
        delete_idempotent(&self.store, &self.source_path(plan_id)).await
    }

    async fn write_pending_source_marker(&self, plan_id: &PlanId) -> anyhow::Result<()> {
        put_bytes(
            &self.store,
            &self.pending_source_marker_path(plan_id),
            Vec::new(),
        )
        .await
    }

    async fn list_pending_source_markers(&self) -> anyhow::Result<Vec<PendingSourceMarker>> {
        Ok(self
            .list_relative(PENDING_PLAN_SOURCES_PREFIX)
            .await?
            .into_iter()
            .filter_map(|(relative, written_at)| match PlanId::try_new(&relative) {
                Ok(plan_id) => Some(PendingSourceMarker {
                    plan_id,
                    written_at,
                }),
                Err(_) => {
                    tracing::warn!(key = %relative, "skipping unparseable pending source marker");
                    None
                }
            })
            .collect())
    }

    async fn delete_pending_source_marker(&self, plan_id: &PlanId) -> anyhow::Result<()> {
        delete_idempotent(&self.store, &self.pending_source_marker_path(plan_id)).await
    }

    async fn put_job_source(
        &self,
        job_id: &JobId,
        body: &serde_json::Value,
    ) -> anyhow::Result<CreateOutcome> {
        put_create(
            &self.store,
            &self.job_source_path(job_id),
            serde_json::to_vec(body)?,
        )
        .await
    }

    async fn get_job_source(&self, job_id: &JobId) -> anyhow::Result<Option<serde_json::Value>> {
        let path = self.job_source_path(job_id);
        let Some(bytes) = get_bytes(&self.store, &path).await? else {
            return Ok(None);
        };
        serde_json::from_slice(&bytes).map(Some).map_err(|e| {
            UnreadableObjectError {
                key: path.to_string(),
                reason: e.to_string(),
            }
            .into()
        })
    }

    async fn put_job_plan_link(
        &self,
        job_id: &JobId,
        plan_id: &PlanId,
    ) -> anyhow::Result<CreateOutcome> {
        // Zero-byte object: the key's existence *is* the link.
        put_create(
            &self.store,
            &self.job_plan_link_path(job_id, plan_id),
            Vec::new(),
        )
        .await
    }

    async fn list_job_plan_links(&self, job_id: &JobId) -> anyhow::Result<Vec<PlanId>> {
        // The trailing `/` keeps `job-1` apart from `job-10`.
        Ok(self
            .list_relative(&format!("{JOB_PLAN_LINKS_PREFIX}{job_id}/"))
            .await?
            .into_iter()
            .filter_map(|(name, _)| {
                PlanId::try_new(&name)
                    .inspect_err(|e| {
                        tracing::warn!(key = %name, error = %e,
                            "skipping unparseable job-plan-links key");
                    })
                    .ok()
            })
            .collect())
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

    fn runtime_output_path(&self, job_id: &JobId, client_id: &ClientId) -> ObjPath {
        obj_path(
            &self.prefix,
            &format!("submissions/runtime-output/{job_id}/{client_id}.json.gz"),
        )
    }

    fn runtime_output_prefix(&self, job_id: &JobId) -> ObjPath {
        obj_path(
            &self.prefix,
            &format!("submissions/runtime-output/{job_id}/"),
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

    /// From the first place that holds it, in [`SubmissionStore::find_outcomes`]
    /// order.
    async fn find_outcome(&self, job_id: &JobId) -> anyhow::Result<Option<SettledOutcome>> {
        let processed = self.processed_path(job_id);
        let read_processed = || async {
            let probe =
                stream_json_as::<OutcomeProbe>(&self.store, &processed, Encoding::Gz).await?;
            anyhow::Ok(probe.map(|probe| probe.outcome()))
        };
        if let Some(outcome) = read_processed().await? {
            return Ok(Some(outcome));
        }
        for path in [
            self.incoming_path(job_id),
            self.stage_path(ScoreQueueStage::ToDo, job_id),
            self.stage_path(ScoreQueueStage::Parked, job_id),
        ] {
            if let Some(probe) =
                stream_json_as::<OutcomeProbe>(&self.store, &path, Encoding::Plain).await?
            {
                return Ok(Some(probe.outcome()));
            }
        }
        let to_finalize = self.stage_path(ScoreQueueStage::ToFinalize, job_id);
        if let Some(probe) =
            stream_json_as::<FinalizeProbe>(&self.store, &to_finalize, Encoding::Plain).await?
        {
            return Ok(Some(probe.submission.outcome()));
        }
        read_processed().await
    }

    fn stage_prefix(&self, stage: ScoreQueueStage) -> ObjPath {
        obj_path(
            &self.prefix,
            &format!("submissions/{}/{}/", ScoreQueueStage::ROOT, stage.leaf()),
        )
    }

    /// An absent object is success: a retried move deletes again.
    async fn delete_object(&self, path: &ObjPath) -> anyhow::Result<()> {
        match self.store.delete(path).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Filtering streams, so `take(limit)` stops paging after `ceil(limit / 1000)`
    /// LIST calls. INVARIANT: the prefix holds only `.json` bodies; a stray object
    /// is not counted, so it would keep the listing paging.
    /// A general-purpose bucket lists in ascending key order, so `after`'s key
    /// is the cursor. `[storage]` is one: only `[todo_storage]` is Express One
    /// Zone, whose listings are unordered.
    async fn list_json_job_ids(
        &self,
        prefix: &ObjPath,
        after: Option<ObjPath>,
        limit: std::num::NonZeroUsize,
    ) -> anyhow::Result<Vec<JobId>> {
        let listing = match &after {
            Some(after) => self.store.list_with_offset(Some(prefix), after),
            None => self.store.list(Some(prefix)),
        };
        let jobs: Vec<JobId> = listing
            .try_filter_map(|m| async move {
                if !is_json_key(m.location.as_ref()) {
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

    async fn write_incoming_new(
        &self,
        job_id: &JobId,
        body: &serde_json::Value,
    ) -> anyhow::Result<CreateOutcome> {
        put_create(
            &self.store,
            &self.incoming_path(job_id),
            serde_json::to_vec_pretty(body)?,
        )
        .await
    }

    async fn write_processed(
        &self,
        job_id: &JobId,
        body: &serde_json::Value,
    ) -> anyhow::Result<()> {
        put_bytes(&self.store, &self.processed_path(job_id), gzip_json(body)?).await
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

    async fn list_incoming_after(
        &self,
        after: Option<&JobId>,
        limit: std::num::NonZeroUsize,
    ) -> anyhow::Result<Vec<JobId>> {
        let prefix = obj_path(&self.prefix, "submissions/incoming/");
        let after = after.map(|job_id| self.incoming_path(job_id));
        self.list_json_job_ids(&prefix, after, limit).await
    }

    async fn mark_processed(&self, job_id: &JobId) -> anyhow::Result<()> {
        // Stream the GET body through a gzip encoder: peak memory is one chunk plus
        // the compressed buffer. One PUT, as multipart costs 3 more ops. A crash
        // between PUT and DELETE reprocesses and overwrites (idempotent).
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

    async fn list_queue_after(
        &self,
        stage: ScoreQueueStage,
        after: Option<&JobId>,
        limit: std::num::NonZeroUsize,
    ) -> anyhow::Result<Vec<JobId>> {
        let after = after.map(|job_id| self.stage_path(stage, job_id));
        self.list_json_job_ids(&self.stage_prefix(stage), after, limit)
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

    async fn has_record(&self, job_id: &JobId) -> anyhow::Result<bool> {
        // Check in the order that the trait documents.
        let paths = [
            self.incoming_path(job_id),
            self.stage_path(ScoreQueueStage::ToDo, job_id),
            self.stage_path(ScoreQueueStage::Parked, job_id),
            self.stage_path(ScoreQueueStage::ToFinalize, job_id),
            self.processed_path(job_id),
        ];
        for path in &paths {
            if object_exists(&self.store, path).await? {
                return Ok(true);
            }
        }
        Ok(false)
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
            if !is_json_key(meta.location.as_ref()) {
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
            if !is_json_key(meta.location.as_ref()) {
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
    ) -> anyhow::Result<pipette_store::PruneSummary> {
        let prefix = obj_path(&self.prefix, "submissions/unverified/");
        let older_than = chrono::Duration::from_std(older_than)
            .map_err(|e| anyhow::anyhow!("prune age out of range: {e}"))?;
        // `LastModified`, matching the filesystem `mtime` on local_fs.
        let cutoff = chrono::Utc::now() - older_than;

        let mut deleted = 0usize;
        let mut kept = 0usize;
        // S3 `list` is a flat walk, so one prefix covers every `{client_id}/`.
        let mut listing = self.store.list(Some(&prefix));
        while let Some(meta) = listing.try_next().await? {
            if !is_json_key(meta.location.as_ref()) {
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
        Ok(pipette_store::PruneSummary { deleted, kept })
    }

    async fn write_runtime_output(
        &self,
        job_id: &JobId,
        client_id: &ClientId,
        output: &RuntimeOutput,
    ) -> anyhow::Result<()> {
        let path = self.runtime_output_path(job_id, client_id);
        put_bytes(&self.store, &path, gzip_json(output)?).await
    }

    async fn list_runtime_output(
        &self,
        job_id: &JobId,
    ) -> anyhow::Result<Vec<(ClientId, RuntimeOutput)>> {
        let prefix = self.runtime_output_prefix(job_id);
        let metas: Vec<_> = self.store.list(Some(&prefix)).try_collect().await?;
        let objects = metas.into_iter().filter_map(|meta| {
            let stem = meta
                .location
                .as_ref()
                .rsplit('/')
                .next()?
                .strip_suffix(".json.gz")?;
            let client_id = ClientId::try_new(stem)
                .inspect_err(|e| {
                    tracing::warn!(path = %meta.location, error = %e, "skipping runtime output with invalid client_id");
                })
                .ok()?;
            Some((meta.location, client_id))
        });
        let reads = stream::iter(objects.map(|(location, client_id)| async move {
            get_bytes(&self.store, &location)
                .await?
                .map(|data| anyhow::Ok((client_id, parse_json_blob(&data, Encoding::Gz)?)))
                .transpose()
        }));
        Ok(reads
            .buffer_unordered(pipette_store::STORAGE_CONCURRENCY)
            .try_collect::<Vec<_>>()
            .await?
            .into_iter()
            .flatten()
            .collect())
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

        // Append-only: top up the tail part, then roll into fresh parts; earlier
        // parts are never rewritten. A PUT replaces the tail atomically.
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

        // Newest first, so the first partition with a match holds the newest copy;
        // `from_latest_rows` then picks its newest scoring run.
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

        // `f` is a single `&mut`, so only the I/O fans out and `f` runs serially.
        // Writes start at the first dirty file, bounding memory to `~max * file_size`.
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

/// Rows under `partition_prefix` whose `result_id` starts with `job_prefix`.
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
        // Skip `.tmp-*` leftovers and any stem that is no job id.
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
// TodoStore: all `aws-sdk-s3`, which `RenameObject` needs; one client lets
// `aws-smithy-mocks` mock every op, renames included.
// ---------------------------------------------------------------------------

/// The timeouts the `todo/` queue's client adds to the SDK's, which bound only
/// the connect: `attempt` for one attempt and `operation` for one operation with
/// its retries ([`Config::todo_attempt_timeout_secs`] and
/// [`Config::todo_operation_timeout_secs`]). Without them an attempt that gets
/// no answer waits as long as the connection stays open.
fn queue_timeouts(attempt: Duration, operation: Duration) -> TimeoutConfig {
    TimeoutConfig::builder()
        .operation_attempt_timeout(attempt)
        .operation_timeout(operation)
        .build()
}

/// `async` because credentials resolve through the `aws-config` chain. An
/// `endpoint` override forces path-style addressing for development and mocks.
/// `timeouts` are merged into the SDK's, so the connect timeout stays.
async fn build_s3_sdk_client(
    storage: &pipette_core::config::StorageConfig,
    timeouts: Option<TimeoutConfig>,
) -> anyhow::Result<aws_sdk_s3::Client> {
    let pipette_core::config::StorageConfig::S3 {
        region, endpoint, ..
    } = storage
    else {
        anyhow::bail!("build_s3_sdk_client called with non-S3 storage config");
    };

    // `object_store` falls back to `us-east-1` when no region is set. The same
    // fallback here keeps the vault copy on the region that its uploads use.
    let region = aws_config::meta::region::RegionProviderChain::first_try(
        region.clone().map(aws_sdk_s3::config::Region::new),
    )
    .or_default_provider()
    .or_else(aws_sdk_s3::config::Region::from_static("us-east-1"));
    let sdk_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(region)
        .load()
        .await;

    let mut s3_conf =
        aws_sdk_s3::config::Builder::from(&sdk_config).interceptor(http_count::CountingInterceptor);
    if let Some(endpoint) = endpoint {
        s3_conf = s3_conf.endpoint_url(endpoint).force_path_style(true);
    }
    if let Some(timeouts) = timeouts {
        s3_conf = s3_conf.timeout_config(timeouts);
    }
    Ok(aws_sdk_s3::Client::from_conf(s3_conf.build()))
}

/// `attempt` and `operation` bound the queue client's requests, as
/// [`queue_timeouts`] says.
pub async fn build_s3_todo_store(
    storage: &pipette_core::config::StorageConfig,
    attempt: Duration,
    operation: Duration,
) -> anyhow::Result<Arc<dyn TodoStore>> {
    let pipette_core::config::StorageConfig::S3 { bucket, prefix, .. } = storage else {
        anyhow::bail!("build_s3_todo_store called with non-S3 storage config");
    };

    Ok(Arc::new(S3TodoStore {
        client: build_s3_sdk_client(storage, Some(queue_timeouts(attempt, operation))).await?,
        bucket: bucket.clone(),
        prefix: prefix.trim_end_matches('/').to_string(),
    }))
}

pub struct S3TodoStore {
    client: aws_sdk_s3::Client,
    bucket: String,
    prefix: String,
}

impl S3TodoStore {
    /// Takes the client from the caller so tests can use a mock transport;
    /// [`build_s3_todo_store`] is the production path.
    pub fn new(
        client: aws_sdk_s3::Client,
        bucket: impl Into<String>,
        prefix: impl Into<String>,
    ) -> Self {
        Self {
            client,
            bucket: bucket.into(),
            prefix: prefix.into(),
        }
    }
}

/// A fatal verdict from [`S3TodoStore::probe_rename`]: every variant refuses
/// startup.
#[derive(Debug, thiserror::Error)]
enum RenameProbeError {
    #[error(
        "todo_storage bucket `{bucket}` does not support the atomic RenameObject \
         operation (returned NotImplemented); it must be an S3 Express One \
         Zone directory bucket (see docs/operations.md)"
    )]
    NotImplemented { bucket: String },
    #[error("RenameObject probe of {src} -> {dst} failed")]
    Failed {
        src: String,
        dst: String,
        /// Boxed: the SDK error would dominate every `Result` in this module.
        #[source]
        source: Box<RenameObjectError>,
    },
    #[error(
        "todo_storage bucket `{bucket}`: RenameObject reported success but left nothing at \
         the destination. This endpoint's rename semantics cannot be trusted for atomic \
         claims. The bucket must be an S3 Express One Zone directory bucket; see \
         docs/operations.md"
    )]
    PhantomRename { bucket: String },
    #[error(
        "todo_storage bucket `{bucket}`: RenameObject reported success but left the source \
         object in place. This is a copy, not a move, so the claim race could grant the \
         same job to two clients. The bucket must be an S3 Express One Zone directory \
         bucket; see docs/operations.md"
    )]
    SourceRetained { bucket: String },
    #[error("RenameObject probe read-back of {key} failed: {source:#}")]
    ReadBack { key: String, source: anyhow::Error },
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

    fn list_prefix(&self, suffix: &str) -> String {
        self.key(suffix)
    }

    /// Full key of one denial marker: `todo/denied/{job_id}.{client_id}`.
    fn denied_key(&self, job_id: &JobId, client_id: &ClientId) -> String {
        self.key(&format!(
            "denied/{}",
            denied_marker_filename(job_id, client_id)
        ))
    }

    /// The rename half of [`TodoStore::validate_backend`]. It is separate so
    /// that its caller deletes the probe objects whatever the verdict is.
    async fn probe_rename(&self, src: &str, dst: &str) -> Result<(), RenameProbeError> {
        if let Err(e) = self
            .client
            .rename_object()
            .bucket(&self.bucket)
            .key(dst)
            .rename_source(src)
            .send()
            .await
        {
            let err = e.into_service_error();
            if err.code() == Some("NotImplemented") {
                return Err(RenameProbeError::NotImplemented {
                    bucket: self.bucket.clone(),
                });
            }
            return Err(RenameProbeError::Failed {
                src: src.to_string(),
                dst: dst.to_string(),
                source: Box::new(err),
            });
        }
        let read_back = |key: &str, e: anyhow::Error| RenameProbeError::ReadBack {
            key: key.to_string(),
            source: e,
        };
        if self
            .get_bytes(dst)
            .await
            .map_err(|e| read_back(dst, e))?
            .is_none()
        {
            return Err(RenameProbeError::PhantomRename {
                bucket: self.bucket.clone(),
            });
        }
        if self
            .get_bytes(src)
            .await
            .map_err(|e| read_back(src, e))?
            .is_some()
        {
            return Err(RenameProbeError::SourceRetained {
                bucket: self.bucket.clone(),
            });
        }
        tracing::info!(
            bucket = %self.bucket,
            "todo storage validated: RenameObject supported (S3 Express One Zone)"
        );
        Ok(())
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
            Ok(out) => Ok(Some(
                out.body
                    .collect()
                    .await
                    .map_err(|e| {
                        anyhow::Error::new(e)
                            .context(format!("S3 GetObject on {key}: reading the body failed"))
                    })?
                    .into_bytes()
                    .to_vec(),
            )),
            // `NoSuchKey` is modeled on `GetObjectError`. Borrowing keeps the `SdkError`
            // whole for `s3_error`.
            Err(e) => match e.as_service_error() {
                Some(GetObjectError::NoSuchKey(_)) => Ok(None),
                _ => Err(s3_error("GetObject", key, e)),
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
            .await
            .map_err(|e| s3_error("PutObject", key, e))?;
        Ok(())
    }

    async fn put_empty(&self, key: &str) -> anyhow::Result<()> {
        self.put_bytes(key, Vec::new()).await
    }

    /// `todo/.{name}`; a missing or corrupt key reads as empty
    /// ([`pipette_store::parse_key_set`]).
    async fn read_key_set<T>(&self, name: &str) -> anyhow::Result<HashSet<T>>
    where
        T: serde::de::DeserializeOwned + Eq + std::hash::Hash + Send,
    {
        Ok(self
            .get_bytes(&self.key(&format!(".{name}")))
            .await?
            .map(|bytes| pipette_store::parse_key_set(&bytes, name))
            .unwrap_or_default())
    }

    async fn write_key_set<T: serde::Serialize + Sync>(
        &self,
        name: &str,
        set: &HashSet<T>,
    ) -> anyhow::Result<()> {
        self.put_bytes(&self.key(&format!(".{name}")), serde_json::to_vec(set)?)
            .await
    }

    /// `DeleteObject` is idempotent on S3 — a missing key still returns success.
    async fn delete_key(&self, key: &str) -> anyhow::Result<()> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| s3_error("DeleteObject", key, e))?;
        Ok(())
    }

    /// `(relative_key, last_modified)` under `list_prefix`. A directory bucket
    /// accepts only a prefix ending in `/` and the mock does not care, so the
    /// check below holds the rule.
    async fn list_objects(
        &self,
        list_prefix: &str,
    ) -> anyhow::Result<Vec<(String, Option<DateTime<Utc>>)>> {
        if !list_prefix.ends_with('/') {
            return Err(NonDelimitedPrefixError(list_prefix.to_string()).into());
        }
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
            let resp = req
                .send()
                .await
                .map_err(|e| s3_error("ListObjectsV2", list_prefix, e))?;
            out.extend(resp.contents().iter().filter_map(|obj| {
                let rel = obj.key()?.strip_prefix(list_prefix)?;
                let modified = obj.last_modified().and_then(utc_from_s3);
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

    async fn list_keys(&self, list_prefix: &str) -> anyhow::Result<Vec<String>> {
        Ok(self
            .list_objects(list_prefix)
            .await?
            .into_iter()
            .map(|(key, _)| key)
            .collect())
    }
}

/// An S3 timestamp as a UTC time, or `None` if it is out of `chrono`'s range.
fn utc_from_s3(dt: &aws_sdk_s3::primitives::DateTime) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp(dt.secs(), dt.subsec_nanos())
}

/// `RenameObjectError` has no modeled `NoSuchKey`, so a vanished source (lost
/// claim race) shows only in the error code.
fn is_no_such_key<E: ProvideErrorMetadata>(err: &E) -> bool {
    err.code() == Some("NoSuchKey")
}

/// A list prefix without the trailing `/`, which a directory bucket refuses.
#[derive(Debug, thiserror::Error)]
#[error(
    "list prefix {0:?} does not end in '/'; a directory bucket supports only a \
     prefix that ends in the delimiter"
)]
struct NonDelimitedPrefixError(String);

/// Both keys are named: the source is the contention point every rename races
/// on, the destination what the request addressed.
fn rename_target(src: &str, dst: &str) -> String {
    format!("{src} -> {dst}")
}

/// Lift the status, S3 code, message and request ids out of an [`SdkError`],
/// which displays as its kind alone, so one log line can be quoted to AWS. The
/// message is `Debug`-quoted so a newline off the wire cannot split the record.
fn s3_error<E>(operation: &str, target: &str, err: SdkError<E>) -> anyhow::Error
where
    E: ProvideErrorMetadata + std::error::Error + Send + Sync + 'static,
{
    let status = err
        .raw_response()
        .map_or_else(|| "-".to_string(), |r| r.status().as_u16().to_string());
    let context = format!(
        "S3 {operation} on {target} failed: status={status} code={} message={:?} \
         request_id={} extended_request_id={}",
        err.code().unwrap_or("-"),
        err.message().unwrap_or("-"),
        err.request_id().unwrap_or("-"),
        err.extended_request_id().unwrap_or("-"),
    );
    anyhow::Error::new(err).context(context)
}

// `x-amz-rename-source` is the key alone (the request carries the bucket). It must
// be URL-encoded, and every key here is URL-safe (`[A-Za-z0-9_./-]`), so it goes on
// the wire unchanged.
#[async_trait]
impl TodoStore for S3TodoStore {
    async fn list_avail(&self) -> anyhow::Result<Vec<String>> {
        // A recursive listing of `avail/` returns the `{job_id}/{expires_at}.json`
        // relative keys directly, and sorting them orders by `job_id` first.
        let mut names: Vec<String> = self
            .list_keys(&self.list_prefix("avail/"))
            .await?
            .into_iter()
            .filter(|n| is_json_key(n))
            .collect();
        names.sort_unstable();
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
        // `avail/` partitions by job: one listing, not a walk of the tree.
        let job_prefix = format!("avail/{job_id}/");
        let keys: Vec<String> = self
            .list_keys(&self.list_prefix(&job_prefix))
            .await?
            .into_iter()
            .filter(|leaf| is_json_key(leaf))
            .map(|leaf| self.key(&format!("{job_prefix}{leaf}")))
            .collect();
        stream::iter(
            keys.into_iter()
                .map(|key| async move { self.delete_key(&key).await }),
        )
        .buffer_unordered(pipette_store::STORAGE_CONCURRENCY)
        .try_collect::<Vec<_>>()
        .await?;
        Ok(())
    }

    async fn get_avail_by_job(&self, job_id: &JobId) -> anyhow::Result<Option<serde_json::Value>> {
        // The same job directory as `delete_avail_by_job`.
        let job_prefix = format!("avail/{job_id}/");
        let Some(leaf) = self
            .list_keys(&self.list_prefix(&job_prefix))
            .await?
            .into_iter()
            .find(|leaf| is_json_key(leaf))
        else {
            return Ok(None);
        };
        self.get_json(&self.key(&format!("{job_prefix}{leaf}")))
            .await
    }

    async fn avail_key_for_job(&self, job_id: &JobId) -> anyhow::Result<Option<String>> {
        // The listing returns the `{expires_at}.json` leaf; the job segment goes back
        // in front to make the key the filename parser reads.
        let job_prefix = format!("avail/{job_id}/");
        Ok(self
            .list_keys(&self.list_prefix(&job_prefix))
            .await?
            .into_iter()
            .find(|leaf| is_json_key(leaf))
            .map(|leaf| format!("{job_id}/{leaf}")))
    }

    async fn redate_avail(
        &self,
        job_id: &JobId,
        from: ExpiresAt,
        to: ExpiresAt,
    ) -> anyhow::Result<RedateResult> {
        // Atomic `RenameObject`, as in `claim_job`: a claim that took the source
        // first fails this with `NoSuchKey`, which is `Gone`.
        let src = self.key(&format!("avail/{}", avail_filename(job_id, from)));
        let dst = self.key(&format!("avail/{}", avail_filename(job_id, to)));
        match self
            .client
            .rename_object()
            .bucket(&self.bucket)
            .key(&dst)
            .rename_source(&src)
            .send()
            .await
        {
            Ok(_) => Ok(RedateResult::Redated),
            Err(e) => {
                if is_no_such_key(&e) {
                    Ok(RedateResult::Gone)
                } else {
                    Err(s3_error("RenameObject", &rename_target(&src, &dst), e))
                }
            }
        }
    }

    async fn list_leased(&self) -> anyhow::Result<Vec<String>> {
        // Keys are `{client_id}/{job_id}.{expires_at}.{lease_expiry}.json`, as listed.
        Ok(self
            .list_keys(&self.list_prefix("leased/"))
            .await?
            .into_iter()
            .filter(|n| is_json_key(n))
            .collect())
    }

    async fn list_leased_for_client(&self, client_id: &ClientId) -> anyhow::Result<Vec<String>> {
        let client = client_id.as_str();
        Ok(self
            .list_keys(&self.list_prefix(&format!("leased/{client}/")))
            .await?
            .into_iter()
            .filter(|leaf| is_json_key(leaf))
            .map(|leaf| format!("{client}/{leaf}"))
            .collect())
    }

    async fn get_leased(
        &self,
        job_id: &JobId,
        expires_at: ExpiresAt,
        client_id: &ClientId,
        lease_expiry: DateTime<Utc>,
    ) -> anyhow::Result<Option<serde_json::Value>> {
        self.get_json(&self.key(&format!(
            "leased/{}",
            leased_key(job_id, expires_at, client_id, lease_expiry)
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

        // Renewal needs the lease to exist, not to be unexpired: only the recycler
        // ends a lease. The new name keeps the job's `expires_at` from the key. A
        // job-prefixed key that does not parse is cruft, passed over.
        let own = self
            .list_leased_for_client(client_id)
            .await?
            .into_iter()
            .filter(|rel| {
                rel.rsplit('/')
                    .next()
                    .is_some_and(|leaf| leaf.starts_with(&job_prefix))
            })
            .find_map(|rel| {
                parse_leased_key(&rel)
                    .ok()
                    .map(|lease| (rel, lease.expires_at))
            });
        if let Some((rel, expires_at)) = own {
            let src = self.key(&format!("leased/{rel}"));
            let dst = self.key(&format!(
                "leased/{}",
                leased_key(job_id, expires_at, client_id, new_expiry)
            ));
            match self
                .client
                .rename_object()
                .bucket(&self.bucket)
                .key(&dst)
                .rename_source(&src)
                .send()
                .await
            {
                Ok(_) => return Ok(RenewLeaseResult::Renewed(expires_at)),
                Err(e) => {
                    // Raced a recycle; fall through to the cross-partition check.
                    if !is_no_such_key(&e) {
                        return Err(s3_error("RenameObject", &rename_target(&src, &dst), e));
                    }
                }
            }
        }

        // No lease of ours: a full sweep tells `WrongClient` from `NotFound`.
        // `list_leased` returns every key or errors, so a read failure never reads
        // as `NotFound`.
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
        expires_at: ExpiresAt,
        client_id: &ClientId,
        expiry: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        self.delete_key(&self.key(&format!(
            "leased/{}",
            leased_key(job_id, expires_at, client_id, expiry)
        )))
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
            leased_key(job_id, expires_at, client_id, lease_expiry)
        ));

        // Atomic move: the *source* is the contention point — a second claimant's
        // rename of the now-gone source fails with `NoSuchKey` → `Gone`.
        match self
            .client
            .rename_object()
            .bucket(&self.bucket)
            .key(&dst)
            .rename_source(&src)
            .send()
            .await
        {
            Ok(_) => {
                // `RenameObject` returns no body, so read the now-leased object; the lease
                // is ours, so nothing can move it first.
                let body = self.get_json(&dst).await?.ok_or_else(|| {
                    anyhow::anyhow!("claimed job body missing after rename: {dst}")
                })?;
                Ok(ClaimResult::Claimed(body))
            }
            Err(e) => {
                if is_no_such_key(&e) {
                    Ok(ClaimResult::Gone)
                } else {
                    Err(s3_error("RenameObject", &rename_target(&src, &dst), e))
                }
            }
        }
    }

    async fn recycle_lease(
        &self,
        job_id: &JobId,
        expires_at: ExpiresAt,
        client_id: &ClientId,
        lease_expiry: DateTime<Utc>,
    ) -> anyhow::Result<RecycleResult> {
        // The destination name comes from the lease key, so no body GET precedes the
        // rename. A missing source is `Gone`: someone else resolved the lease.
        let src = self.key(&format!(
            "leased/{}",
            leased_key(job_id, expires_at, client_id, lease_expiry)
        ));
        let dst = self.key(&format!("avail/{}", avail_filename(job_id, expires_at)));
        match self
            .client
            .rename_object()
            .bucket(&self.bucket)
            .key(&dst)
            .rename_source(&src)
            .send()
            .await
        {
            Ok(_) => Ok(RecycleResult::Recycled),
            Err(e) => {
                if is_no_such_key(&e) {
                    Ok(RecycleResult::Gone)
                } else {
                    Err(s3_error("RenameObject", &rename_target(&src, &dst), e))
                }
            }
        }
    }

    async fn write_denied(
        &self,
        job_id: &JobId,
        client_id: &ClientId,
        record: &DenialRecord,
    ) -> anyhow::Result<()> {
        self.put_bytes(&self.denied_key(job_id, client_id), record.to_bytes()?)
            .await
    }

    async fn get_denied(
        &self,
        job_id: &JobId,
        client_id: &ClientId,
    ) -> anyhow::Result<Option<DenialRecord>> {
        let key = self.denied_key(job_id, client_id);
        self.get_bytes(&key)
            .await?
            .map(|bytes| {
                DenialRecord::from_bytes(&bytes)
                    .with_context(|| format!("denied marker {key} does not parse"))
            })
            .transpose()
    }

    async fn is_denied(&self, job_id: &JobId, client_id: &ClientId) -> anyhow::Result<bool> {
        // The flat `denied/{job_id}.{client_id}` key makes this a point read: a
        // directory bucket lists only `/`-ended prefixes, so a listing would take the
        // whole `denied/` namespace.
        Ok(self
            .get_bytes(&self.denied_key(job_id, client_id))
            .await?
            .is_some())
    }

    async fn delete_denied_for_job(&self, job_id: &JobId) -> anyhow::Result<()> {
        let prefix = denied_job_prefix(job_id);
        let names = self.list_keys(&self.list_prefix("denied/")).await?;
        for name in names.into_iter().filter(|n| n.starts_with(&prefix)) {
            self.delete_key(&self.key(&format!("denied/{name}")))
                .await?;
        }
        Ok(())
    }

    async fn delete_denied(&self, job_id: &JobId, client_id: &ClientId) -> anyhow::Result<()> {
        self.delete_key(&self.denied_key(job_id, client_id)).await
    }

    async fn list_all_denied(&self) -> anyhow::Result<Vec<(JobId, ClientId)>> {
        // Malformed names are logged and skipped.
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
                let client_id = match id_from_key(client_id, ClientId::try_new) {
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
            pipette_core::todo_filename::pending_reindex_filename(client_id)
        )))
        .await
    }

    async fn list_pending_reindex(&self) -> anyhow::Result<Vec<(ClientId, String)>> {
        // Listing errors propagate (the gate must not lose a flag); an unparseable
        // name is cruft, warned and skipped.
        Ok(self
            .list_keys(&self.list_prefix("pending-reindex/"))
            .await?
            .into_iter()
            .filter_map(|name| {
                match pipette_core::todo_filename::parse_pending_reindex_filename(&name) {
                    Ok(client_id) => Some((client_id, name)),
                    Err(_) => {
                        tracing::warn!(key = %name, "skipping unparseable pending-reindex flag");
                        None
                    }
                }
            })
            .collect())
    }

    async fn delete_pending_reindex(&self, key: &str) -> anyhow::Result<()> {
        self.delete_key(&self.key(&format!("pending-reindex/{key}")))
            .await
    }

    async fn has_pending_reindex(&self, client_id: &ClientId) -> anyhow::Result<bool> {
        // List the full prefix and compare parsed ids: a `pending-reindex/{client_id}`
        // prefix would also match a longer id. Flags are few, so the listing is small.
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
        // Listing errors propagate (a lost flag is lost reindex debt); an unparseable
        // name is warned and skipped rather than wedge every run.
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

    async fn write_lost_lease(
        &self,
        job_id: &JobId,
        client_id: &ClientId,
        lease_expiry: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        self.put_empty(&self.key(&format!(
            "lost/{job_id}/{}",
            lost_lease_filename(client_id, lease_expiry)
        )))
        .await
    }

    async fn list_lost_leases(
        &self,
        job_id: &JobId,
    ) -> anyhow::Result<Vec<(ClientId, DateTime<Utc>)>> {
        let mut losses: Vec<_> = self
            .list_keys(&self.list_prefix(&format!("lost/{job_id}/")))
            .await?
            .into_iter()
            .filter_map(|name| {
                parse_lost_lease_filename(&name)
                    .inspect_err(|e| {
                        tracing::warn!(key = %name, error = %e, "skipping unparseable lost-lease marker");
                    })
                    .ok()
            })
            .collect();
        losses.sort_by_key(|(_, expiry)| *expiry);
        Ok(losses)
    }

    async fn delete_lost_leases_for_job(&self, job_id: &JobId) -> anyhow::Result<()> {
        let prefix = format!("lost/{job_id}/");
        for name in self.list_keys(&self.list_prefix(&prefix)).await? {
            self.delete_key(&self.key(&format!("{prefix}{name}")))
                .await?;
        }
        Ok(())
    }

    async fn list_lost_lease_jobs(&self) -> anyhow::Result<Vec<JobId>> {
        // A recursive listing returns `{job_id}/{marker}`, one per marker, and a
        // directory bucket does not list in key order: sort before `dedup`.
        let mut jobs: Vec<JobId> = self
            .list_keys(&self.list_prefix("lost/"))
            .await?
            .into_iter()
            .filter_map(|name| {
                let job = name.split_once('/').map_or(name.as_str(), |(job, _)| job);
                JobId::try_new(job)
                    .inspect_err(
                        |_| tracing::warn!(key = %name, "skipping unparseable lost/ entry"),
                    )
                    .ok()
            })
            .collect();
        jobs.sort_unstable();
        jobs.dedup();
        Ok(jobs)
    }

    async fn write_tmp(&self, job_id: &JobId, body: &serde_json::Value) -> anyhow::Result<()> {
        let key = self.key(&format!("tmp/{}", tmp_filename(job_id)));
        self.put_bytes(&key, serde_json::to_vec(body)?).await
    }

    async fn promote_avail(&self, job_id: &JobId, expires_at: ExpiresAt) -> anyhow::Result<()> {
        // Atomic `RenameObject`, as in `claim_job`: the staged body appears in
        // `avail/` whole or not at all. A vanished source is an error, not a no-op.
        let src = self.key(&format!("tmp/{}", tmp_filename(job_id)));
        let dst = self.key(&format!("avail/{}", avail_filename(job_id, expires_at)));
        self.client
            .rename_object()
            .bucket(&self.bucket)
            .key(&dst)
            .rename_source(&src)
            .send()
            .await
            .map_err(|e| s3_error("RenameObject", &rename_target(&src, &dst), e))?;
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

    async fn read_eligible_indexed(&self) -> anyhow::Result<HashSet<String>> {
        self.read_key_set("eligible-indexed").await
    }

    async fn write_eligible_indexed(&self, indexed: &HashSet<String>) -> anyhow::Result<()> {
        self.write_key_set("eligible-indexed", indexed).await
    }

    async fn read_eligible_workers(&self) -> anyhow::Result<HashSet<ClientId>> {
        self.read_key_set("eligible-workers").await
    }

    async fn write_eligible_workers(&self, workers: &HashSet<ClientId>) -> anyhow::Result<()> {
        self.write_key_set("eligible-workers", workers).await
    }

    async fn read_backlog(&self) -> anyhow::Result<Option<BacklogSnapshot>> {
        Ok(self
            .get_bytes(&self.key(".backlog"))
            .await?
            .and_then(|bytes| pipette_store::parse_snapshot(&bytes, "todo/.backlog")))
    }

    async fn write_backlog(&self, snapshot: &BacklogSnapshot) -> anyhow::Result<()> {
        self.put_bytes(&self.key(".backlog"), serde_json::to_vec(snapshot)?)
            .await
    }

    async fn read_campaign_backlog(&self) -> anyhow::Result<Option<CampaignBacklogSnapshot>> {
        Ok(self
            .get_bytes(&self.key(".backlog-campaigns"))
            .await?
            .and_then(|bytes| pipette_store::parse_snapshot(&bytes, "todo/.backlog-campaigns")))
    }

    async fn write_campaign_backlog(
        &self,
        snapshot: &CampaignBacklogSnapshot,
    ) -> anyhow::Result<()> {
        self.put_bytes(
            &self.key(".backlog-campaigns"),
            serde_json::to_vec(snapshot)?,
        )
        .await
    }

    async fn read_gc_candidates(&self) -> anyhow::Result<HashSet<String>> {
        self.read_key_set("gc-candidates").await
    }

    async fn write_gc_candidates(&self, candidates: &HashSet<String>) -> anyhow::Result<()> {
        self.write_key_set("gc-candidates", candidates).await
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

    async fn list_suspended_ids(&self) -> anyhow::Result<Vec<ClientId>> {
        let mut ids: Vec<ClientId> = self
            .list_keys(&self.list_prefix("suspended/"))
            .await?
            .iter()
            .filter_map(|name| pipette_store::flat_keyspace_id(name, ClientId::try_new))
            .collect();
        ids.sort();
        Ok(ids)
    }

    async fn stamp_liveness(&self, client_id: &ClientId) -> anyhow::Result<()> {
        self.put_empty(&self.key(&format!("liveness/{}", client_id.as_str())))
            .await
    }

    async fn read_liveness(&self, client_id: &ClientId) -> anyhow::Result<Option<DateTime<Utc>>> {
        let key = self.key(&format!("liveness/{}", client_id.as_str()));
        match self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(&key)
            .send()
            .await
        {
            Ok(out) => out
                .last_modified()
                .and_then(utc_from_s3)
                .map(Some)
                .ok_or_else(|| anyhow::anyhow!("S3 GetObject on {key} returned no LastModified")),
            Err(e) => match e.as_service_error() {
                Some(GetObjectError::NoSuchKey(_)) => Ok(None),
                _ => Err(s3_error("GetObject", &key, e)),
            },
        }
    }

    async fn list_liveness(&self) -> anyhow::Result<Vec<(ClientId, DateTime<Utc>)>> {
        Ok(self
            .list_objects(&self.list_prefix("liveness/"))
            .await?
            .into_iter()
            .filter_map(|(name, modified)| {
                let Ok(client_id) = id_from_key(&name, ClientId::try_new) else {
                    tracing::warn!(key = %name, "skipping unparseable liveness marker");
                    return None;
                };
                let Some(modified) = modified else {
                    tracing::warn!(key = %name, "skipping liveness marker listed without LastModified");
                    return None;
                };
                Some((client_id, modified))
            })
            .collect())
    }

    async fn delete_liveness(&self, client_id: &ClientId) -> anyhow::Result<()> {
        self.delete_key(&self.key(&format!("liveness/{}", client_id.as_str())))
            .await
    }

    async fn list_suspensions(&self) -> anyhow::Result<Vec<(ClientId, SuspensionRecord)>> {
        let names = self.list_keys(&self.list_prefix("suspended/")).await?;
        let mut result = Vec::new();
        for name in names {
            let Some(id_str) = name.strip_suffix(".json") else {
                continue;
            };
            let client_id = id_from_key(id_str, ClientId::try_new)?;
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
        // Stage a probe under a random `tmp/` key, rename it, and read both keys
        // back: a rename that moves nothing or keeps the source would let two claims
        // win, or every claim lose. `NotImplemented` (a general-purpose bucket) is
        // fatal, as `claim_job` would be a non-atomic copy-then-delete; any other
        // error propagates unchanged. Random, as `tmp/` is live: a fixed key could
        // hit a job body being staged. It proves a move, not atomicity; on AWS the
        // two coincide, as only Express One Zone has the API.
        let probe = format!("tmp/.rename-probe-{}", uuid::Uuid::new_v4());
        let src = self.key(&probe);
        let dst = self.key(&format!("{probe}.moved"));
        self.put_empty(&src)
            .await
            .map_err(|e| e.context(format!("staging the RenameObject probe object at {src}")))?;

        let verdict = self.probe_rename(&src, &dst).await;

        // Delete both keys on every path: a failed or copying rename leaves either.
        // A failed delete is logged; the stale-`tmp/` sweep collects it.
        for key in [&src, &dst] {
            if let Err(e) = self.delete_key(key).await {
                tracing::warn!(
                    key = %key,
                    error = %e,
                    "could not delete a RenameObject probe object"
                );
            }
        }
        Ok(verdict?)
    }
}

// ---------------------------------------------------------------------------
// Tests (using in-memory ObjectStore)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use anyhow::Context;

    use pipette_core::types::Mint;
    use pipette_plan_types::{BenchmarkId, ClientId, JobId};
    use pipette_store_testkit::TEST_LIST_LIMIT;

    use crate::*;

    /// The contracts' raw-write hook: `key` relative to the test prefix.
    pub(crate) fn write_raw_under(
        store: &Arc<dyn ObjectStore>,
    ) -> impl Fn(String, Vec<u8>) -> futures::future::BoxFuture<'static, anyhow::Result<()>> + '_
    {
        move |key, bytes| {
            let path = obj_path("test-prefix", &key);
            let handle = store.clone();
            Box::pin(async move { put_bytes(&handle, &path, bytes).await })
        }
    }

    /// A malformed object must not cost the deployment its whole catalog.
    #[tokio::test]
    async fn load_catalog_skips_an_unreadable_object() -> anyhow::Result<()> {
        let store: Arc<dyn ObjectStore> = Arc::new(object_store::memory::InMemory::new());
        put_bytes(
            &store,
            &obj_path("test", "benchmarks/prefill_throughput_256.toml"),
            b"benchmark_type = \"prefill_throughput\"\nparameter_prefill_tokens = 256".to_vec(),
        )
        .await?;
        put_bytes(
            &store,
            &obj_path("test", "benchmarks/bad_unknown_key.toml"),
            b"benchmark_type = \"prefill_throughput\"\nparameter_prefill_tokens = 1\nnope = 1"
                .to_vec(),
        )
        .await?;

        let catalog = S3CatalogStore {
            store,
            prefix: "test".to_string(),
        }
        .load_catalog()
        .await?;

        assert_eq!(catalog.len(), 1);
        assert!(catalog.contains_key(&BenchmarkId::try_new("prefill_throughput_256")?));
        Ok(())
    }

    #[tokio::test]
    async fn org_catalog_meets_the_shared_contract() -> anyhow::Result<()> {
        let stores = make_test_stores("test");
        pipette_store_testkit::assert_org_catalog_store_roundtrip(&*stores.catalog).await
    }

    #[tokio::test]
    async fn org_benchmark_listing_meets_the_shared_contract() -> anyhow::Result<()> {
        let store: Arc<dyn ObjectStore> = Arc::new(object_store::memory::InMemory::new());
        let catalog = S3CatalogStore {
            store: store.clone(),
            prefix: "test".to_string(),
        };
        pipette_store_testkit::assert_org_benchmark_listing(&catalog, |org_id, id, body| {
            let store = store.clone();
            async move {
                let key = format!("org-benchmarks/{org_id}/{id}.toml");
                put_bytes(&store, &obj_path("test", &key), body.as_bytes().to_vec()).await
            }
        })
        .await
    }

    #[tokio::test]
    async fn org_benchmark_ids_meet_the_shared_contract() -> anyhow::Result<()> {
        let stores = make_test_stores("test");
        pipette_store_testkit::assert_org_benchmark_ids_span_every_org(&*stores.catalog).await
    }

    /// A deeper key, another extension or a non-org segment is passed over.
    #[tokio::test]
    async fn org_benchmark_ids_skip_what_is_not_an_entry() -> anyhow::Result<()> {
        let store: Arc<dyn ObjectStore> = Arc::new(object_store::memory::InMemory::new());
        let catalog = S3CatalogStore {
            store: store.clone(),
            prefix: "test".to_string(),
        };
        let acme = OrgId::mint();
        let entry = pipette_store_testkit::an_org_benchmark()?;
        catalog.put_org_benchmark(&acme, &entry).await?;
        for stray in [
            format!("org-benchmarks/{acme}/nested/x.toml"),
            format!("org-benchmarks/{acme}/notes.txt"),
            "org-benchmarks/not-an-org/x.toml".to_string(),
        ] {
            put_bytes(&store, &obj_path("test", &stray), Vec::new()).await?;
        }

        assert_eq!(
            catalog.list_org_benchmark_ids().await?,
            vec![(acme, entry.benchmark_id().clone())]
        );
        Ok(())
    }

    /// An organization's entries sit beside `benchmarks/`, so the public
    /// loader's recursive listing never reaches them.
    #[tokio::test]
    async fn org_catalog_stays_out_of_the_public_listing() -> anyhow::Result<()> {
        let store: Arc<dyn ObjectStore> = Arc::new(object_store::memory::InMemory::new());
        put_bytes(
            &store,
            &obj_path("test", "benchmarks/prefill_throughput_256.toml"),
            b"benchmark_type = \"prefill_throughput\"\nparameter_prefill_tokens = 256".to_vec(),
        )
        .await?;
        let catalog = S3CatalogStore {
            store: store.clone(),
            prefix: "test".to_string(),
        };
        let acme = OrgId::mint();
        catalog
            .put_org_benchmark(&acme, &pipette_store_testkit::an_org_benchmark()?)
            .await?;

        let keys: Vec<String> = store
            .list(None)
            .try_collect::<Vec<_>>()
            .await?
            .into_iter()
            .map(|meta| meta.location.to_string())
            .collect();
        assert!(
            keys.iter()
                .any(|k| k
                    == &format!("test/org-benchmarks/{acme}/prefill_throughput_acme_4096.toml")),
            "{keys:?}"
        );
        assert_eq!(catalog.load_catalog().await?.len(), 1);
        Ok(())
    }

    /// An unreadable object is skipped by a listing and named by a read by id; an
    /// object without the `.toml` suffix is no entry.
    #[tokio::test]
    async fn org_catalog_unreadable_object() -> anyhow::Result<()> {
        let store: Arc<dyn ObjectStore> = Arc::new(object_store::memory::InMemory::new());
        let catalog = S3CatalogStore {
            store: store.clone(),
            prefix: "test".to_string(),
        };
        let acme = OrgId::mint();
        catalog
            .put_org_benchmark(&acme, &pipette_store_testkit::an_org_benchmark()?)
            .await?;
        put_bytes(
            &store,
            &obj_path("test", &format!("org-benchmarks/{acme}/broken.toml")),
            b"benchmark_type = \"nope\"".to_vec(),
        )
        .await?;
        put_bytes(
            &store,
            &obj_path("test", &format!("org-benchmarks/{acme}/binary.toml")),
            vec![0xff, 0xfe],
        )
        .await?;
        put_bytes(
            &store,
            &obj_path("test", &format!("org-benchmarks/{acme}/notes.txt")),
            b"not an entry".to_vec(),
        )
        .await?;

        let loaded = catalog.load_org_catalog(&acme).await?;
        assert_eq!(loaded.len(), 1);
        assert!(loaded.contains_key(&BenchmarkId::try_new("prefill_throughput_acme_4096")?));
        let reads = [
            catalog
                .get_org_benchmark(&acme, &BenchmarkId::try_new("broken")?)
                .await,
            catalog
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

    /// An in-memory bucket whose reads take a moment and record how many were
    /// in flight at once.
    #[derive(Debug, Default)]
    struct SlowReads {
        inner: object_store::memory::InMemory,
        in_flight: std::sync::atomic::AtomicUsize,
        peak: std::sync::atomic::AtomicUsize,
    }

    impl std::fmt::Display for SlowReads {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "SlowReads")
        }
    }

    #[async_trait]
    impl ObjectStore for SlowReads {
        async fn put_opts(
            &self,
            location: &ObjPath,
            payload: PutPayload,
            opts: object_store::PutOptions,
        ) -> object_store::Result<object_store::PutResult> {
            self.inner.put_opts(location, payload, opts).await
        }

        async fn put_multipart_opts(
            &self,
            location: &ObjPath,
            opts: object_store::PutMultipartOptions,
        ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
            self.inner.put_multipart_opts(location, opts).await
        }

        async fn get_opts(
            &self,
            location: &ObjPath,
            options: object_store::GetOptions,
        ) -> object_store::Result<object_store::GetResult> {
            use std::sync::atomic::Ordering::SeqCst;
            let now = self.in_flight.fetch_add(1, SeqCst) + 1;
            self.peak.fetch_max(now, SeqCst);
            tokio::time::sleep(Duration::from_millis(5)).await;
            self.in_flight.fetch_sub(1, SeqCst);
            self.inner.get_opts(location, options).await
        }

        fn delete_stream(
            &self,
            locations: futures::stream::BoxStream<'static, object_store::Result<ObjPath>>,
        ) -> futures::stream::BoxStream<'static, object_store::Result<ObjPath>> {
            self.inner.delete_stream(locations)
        }

        fn list(
            &self,
            prefix: Option<&ObjPath>,
        ) -> futures::stream::BoxStream<'static, object_store::Result<object_store::ObjectMeta>>
        {
            self.inner.list(prefix)
        }

        async fn list_with_delimiter(
            &self,
            prefix: Option<&ObjPath>,
        ) -> object_store::Result<object_store::ListResult> {
            self.inner.list_with_delimiter(prefix).await
        }

        async fn copy_opts(
            &self,
            from: &ObjPath,
            to: &ObjPath,
            options: object_store::CopyOptions,
        ) -> object_store::Result<()> {
            self.inner.copy_opts(from, to, options).await
        }
    }

    /// A signed read fetches the whole org catalog, so entries load side by side.
    #[tokio::test]
    async fn org_catalog_reads_its_entries_concurrently() -> anyhow::Result<()> {
        let bucket = Arc::new(SlowReads::default());
        let catalog = S3CatalogStore {
            store: bucket.clone(),
            prefix: "test".to_string(),
        };
        let acme = OrgId::mint();
        for n in 0..8 {
            put_bytes(
                &catalog.store,
                &obj_path(
                    "test",
                    &format!("org-benchmarks/{acme}/prefill_acme_{n}.toml"),
                ),
                b"benchmark_type = \"prefill_throughput\"\nparameter_prefill_tokens = 256".to_vec(),
            )
            .await?;
        }

        assert_eq!(catalog.load_org_catalog(&acme).await?.len(), 8);
        let peak = bucket.peak.load(std::sync::atomic::Ordering::SeqCst);
        assert!(peak > 1, "the reads ran one at a time");
        Ok(())
    }

    #[tokio::test]
    async fn load_catalog_rejects_an_empty_prefix() -> anyhow::Result<()> {
        let stores = make_test_stores("test");
        match stores.catalog.load_catalog().await {
            Err(e) => {
                let msg = format!("{e:#}");
                assert!(msg.contains("no benchmark objects found"), "{msg}");
            }
            Ok(catalog) => {
                anyhow::bail!(
                    "an empty prefix must not load, got {} entries",
                    catalog.len()
                )
            }
        }
        Ok(())
    }

    fn make_test_stores(prefix: &str) -> Stores {
        let store: Arc<dyn ObjectStore> = Arc::new(object_store::memory::InMemory::new());
        let prefix = prefix.to_string();
        Stores {
            catalog: Arc::new(S3CatalogStore {
                store: store.clone(),
                prefix: prefix.clone(),
            }),
            clients: Arc::new(S3ClientStore::new(store.clone(), prefix.clone())),
            users: Arc::new(S3UserStore::new(store.clone(), prefix.clone())),
            orgs: Arc::new(S3OrgStore::new(store.clone(), prefix.clone())),
            tags: Arc::new(S3TagStore::new(store.clone(), prefix.clone())),
            key_index: Arc::new(S3KeyIndexStore::new(store.clone(), prefix.clone())),
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
                store: store.clone(),
                prefix: prefix.clone(),
                writer_opts: WriterOpts::default(),
            }),
            dataset_cache: Arc::new(dataset_cache::S3DatasetCacheStore { store, prefix }),
            todo: Arc::new(pipette_store_testkit::TodoStoreUnimplemented),
            plans: Arc::new(pipette_store_testkit::PlanStoreUnimplemented),
            campaigns: Arc::new(pipette_store_testkit::CampaignStoreUnimplemented),
            model_params: Arc::new(pipette_store_testkit::ModelParamsStoreUnimplemented),
            audit: None,
            descriptor_catalog: None,
            catalog_import_runs: None,
            vault: Arc::new(pipette_store_testkit::VaultStoreUnimplemented),
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
            value: Some(value),
            value_stddev,
            unit: unit.to_string(),
            submitted_at: 1_741_608_060_000_000,
            scored_at: 1_741_608_120_000_000,
            parameter_prefill_tokens: Some(256),
            ..Default::default()
        })
    }

    /// Held to the shared contract: `If-None-Match: *` refuses a taken key where
    /// local_fs's `hard_link` does.
    #[tokio::test]
    async fn test_s3_exclusive_incoming_write_meets_the_shared_contract() -> anyhow::Result<()> {
        let stores = make_test_stores("");
        pipette_store_testkit::assert_exclusive_incoming_write(stores.submissions.as_ref()).await
    }

    #[tokio::test]
    async fn test_s3_has_record_meets_the_shared_contract() -> anyhow::Result<()> {
        let stores = make_test_stores("");
        pipette_store_testkit::assert_has_record_contract(stores.submissions.as_ref()).await
    }

    #[tokio::test]
    async fn test_s3_find_outcomes_meets_the_shared_contract() -> anyhow::Result<()> {
        let stores = make_test_stores("");
        pipette_store_testkit::assert_find_outcomes_contract(stores.submissions.as_ref()).await
    }

    /// The kind of read that a [`RefusingReads`] store refuses.
    #[derive(Debug, Clone, Copy)]
    enum Refuse {
        Get,
        Head,
    }

    /// Refuses one kind of read with an error other than `NotFound`, as a denying
    /// bucket policy does.
    #[derive(Debug)]
    struct RefusingReads {
        inner: object_store::memory::InMemory,
        refuse: Refuse,
    }

    impl std::fmt::Display for RefusingReads {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "RefusingReads({:?})", self.refuse)
        }
    }

    #[async_trait::async_trait]
    impl ObjectStore for RefusingReads {
        async fn put_opts(
            &self,
            location: &ObjPath,
            payload: PutPayload,
            opts: object_store::PutOptions,
        ) -> object_store::Result<object_store::PutResult> {
            self.inner.put_opts(location, payload, opts).await
        }

        async fn put_multipart_opts(
            &self,
            location: &ObjPath,
            opts: object_store::PutMultipartOptions,
        ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
            self.inner.put_multipart_opts(location, opts).await
        }

        async fn get_opts(
            &self,
            location: &ObjPath,
            options: object_store::GetOptions,
        ) -> object_store::Result<object_store::GetResult> {
            match (self.refuse, options.head) {
                (Refuse::Get, false) | (Refuse::Head, true) => Err(object_store::Error::Generic {
                    store: "RefusingReads",
                    source: format!("access denied to {location}").into(),
                }),
                (Refuse::Get, true) | (Refuse::Head, false) => {
                    self.inner.get_opts(location, options).await
                }
            }
        }

        fn delete_stream(
            &self,
            locations: futures::stream::BoxStream<'static, object_store::Result<ObjPath>>,
        ) -> futures::stream::BoxStream<'static, object_store::Result<ObjPath>> {
            self.inner.delete_stream(locations)
        }

        fn list(
            &self,
            prefix: Option<&ObjPath>,
        ) -> futures::stream::BoxStream<'static, object_store::Result<object_store::ObjectMeta>>
        {
            self.inner.list(prefix)
        }

        async fn list_with_delimiter(
            &self,
            prefix: Option<&ObjPath>,
        ) -> object_store::Result<object_store::ListResult> {
            self.inner.list_with_delimiter(prefix).await
        }

        async fn copy_opts(
            &self,
            from: &ObjPath,
            to: &ObjPath,
            options: object_store::CopyOptions,
        ) -> object_store::Result<()> {
            self.inner.copy_opts(from, to, options).await
        }
    }

    /// `has_record` reads only metadata, and a metadata error other than
    /// `NotFound` propagates, since `false` means "the job has no result".
    #[rstest::rstest]
    #[case::get_refused(Refuse::Get, Some([true, true, false]))]
    #[case::head_refused(Refuse::Head, None)]
    #[tokio::test]
    async fn test_s3_has_record_reads_only_metadata(
        #[case] refuse: Refuse,
        #[case] expected: Option<[bool; 3]>,
    ) -> anyhow::Result<()> {
        let (sub, incoming, processed) = refusing_submissions(refuse).await?;
        let none = JobId::new_unchecked("job-none");
        let found: Vec<_> = stream::iter([&incoming, &processed, &none])
            .then(|job_id| sub.has_record(job_id))
            .collect()
            .await;
        match expected {
            Some(expected) => {
                let found = found.into_iter().collect::<anyhow::Result<Vec<_>>>()?;
                assert_eq!(found, expected);
            }
            None => assert!(found.iter().all(Result::is_err), "{found:?}"),
        }
        Ok(())
    }

    /// Each `GET` counts in [`S3_DECODE`], and a refused one errors. The counters
    /// are per process, so this needs a process of its own, which `cargo nextest` gives.
    #[rstest::rstest]
    #[case::head_refused(Refuse::Head, Some(2))]
    #[case::get_refused(Refuse::Get, None)]
    #[tokio::test]
    async fn test_s3_find_outcomes_counts_each_body(
        #[case] refuse: Refuse,
        #[case] bodies: Option<u64>,
    ) -> anyhow::Result<()> {
        let (sub, incoming, processed) = refusing_submissions(refuse).await?;

        let start = S3_DECODE.checkpoint();
        let found = sub.find_outcomes(&[incoming, processed]).await;
        let decoded = S3_DECODE.checkpoint().since(start);
        match bodies {
            Some(bodies) => {
                assert_eq!(found?.len(), 2);
                assert_eq!(decoded.bodies, bodies);
                assert!(decoded.decoded_bytes > decoded.stored_bytes, "{decoded:?}");
            }
            None => {
                assert!(found.is_err(), "{found:?}");
                assert_eq!(decoded.bodies, 0);
            }
        }
        Ok(())
    }

    /// The `processed/` record carries padding, so its gzipped body is smaller
    /// than its JSON.
    async fn refusing_submissions(
        refuse: Refuse,
    ) -> anyhow::Result<(S3SubmissionStore, JobId, JobId)> {
        let sub = S3SubmissionStore {
            store: Arc::new(RefusingReads {
                inner: object_store::memory::InMemory::new(),
                refuse,
            }),
            prefix: String::new(),
        };
        let incoming = JobId::new_unchecked("job-incoming");
        let processed = JobId::new_unchecked("job-processed");
        let mut body = sample_submission(processed.as_str(), "ev1_client1");
        body["padding"] = serde_json::Value::String("x".repeat(10_000));
        sub.write_incoming(
            &incoming,
            &sample_submission(incoming.as_str(), "ev1_client1"),
        )
        .await?;
        sub.write_processed(&processed, &body).await?;
        Ok((sub, incoming, processed))
    }

    #[tokio::test]
    async fn test_s3_runtime_output_meets_the_shared_contract() -> anyhow::Result<()> {
        let stores = make_test_stores("");
        pipette_store_testkit::assert_runtime_output_roundtrip(stores.submissions.as_ref()).await
    }

    #[tokio::test]
    async fn test_s3_submission_listings_meet_the_shared_contract() -> anyhow::Result<()> {
        let stores = make_test_stores("");
        pipette_store_testkit::assert_submission_listings_page(stores.submissions.as_ref()).await
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
        // ~1 MB exercises the chunked gzip streaming in mark_processed.
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

    /// The counters are per process, so this needs a process of its own, which
    /// `cargo nextest` gives.
    #[tokio::test]
    async fn a_processed_read_counts_its_decode() -> anyhow::Result<()> {
        let stores = make_test_stores("");
        let sub = stores.submissions.as_ref();
        let job_id = JobId::new_unchecked("jobD");
        let mut body = sample_submission(job_id.as_str(), "ev1_clientD");
        body["padding"] = serde_json::Value::String("x".repeat(10_000));
        sub.write_incoming(&job_id, &body).await?;
        sub.mark_processed(&job_id).await?;

        let start = S3_DECODE.checkpoint();
        let found = sub
            .find_job(&job_id)
            .await?
            .context("expected the record")?;
        let decoded = S3_DECODE.checkpoint().since(start);
        assert_eq!(found.state, JobState::Processed);
        assert_eq!(decoded.bodies, 1);
        assert!(decoded.decoded_bytes > decoded.stored_bytes, "{decoded:?}");
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
        // A re-scored job resolves to its newest copy by scored_at.
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

    /// Rows from every part file count, not just the first part with a match.
    #[tokio::test]
    async fn test_s3_read_job_metrics_aggregates_across_part_files() -> anyhow::Result<()> {
        // max_rows_per_part = 1: each row lands in its own part.
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

        // Two part files, or this test passes for the wrong reason.
        let part_prefix = ObjPath::from(
            "warehouse/results/benchmark_id=bench1/client_id=ev1_client1/day=2025-03-10/",
        );
        let entries: Vec<_> = store.list(Some(&part_prefix)).try_collect().await?;
        assert_eq!(
            entries.len(),
            2,
            "expected 2 part files for max_rows_per_part=1 + 2 rows"
        );

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
                measures: None,
                chat_template_kwargs: None,
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
                measures: None,
                chat_template_kwargs: None,
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
// S3TodoStore tests (aws-smithy-mocks)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod todo_mock_tests {
    use anyhow::Context;
    use aws_sdk_s3::error::ErrorMetadata;
    use aws_sdk_s3::operation::delete_object::DeleteObjectOutput;
    use aws_sdk_s3::operation::get_object::{GetObjectError, GetObjectOutput};
    use aws_sdk_s3::operation::list_objects_v2::ListObjectsV2Output;
    use aws_sdk_s3::operation::put_object::PutObjectOutput;
    use aws_sdk_s3::operation::rename_object::{RenameObjectError, RenameObjectOutput};
    use aws_sdk_s3::primitives::DateTime as AwsDateTime;
    use aws_sdk_s3::types::Object;
    use aws_sdk_s3::types::error::NoSuchKey;
    use aws_smithy_mocks::{Rule, RuleMode, mock};
    use rstest::rstest;

    use super::mock_bucket::{bucket_list_rule, store};
    use super::*;

    /// The three reads that address a job's `avail/` entry by `job_id` alone.
    #[derive(Debug, Clone, Copy)]
    enum JobScoped {
        Body,
        Key,
        Delete,
    }

    fn job(s: &str) -> JobId {
        JobId::new_unchecked(s)
    }

    fn cid(s: &str) -> anyhow::Result<ClientId> {
        Ok(ClientId::try_new(s)?)
    }

    fn ts() -> DateTime<Utc> {
        dt("2026-01-01T00:00:00Z")
    }

    /// Infallible so it fits in a `#[case]`; an unparsable literal falls back to
    /// the epoch, which no assertion expects.
    fn dt(s: &str) -> DateTime<Utc> {
        s.parse().unwrap_or(DateTime::UNIX_EPOCH)
    }

    fn get_no_such_key() -> GetObjectError {
        GetObjectError::NoSuchKey(NoSuchKey::builder().build())
    }

    /// `RenameObjectError` has no modeled `NoSuchKey`; S3 sends only the code.
    fn rename_no_such_key() -> RenameObjectError {
        RenameObjectError::generic(
            ErrorMetadata::builder()
                .code("NoSuchKey")
                .message("source gone")
                .build(),
        )
    }

    /// A single-page `list_objects_v2` rule for `prefix`; `keys` are absolute.
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
            .match_requests(|r| r.key() == Some("todo/avail/job1/never.json"))
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
    async fn get_avail_by_job_lists_only_the_jobs_prefix() -> anyhow::Result<()> {
        let page = list_rule(
            "todo/avail/job1/",
            vec!["todo/avail/job1/never.json".to_string()],
        );
        let body = mock!(aws_sdk_s3::Client::get_object)
            .match_requests(|r| r.key() == Some("todo/avail/job1/never.json"))
            .then_output(|| {
                GetObjectOutput::builder()
                    .body(ByteStream::from_static(br#"{"clients":["ev1_a"]}"#))
                    .build()
            });
        let s = store(aws_smithy_mocks::mock_client!(
            aws_sdk_s3,
            RuleMode::Sequential,
            &[&page, &body]
        ));
        let read = s
            .get_avail_by_job(&job("job1"))
            .await?
            .ok_or_else(|| anyhow::anyhow!("expected avail body"))?;
        assert_eq!(read["clients"][0], "ev1_a");
        Ok(())
    }

    #[tokio::test]
    async fn get_avail_by_job_missing_is_none() -> anyhow::Result<()> {
        let page = list_rule("todo/avail/job1/", Vec::new());
        let s = store(aws_smithy_mocks::mock_client!(
            aws_sdk_s3,
            RuleMode::Sequential,
            &[&page]
        ));
        assert!(s.get_avail_by_job(&job("job1")).await?.is_none());
        Ok(())
    }

    /// The `SdkError` alone displays as `service error`, so the message must carry
    /// the operation, the target and the verdict.
    #[tokio::test]
    async fn s3_error_reports_the_operation_target_and_verdict() -> anyhow::Result<()> {
        const DENIED: &str = concat!(
            r#"<?xml version="1.0" encoding="UTF-8"?>"#,
            "<Error><Code>AccessDenied</Code><Message>",
            "User is not authorized to perform: s3express:CreateSession",
            "</Message></Error>",
        );
        let denied = mock!(aws_sdk_s3::Client::list_objects_v2)
            .sequence()
            .http_status(403, Some(DENIED.to_string()))
            .build();
        let s = store(aws_smithy_mocks::mock_client!(
            aws_sdk_s3,
            RuleMode::Sequential,
            &[&denied]
        ));

        let err = s
            .get_avail_by_job(&job("job1"))
            .await
            .err()
            .context("expected the listing to fail")?;
        let text = format!("{err:#}");
        for expected in [
            "S3 ListObjectsV2 on todo/avail/job1/ failed",
            "status=403",
            "code=AccessDenied",
            "s3express:CreateSession",
            // A response with no request-id headers reads `-` for both.
            "request_id=- extended_request_id=-",
            // The SDK error stays the source, so `{e:#}` keeps its chain.
            "service error",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in {text:?}");
        }
        Ok(())
    }

    #[tokio::test]
    async fn delete_avail_by_job_deletes_each_listed_entry() -> anyhow::Result<()> {
        let page = list_rule(
            "todo/avail/job1/",
            vec![
                "todo/avail/job1/never.json".to_string(),
                "todo/avail/job1/20260908T000000Z.json".to_string(),
            ],
        );
        let first = mock!(aws_sdk_s3::Client::delete_object)
            .match_requests(|r| r.key() == Some("todo/avail/job1/never.json"))
            .then_output(|| DeleteObjectOutput::builder().build());
        let second = mock!(aws_sdk_s3::Client::delete_object)
            .match_requests(|r| r.key() == Some("todo/avail/job1/20260908T000000Z.json"))
            .then_output(|| DeleteObjectOutput::builder().build());
        let s = store(aws_smithy_mocks::mock_client!(
            aws_sdk_s3,
            RuleMode::Sequential,
            &[&page, &first, &second]
        ));
        s.delete_avail_by_job(&job("job1")).await?;
        Ok(())
    }

    #[tokio::test]
    async fn list_avail_sorts_the_prefix() -> anyhow::Result<()> {
        // The bucket lists in its own order; the store sorts client-side.
        let page = mock!(aws_sdk_s3::Client::list_objects_v2)
            .match_requests(|r| r.prefix() == Some("todo/avail/"))
            .then_output(|| {
                ListObjectsV2Output::builder()
                    .set_contents(Some(vec![
                        Object::builder().key("todo/avail/b/never.json").build(),
                        Object::builder().key("todo/avail/a/never.json").build(),
                        Object::builder().key("todo/avail/c/never.json").build(),
                    ]))
                    .is_truncated(false)
                    .build()
            });
        let s = store(aws_smithy_mocks::mock_client!(
            aws_sdk_s3,
            RuleMode::Sequential,
            &[&page]
        ));
        let names = s.list_avail().await?;
        assert_eq!(
            names,
            vec![
                "a/never.json".to_string(),
                "b/never.json".to_string(),
                "c/never.json".to_string()
            ]
        );
        Ok(())
    }

    // ── claim (rename → get) ───────────────────────────────────────────────────

    #[tokio::test]
    async fn claim_renames_then_returns_body() -> anyhow::Result<()> {
        // The source is the `avail/` key alone — the bucket rides on the request
        // itself, and a source that names it is a key S3 cannot find.
        let rename = mock!(aws_sdk_s3::Client::rename_object)
            .match_requests(|r| r.rename_source() == Some("todo/avail/job1/never.json"))
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
            .claim_job(&job("job1"), ExpiresAt::Never, &cid("ev1_me")?, ts())
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
            .claim_job(&job("job1"), ExpiresAt::Never, &cid("ev1_me")?, ts())
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
        // Renames from the tmp slot to the expires_at-encoded avail name.
        let rename = mock!(aws_sdk_s3::Client::rename_object)
            .match_requests(|r| {
                r.key() == Some("todo/avail/job1/never.json")
                    && r.rename_source() == Some("todo/tmp/job1.json")
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

    /// `err_names_bucket_type`: `None` passes; `Some(named)` is fatal, and `named`
    /// says whether the error blames the bucket type. `absent_source_is_fatal`: a
    /// `NoSuchKey` rename means a mis-addressed source, which fails startup
    /// without blaming the bucket.
    #[rstest]
    #[case::rename_moves_the_object(None, true, false, None)]
    #[case::copy_not_move_is_fatal(None, true, true, Some(true))]
    #[case::phantom_rename_is_fatal(None, false, false, Some(true))]
    #[case::general_purpose_bucket_is_fatal(Some("NotImplemented"), false, false, Some(true))]
    #[case::absent_source_is_fatal(Some("NoSuchKey"), false, false, Some(false))]
    #[case::unrelated_errors_propagate(Some("AccessDenied"), false, false, Some(false))]
    #[tokio::test]
    async fn validate_backend_verdicts(
        #[case] rename_code: Option<&'static str>,
        #[case] moved: bool,
        #[case] src_retained: bool,
        #[case] err_names_bucket_type: Option<bool>,
    ) -> anyhow::Result<()> {
        let stage = mock!(aws_sdk_s3::Client::put_object)
            .match_requests(|r| {
                r.key()
                    .is_some_and(|k| k.starts_with("todo/tmp/.rename-probe-"))
            })
            .then_output(|| PutObjectOutput::builder().build());
        let rename = match rename_code {
            Some(code) => mock!(aws_sdk_s3::Client::rename_object).then_error(move || {
                RenameObjectError::generic(ErrorMetadata::builder().code(code).build())
            }),
            None => mock!(aws_sdk_s3::Client::rename_object)
                .match_requests(|r| {
                    // The source names the staged key alone, and the probe stays
                    // inside `tmp/`, where the stale sweep collects any debris.
                    r.rename_source().zip(r.key()).is_some_and(|(src, dst)| {
                        src.starts_with("todo/tmp/.rename-probe-")
                            && dst.strip_suffix(".moved") == Some(src)
                    })
                })
                .then_output(|| RenameObjectOutput::builder().build()),
        };
        let read_dst = if moved {
            mock!(aws_sdk_s3::Client::get_object).then_output(|| {
                GetObjectOutput::builder()
                    .body(ByteStream::from_static(b""))
                    .build()
            })
        } else {
            mock!(aws_sdk_s3::Client::get_object).then_error(get_no_such_key)
        };
        let read_src = if src_retained {
            mock!(aws_sdk_s3::Client::get_object).then_output(|| {
                GetObjectOutput::builder()
                    .body(ByteStream::from_static(b""))
                    .build()
            })
        } else {
            mock!(aws_sdk_s3::Client::get_object).then_error(get_no_such_key)
        };
        let delete_src = mock!(aws_sdk_s3::Client::delete_object)
            .then_output(|| DeleteObjectOutput::builder().build());
        let delete_dst = mock!(aws_sdk_s3::Client::delete_object)
            .then_output(|| DeleteObjectOutput::builder().build());

        // Sequential rules follow the call order: each read-back runs only after the
        // step before it succeeded; both deletes always run.
        let mut rules: Vec<&Rule> = vec![&stage, &rename];
        if rename_code.is_none() {
            rules.push(&read_dst);
            if moved {
                rules.push(&read_src);
            }
        }
        rules.push(&delete_src);
        rules.push(&delete_dst);

        let s = store(aws_smithy_mocks::mock_client!(
            aws_sdk_s3,
            RuleMode::Sequential,
            &rules
        ));
        match err_names_bucket_type {
            None => s.validate_backend().await?,
            Some(named) => {
                let Err(err) = s.validate_backend().await else {
                    anyhow::bail!("probe must be fatal");
                };
                assert_eq!(
                    format!("{err:#}").contains("Express One Zone"),
                    named,
                    "{err:#}"
                );
            }
        }
        Ok(())
    }

    // ── renew ──────────────────────────────────────────────────────────────────

    #[rstest]
    #[case::renews_when_client_holds_lease(
        true,
        false,
        RenewLeaseResult::Renewed(ExpiresAt::Never)
    )]
    #[case::not_found_when_no_lease_anywhere(false, false, RenewLeaseResult::NotFound)]
    #[case::wrong_client_when_held_elsewhere(false, true, RenewLeaseResult::WrongClient)]
    #[tokio::test]
    async fn renew_lease_outcomes(
        #[case] own_holds: bool,
        #[case] other_holds: bool,
        #[case] expected: RenewLeaseResult,
    ) -> anyhow::Result<()> {
        let own_key = format!(
            "todo/leased/{}",
            leased_key(&job("job1"), ExpiresAt::Never, &cid("ev1_me")?, ts())
        );
        let other_key = format!(
            "todo/leased/{}",
            leased_key(&job("job1"), ExpiresAt::Never, &cid("ev1_other")?, ts())
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
        let result = s.renew_lease(&job("job1"), &cid("ev1_me")?, ts()).await?;
        assert_eq!(result, expected);
        Ok(())
    }

    /// A job-prefixed key that does not parse is cruft: renewal scans past it to
    /// the real lease. The mock matches the destination, so adopting it fails.
    #[tokio::test]
    async fn renew_skips_unparseable_key() -> anyhow::Result<()> {
        let renewed = ts() + chrono::Duration::minutes(5);
        // A two-field leaf is the old key shape and one `.` short of a lease.
        let cruft = "todo/leased/ev1_me/job1.20260101T000000Z.json".to_string();
        let real = format!(
            "todo/leased/{}",
            leased_key(&job("job1"), ExpiresAt::Never, &cid("ev1_me")?, ts())
        );
        let want = format!(
            "todo/leased/{}",
            leased_key(&job("job1"), ExpiresAt::Never, &cid("ev1_me")?, renewed)
        );
        // Cruft sorts first, so a scan that stopped at the first job-prefixed
        // key would never reach the real lease.
        let own_list = list_rule("todo/leased/ev1_me/", vec![cruft, real]);
        let rename = mock!(aws_sdk_s3::Client::rename_object)
            .match_requests(move |r| r.key() == Some(want.as_str()))
            .then_output(|| RenameObjectOutput::builder().build());
        let s = store(aws_smithy_mocks::mock_client!(
            aws_sdk_s3,
            RuleMode::Sequential,
            &[&own_list, &rename]
        ));
        assert_eq!(
            s.renew_lease(&job("job1"), &cid("ev1_me")?, renewed)
                .await?,
            RenewLeaseResult::Renewed(ExpiresAt::Never)
        );
        Ok(())
    }

    /// The mock matches the destination key, so a renewal that wrote any other
    /// `expires_at` fails.
    #[rstest]
    #[case::never(ExpiresAt::Never)]
    #[case::dated(ExpiresAt::At(dt("2026-09-08T00:00:00Z")))]
    #[tokio::test]
    async fn renew_preserves_the_jobs_expires_at(
        #[case] expires_at: ExpiresAt,
    ) -> anyhow::Result<()> {
        let renewed = ts() + chrono::Duration::minutes(5);
        let own_key = format!(
            "todo/leased/{}",
            leased_key(&job("job1"), expires_at, &cid("ev1_me")?, ts())
        );
        let want = format!(
            "todo/leased/{}",
            leased_key(&job("job1"), expires_at, &cid("ev1_me")?, renewed)
        );
        let own_list = list_rule("todo/leased/ev1_me/", vec![own_key]);
        let rename = mock!(aws_sdk_s3::Client::rename_object)
            .match_requests(move |r| r.key() == Some(want.as_str()))
            .then_output(|| RenameObjectOutput::builder().build());
        let s = store(aws_smithy_mocks::mock_client!(
            aws_sdk_s3,
            RuleMode::Sequential,
            &[&own_list, &rename]
        ));
        assert_eq!(
            s.renew_lease(&job("job1"), &cid("ev1_me")?, renewed)
                .await?,
            RenewLeaseResult::Renewed(expires_at)
        );
        Ok(())
    }

    // ── recycle (one rename), verifying the expires_at→avail-key mapping ────────

    /// The recycle target comes from the lease key's `expires_at`, with no body
    /// GET: the mock models only the rename.
    #[rstest]
    #[case::never(ExpiresAt::Never, "todo/avail/job1/never.json")]
    #[case::dated(
        ExpiresAt::At(dt("2026-09-08T00:00:00Z")),
        "todo/avail/job1/20260908T000000Z.json"
    )]
    #[tokio::test]
    async fn recycle_renames_to_the_avail_key_the_lease_names(
        #[case] expires_at: ExpiresAt,
        #[case] want_key: &'static str,
    ) -> anyhow::Result<()> {
        let rename = mock!(aws_sdk_s3::Client::rename_object)
            .match_requests(move |r| r.key() == Some(want_key))
            .then_output(|| RenameObjectOutput::builder().build());
        let s = store(aws_smithy_mocks::mock_client!(
            aws_sdk_s3,
            RuleMode::Sequential,
            &[&rename]
        ));
        assert_eq!(
            s.recycle_lease(&job("job1"), expires_at, &cid("ev1_me")?, ts())
                .await?,
            RecycleResult::Recycled
        );
        Ok(())
    }

    /// A caller holding the lease's `expires_at` reads the recycled body with one
    /// `GetObject`; the mock has no list rule.
    #[rstest]
    #[case::never(ExpiresAt::Never, "todo/avail/job1/never.json")]
    #[case::dated(
        ExpiresAt::At(dt("2026-09-08T00:00:00Z")),
        "todo/avail/job1/20260908T000000Z.json"
    )]
    #[tokio::test]
    async fn get_avail_reads_the_key_recycle_wrote(
        #[case] expires_at: ExpiresAt,
        #[case] want_key: &'static str,
    ) -> anyhow::Result<()> {
        let rename = mock!(aws_sdk_s3::Client::rename_object)
            .match_requests(move |r| r.key() == Some(want_key))
            .then_output(|| RenameObjectOutput::builder().build());
        let body = mock!(aws_sdk_s3::Client::get_object)
            .match_requests(move |r| r.key() == Some(want_key))
            .then_output(|| {
                GetObjectOutput::builder()
                    .body(ByteStream::from_static(
                        br#"{"spec":{"benchmark":"prefill_throughput_256"}}"#,
                    ))
                    .build()
            });
        let s = store(aws_smithy_mocks::mock_client!(
            aws_sdk_s3,
            RuleMode::Sequential,
            &[&rename, &body]
        ));

        assert_eq!(
            s.recycle_lease(&job("job1"), expires_at, &cid("ev1_me")?, ts())
                .await?,
            RecycleResult::Recycled
        );
        let read = s
            .get_avail(&job("job1"), expires_at)
            .await?
            .context("expected the recycled body")?;
        assert_eq!(read["spec"]["benchmark"], "prefill_throughput_256");
        Ok(())
    }

    /// The lease vanished before the rename (a terminal submission or another
    /// recycler): `NoSuchKey` resolves to `Gone`.
    #[tokio::test]
    async fn recycle_gone_when_lease_missing() -> anyhow::Result<()> {
        let rename = mock!(aws_sdk_s3::Client::rename_object).then_error(rename_no_such_key);
        let s = store(aws_smithy_mocks::mock_client!(
            aws_sdk_s3,
            RuleMode::Sequential,
            &[&rename]
        ));
        assert_eq!(
            s.recycle_lease(&job("job1"), ExpiresAt::Never, &cid("ev1_me")?, ts())
                .await?,
            RecycleResult::Gone
        );
        Ok(())
    }

    // ── re-date (one rename within avail/) ─────────────────────────────────────

    /// One rename within `avail/` and no body read.
    #[tokio::test]
    async fn redate_renames_within_avail_to_the_new_deadline() -> anyhow::Result<()> {
        let rename = mock!(aws_sdk_s3::Client::rename_object)
            .match_requests(|r| {
                r.key() == Some("todo/avail/job1/20260908T000000Z.json")
                    && r.rename_source() == Some("todo/avail/job1/20260801T000000Z.json")
            })
            .then_output(|| RenameObjectOutput::builder().build());
        let s = store(aws_smithy_mocks::mock_client!(
            aws_sdk_s3,
            RuleMode::Sequential,
            &[&rename]
        ));
        assert_eq!(
            s.redate_avail(
                &job("job1"),
                ExpiresAt::At(dt("2026-08-01T00:00:00Z")),
                ExpiresAt::At(dt("2026-09-08T00:00:00Z")),
            )
            .await?,
            RedateResult::Redated
        );
        Ok(())
    }

    /// A claim took the entry first: `NoSuchKey` resolves to `Gone`.
    #[tokio::test]
    async fn redate_gone_when_the_entry_was_claimed() -> anyhow::Result<()> {
        let rename = mock!(aws_sdk_s3::Client::rename_object).then_error(rename_no_such_key);
        let s = store(aws_smithy_mocks::mock_client!(
            aws_sdk_s3,
            RuleMode::Sequential,
            &[&rename]
        ));
        assert_eq!(
            s.redate_avail(&job("job1"), ExpiresAt::Never, ExpiresAt::Never)
                .await?,
            RedateResult::Gone
        );
        Ok(())
    }

    /// The queue's client ends a stalled attempt and operation and keeps the
    /// SDK's connect timeout; the vault's part copier adds neither.
    #[tokio::test]
    async fn the_queue_client_bounds_every_request() -> anyhow::Result<()> {
        let storage = pipette_core::config::StorageConfig::S3 {
            bucket: "todo--use1-az4--x-s3".to_string(),
            prefix: String::new(),
            region: Some("us-east-1".to_string()),
            endpoint: Some("http://127.0.0.1:9".to_string()),
            max_concurrent_requests: 1,
            read_timeout_secs: None,
        };
        let timeouts = async |added| {
            let client = build_s3_sdk_client(&storage, added).await?;
            client
                .config()
                .timeout_config()
                .cloned()
                .context("the client holds a timeout config")
        };

        let (attempt, operation) = (Duration::from_secs(2), Duration::from_secs(9));
        let queue = timeouts(Some(queue_timeouts(attempt, operation))).await?;
        assert_eq!(
            (queue.operation_attempt_timeout(), queue.operation_timeout()),
            (Some(attempt), Some(operation))
        );
        let copier = timeouts(None).await?;
        assert_eq!(
            (
                copier.operation_attempt_timeout(),
                copier.operation_timeout()
            ),
            (None, None)
        );
        assert!(copier.connect_timeout().is_some(), "{copier:?}");
        assert_eq!(queue.connect_timeout(), copier.connect_timeout());
        Ok(())
    }

    #[tokio::test]
    async fn avail_key_for_job_returns_a_parseable_filename() -> anyhow::Result<()> {
        let page = list_rule(
            "todo/avail/job1/",
            vec!["todo/avail/job1/20260908T000000Z.json".to_string()],
        );
        let s = store(aws_smithy_mocks::mock_client!(
            aws_sdk_s3,
            RuleMode::Sequential,
            &[&page]
        ));
        let name = s
            .avail_key_for_job(&job("job1"))
            .await?
            .context("expected this job's avail key")?;
        assert_eq!(name, "job1/20260908T000000Z.json");
        assert_eq!(
            pipette_core::todo_filename::parse_avail_filename(&name)?,
            (job("job1"), ExpiresAt::At(dt("2026-09-08T00:00:00Z")))
        );
        Ok(())
    }

    /// Each addresses `todo/avail/{job_id}/`, so jobs sorting ahead stay invisible.
    #[rstest]
    #[case::body(JobScoped::Body)]
    #[case::key(JobScoped::Key)]
    #[case::delete(JobScoped::Delete)]
    #[tokio::test]
    async fn a_job_scoped_read_sees_only_its_own_job(
        #[case] method: JobScoped,
    ) -> anyhow::Result<()> {
        const BUCKET: &[&str] = &[
            "todo/avail/job0/never.json",
            "todo/avail/job1/20260908T000000Z.json",
            "todo/avail/job2/never.json",
        ];
        let list = bucket_list_rule(BUCKET);
        let get = mock!(aws_sdk_s3::Client::get_object)
            .match_requests(|r| r.key() == Some("todo/avail/job1/20260908T000000Z.json"))
            .then_output(|| {
                GetObjectOutput::builder()
                    .body(ByteStream::from_static(br#"{"job_id":"job1"}"#))
                    .build()
            });
        let delete = mock!(aws_sdk_s3::Client::delete_object)
            .match_requests(|r| r.key() == Some("todo/avail/job1/20260908T000000Z.json"))
            .then_output(|| DeleteObjectOutput::builder().build());
        let s = store(aws_smithy_mocks::mock_client!(
            aws_sdk_s3,
            RuleMode::MatchAny,
            &[&list, &get, &delete]
        ));

        match method {
            JobScoped::Body => {
                let body = s
                    .get_avail_by_job(&job("job1"))
                    .await?
                    .context("expected this job's body")?;
                assert_eq!(body["job_id"], "job1");
            }
            JobScoped::Key => {
                assert_eq!(
                    s.avail_key_for_job(&job("job1")).await?,
                    Some("job1/20260908T000000Z.json".to_string())
                );
            }
            // A delete of any other job's key finds no rule and fails the test.
            JobScoped::Delete => s.delete_avail_by_job(&job("job1")).await?,
        }
        Ok(())
    }

    /// The mocks answer a prefix without `/` that a directory bucket rejects, so
    /// the store refuses it itself.
    #[tokio::test]
    async fn a_list_prefix_that_does_not_end_in_a_delimiter_is_refused() -> anyhow::Result<()> {
        let list = bucket_list_rule(&["todo/avail/job1/never.json"]);
        let s = store(aws_smithy_mocks::mock_client!(
            aws_sdk_s3,
            RuleMode::MatchAny,
            &[&list]
        ));
        let err = format!(
            "{:#}",
            s.list_objects("todo/avail/job1.")
                .await
                .err()
                .context("expected the listing to be refused")?
        );
        assert!(err.contains("does not end in '/'"), "{err}");
        // The `/`-terminated prefix the store builds is accepted.
        assert_eq!(s.list_keys("todo/avail/job1/").await?, vec!["never.json"]);
        Ok(())
    }

    // ── marker listing / parsing ───────────────────────────────────────────────

    /// A point read of `denied/{job_id}.{client_id}`: the mock matches the exact
    /// key, so reaching for a listing fails the test.
    #[rstest]
    #[case::denied(true)]
    #[case::not_denied(false)]
    #[tokio::test]
    async fn is_denied_reads_the_pair_key(#[case] marker_present: bool) -> anyhow::Result<()> {
        let rule = mock!(aws_sdk_s3::Client::get_object)
            .match_requests(|r| r.key() == Some("todo/denied/job1.ev1_abc"));
        let rule = if marker_present {
            // Presence is the whole answer; the empty body is not read.
            rule.then_output(|| {
                GetObjectOutput::builder()
                    .body(ByteStream::from_static(b""))
                    .build()
            })
        } else {
            rule.then_error(get_no_such_key)
        };
        let s = store(aws_smithy_mocks::mock_client!(
            aws_sdk_s3,
            RuleMode::Sequential,
            &[&rule]
        ));
        assert_eq!(
            s.is_denied(&job("job1"), &cid("ev1_abc")?).await?,
            marker_present
        );
        Ok(())
    }

    /// A refused read must not read back as "this client may take this job".
    #[tokio::test]
    async fn is_denied_propagates_a_non_notfound_error() -> anyhow::Result<()> {
        const DENIED: &str = concat!(
            r#"<?xml version="1.0" encoding="UTF-8"?>"#,
            "<Error><Code>AccessDenied</Code><Message>",
            "User is not authorized to perform: s3:GetObject",
            "</Message></Error>",
        );
        let denied = mock!(aws_sdk_s3::Client::get_object)
            .sequence()
            .http_status(403, Some(DENIED.to_string()))
            .build();
        let s = store(aws_smithy_mocks::mock_client!(
            aws_sdk_s3,
            RuleMode::Sequential,
            &[&denied]
        ));

        let err = s
            .is_denied(&job("job1"), &cid("ev1_abc")?)
            .await
            .err()
            .context("a refused read must not report the pair as undenied")?;
        let text = format!("{err:#}");
        assert!(text.contains("todo/denied/job1.ev1_abc"), "{text}");
        assert!(text.contains("code=AccessDenied"), "{text}");
        Ok(())
    }

    /// `get_denied` reads the record from the pair key. An absent key is
    /// `None`, and an empty body reads as the default record.
    #[rstest]
    #[case::absent(None, None)]
    #[case::empty(Some(b"".as_slice()), Some(DenialRecord::default()))]
    #[case::record(
        Some(br#"{"failure_reason":"runtime OOM","client_version":"1.4.2"}"#.as_slice()),
        Some(DenialRecord {
            failure_reason: Some("runtime OOM".to_owned()),
            client_version: Some("1.4.2".to_owned()),
            ..DenialRecord::default()
        })
    )]
    #[tokio::test]
    async fn get_denied_reads_the_marker_record(
        #[case] body: Option<&'static [u8]>,
        #[case] expected: Option<DenialRecord>,
    ) -> anyhow::Result<()> {
        let rule = mock!(aws_sdk_s3::Client::get_object)
            .match_requests(|r| r.key() == Some("todo/denied/job1.ev1_abc"));
        let rule = match body {
            Some(bytes) => rule.then_output(move || {
                GetObjectOutput::builder()
                    .body(ByteStream::from_static(bytes))
                    .build()
            }),
            None => rule.then_error(get_no_such_key),
        };
        let s = store(aws_smithy_mocks::mock_client!(
            aws_sdk_s3,
            RuleMode::Sequential,
            &[&rule]
        ));
        assert_eq!(
            s.get_denied(&job("job1"), &cid("ev1_abc")?).await?,
            expected
        );
        Ok(())
    }

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
        assert_eq!(denied, vec![(job("job1"), cid("ev1_abc")?)]);
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

    /// The listing maps each marker to its `LastModified`. It skips a name that
    /// is not a client ID, and an object that the listing gives no time for.
    #[tokio::test]
    async fn list_liveness_reads_times_from_the_listing() -> anyhow::Result<()> {
        let list = mock!(aws_sdk_s3::Client::list_objects_v2)
            .match_requests(|r| r.prefix() == Some("todo/liveness/"))
            .then_output(|| {
                ListObjectsV2Output::builder()
                    .set_contents(Some(vec![
                        Object::builder()
                            .key("todo/liveness/ev1_a")
                            .last_modified(AwsDateTime::from_secs(1_800_000_000))
                            .build(),
                        Object::builder().key("todo/liveness/ev1_untimed").build(),
                        Object::builder()
                            .key("todo/liveness/not a client")
                            .last_modified(AwsDateTime::from_secs(1_800_000_000))
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
        let at = DateTime::from_timestamp(1_800_000_000, 0).context("timestamp")?;
        assert_eq!(
            s.list_liveness().await?,
            vec![(ClientId::try_new("ev1_a")?, at)]
        );
        Ok(())
    }

    /// A single read returns the marker's `LastModified`, and a missing marker
    /// reads as `None`.
    #[tokio::test]
    async fn read_liveness_returns_last_modified() -> anyhow::Result<()> {
        let present = mock!(aws_sdk_s3::Client::get_object)
            .match_requests(|r| r.key() == Some("todo/liveness/ev1_a"))
            .then_output(|| {
                GetObjectOutput::builder()
                    .last_modified(AwsDateTime::from_secs(1_800_000_000))
                    .build()
            });
        let missing = mock!(aws_sdk_s3::Client::get_object).then_error(get_no_such_key);
        let s = store(aws_smithy_mocks::mock_client!(
            aws_sdk_s3,
            RuleMode::Sequential,
            &[&present, &missing]
        ));
        let client = ClientId::try_new("ev1_a")?;
        let at = DateTime::from_timestamp(1_800_000_000, 0).context("timestamp")?;
        assert_eq!(s.read_liveness(&client).await?, Some(at));
        assert_eq!(s.read_liveness(&client).await?, None);
        Ok(())
    }

    /// One persisted `queue-maintenance` key set, so one test covers all three.
    #[derive(Debug, Clone, Copy)]
    enum KeySet {
        GcCandidates,
        EligibleIndexed,
        EligibleWorkers,
    }

    impl KeySet {
        async fn reads_empty(self, store: &S3TodoStore) -> anyhow::Result<bool> {
            Ok(match self {
                Self::GcCandidates => store.read_gc_candidates().await?.is_empty(),
                Self::EligibleIndexed => store.read_eligible_indexed().await?.is_empty(),
                Self::EligibleWorkers => store.read_eligible_workers().await?.is_empty(),
            })
        }
    }

    /// A missing key set reads as empty, not an error; the next run re-indexes,
    /// rebuilds, or re-stages from scratch.
    #[rstest]
    #[case::gc_candidates(KeySet::GcCandidates)]
    #[case::eligible_indexed(KeySet::EligibleIndexed)]
    #[case::eligible_workers(KeySet::EligibleWorkers)]
    #[tokio::test]
    async fn read_key_set_missing_key_is_empty(#[case] set: KeySet) -> anyhow::Result<()> {
        let missing = mock!(aws_sdk_s3::Client::get_object).then_error(get_no_such_key);
        let s = store(aws_smithy_mocks::mock_client!(
            aws_sdk_s3,
            RuleMode::Sequential,
            &[&missing]
        ));
        assert!(set.reads_empty(&s).await?);
        Ok(())
    }

    /// The backlog snapshot reads from `todo/.backlog`. A missing key and a
    /// corrupt body both read as `None`.
    #[tokio::test]
    async fn read_backlog_reads_the_snapshot() -> anyhow::Result<()> {
        let stored = mock!(aws_sdk_s3::Client::get_object)
            .match_requests(|r| r.key() == Some("todo/.backlog"))
            .then_output(|| {
                GetObjectOutput::builder()
                    .body(ByteStream::from_static(
                        br#"{"counted_at":"2027-01-15T08:00:00Z","jobs":{"ev1_a":2}}"#,
                    ))
                    .build()
            });
        let missing = mock!(aws_sdk_s3::Client::get_object).then_error(get_no_such_key);
        let corrupt = mock!(aws_sdk_s3::Client::get_object).then_output(|| {
            GetObjectOutput::builder()
                .body(ByteStream::from_static(b"not json"))
                .build()
        });
        let s = store(aws_smithy_mocks::mock_client!(
            aws_sdk_s3,
            RuleMode::Sequential,
            &[&stored, &missing, &corrupt]
        ));
        let expected = BacklogSnapshot {
            counted_at: DateTime::from_timestamp(1_800_000_000, 0).context("timestamp")?,
            jobs: [(cid("ev1_a")?, 2)].into_iter().collect(),
        };
        assert_eq!(s.read_backlog().await?, Some(expected));
        assert_eq!(s.read_backlog().await?, None);
        assert_eq!(s.read_backlog().await?, None);
        Ok(())
    }

    /// The campaign backlog snapshot reads from `todo/.backlog-campaigns`. A
    /// missing key and a corrupt body both read as `None`.
    #[tokio::test]
    async fn read_campaign_backlog_reads_the_snapshot() -> anyhow::Result<()> {
        const CAMPAIGN: &str = "campaign_00000000-0000-0000-0000-000000000001";
        let stored = mock!(aws_sdk_s3::Client::get_object)
            .match_requests(|r| r.key() == Some("todo/.backlog-campaigns"))
            .then_output(|| {
                GetObjectOutput::builder()
                    .body(ByteStream::from(format!(
                        r#"{{"counted_at":"2027-01-15T08:00:00Z","devices":{{"ev1_a":{{"by_campaign":{{"{CAMPAIGN}":2}},"outside_campaigns":1}}}}}}"#
                    ).into_bytes()))
                    .build()
            });
        let missing = mock!(aws_sdk_s3::Client::get_object).then_error(get_no_such_key);
        let corrupt = mock!(aws_sdk_s3::Client::get_object).then_output(|| {
            GetObjectOutput::builder()
                .body(ByteStream::from_static(b"not json"))
                .build()
        });
        let s = store(aws_smithy_mocks::mock_client!(
            aws_sdk_s3,
            RuleMode::Sequential,
            &[&stored, &missing, &corrupt]
        ));
        let expected = CampaignBacklogSnapshot {
            counted_at: DateTime::from_timestamp(1_800_000_000, 0).context("timestamp")?,
            devices: [(
                cid("ev1_a")?,
                pipette_store::CampaignBacklog {
                    by_campaign: [(pipette_plan_types::CampaignId::try_new(CAMPAIGN)?, 2)]
                        .into_iter()
                        .collect(),
                    outside_campaigns: 1,
                },
            )]
            .into_iter()
            .collect(),
        };
        assert_eq!(s.read_campaign_backlog().await?, Some(expected));
        assert_eq!(s.read_campaign_backlog().await?, None);
        assert_eq!(s.read_campaign_backlog().await?, None);
        Ok(())
    }

    /// A canonical hyphenated v7 uuid, as `pending_reindex_filename` mints.
    const NONCE: &str = "01890a5d-ac96-774b-bcce-b302099a8057";

    /// The `_`-bearing `ev1_abc` exercises the split on `.`; the foreign name
    /// must be skipped.
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
            vec![(cid("ev1_abc")?, format!("ev1_abc.{NONCE}"))]
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
        assert_eq!(s.has_pending_reindex(&cid(client)?).await?, expected);
        Ok(())
    }

    /// One stray object must not wedge the maintenance run.
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

    fn lost_listing(prefix: &'static str, keys: &'static [&'static str]) -> Rule {
        mock!(aws_sdk_s3::Client::list_objects_v2)
            .match_requests(move |r| r.prefix() == Some(prefix))
            .then_output(move || {
                ListObjectsV2Output::builder()
                    .set_contents(Some(
                        keys.iter()
                            .map(|k| Object::builder().key(*k).build())
                            .collect(),
                    ))
                    .is_truncated(false)
                    .build()
            })
    }

    /// A job's losses list from its own `/`-ended prefix, oldest first, with
    /// foreign names skipped.
    #[tokio::test]
    async fn list_lost_leases_reads_the_job_prefix() -> anyhow::Result<()> {
        let list = lost_listing(
            "todo/lost/job1/",
            &[
                "todo/lost/job1/ev1_b.20261003T050000Z",
                "todo/lost/job1/.DS_Store",
                "todo/lost/job1/ev1_a.20261003T045000Z",
            ],
        );
        let s = store(aws_smithy_mocks::mock_client!(
            aws_sdk_s3,
            RuleMode::Sequential,
            &[&list]
        ));
        let losses: Vec<_> = s
            .list_lost_leases(&job("job1"))
            .await?
            .into_iter()
            .map(|(client, _)| client.to_string())
            .collect();
        assert_eq!(losses, ["ev1_a", "ev1_b"]);
        Ok(())
    }

    /// The GC listing names each job once, however many losses it holds, in
    /// whatever order the bucket lists them.
    #[tokio::test]
    async fn list_lost_lease_jobs_names_each_job_once() -> anyhow::Result<()> {
        let list = lost_listing(
            "todo/lost/",
            &[
                "todo/lost/job2/ev1_a.20261003T045000Z",
                "todo/lost/job1/ev1_a.20261003T045000Z",
                "todo/lost/.DS_Store",
                "todo/lost/job1/ev1_b.20261003T050000Z",
            ],
        );
        let s = store(aws_smithy_mocks::mock_client!(
            aws_sdk_s3,
            RuleMode::Sequential,
            &[&list]
        ));
        assert_eq!(
            s.list_lost_lease_jobs().await?,
            vec![job("job1"), job("job2")]
        );
        Ok(())
    }
}

/// `PlanStore` uses `object_store`, not the `aws-sdk` rename path, so it runs
/// on the in-memory backend.
#[cfg(test)]
mod plan_store_tests {
    use rstest::rstest;

    use pipette_core::plan::sample_manifest;
    use pipette_plan_types::PlanStatus;

    use super::*;

    fn plan_store(prefix: &str) -> S3PlanStore {
        S3PlanStore {
            store: Arc::new(object_store::memory::InMemory::new()),
            prefix: prefix.to_string(),
        }
    }

    /// Held to the shared contract, so `If-None-Match` / `If-Match` answer as the
    /// local_fs digest check does.
    #[tokio::test]
    async fn content_key_index_meets_the_shared_contract() -> anyhow::Result<()> {
        pipette_store_testkit::assert_content_key_roundtrip(&plan_store("root")).await
    }

    #[tokio::test]
    async fn job_plan_markers_meet_the_shared_contract() -> anyhow::Result<()> {
        pipette_store_testkit::assert_job_plan_marker_roundtrip(&plan_store("root")).await
    }

    /// Under a prefix and at the bucket root: `plan-sources/` and
    /// `plan-sources-pending/` share a stem, and neither listing may see the
    /// other's keys.
    #[rstest]
    #[case::prefixed("root")]
    #[case::bucket_root("")]
    #[tokio::test]
    async fn plan_sources_meet_the_shared_contract(#[case] prefix: &str) -> anyhow::Result<()> {
        let store = plan_store(prefix);
        pipette_store_testkit::assert_plan_source_roundtrip(&store).await?;
        pipette_store_testkit::assert_pending_source_marker_roundtrip(&store).await
    }

    /// Under a prefix and at the bucket root: `job-plans/` and
    /// `job-plan-links/` share a stem, and neither listing may see the other's
    /// keys.
    #[rstest]
    #[case::prefixed("root")]
    #[case::bucket_root("")]
    #[tokio::test]
    async fn job_sources_and_links_meet_the_shared_contract(
        #[case] prefix: &str,
    ) -> anyhow::Result<()> {
        let store = plan_store(prefix);
        pipette_store_testkit::assert_job_source_roundtrip(&store).await?;
        pipette_store_testkit::assert_job_plan_link_roundtrip(&store).await
    }

    #[tokio::test]
    async fn job_plan_links_skip_a_foreign_key() -> anyhow::Result<()> {
        let store = plan_store("root");
        let job_id = JobId::from_uuid(uuid::Uuid::from_u128(1));
        let plan_id = PlanId::from_uuid(uuid::Uuid::from_u128(2));
        store.put_job_plan_link(&job_id, &plan_id).await?;
        store
            .store
            .put(
                &obj_path(
                    &store.prefix,
                    &format!("{JOB_PLAN_LINKS_PREFIX}{job_id}/stray.tmp"),
                ),
                Vec::new().into(),
            )
            .await?;

        assert_eq!(store.list_job_plan_links(&job_id).await?, vec![plan_id]);
        Ok(())
    }

    #[tokio::test]
    async fn plan_filter_meets_the_shared_contract() -> anyhow::Result<()> {
        pipette_store_testkit::assert_plan_filter_contract(&plan_store("root")).await
    }

    #[tokio::test]
    async fn plan_reach_meets_the_shared_contract() -> anyhow::Result<()> {
        pipette_store_testkit::assert_plan_reach_contract(&plan_store("root")).await
    }

    /// The plan listing is held to the shared pagination contract, so
    /// `list_with_offset` answers the same questions the filesystem sort does.
    #[tokio::test]
    async fn plan_listing_meets_the_shared_pagination_contract() -> anyhow::Result<()> {
        let store = plan_store("root");
        pipette_store_testkit::assert_plan_listing_pagination(&store, |leaf, bytes| {
            let path = obj_path(&store.prefix, &format!("plans/{leaf}"));
            let handle = store.store.clone();
            async move { put_bytes(&handle, &path, bytes).await }
        })
        .await
    }

    #[tokio::test]
    async fn get_plans_meets_the_shared_contract() -> anyhow::Result<()> {
        let store = plan_store("root");
        pipette_store_testkit::assert_get_plans_contract(&store, |leaf, bytes| {
            let path = obj_path(&store.prefix, &format!("plans/{leaf}"));
            let handle = store.store.clone();
            async move { put_bytes(&handle, &path, bytes).await }
        })
        .await
    }

    #[tokio::test]
    async fn roundtrip_list_filter_and_delete() -> anyhow::Result<()> {
        let store = plan_store("root");
        let active_id = PlanId::from_uuid(uuid::Uuid::from_u128(1));
        let done_id = PlanId::from_uuid(uuid::Uuid::from_u128(2));
        // With a progress_snapshot and without one.
        let active = sample_manifest(active_id.clone(), PlanStatus::Active, true)?;
        let done = sample_manifest(done_id.clone(), PlanStatus::Complete, false)?;
        store.put_plan(&active).await?;
        store.put_plan(&done).await?;

        assert_eq!(store.get_plan(&active_id).await?.as_ref(), Some(&active));
        assert_eq!(store.get_plan(&done_id).await?.as_ref(), Some(&done));

        let all = pipette_store::drain_plans(&store, None).await?.items;
        assert_eq!(all, vec![active.clone(), done.clone()]);
        assert_eq!(
            pipette_store::drain_plans(&store, Some(PlanStatus::Complete))
                .await?
                .items,
            vec![done]
        );
        assert!(
            pipette_store::drain_plans(&store, Some(PlanStatus::Cancelled))
                .await?
                .items
                .is_empty()
        );
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

    /// Cancel markers round-trip, are idempotent, and stay out of `list_plans`.
    #[rstest]
    // With and without a key prefix: the sibling keyspace stays disjoint.
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
            .put_plan(&sample_manifest(a.clone(), PlanStatus::Active, false)?)
            .await?;
        assert_eq!(
            pipette_store::drain_plans(&store, None).await?.items.len(),
            1
        );
        assert_eq!(store.list_cancel_markers().await?.len(), 1);

        store.delete_cancel_marker(&a).await?;
        assert!(!store.has_cancel_marker(&a).await?);
        store.delete_cancel_marker(&a).await?; // idempotent
        assert!(store.get_plan(&a).await?.is_some(), "manifest survives");
        assert!(store.list_cancel_markers().await?.is_empty());
        Ok(())
    }

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

    #[tokio::test]
    async fn pending_source_marker_listing_skips_unparseable() -> anyhow::Result<()> {
        let store = plan_store("root");
        let good = PlanId::from_uuid(uuid::Uuid::from_u128(1));
        store.write_pending_source_marker(&good).await?;
        put_bytes(
            &store.store,
            &obj_path(&store.prefix, "plan-sources-pending/not.a.plan.id"),
            Vec::new(),
        )
        .await?;

        let listed: Vec<PlanId> = store
            .list_pending_source_markers()
            .await?
            .into_iter()
            .map(|m| m.plan_id)
            .collect();
        assert_eq!(listed, vec![good]);
        Ok(())
    }
}
