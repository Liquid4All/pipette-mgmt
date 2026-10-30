use std::collections::{BTreeSet, HashSet};
use std::num::{NonZeroU32, NonZeroU64, NonZeroUsize};
use std::path::PathBuf;

use parquet::basic::ZstdLevel;
use serde::{Deserialize, Deserializer};
use strum::IntoEnumIterator as _;

use pipette_plan_types::{CapabilityFlag, LlamaCppFlavor, OvmsFlavor, RuntimeType};

use crate::parquet_utils::WriterOpts;

/// One object-store section (`[storage]`, `[auth_storage]`, `[todo_storage]`),
/// chosen by `backend`. S3 credentials come from the environment (`AWS_*`, or
/// an instance or task role), never from this file.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "backend", rename_all = "snake_case")]
pub enum StorageConfig {
    LocalFs {
        /// Root directory. Defaults to `./data`.
        #[serde(default = "default_data_dir")]
        data_dir: PathBuf,
    },
    S3 {
        bucket: String,
        /// Key prefix for every object, such as `v1/`. Defaults to none.
        #[serde(default)]
        prefix: String,
        /// AWS region. Also reads `AWS_REGION` / `AWS_DEFAULT_REGION` env vars.
        region: Option<String>,
        /// Custom S3-compatible endpoint (e.g. MinIO, Cloudflare R2). Setting
        /// it also admits a plain `http://` endpoint.
        endpoint: Option<String>,
        /// Cap on the requests in flight in one warehouse walk (the `fix-*`
        /// rewrites, `requeue-eval`, `models unresolved`), so only `[storage]`
        /// reads it. Defaults to 32.
        #[serde(default = "default_max_concurrent_requests")]
        max_concurrent_requests: usize,
        /// Fail an `object_store` request after this many seconds with no
        /// response bytes. Only [`Config::for_queue_maintenance`] sets it, from
        /// `queue_maintenance_read_timeout_secs`, so the config file cannot.
        #[serde(skip)]
        read_timeout_secs: Option<NonZeroU64>,
    },
}

fn default_max_concurrent_requests() -> usize {
    32
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self::local_fs(default_data_dir())
    }
}

impl StorageConfig {
    pub fn local_fs(data_dir: PathBuf) -> Self {
        Self::LocalFs { data_dir }
    }

    pub fn data_dir(&self) -> Option<&PathBuf> {
        match self {
            Self::LocalFs { data_dir } => Some(data_dir),
            Self::S3 { .. } => None,
        }
    }
}

fn default_data_dir() -> PathBuf {
    PathBuf::from("./data")
}

/// The `pipette-mgmt` config file. Each field's doc comment is the reference
/// for its key; `examples/config.toml` is the annotated example.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Descriptor catalog offer rules and importer precedence. Defaults to no
    /// rules, which offers no catalog members to any device.
    #[serde(default)]
    pub descriptor_catalog: DescriptorCatalogConfig,
    /// Required. Base URL of the edge-evals scoring service: `score-eval`
    /// scores through it, `serve` fetches an eval benchmark's samples from it,
    /// and `dataset-cache` probes and builds a vlm_eval's corpus from it.
    pub evals_server_url: String,
    /// Address `serve` listens on. Plain HTTP on every interface by default,
    /// so put a TLS proxy in front (`docs/operations.md`). Defaults to
    /// `0.0.0.0:3000`.
    #[serde(default = "default_listen_addr")]
    pub listen_addr: String,
    /// Seconds `serve` waits for a request's headers, an idle keep-alive
    /// connection's next request included, and for each chunk of its body. Set
    /// it above the idle timeout of a load balancer in front, so the balancer
    /// closes an idle connection first. Defaults to 75.
    #[serde(default = "default_read_timeout_secs")]
    pub read_timeout_secs: NonZeroU64,
    /// Connections `serve` holds open at once; a further one waits in the
    /// listen backlog. Keep it below the process's open-file limit. Defaults
    /// to 512.
    #[serde(default = "default_max_connections")]
    pub max_connections: NonZeroUsize,
    /// How many seconds `serve` caches the benchmark catalog before re-reading
    /// it from `[storage]`. Defaults to 180.
    #[serde(default = "default_catalog_ttl_secs")]
    pub catalog_ttl_secs: u64,
    /// How many days back a device's job read looks up metrics, across `day=`
    /// and overlapping legacy `month=` partitions. A hard cap: an older job
    /// reads `metrics: null`, its rows still in the warehouse. Defaults to 14.
    #[serde(default = "default_warehouse_read_days")]
    pub warehouse_read_days: u32,
    /// Maximum rows per Parquet part file in a warehouse partition. Writes
    /// append to the tail part and roll to a new one at this size, so it also
    /// bounds the rows read+rewritten per write. Defaults to 1000.
    #[serde(default = "default_warehouse_max_rows_per_part")]
    pub warehouse_max_rows_per_part: usize,
    /// Outbound HTTP request timeout in seconds for the scoring service calls,
    /// not inbound requests. Defaults to 600, sized for a slow scorer rather
    /// than for interactive latency.
    #[serde(default = "default_http_timeout_secs")]
    pub http_timeout_secs: u64,
    /// Give up opening a connection after this many seconds: on `serve`'s
    /// client, which calls pipette-duckdb and the evals server, on the scoring
    /// commands' clients, and on `plans results`' pipette-duckdb client. A name
    /// that resolves to several addresses splits it evenly between them, so an
    /// address that never answers, such as a stopped task still in DNS, hands
    /// over to the next one within it; a refused connection moves on at once.
    /// Must be non-zero. Defaults to 3.
    #[serde(default = "default_http_connect_timeout_secs")]
    pub http_connect_timeout_secs: NonZeroU64,
    /// zstd compression level for every warehouse Parquet write, the `fix-*`
    /// rewrites included. Valid range 1..=22, checked at load. Defaults to 3.
    #[serde(
        default = "default_parquet_zstd_level",
        deserialize_with = "deserialize_parquet_zstd_level"
    )]
    pub parquet_zstd_level: ZstdLevel,
    /// Submissions (`process-submissions`) or queued evals (`score-eval`)
    /// listed per chunk. Each run still drains its whole backlog, chunk by
    /// chunk; the size bounds one chunk's listing. Defaults to 50.
    #[serde(default = "default_score_chunk_size")]
    pub score_chunk_size: NonZeroUsize,
    /// Lease duration, in seconds, of the storage mutate lock and the
    /// `score-eval` lock on `s3` (`docs/cli.md`); a crashed command's lease is
    /// taken over after it. Set it above the longest `process-submissions`,
    /// `score-eval` or `fix-*` run, which could otherwise lose the lock
    /// mid-write. Unused on `local_fs`, where `flock(2)` dies with its process.
    /// Must be non-zero. Defaults to 1800.
    #[serde(default = "default_mutate_lock_ttl_secs")]
    pub mutate_lock_ttl_secs: NonZeroU64,
    /// Oldest `dataset-cache probe` verdict `GET /datasets/{benchmark_id}`
    /// still serves on. An older one refuses with `503`, so a stalled probe
    /// stops serving rather than serving on a stale `ok`. Defaults to 180
    /// (three one-minute probe runs).
    #[serde(default = "default_dataset_probe_max_age_secs")]
    pub dataset_probe_max_age_secs: NonZeroU64,
    /// Deadline for one artifact's upstream probe, all `git` steps together.
    /// Past it the upstream is recorded `unavailable`. Defaults to 30.
    #[serde(default = "default_dataset_probe_timeout_secs")]
    pub dataset_probe_timeout_secs: NonZeroU64,
    /// Deadline for one dataset build: fetch, verify and pack together.
    /// Defaults to 900.
    #[serde(default = "default_dataset_build_timeout_secs")]
    pub dataset_build_timeout_secs: NonZeroU64,
    /// [`Self::dataset_build_timeout_secs`] for an `asr-eval-corpus/1`, whose
    /// build streams gigabytes of audio from the evals server (SPGISpeech's
    /// largest profile about 15 GB of base64). The same deadline bounds
    /// `dataset-cache pins` for such an entry. Defaults to 7200.
    #[serde(default = "default_dataset_asr_build_timeout_secs")]
    pub dataset_asr_build_timeout_secs: NonZeroU64,
    /// The object store for the catalog, submissions, the warehouse, blobs,
    /// the dataset cache and, without `[todo_storage]`, the queue. Without
    /// `[database]` it also holds the records, the relations that decide access
    /// among them. Defaults to `local_fs` at `./data`.
    #[serde(default)]
    pub storage: StorageConfig,
    /// Required. The object store for identities, keys and their claims, kept
    /// apart so a private bucket can hold them. With `[database]`, those
    /// records live there instead.
    pub auth_storage: StorageConfig,
    /// Unverified-submission archive settings. Off by default. When disabled, a
    /// result submission from an unapproved client is refused with `403`.
    #[serde(default)]
    pub unverified_submissions: UnverifiedSubmissionsConfig,
    /// Auto-approve rules matched against a client's `contact_email` at
    /// registration. Off by default. NOT a security control — email is
    /// self-reported and unverified.
    #[serde(default)]
    pub auto_approve: AutoApproveConfig,
    /// When `true`, a device's registration requires a valid pre-auth key —
    /// keyless registrations are rejected with `403`. Off by default, so
    /// registration stays open (governed only by `auto_approve`).
    #[serde(default)]
    pub require_client_preauth_key: bool,
    /// When `true`, a user key's registration requires a valid user pre-auth
    /// key; a keyless request gets `403`. Off by default, so a machine can ask
    /// for an account and wait for an operator. Separate from the device switch:
    /// a deployment may hand-mint operators yet let devices self-register.
    #[serde(default)]
    pub require_user_preauth_key: bool,
    /// When `true`, a signature over the bare `X-Timestamp` value is accepted
    /// besides one over the `v1` payload, logged at `warn` with the client id;
    /// once those lines stop appearing, set it `false`. A migration aid, not a
    /// mode: such a signature replays against any endpoint within the freshness
    /// window. A client's first `v1` signature withdraws it for good. Defaults
    /// to `true`.
    #[serde(default = "default_accept_legacy_signatures")]
    pub accept_legacy_signatures: bool,
    /// When `true`, a user may sign the `v2` `X-Signature` payload besides RFC
    /// 9421. `v2` covers no authority, so it replays against another deployment
    /// that holds the same key; once the `user signs v2` lines stop, set it
    /// `false`, and every user request is held to `authorities`. Defaults to
    /// `true`.
    #[serde(default = "default_accept_v2_signatures")]
    pub accept_v2_signatures: bool,
    /// The `Host` values (`host` or `host:port`) an RFC 9421 signature's
    /// `@authority`, or a device `v3` signature's authority, must match,
    /// case-insensitively, or `401`, so a request signed for another
    /// deployment cannot be replayed here. Empty, the default, refuses every
    /// RFC 9421 and `v3` signature.
    #[serde(default)]
    pub authorities: Vec<String>,
    /// How many proxies in front of `serve` append the address they received
    /// from to `X-Forwarded-For`. The client address the keyless registration
    /// limit counts is the entry this many from the right; `0` ignores the
    /// header and takes the peer. A header with fewer entries names no client,
    /// and only the global ceiling applies. Defaults to `1`, one load balancer.
    #[serde(default = "default_trusted_proxy_hops")]
    pub trusted_proxy_hops: usize,
    /// When `true`, a process that depends on an `s3` backend honoring
    /// conditional `PUT` probes it at startup and refuses to start if it does
    /// not: the mutate lock and the exactly-once pre-auth key spend rely on it,
    /// and an endpoint that ignores the precondition fails silently. Set `false`
    /// only where the backend is known to honor it (AWS S3 does), to skip the
    /// probe's writes and, on a versioned bucket, the noncurrent versions it
    /// leaves under `probe/`; the `todo/` probe is unaffected. Defaults to `true`.
    #[serde(default = "default_probe_backend_capabilities")]
    pub probe_backend_capabilities: bool,
    /// Storage for the `todo/` job queue. Defaults to `[storage]`, for
    /// `local_fs` development only: production needs S3 Express One Zone, the
    /// only bucket with the atomic `RenameObject` the queue relies on.
    #[serde(default)]
    pub todo_storage: Option<StorageConfig>,
    /// Lease duration, in seconds, granted on a successful claim and
    /// extended by each heartbeat. Clients see it as the claim's `time_window`
    /// and heartbeat at half of it. Must be non-zero. Defaults to 300
    /// (5 minutes).
    #[serde(default = "default_plan_lease_duration_secs")]
    pub plan_lease_duration_secs: NonZeroU64,
    /// Leases a job may lose before `queue-maintenance` retires it as
    /// `lease_lost`. A lease is lost when it lapses with no result, or when its
    /// holder claims again while still holding it. `0` turns the bound off.
    /// Defaults to 3.
    #[serde(default = "default_plan_lost_lease_limit")]
    pub plan_lost_lease_limit: usize,
    /// Fail a `queue-maintenance` request to `[storage]` or `[auth_storage]`
    /// after this many seconds with no response bytes. The timer starts with the
    /// request, so it covers uploading the body too, and restarts at each
    /// response body byte. Without it a request waits the client's 30 s
    /// timeout. Other commands do not use it, nor do the `aws-sdk-s3` clients
    /// of the `todo/` queue and of vault part copies.
    #[serde(default)]
    pub queue_maintenance_read_timeout_secs: Option<NonZeroU64>,
    /// Fail one attempt of a request to the `todo/` queue after this many
    /// seconds, and retry it. S3 Express answers in milliseconds, so this ends
    /// a stalled attempt, not a slow one. The attempt includes its connect,
    /// which times out at 3.1 s, so keep this above that. Every command's queue
    /// client reads it. Defaults to 5.
    #[serde(default = "default_todo_attempt_timeout_secs")]
    pub todo_attempt_timeout_secs: NonZeroU64,
    /// Fail a request to the `todo/` queue after this many seconds, its
    /// retries included. Defaults to 20, which holds the client's three
    /// attempts at the default attempt timeout and the backoff between them.
    #[serde(default = "default_todo_operation_timeout_secs")]
    pub todo_operation_timeout_secs: NonZeroU64,
    /// Ceiling, in bytes, on a request body for every route that does not set
    /// its own. Bodies are buffered whole and `POST /clients/register` is
    /// unauthenticated, so it is at most 1048576 (1 MB). Must be non-zero.
    /// Defaults to 65536 (64 KB).
    #[serde(default = "default_body_limit_bytes")]
    pub default_body_limit_bytes: NonZeroUsize,
    /// Ceiling, in bytes, on the request body that files or checks a plan
    /// (the validate, campaign filing, variants and document routes). Its size
    /// follows the source document; `plan_max_jobs` limits the expansion. A
    /// body past it gets `413` before it is parsed. Must be non-zero. Defaults
    /// to 134217728 (128 MB).
    #[serde(default = "default_plan_body_limit_bytes")]
    pub plan_body_limit_bytes: NonZeroUsize,
    /// Ceiling, in bytes, on a `POST /benchmarks` or `POST /benchmarks/batch`
    /// body. It, not the item count, bounds a batch. A body past it, or one
    /// whose parsed form would take more memory than it (a scalar costs a
    /// 32-byte `Value`, an object a whole `BTreeMap` node), gets `413` before
    /// it is parsed. Must be non-zero. Defaults to 524288000 (500 MB). At worst
    /// the two routes hold about eight times it at once, so size it to the
    /// host's memory.
    #[serde(default = "default_submission_body_limit_bytes")]
    pub submission_body_limit_bytes: NonZeroUsize,
    /// Ceiling, in bytes, on one file uploaded to the vault. The body streams,
    /// so a declared size past it gets `413` before reading, and one that runs
    /// past it is stopped, keeping nothing. Must be non-zero. Defaults to
    /// 137438953472 (128 GiB).
    #[serde(default = "default_vault_blob_limit_bytes")]
    pub vault_blob_limit_bytes: NonZeroU64,
    /// The HuggingFace endpoint ingestion imports gated models from, and a dry
    /// run resolves them against. Downloads redirect to a CDN host the egress rules must allow
    /// (`docs/operations.md`). Must be an absolute URL, checked at startup.
    /// Defaults to `https://huggingface.co`.
    #[serde(default = "default_hf_import_endpoint")]
    pub hf_import_endpoint: String,
    /// The maximum number of ingests that import from HuggingFace at the same
    /// time in one process. An ingest that needs an import past this ceiling
    /// gets `429`. Each import streams one blob at a time. An S3 upload buffers
    /// about 190 MiB of parts. Must be from 1 to 1023, and less than
    /// `hf_max_concurrent_requests`. Defaults to 2.
    #[serde(default = "default_hf_import_max_concurrent")]
    pub hf_import_max_concurrent: NonZeroUsize,
    /// The maximum number of HuggingFace requests that one process makes at
    /// the same time. The resolve of each model, in an ingest or a dry run,
    /// and each download share this number. A request waits for a permit when
    /// every permit is held. A download holds its permit until its body ends.
    /// Must be from 1 to 1024, and greater than `hf_import_max_concurrent`, so
    /// that the downloads never hold every permit. Defaults to 8.
    #[serde(default = "default_hf_max_concurrent_requests")]
    pub hf_max_concurrent_requests: NonZeroUsize,
    /// Ceiling on the jobs one plan may declare, on every route and `campaigns`
    /// command that files or checks a plan. Counted before any write, so a plan
    /// past it is rejected whole. It also bounds a read: the jobs reads list a
    /// plan in its document's order only where the document expands within it.
    /// Must be non-zero. Defaults to 5000.
    #[serde(default = "default_plan_max_jobs")]
    pub plan_max_jobs: NonZeroUsize,
    /// Rows a list route returns when the caller names no `limit`. Every list
    /// route shares it, so one setting moves the whole API's page size. Must be
    /// non-zero and at most `list_max_limit`. Defaults to 50.
    #[serde(default = "default_list_default_limit")]
    pub list_default_limit: NonZeroUsize,
    /// Ceiling on the `limit` a caller may ask for. A larger `limit` is refused
    /// with `400` rather than clamped, so a caller is told its page size was not
    /// honored instead of silently reading a short page. Raising it raises the
    /// objects one request may read. Must be non-zero. Defaults to 200.
    #[serde(default = "default_list_max_limit")]
    pub list_max_limit: NonZeroUsize,
    /// Age, in seconds, past which `queue-maintenance` deletes a partial job
    /// file under `todo/tmp/` (left behind by a crashed planner). Set it
    /// comfortably above the planner's longest write-then-rename window. On S3,
    /// a lifecycle rule on the `todo/tmp/` prefix can substitute for this pass.
    /// Must be non-zero. Defaults to 86400 (24 hours).
    #[serde(default = "default_todo_tmp_max_age_secs")]
    pub todo_tmp_max_age_secs: NonZeroU64,
    /// Age, in seconds, past which `queue-maintenance` tears down a plan still
    /// in `creating`, as a crashed ingest leaves it. Set it well above the
    /// longest ingest: tearing down a live one leaves its promoted jobs under a
    /// plan nothing retires. Must be non-zero. Defaults to 21600 (6 hours).
    #[serde(default = "default_plan_creating_max_age_secs")]
    pub plan_creating_max_age_secs: NonZeroU64,
    /// How long, in seconds, a warehouse day partition stays open to writes
    /// after its day begins; `process-submissions` archives a failure body
    /// unrecorded past it. The backend refuses writes to a closed partition, and
    /// that aborts the whole chunk and wedges every later run, so the window runs
    /// from the *start* of the day. This narrow reading skips at most one extra
    /// day of failure rows; the wide one, from its end, stalls the scorer. The
    /// default of 259200 (72 hours) leaves three day partitions open. Must be
    /// non-zero.
    #[serde(default = "default_warehouse_lock_age_secs")]
    pub warehouse_lock_age_secs: NonZeroU64,
    /// pipette-duckdb, which plan and campaign results are read from. Unset,
    /// `GET /api/v1/plans/{plan_id}/results.csv` answers `404`, a jobs read
    /// with `results=true` answers `results: off`, and `plans results`
    /// refuses.
    #[serde(default)]
    pub duckdb: Option<DuckDbConfig>,
    /// The Clerk instance the console signs readers in through. Unset, the
    /// console signs in with keys alone.
    #[serde(default)]
    pub clerk: Option<ClerkConfig>,
    /// The PostgreSQL database the records live in
    /// (`docs/design/sql-record-store.md`). Absent, the records stay on
    /// `[storage]` and `[auth_storage]`, which is for tests only.
    #[serde(default)]
    pub database: Option<DatabaseConfig>,
}

/// Offer rules for known runtime flavors, and the flavor order that importers
/// copy into new entries.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DescriptorCatalogConfig {
    /// Rules for known runtime flavors. Defaults to empty (deny all).
    pub offer_rules: Vec<OfferRule>,
    /// Per-importer member order for new entries. Defaults to empty.
    pub precedence: PrecedenceDefaults,
    /// Minimum age, in seconds, for a llama.cpp daily build. Default: 10800.
    /// Run the import at least this long after 00:00 UTC. An earlier run can
    /// name a day after an older build, because it skips the younger builds.
    pub llamacpp_min_build_age_secs: u64,
    /// When an importer entry moves from active to hidden.
    pub expiry: ExpiryConfig,
}

/// Thresholds for the automatic expiry of importer entries. Expiry reads the
/// release time of an entry. It hides only active entries.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExpiryConfig {
    /// The number of newest active or pinned stable entries of one upstream
    /// that stay active at every age. Default: 2.
    pub stable_keep_newest: usize,
    /// A stable entry outside the newest ones stays active for this many days
    /// after its release. Default: 60.
    pub stable_max_age_days: u32,
    /// A daily entry stays active for this many days after its release.
    /// Default: 14.
    pub daily_max_age_days: u32,
}

impl Default for ExpiryConfig {
    fn default() -> Self {
        Self {
            stable_keep_newest: 2,
            stable_max_age_days: 60,
            daily_max_age_days: 14,
        }
    }
}

fn default_llamacpp_min_build_age_secs() -> u64 {
    10_800
}

impl Default for DescriptorCatalogConfig {
    fn default() -> Self {
        Self {
            offer_rules: Vec::new(),
            precedence: PrecedenceDefaults::default(),
            llamacpp_min_build_age_secs: default_llamacpp_min_build_age_secs(),
            expiry: ExpiryConfig::default(),
        }
    }
}

/// A rule that offers a runtime flavor to each device that meets its `requires`
/// and `any_of` clauses.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OfferRule {
    /// Runtime type: `llamacpp_cli_stock_tools` or `ovms`.
    pub runtime_type: RuntimeType,
    /// Known flavor of that runtime type.
    pub flavor: String,
    /// Flags that a device must hold. The list must not be empty.
    pub requires: BTreeSet<String>,
    /// Groups of flags of which a device must hold at least one. Defaults to empty.
    #[serde(default)]
    pub any_of: Vec<BTreeSet<String>>,
}

/// Flavor order to copy into each imported entry.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PrecedenceDefaults {
    /// llama.cpp importer flavor order. Defaults to empty.
    pub llamacpp: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
enum OfferRuleError {
    #[error("offer rule {runtime_type} {flavor}: unsupported runtime type")]
    Unsupported {
        runtime_type: RuntimeType,
        flavor: String,
    },
    #[error("offer rule {runtime_type} {flavor}: unknown flavor")]
    UnknownFlavor {
        runtime_type: RuntimeType,
        flavor: String,
    },
    #[error("offer rule {runtime_type} {flavor}: non-canonical flag `{flag}`")]
    Flag {
        runtime_type: RuntimeType,
        flavor: String,
        flag: String,
    },
    #[error("offer rule {runtime_type} {flavor}: empty requires")]
    EmptyRequires {
        runtime_type: RuntimeType,
        flavor: String,
    },
    #[error("offer rule {runtime_type} {flavor}: empty any_of group at index {index}")]
    EmptyAnyOf {
        runtime_type: RuntimeType,
        flavor: String,
        index: usize,
    },
    #[error("offer rule {runtime_type} {flavor}: duplicate rule")]
    Duplicate {
        runtime_type: RuntimeType,
        flavor: String,
    },
    #[error("llamacpp precedence flavor `{flavor}` has no offer rule for llamacpp_cli_stock_tools")]
    PrecedenceNoRule { flavor: String },
    #[error("llamacpp precedence flavor `{flavor}` appears more than once")]
    PrecedenceDuplicate { flavor: String },
}

impl DescriptorCatalogConfig {
    /// The rule for a runtime flavor, if configured.
    pub fn rule_for(&self, runtime_type: RuntimeType, flavor: &str) -> Option<&OfferRule> {
        self.offer_rules
            .iter()
            .find(|rule| rule.runtime_type == runtime_type && rule.flavor == flavor)
    }

    /// The known flavors of `runtime_type`, or `None` if offer rules do not
    /// support that runtime type.
    pub fn known_flavors(runtime_type: RuntimeType) -> Option<Vec<String>> {
        match runtime_type {
            RuntimeType::LlamacppCliStockTools => Some(
                LlamaCppFlavor::known()
                    .map(|flavor| flavor.as_str().to_owned())
                    .collect(),
            ),
            RuntimeType::Ovms => Some(
                OvmsFlavor::known()
                    .map(|flavor| flavor.as_str().to_owned())
                    .collect(),
            ),
            RuntimeType::LlamacppApkPipette
            | RuntimeType::LlamacppIosPipette
            | RuntimeType::MlxMacosPipette
            | RuntimeType::MlxIosPipette
            | RuntimeType::DockerVllm
            | RuntimeType::DockerSglang
            | RuntimeType::UvVllm
            | RuntimeType::UvSglang
            | RuntimeType::UvOpenvino
            | RuntimeType::UvOnnxGenai
            | RuntimeType::OnnxGenaiIosPipette
            | RuntimeType::AppleFoundation
            | RuntimeType::TrtEdgellm => None,
        }
    }

    /// Known runtime flavors without an offer rule.
    pub fn unruled_flavors(&self) -> impl Iterator<Item = (RuntimeType, String)> + '_ {
        RuntimeType::iter()
            .filter_map(|runtime_type| {
                Self::known_flavors(runtime_type).map(|flavors| {
                    flavors
                        .into_iter()
                        .map(move |flavor| (runtime_type, flavor))
                })
            })
            .flatten()
            .filter(|(runtime_type, flavor)| self.rule_for(*runtime_type, flavor).is_none())
    }

    fn validate(&self) -> Result<(), OfferRuleError> {
        let mut seen = HashSet::new();
        for rule in &self.offer_rules {
            let runtime_type = rule.runtime_type;
            let flavor = rule.flavor.clone();
            let Some(known) = Self::known_flavors(runtime_type) else {
                return Err(OfferRuleError::Unsupported {
                    runtime_type,
                    flavor,
                });
            };
            if !known.contains(&flavor) {
                return Err(OfferRuleError::UnknownFlavor {
                    runtime_type,
                    flavor,
                });
            }
            if rule.requires.is_empty() {
                return Err(OfferRuleError::EmptyRequires {
                    runtime_type,
                    flavor,
                });
            }
            if let Some(flag) = rule
                .requires
                .iter()
                .chain(rule.any_of.iter().flatten())
                .find(|flag| CapabilityFlag::try_new((*flag).clone()).is_err())
            {
                return Err(OfferRuleError::Flag {
                    runtime_type,
                    flavor,
                    flag: flag.clone(),
                });
            }
            if let Some(index) = rule.any_of.iter().position(BTreeSet::is_empty) {
                return Err(OfferRuleError::EmptyAnyOf {
                    runtime_type,
                    flavor,
                    index,
                });
            }
            if !seen.insert((runtime_type, flavor.clone())) {
                return Err(OfferRuleError::Duplicate {
                    runtime_type,
                    flavor,
                });
            }
        }
        let mut precedence = HashSet::new();
        for flavor in &self.precedence.llamacpp {
            if !precedence.insert(flavor) {
                return Err(OfferRuleError::PrecedenceDuplicate {
                    flavor: flavor.clone(),
                });
            }
            if self
                .rule_for(RuntimeType::LlamacppCliStockTools, flavor)
                .is_none()
            {
                return Err(OfferRuleError::PrecedenceNoRule {
                    flavor: flavor.clone(),
                });
            }
        }
        Ok(())
    }
}

/// The PostgreSQL server the records live on: PostgreSQL 16 or later, with a
/// database that sorts text in byte order (`C` collation), the order the object
/// stores list keys in. If a key is absent, the server reads the matching libpq
/// variable (`PGHOST`, `PGPORT`, `PGUSER`, `PGDATABASE`, `PGSSLMODE`). A
/// container that sets those variables needs only the `[database]` section. The
/// password comes from `PGPASSWORD` only.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseConfig {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub user: Option<String>,
    pub database: Option<String>,
    /// `disable`, `prefer`, `require` or `verify-full`. Defaults to
    /// `PGSSLMODE`, else `prefer`.
    pub ssl_mode: Option<PostgresSslMode>,
    /// The connection pool's size. Defaults to 8.
    #[serde(default = "default_database_max_connections")]
    pub max_connections: NonZeroU32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PostgresSslMode {
    Disable,
    Prefer,
    Require,
    VerifyFull,
}

fn default_database_max_connections() -> NonZeroU32 {
    NonZeroU32::MIN.saturating_add(7)
}

/// A Clerk instance, verified offline: nothing here is a secret.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClerkConfig {
    /// `pk_live_…` or `pk_test_…`, which names the instance's Frontend API.
    pub publishable_key: String,
    /// The instance's PEM public key, from the dashboard's API keys page.
    pub public_key: String,
    /// The origins the console is served from: a token's `azp` must be one, and
    /// the sign-in page offers Clerk at no other host.
    pub authorized_parties: Vec<String>,
    /// The JWT template the sign-in page asks Clerk for. A token's `aud` must be
    /// this name, so one from another app's template is refused. A blank name
    /// stops the server at startup. Defaults to `pipette-mgmt`.
    #[serde(default = "default_clerk_jwt_template")]
    pub jwt_template: String,
    /// The domains whose addresses may sign in through Clerk, matched exactly
    /// and case-insensitively. Empty admits any verified address. A provision
    /// rule outside the list stops the server at startup.
    #[serde(default)]
    pub allowed_domains: Vec<String>,
    /// `[[clerk.provision]]` rules, one per domain.
    #[serde(default)]
    pub provision: Vec<ClerkProvisionRule>,
}

/// The `[clerk] jwt_template` of a section that names none.
pub const DEFAULT_CLERK_JWT_TEMPLATE: &str = "pipette-mgmt";

fn default_clerk_jwt_template() -> String {
    DEFAULT_CLERK_JWT_TEMPLATE.to_string()
}

/// Addresses at `domain` that no account holds get an `org_member` account in
/// `org` on their first sign-in.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClerkProvisionRule {
    /// The part after `@`, matched exactly and case-insensitively.
    pub domain: String,
    /// The organization's handle or id.
    pub org: String,
}

/// Where pipette-duckdb answers, and the Basic credential mgmt reads it with.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DuckDbConfig {
    /// Base URL, without `/api`.
    pub url: String,
    /// The HTTP Basic credential, read from this file (unlike the S3 and
    /// PostgreSQL ones). It must reach `database`.
    pub user: String,
    pub password: DuckDbPassword,
    /// The database queried: it must publish what `docs/storage.md` lists for
    /// the results reads.
    /// Defaults to `internal`.
    #[serde(default = "default_duckdb_database")]
    pub database: String,
    /// Per-request timeout. Shorter than `http_timeout_secs`, which is sized
    /// for the scorer: a reader is waiting on this one. A page of a plan's jobs
    /// waits at most 3 s, or this if shorter. Defaults to 30.
    #[serde(default = "default_duckdb_timeout_secs")]
    pub timeout_secs: NonZeroU64,
}

/// The `[duckdb]` password. Redacted in `Debug`, like the pre-auth secrets, so a
/// logged config does not publish it.
#[nutype::nutype(derive(Clone, AsRef, Deserialize))]
pub struct DuckDbPassword(String);

impl DuckDbPassword {
    /// The raw password. Named `expose` so every disclosure site is easy to audit.
    pub fn expose(&self) -> &str {
        self.as_ref()
    }
}

impl std::fmt::Debug for DuckDbPassword {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DuckDbPassword(redacted)")
    }
}

fn default_duckdb_database() -> String {
    "internal".to_string()
}

fn default_duckdb_timeout_secs() -> NonZeroU64 {
    nonzero_u64(30)
}

/// Email allow rules that approve a client at registration, by its
/// self-reported `contact_email`: not a security control. Empty lists, the
/// default, approve nobody.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AutoApproveConfig {
    /// Full addresses, case-insensitive exact match.
    #[serde(default)]
    pub emails: Vec<String>,
    /// Domains (part after `@`), case-insensitive; a leading `@` is tolerated.
    #[serde(default)]
    pub domains: Vec<String>,
}

impl AutoApproveConfig {
    /// Whether `email` matches any allow rule. Always `false` when both
    /// lists are empty, so the feature stays off until configured.
    pub fn approves(&self, email: &str) -> bool {
        let email = email.trim().to_ascii_lowercase();
        if self
            .emails
            .iter()
            .any(|allowed| allowed.trim().eq_ignore_ascii_case(&email))
        {
            return true;
        }
        let Some((_, domain)) = email.rsplit_once('@') else {
            return false;
        };
        self.domains.iter().any(|allowed| {
            let allowed = allowed.trim().trim_start_matches('@');
            allowed.eq_ignore_ascii_case(domain)
        })
    }
}

/// Config for the write-only unverified submission archive. When `enabled`, the
/// result submissions hold a signed submission from an unapproved client under
/// `submissions/unverified/` rather than refusing it.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UnverifiedSubmissionsConfig {
    #[serde(default)]
    pub enabled: bool,
}

fn default_warehouse_read_days() -> u32 {
    14
}

fn default_warehouse_max_rows_per_part() -> usize {
    1_000
}

fn default_catalog_ttl_secs() -> u64 {
    180
}

fn default_http_timeout_secs() -> u64 {
    600
}

fn default_http_connect_timeout_secs() -> NonZeroU64 {
    nonzero_u64(3)
}

fn default_parquet_zstd_level() -> ZstdLevel {
    crate::parquet_utils::default_zstd_level()
}

/// Rejects an invalid level at load rather than on the first warehouse write.
fn deserialize_parquet_zstd_level<'de, D: Deserializer<'de>>(d: D) -> Result<ZstdLevel, D::Error> {
    let raw = i32::deserialize(d)?;
    // parquet 60 also accepts zstd's negative fast levels and 0; keep the documented 1..=22.
    if !(1..=22).contains(&raw) {
        return Err(serde::de::Error::custom(format!(
            "invalid parquet_zstd_level {raw}: must be in 1..=22"
        )));
    }
    ZstdLevel::try_new(raw)
        .map_err(|e| serde::de::Error::custom(format!("invalid parquet_zstd_level {raw}: {e}")))
}

/// A `NonZeroU64` written as a literal. Panicking constructors are denied
/// crate-wide; as a `const fn` over literals the zero arm is unreachable unless
/// typed here, and then yields 1, a default an operator notices.
const fn nonzero_u64(value: u64) -> NonZeroU64 {
    match NonZeroU64::new(value) {
        Some(n) => n,
        None => NonZeroU64::MIN,
    }
}

/// A `NonZeroUsize` written as a literal. See [`nonzero_u64`].
const fn nonzero_usize(value: usize) -> NonZeroUsize {
    match NonZeroUsize::new(value) {
        Some(n) => n,
        None => NonZeroUsize::MIN,
    }
}

fn default_score_chunk_size() -> NonZeroUsize {
    nonzero_usize(50)
}

fn default_mutate_lock_ttl_secs() -> NonZeroU64 {
    nonzero_u64(1800)
}

fn default_dataset_probe_max_age_secs() -> NonZeroU64 {
    nonzero_u64(180)
}

fn default_dataset_probe_timeout_secs() -> NonZeroU64 {
    nonzero_u64(30)
}

fn default_dataset_build_timeout_secs() -> NonZeroU64 {
    nonzero_u64(900)
}

fn default_dataset_asr_build_timeout_secs() -> NonZeroU64 {
    nonzero_u64(7200)
}

fn default_plan_lease_duration_secs() -> NonZeroU64 {
    nonzero_u64(300)
}

fn default_plan_lost_lease_limit() -> usize {
    3
}

fn default_body_limit_bytes() -> NonZeroUsize {
    nonzero_usize(64 * 1024)
}

fn default_plan_body_limit_bytes() -> NonZeroUsize {
    nonzero_usize(128 * 1024 * 1024)
}

fn default_submission_body_limit_bytes() -> NonZeroUsize {
    nonzero_usize(500 * 1024 * 1024)
}

fn default_vault_blob_limit_bytes() -> NonZeroU64 {
    nonzero_u64(128 * 1024 * 1024 * 1024)
}

fn default_hf_import_endpoint() -> String {
    "https://huggingface.co".to_owned()
}

fn default_hf_import_max_concurrent() -> NonZeroUsize {
    nonzero_usize(2)
}

fn default_hf_max_concurrent_requests() -> NonZeroUsize {
    nonzero_usize(8)
}

fn default_plan_max_jobs() -> NonZeroUsize {
    nonzero_usize(5000)
}

fn default_list_default_limit() -> NonZeroUsize {
    nonzero_usize(50)
}

fn default_list_max_limit() -> NonZeroUsize {
    nonzero_usize(200)
}

fn default_todo_tmp_max_age_secs() -> NonZeroU64 {
    nonzero_u64(86_400)
}

fn default_warehouse_lock_age_secs() -> NonZeroU64 {
    nonzero_u64(259_200)
}

fn default_plan_creating_max_age_secs() -> NonZeroU64 {
    nonzero_u64(21_600)
}

fn default_probe_backend_capabilities() -> bool {
    true
}

fn default_accept_legacy_signatures() -> bool {
    true
}

fn default_accept_v2_signatures() -> bool {
    true
}

fn default_trusted_proxy_hops() -> usize {
    1
}

fn default_read_timeout_secs() -> NonZeroU64 {
    nonzero_u64(75)
}

fn default_todo_attempt_timeout_secs() -> NonZeroU64 {
    nonzero_u64(5)
}

fn default_todo_operation_timeout_secs() -> NonZeroU64 {
    nonzero_u64(20)
}

fn default_max_connections() -> NonZeroUsize {
    nonzero_usize(512)
}

fn default_listen_addr() -> String {
    "0.0.0.0:3000".to_string()
}

impl Default for Config {
    /// For test fixtures and `..Default::default()`. `evals_server_url` is
    /// empty, so a `Config` from `Default` is not a usable runtime config.
    fn default() -> Self {
        Self {
            descriptor_catalog: DescriptorCatalogConfig::default(),
            evals_server_url: String::new(),
            listen_addr: default_listen_addr(),
            read_timeout_secs: default_read_timeout_secs(),
            max_connections: default_max_connections(),
            catalog_ttl_secs: default_catalog_ttl_secs(),
            warehouse_read_days: default_warehouse_read_days(),
            warehouse_max_rows_per_part: default_warehouse_max_rows_per_part(),
            http_timeout_secs: default_http_timeout_secs(),
            http_connect_timeout_secs: default_http_connect_timeout_secs(),
            parquet_zstd_level: default_parquet_zstd_level(),
            score_chunk_size: default_score_chunk_size(),
            mutate_lock_ttl_secs: default_mutate_lock_ttl_secs(),
            dataset_probe_max_age_secs: default_dataset_probe_max_age_secs(),
            dataset_probe_timeout_secs: default_dataset_probe_timeout_secs(),
            dataset_build_timeout_secs: default_dataset_build_timeout_secs(),
            dataset_asr_build_timeout_secs: default_dataset_asr_build_timeout_secs(),
            storage: StorageConfig::default(),
            auth_storage: StorageConfig::default(),
            unverified_submissions: UnverifiedSubmissionsConfig::default(),
            auto_approve: AutoApproveConfig::default(),
            require_client_preauth_key: false,
            require_user_preauth_key: false,
            accept_legacy_signatures: default_accept_legacy_signatures(),
            accept_v2_signatures: default_accept_v2_signatures(),
            authorities: Vec::new(),
            trusted_proxy_hops: default_trusted_proxy_hops(),
            probe_backend_capabilities: default_probe_backend_capabilities(),
            todo_storage: None,
            plan_lease_duration_secs: default_plan_lease_duration_secs(),
            plan_lost_lease_limit: default_plan_lost_lease_limit(),
            queue_maintenance_read_timeout_secs: None,
            todo_attempt_timeout_secs: default_todo_attempt_timeout_secs(),
            todo_operation_timeout_secs: default_todo_operation_timeout_secs(),
            default_body_limit_bytes: default_body_limit_bytes(),
            plan_body_limit_bytes: default_plan_body_limit_bytes(),
            submission_body_limit_bytes: default_submission_body_limit_bytes(),
            vault_blob_limit_bytes: default_vault_blob_limit_bytes(),
            hf_import_endpoint: default_hf_import_endpoint(),
            hf_import_max_concurrent: default_hf_import_max_concurrent(),
            hf_max_concurrent_requests: default_hf_max_concurrent_requests(),
            plan_max_jobs: default_plan_max_jobs(),
            list_default_limit: default_list_default_limit(),
            list_max_limit: default_list_max_limit(),
            todo_tmp_max_age_secs: default_todo_tmp_max_age_secs(),
            plan_creating_max_age_secs: default_plan_creating_max_age_secs(),
            warehouse_lock_age_secs: default_warehouse_lock_age_secs(),
            duckdb: None,
            clerk: None,
            database: None,
        }
    }
}

/// Config keys this binary has renamed, oldest spelling first. A stale key
/// would be dropped and the field defaulted (`require_preauth_key` to `false`,
/// reopening keyless registration), so a rename is a load error.
const RENAMED_KEYS: &[(&str, &str)] = &[("require_preauth_key", "require_client_preauth_key")];

#[derive(Debug, thiserror::Error)]
#[error(
    "list_default_limit = {default} exceeds list_max_limit = {max}; every default page would exceed its own ceiling"
)]
struct ListDefaultLimitExceedsMaxError {
    default: NonZeroUsize,
    max: NonZeroUsize,
}

/// Refuse a config file that still uses a pre-rename key spelling.
fn reject_renamed_keys(table: &toml::Table) -> anyhow::Result<()> {
    match RENAMED_KEYS
        .iter()
        .find(|(old, _)| table.contains_key(*old))
    {
        Some((old, new)) => Err(anyhow::anyhow!(
            "config key `{old}` was renamed to `{new}`; update the config file"
        )),
        None => Ok(()),
    }
}

fn read_config(path: &std::path::Path) -> anyhow::Result<String> {
    std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("failed to read config file {}: {e}", path.display()))
}

fn parse_error(path: &std::path::Path, e: toml::de::Error) -> anyhow::Error {
    anyhow::anyhow!("failed to parse config file {}: {e}", path.display())
}

impl Config {
    /// The `todo/` storage, falling back to `[storage]`. Commands that touch
    /// `todo/` check for atomic `RenameObject` at startup
    /// (`TodoStore::validate_backend`), so a regular S3 fallback is rejected.
    pub fn todo_storage(&self) -> &StorageConfig {
        self.todo_storage.as_ref().unwrap_or(&self.storage)
    }

    /// This config, with `queue_maintenance_read_timeout_secs` applied to the
    /// S3 sections of `[storage]` and `[auth_storage]`.
    pub fn for_queue_maintenance(mut self) -> Self {
        let timeout = self.queue_maintenance_read_timeout_secs;
        [&mut self.storage, &mut self.auth_storage]
            .into_iter()
            .for_each(|storage| {
                if let StorageConfig::S3 {
                    read_timeout_secs, ..
                } = storage
                {
                    *read_timeout_secs = timeout;
                }
            });
        self
    }

    /// The config file at `path`, parsed and validated. Each error names
    /// `path`.
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let content = read_config(path)?;
        let table: toml::Table = toml::from_str(&content).map_err(|e| parse_error(path, e))?;
        let invalid =
            |e: anyhow::Error| anyhow::anyhow!("invalid config file {}: {e:#}", path.display());
        reject_renamed_keys(&table).map_err(invalid)?;
        let config: Config = toml::from_str(&content).map_err(|e| parse_error(path, e))?;
        config.validate().map_err(invalid)?;
        Ok(config)
    }

    /// The dotted path of each key in the config file at `path` that no
    /// `Config` field reads. `load` ignores such a key, so its default applies.
    /// A key inside a storage section is not seen: the section deserializes as
    /// a tagged enum, which takes its keys as one unit.
    pub fn unknown_keys(path: &std::path::Path) -> anyhow::Result<Vec<String>> {
        let content = read_config(path)?;
        let value: toml::Value = toml::from_str(&content).map_err(|e| parse_error(path, e))?;
        let mut unknown = Vec::new();
        serde_ignored::deserialize::<_, _, Config>(value, |key| unknown.push(key.to_string()))
            .map_err(|e| parse_error(path, e))?;
        Ok(unknown)
    }

    /// Bounds the field types cannot express. Each duration is capped at 10
    /// years, far below where its `chrono` arithmetic breaks, so an oversized
    /// one fails at load rather than mid-pass. `hf_max_concurrent_requests` must
    /// exceed `hf_import_max_concurrent`, or the downloads could hold every
    /// permit, and `list_default_limit` must not exceed `list_max_limit`.
    fn validate(&self) -> anyhow::Result<()> {
        const MAX_DURATION_SECS: u64 = 10 * 365 * 24 * 60 * 60;
        const MAX_HF_IMPORTS: usize = 1024;
        const MAX_DEFAULT_BODY_LIMIT: usize = 1024 * 1024;
        self.descriptor_catalog.validate()?;
        if self.plan_lease_duration_secs.get() > MAX_DURATION_SECS {
            anyhow::bail!(
                "plan_lease_duration_secs = {} exceeds the maximum of {MAX_DURATION_SECS} (10 years)",
                self.plan_lease_duration_secs
            );
        }
        if self.todo_tmp_max_age_secs.get() > MAX_DURATION_SECS {
            anyhow::bail!(
                "todo_tmp_max_age_secs = {} exceeds the maximum of {MAX_DURATION_SECS} (10 years)",
                self.todo_tmp_max_age_secs
            );
        }
        if self.warehouse_lock_age_secs.get() > MAX_DURATION_SECS {
            anyhow::bail!(
                "warehouse_lock_age_secs = {} exceeds the maximum of {MAX_DURATION_SECS} (10 years)",
                self.warehouse_lock_age_secs
            );
        }
        if self.plan_creating_max_age_secs.get() > MAX_DURATION_SECS {
            anyhow::bail!(
                "plan_creating_max_age_secs = {} exceeds the maximum of {MAX_DURATION_SECS} (10 years)",
                self.plan_creating_max_age_secs
            );
        }
        if self.default_body_limit_bytes.get() > MAX_DEFAULT_BODY_LIMIT {
            anyhow::bail!(
                "default_body_limit_bytes = {} exceeds the maximum of {MAX_DEFAULT_BODY_LIMIT} (1 MB)",
                self.default_body_limit_bytes
            );
        }
        if self.hf_import_max_concurrent.get() > MAX_HF_IMPORTS {
            anyhow::bail!(
                "hf_import_max_concurrent = {} exceeds the maximum of {MAX_HF_IMPORTS}",
                self.hf_import_max_concurrent
            );
        }
        if self.hf_max_concurrent_requests.get() > MAX_HF_IMPORTS {
            anyhow::bail!(
                "hf_max_concurrent_requests = {} exceeds the maximum of {MAX_HF_IMPORTS}",
                self.hf_max_concurrent_requests
            );
        }
        if self.hf_max_concurrent_requests <= self.hf_import_max_concurrent {
            anyhow::bail!(
                "hf_max_concurrent_requests = {} must be greater than hf_import_max_concurrent = {}, \
                 so that the downloads never hold every request permit",
                self.hf_max_concurrent_requests,
                self.hf_import_max_concurrent
            );
        }
        if self.list_default_limit > self.list_max_limit {
            return Err(ListDefaultLimitExceedsMaxError {
                default: self.list_default_limit,
                max: self.list_max_limit,
            }
            .into());
        }
        Ok(())
    }

    /// A lease expiry `plan_lease_duration_secs` after `now`: the one definition
    /// of a fresh lease. Infallible because [`Config::validate`] bounds the
    /// duration; a hand-built `Config` must respect it.
    #[expect(
        clippy::cast_possible_wrap,
        reason = "`Config::validate` bounds the duration at 10 years, far below `i64::MAX`"
    )]
    pub fn lease_expiry_from(
        &self,
        now: chrono::DateTime<chrono::Utc>,
    ) -> chrono::DateTime<chrono::Utc> {
        now + chrono::Duration::seconds(self.plan_lease_duration_secs.get() as i64)
    }

    pub fn writer_opts(&self) -> WriterOpts {
        WriterOpts {
            zstd_level: self.parquet_zstd_level,
        }
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Context;
    use rstest::rstest;

    use crate::config::*;

    /// The smallest config that loads, with `lines` as its top-level keys.
    fn minimal_toml(lines: &str) -> String {
        format!(
            r#"
evals_server_url = "http://evals:8000"
{lines}

[auth_storage]
backend = "local_fs"
"#
        )
    }

    const LLAMA: &str = "llamacpp_cli_stock_tools";
    const MACOS: &str = r#"["os:macos"]"#;

    /// One offer rule table for the validation cases.
    fn rule(runtime_type: &str, flavor: &str, requires: &str) -> String {
        format!(
            "[[descriptor_catalog.offer_rules]]\nruntime_type = \"{runtime_type}\"\nflavor = \"{flavor}\"\nrequires = {requires}\n"
        )
    }

    #[rstest]
    #[case::valid(rule(LLAMA, "macos-x64", MACOS), None)]
    #[case::unsupported(
        rule("mlx_macos_pipette", "macos-x64", MACOS),
        Some("offer rule mlx_macos_pipette macos-x64: unsupported runtime type")
    )]
    #[case::unknown_flavor(
        rule(LLAMA, "macos-riscv", MACOS),
        Some("offer rule llamacpp_cli_stock_tools macos-riscv: unknown flavor")
    )]
    #[case::custom_flavor(
        rule("ovms", "custom-x64", MACOS),
        Some("offer rule ovms custom-x64: unknown flavor")
    )]
    #[case::noncanonical_flag(
        rule(LLAMA, "macos-x64", r#"["OS:macOS"]"#),
        Some("offer rule llamacpp_cli_stock_tools macos-x64: non-canonical flag `OS:macOS`")
    )]
    #[case::empty_requires(
        rule(LLAMA, "macos-x64", "[]"),
        Some("offer rule llamacpp_cli_stock_tools macos-x64: empty requires")
    )]
    #[case::empty_any_of(
        rule(LLAMA, "macos-x64", MACOS) + "any_of = [[]]\n",
        Some("offer rule llamacpp_cli_stock_tools macos-x64: empty any_of group at index 0")
    )]
    #[case::bad_any_of_flag(
        rule(LLAMA, "macos-x64", MACOS) + "any_of = [[\"OS:macOS\"]]\n",
        Some("offer rule llamacpp_cli_stock_tools macos-x64: non-canonical flag `OS:macOS`")
    )]
    #[case::duplicate_rule(
        rule(LLAMA, "macos-x64", MACOS).repeat(2),
        Some("offer rule llamacpp_cli_stock_tools macos-x64: duplicate rule")
    )]
    #[case::precedence_no_rule(
        "[descriptor_catalog.precedence]\nllamacpp = [\"macos-arm64\"]\n".to_owned(),
        Some(
            "llamacpp precedence flavor `macos-arm64` has no offer rule for llamacpp_cli_stock_tools"
        )
    )]
    #[case::precedence_duplicate(
        "[descriptor_catalog.precedence]\nllamacpp = [\"macos-x64\", \"macos-x64\"]\n".to_owned()
            + &rule(LLAMA, "macos-x64", MACOS),
        Some("llamacpp precedence flavor `macos-x64` appears more than once")
    )]
    fn descriptor_catalog_validation(
        #[case] section: String,
        #[case] error: Option<&str>,
    ) -> anyhow::Result<()> {
        let config: Config = toml::from_str(&(minimal_toml("") + &section))?;
        assert_eq!(
            config
                .validate()
                .err()
                .map(|err| err.to_string())
                .as_deref(),
            error
        );
        Ok(())
    }

    #[test]
    fn descriptor_catalog_refuses_unknown_keys() {
        assert!(
            toml::from_str::<Config>(&(minimal_toml("") + "\n[descriptor_catalog]\nunknown = 1\n"))
                .is_err()
        );
    }

    #[test]
    fn descriptor_catalog_defaults_to_no_rules() -> anyhow::Result<()> {
        let config: Config = toml::from_str(&minimal_toml(""))?;
        assert!(config.descriptor_catalog.offer_rules.is_empty());
        assert!(config.descriptor_catalog.precedence.llamacpp.is_empty());
        assert_eq!(
            config.descriptor_catalog.llamacpp_min_build_age_secs,
            10_800
        );
        assert_eq!(
            config.descriptor_catalog.unruled_flavors().count(),
            LlamaCppFlavor::known().count() + OvmsFlavor::known().count()
        );
        Ok(())
    }

    #[test]
    fn catalog_build_age_can_be_set() -> anyhow::Result<()> {
        let config: Config = toml::from_str(
            &(minimal_toml("") + "\n[descriptor_catalog]\nllamacpp_min_build_age_secs = 21600\n"),
        )?;
        assert_eq!(
            config.descriptor_catalog.llamacpp_min_build_age_secs,
            21_600
        );
        Ok(())
    }

    #[rstest::rstest]
    #[case::defaults("", (2, 60, 14))]
    #[case::set(
        "\n[descriptor_catalog.expiry]\nstable_keep_newest = 3\nstable_max_age_days = 90\ndaily_max_age_days = 7\n",
        (3, 90, 7)
    )]
    #[case::partial("\n[descriptor_catalog.expiry]\ndaily_max_age_days = 30\n", (2, 60, 30))]
    fn catalog_expiry_reads_its_thresholds(
        #[case] extra: &str,
        #[case] expected: (usize, u32, u32),
    ) -> anyhow::Result<()> {
        let config: Config = toml::from_str(&(minimal_toml("") + extra))?;
        let expiry = &config.descriptor_catalog.expiry;
        assert_eq!(
            (
                expiry.stable_keep_newest,
                expiry.stable_max_age_days,
                expiry.daily_max_age_days
            ),
            expected
        );
        Ok(())
    }

    #[test]
    fn catalog_expiry_refuses_unknown_keys() {
        assert!(
            toml::from_str::<Config>(
                &(minimal_toml("") + "\n[descriptor_catalog.expiry]\nstable_max_age_months = 2\n")
            )
            .is_err()
        );
    }

    #[test]
    fn descriptor_catalog_example_rules_validate() -> anyhow::Result<()> {
        let block = include_str!("../../../examples/config.toml")
            .lines()
            .skip_while(|line| *line != "# [descriptor_catalog]")
            .take_while(|line| line.starts_with('#'))
            .map(|line| line.strip_prefix("# ").unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n");
        let config: Config = toml::from_str(&(minimal_toml("") + &block))?;
        config.validate()?;
        assert_eq!(config.descriptor_catalog.offer_rules.len(), 2);
        assert_eq!(
            config
                .descriptor_catalog
                .offer_rules
                .first()
                .and_then(|rule| rule.any_of.first())
                .map(BTreeSet::len),
            Some(20)
        );
        assert_eq!(
            config.descriptor_catalog.precedence.llamacpp,
            ["macos-arm64", "macos-x64"]
        );
        assert_eq!(config.descriptor_catalog.expiry.stable_max_age_days, 60);
        Ok(())
    }

    /// The stale spelling must stop the process, not fall through to the
    /// renamed field's default, which would silently reopen keyless registration.
    #[test]
    fn a_renamed_config_key_is_refused_at_load() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("config.toml");

        std::fs::write(&path, minimal_toml("require_preauth_key = true"))?;
        let err = Config::load(&path)
            .err()
            .context("the earlier spelling must not load")?
            .to_string();
        assert!(err.contains("require_client_preauth_key"), "{err}");

        // The current spelling still loads, and still gates.
        std::fs::write(&path, minimal_toml("require_client_preauth_key = true"))?;
        assert!(Config::load(&path)?.require_client_preauth_key);
        Ok(())
    }

    /// Retention is gone, not renamed: a deployed config that still sets it
    /// must still load.
    #[test]
    fn a_config_setting_the_removed_retention_still_loads() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("config.toml");
        std::fs::write(&path, minimal_toml("terminal_plan_retention_days = 30"))?;
        Config::load(&path)?;
        Ok(())
    }

    #[test]
    fn test_validate_bounds_durations() -> anyhow::Result<()> {
        let config: Config = toml::from_str(&minimal_toml(""))?;
        assert!(config.validate().is_ok(), "default durations are valid");

        // Oversized values would break chrono arithmetic downstream; validation
        // rejects them at load.
        let config: Config =
            toml::from_str(&minimal_toml("plan_lease_duration_secs = 99999999999999"))?;
        assert!(config.validate().is_err());
        let config: Config =
            toml::from_str(&minimal_toml("todo_tmp_max_age_secs = 99999999999999"))?;
        assert!(config.validate().is_err());
        let config: Config =
            toml::from_str(&minimal_toml("plan_creating_max_age_secs = 99999999999999"))?;
        assert!(config.validate().is_err());
        Ok(())
    }

    #[rstest]
    #[case::default("", true)]
    #[case::at_the_maximum("default_body_limit_bytes = 1048576", true)]
    #[case::above_the_maximum("default_body_limit_bytes = 1048577", false)]
    fn test_default_body_limit_bytes_bounds(
        #[case] lines: &str,
        #[case] is_valid: bool,
    ) -> anyhow::Result<()> {
        let config: Config = toml::from_str(&minimal_toml(lines))?;
        assert_eq!(config.validate().is_ok(), is_valid, "{lines}");
        Ok(())
    }

    /// The request cap must be from 1 to 1024, and greater than the import
    /// cap. Otherwise the downloads can hold every request permit. Thus the
    /// largest import cap that loads is 1023.
    #[rstest]
    #[case::defaults("", true)]
    #[case::at_the_maximum("hf_max_concurrent_requests = 1024", true)]
    #[case::above_the_maximum("hf_max_concurrent_requests = 1025", false)]
    #[case::equal_to_the_import_cap(
        "hf_import_max_concurrent = 4\nhf_max_concurrent_requests = 4",
        false
    )]
    #[case::one_above_the_import_cap(
        "hf_import_max_concurrent = 4\nhf_max_concurrent_requests = 5",
        true
    )]
    #[case::below_the_default_import_cap("hf_max_concurrent_requests = 1", false)]
    #[case::import_cap_at_the_default_request_cap("hf_import_max_concurrent = 8", false)]
    #[case::import_cap_above_the_maximum("hf_import_max_concurrent = 1025", false)]
    #[case::largest_import_cap_that_loads(
        "hf_import_max_concurrent = 1023\nhf_max_concurrent_requests = 1024",
        true
    )]
    fn test_hf_max_concurrent_requests_bounds(
        #[case] lines: &str,
        #[case] is_valid: bool,
    ) -> anyhow::Result<()> {
        let config: Config = toml::from_str(&minimal_toml(lines))?;
        assert_eq!(config.validate().is_ok(), is_valid, "{lines}");
        Ok(())
    }

    #[rstest]
    #[case::default_above_maximum(500, 200, false)]
    #[case::default_equals_maximum(200, 200, true)]
    fn test_list_default_limit_must_not_exceed_max(
        #[case] default: usize,
        #[case] max: usize,
        #[case] is_valid: bool,
    ) -> anyhow::Result<()> {
        let config_toml = minimal_toml(&format!(
            "list_default_limit = {default}\nlist_max_limit = {max}"
        ));
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("config.toml");
        std::fs::write(&path, config_toml)?;

        let result = Config::load(&path);
        if is_valid {
            result.context("an equal default and maximum must be valid")?;
        } else {
            // The message names both keys, because either one may be the value
            // the operator intended to change.
            let message = result
                .err()
                .context("a default page above its own ceiling is a load error")?
                .to_string();
            assert!(
                message.contains("list_default_limit") && message.contains("list_max_limit"),
                "the message names both keys: {message}"
            );
        }
        Ok(())
    }

    #[rstest]
    #[case("list_default_limit")]
    #[case("list_max_limit")]
    fn test_list_limit_zero_rejected(#[case] key: &str) {
        let result: Result<Config, _> = toml::from_str(&minimal_toml(&format!("{key} = 0")));
        assert!(
            result.is_err(),
            "{key} = 0 must be rejected at parse time — a zero page returns no rows, \
             so every drain would loop forever"
        );
    }

    /// The smallest config with a `[duckdb]` section, its optional keys unset.
    fn duckdb_config() -> String {
        minimal_toml("")
            + r#"
[duckdb]
url = "http://duckdb:3100"
user = "mgmt"
password = "hunter2"
"#
    }

    #[test]
    fn test_duckdb_section_defaults_and_keeps_its_password_out_of_debug() -> anyhow::Result<()> {
        let config: Config = toml::from_str(&duckdb_config())?;
        let duckdb = config.duckdb.context("[duckdb] parsed")?;
        assert_eq!(duckdb.database, "internal");
        assert_eq!(duckdb.timeout_secs.get(), 30);
        assert!(!format!("{duckdb:?}").contains("hunter2"));
        Ok(())
    }

    #[test]
    fn test_duckdb_section_refuses_an_unknown_key() {
        let toml = format!("{}db = \"internal\"\n", duckdb_config());
        assert!(toml::from_str::<Config>(&toml).is_err());
    }

    /// The smallest config with a bare `[database]` section.
    fn database_config() -> String {
        minimal_toml("") + "\n[database]\n"
    }

    /// The RDS (Amazon Relational Database Service) stack sets the libpq
    /// variables in the mgmt container. The container needs only the bare
    /// `[database]` section.
    #[test]
    fn a_bare_database_section_leaves_the_connection_to_libpq() -> anyhow::Result<()> {
        let config: Config = toml::from_str(&database_config())?;
        let Some(DatabaseConfig {
            host: None,
            port: None,
            user: None,
            database: None,
            ssl_mode: None,
            max_connections,
        }) = config.database
        else {
            anyhow::bail!("expected a bare section, got {:?}", config.database);
        };
        assert_eq!(max_connections.get(), 8);
        Ok(())
    }

    #[test]
    fn the_database_section_refuses_an_unknown_key() {
        let toml = format!("{}password = \"hunter2\"\n", database_config());
        assert!(toml::from_str::<Config>(&toml).is_err());
    }

    /// The smallest config with a `[clerk]` section and one provision rule.
    fn clerk_config() -> String {
        minimal_toml("")
            + r#"
[clerk]
publishable_key = "pk_test_Y2xlcmsuZXhhbXBsZS50ZXN0JA"
public_key = "-----BEGIN PUBLIC KEY-----"
authorized_parties = ["https://console.example.test"]
allowed_domains = ["example.test"]

[[clerk.provision]]
domain = "example.test"
org = "acme"
"#
    }

    #[test]
    fn test_clerk_section_parses_its_provision_rules() -> anyhow::Result<()> {
        let config: Config = toml::from_str(&clerk_config())?;
        let clerk = config.clerk.context("[clerk] parsed")?;
        assert_eq!(clerk.authorized_parties, ["https://console.example.test"]);
        assert_eq!(clerk.allowed_domains, ["example.test"]);
        let [rule] = clerk.provision.as_slice() else {
            anyhow::bail!("one rule, got {:?}", clerk.provision);
        };
        assert_eq!(
            (rule.domain.as_str(), rule.org.as_str()),
            ("example.test", "acme")
        );
        Ok(())
    }

    #[rstest]
    #[case::absent("", "pipette-mgmt")]
    #[case::named("jwt_template = \"acme-console\"", "acme-console")]
    fn test_clerk_section_names_its_jwt_template(
        #[case] line: &str,
        #[case] template: &str,
    ) -> anyhow::Result<()> {
        let toml = clerk_config().replacen("[clerk]", &format!("[clerk]\n{line}"), 1);
        let clerk = toml::from_str::<Config>(&toml)?
            .clerk
            .context("[clerk] parsed")?;
        assert_eq!(clerk.jwt_template, template);
        Ok(())
    }

    #[rstest]
    #[case::section("[clerk]", "frontend_api = \"clerk.example.test\"")]
    #[case::rule("[[clerk.provision]]", "group = \"server_admin\"")]
    fn test_clerk_section_refuses_an_unknown_key(#[case] table: &str, #[case] line: &str) {
        let toml = clerk_config().replacen(table, &format!("{table}\n{line}"), 1);
        assert!(
            toml::from_str::<Config>(&toml).is_err(),
            "{line} under {table}"
        );
    }

    #[test]
    fn test_parse_full_config() -> anyhow::Result<()> {
        let toml = r#"
evals_server_url = "http://evals:8000"
listen_addr = "127.0.0.1:9000"

[storage]
backend = "local_fs"
data_dir = "/var/lib/pipette-mgmt"

[auth_storage]
backend = "local_fs"
data_dir = "/var/lib/pipette-mgmt/auth"
"#;
        let config: Config = toml::from_str(toml)?;
        assert_eq!(config.evals_server_url, "http://evals:8000");
        assert_eq!(config.listen_addr, "127.0.0.1:9000");
        assert_eq!(
            config.storage.data_dir(),
            Some(&PathBuf::from("/var/lib/pipette-mgmt"))
        );
        Ok(())
    }

    #[test]
    fn test_parse_minimal_config_fails_without_auth_storage() {
        let toml = r#"evals_server_url = "http://evals:8000""#;
        let result: Result<Config, _> = toml::from_str(toml);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_minimal_config() -> anyhow::Result<()> {
        let config: Config = toml::from_str(&minimal_toml(""))?;
        assert_eq!(config.evals_server_url, "http://evals:8000");
        assert_eq!(config.listen_addr, "0.0.0.0:3000");
        assert_eq!(config.catalog_ttl_secs, 180);
        assert_eq!(config.warehouse_read_days, 14);
        assert_eq!(config.warehouse_max_rows_per_part, 1_000);
        assert_eq!(config.http_timeout_secs, 600);
        assert_eq!(config.http_connect_timeout_secs.get(), 3);
        assert_eq!(config.parquet_zstd_level.compression_level(), 3);
        assert_eq!(config.score_chunk_size.get(), 50);
        assert_eq!(config.list_default_limit.get(), 50);
        assert_eq!(config.list_max_limit.get(), 200);
        assert_eq!(config.mutate_lock_ttl_secs.get(), 1800);
        assert_eq!(config.hf_import_endpoint, "https://huggingface.co");
        assert_eq!(config.hf_import_max_concurrent.get(), 2);
        assert_eq!(config.hf_max_concurrent_requests.get(), 8);
        assert_eq!(config.storage.data_dir(), Some(&PathBuf::from("./data")));
        assert!(!config.unverified_submissions.enabled);
        // Fail-closed by default: an unconfigured deployment probes its backend.
        assert!(config.probe_backend_capabilities);
        Ok(())
    }

    #[test]
    fn test_unverified_submissions_defaults_disabled() -> anyhow::Result<()> {
        let config: Config = toml::from_str(&minimal_toml(""))?;
        assert!(
            !config.unverified_submissions.enabled,
            "unverified submissions must be opt-in (off by default)"
        );
        Ok(())
    }

    #[test]
    fn test_unverified_submissions_enabled() -> anyhow::Result<()> {
        let toml = minimal_toml("") + "\n[unverified_submissions]\nenabled = true\n";
        let config: Config = toml::from_str(&toml)?;
        assert!(config.unverified_submissions.enabled);
        Ok(())
    }

    #[test]
    fn test_auto_approve_defaults_empty_and_approves_nothing() -> anyhow::Result<()> {
        let config: Config = toml::from_str(&minimal_toml(""))?;
        assert!(config.auto_approve.emails.is_empty());
        assert!(config.auto_approve.domains.is_empty());
        assert!(!config.auto_approve.approves("anyone@example.org"));
        Ok(())
    }

    #[test]
    fn test_auto_approve_parse_and_match() -> anyhow::Result<()> {
        let toml = minimal_toml("")
            + r#"
[auto_approve]
emails = ["alice@example.com"]
domains = ["example.org"]
"#;
        let config: Config = toml::from_str(&toml)?;
        let aa = &config.auto_approve;

        // Exact email, case-insensitive.
        assert!(aa.approves("alice@example.com"));
        assert!(aa.approves("Alice@Example.COM"));
        assert!(aa.approves("  alice@example.com  "));
        // A different address on the same (non-allowed) domain is rejected.
        assert!(!aa.approves("bob@example.com"));

        // Domain rule, case-insensitive on the part after `@`.
        assert!(aa.approves("anyone@example.org"));
        assert!(aa.approves("Anyone@Example.ORG"));
        assert!(!aa.approves("anyone@notexample.org"));
        assert!(!aa.approves("anyone@evil.com"));
        Ok(())
    }

    #[test]
    fn test_auto_approve_tolerates_leading_at_on_domain() {
        let aa = AutoApproveConfig {
            emails: vec![],
            domains: vec!["@example.org".to_string()],
        };
        assert!(aa.approves("anyone@example.org"));
    }

    #[test]
    fn test_auto_approve_no_at_never_matches_domain() {
        let aa = AutoApproveConfig {
            emails: vec![],
            domains: vec!["example.org".to_string()],
        };
        // Not a real email shape — no domain to compare, so no match.
        assert!(!aa.approves("example.org"));
    }

    #[test]
    fn test_parse_s3_config() -> anyhow::Result<()> {
        let toml = r#"
evals_server_url = "http://evals:8000"

[storage]
backend = "s3"
bucket = "my-bucket"
prefix = "v1/"
region = "us-west-2"
endpoint = "https://s3.custom.example.com"

[auth_storage]
backend = "s3"
bucket = "my-auth-bucket"
"#;
        let config: Config = toml::from_str(toml)?;
        let StorageConfig::S3 {
            bucket,
            prefix,
            region,
            endpoint,
            max_concurrent_requests,
            read_timeout_secs,
        } = &config.storage
        else {
            anyhow::bail!("expected S3 config");
        };
        assert_eq!(bucket, "my-bucket");
        assert_eq!(prefix, "v1/");
        assert_eq!(region.as_deref(), Some("us-west-2"));
        assert_eq!(endpoint.as_deref(), Some("https://s3.custom.example.com"));
        assert_eq!(*max_concurrent_requests, 32);
        assert_eq!(*read_timeout_secs, None);
        Ok(())
    }

    /// `queue_maintenance_read_timeout_secs` accepts a positive number of
    /// seconds. It rejects 0, because 0 fails every request. Only
    /// `for_queue_maintenance` copies it into the S3 sections.
    #[rstest]
    #[case::set("queue_maintenance_read_timeout_secs = 5", Some(Some(5)))]
    #[case::unset("", Some(None))]
    #[case::zero("queue_maintenance_read_timeout_secs = 0", None)]
    fn queue_maintenance_read_timeout_secs_applies_to_maintenance(
        #[case] line: &str,
        #[case] expected: Option<Option<u64>>,
    ) -> anyhow::Result<()> {
        let toml = format!(
            "evals_server_url = \"http://evals:8000\"\n{line}\n\n\
             [storage]\nbackend = \"s3\"\nbucket = \"data\"\n\n\
             [auth_storage]\nbackend = \"s3\"\nbucket = \"auth\"\n"
        );
        let Some(expected) = expected else {
            assert!(toml::from_str::<Config>(&toml).is_err());
            return Ok(());
        };
        let config: Config = toml::from_str(&toml)?;
        let timeouts = |config: &Config| {
            [&config.storage, &config.auth_storage].map(|storage| match storage {
                StorageConfig::S3 {
                    read_timeout_secs, ..
                } => read_timeout_secs.map(NonZeroU64::get),
                StorageConfig::LocalFs { .. } => None,
            })
        };
        assert_eq!(timeouts(&config), [None, None], "only maintenance sets it");
        assert_eq!(timeouts(&config.for_queue_maintenance()), [expected; 2]);
        Ok(())
    }

    /// The `todo/` queue's timeouts default to 5 s and 20 s and take any
    /// positive number of seconds. Each rejects 0, because 0 fails every request.
    #[rstest]
    #[case::defaults("", Some((5, 20)))]
    #[case::set(
        "todo_attempt_timeout_secs = 2\ntodo_operation_timeout_secs = 9",
        Some((2, 9))
    )]
    #[case::zero_attempt("todo_attempt_timeout_secs = 0", None)]
    #[case::zero_operation("todo_operation_timeout_secs = 0", None)]
    fn todo_timeouts_default_and_refuse_zero(
        #[case] lines: &str,
        #[case] expected: Option<(u64, u64)>,
    ) -> anyhow::Result<()> {
        let parsed = toml::from_str::<Config>(&minimal_toml(lines));
        let Some(expected) = expected else {
            assert!(parsed.is_err());
            return Ok(());
        };
        let config = parsed?;
        assert_eq!(
            (
                config.todo_attempt_timeout_secs.get(),
                config.todo_operation_timeout_secs.get()
            ),
            expected
        );
        Ok(())
    }

    /// `http_connect_timeout_secs` takes any positive number of seconds and
    /// rejects 0, because 0 fails every connection.
    #[rstest]
    #[case::set("http_connect_timeout_secs = 7", Some(7))]
    #[case::zero("http_connect_timeout_secs = 0", None)]
    fn http_connect_timeout_refuses_zero(
        #[case] line: &str,
        #[case] expected: Option<u64>,
    ) -> anyhow::Result<()> {
        let parsed = toml::from_str::<Config>(&minimal_toml(line));
        let Some(expected) = expected else {
            assert!(parsed.is_err());
            return Ok(());
        };
        assert_eq!(parsed?.http_connect_timeout_secs.get(), expected);
        Ok(())
    }

    #[test]
    fn test_s3_config_missing_bucket_fails() {
        let toml = r#"
evals_server_url = "http://evals:8000"

[storage]
backend = "s3"

[auth_storage]
backend = "s3"
bucket = "auth"
"#;
        let result: Result<Config, _> = toml::from_str(toml);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_with_auth_storage() -> anyhow::Result<()> {
        let toml = r#"
evals_server_url = "http://evals:8000"

[storage]
backend = "s3"
bucket = "data-bucket"

[auth_storage]
backend = "s3"
bucket = "auth-bucket"
"#;
        let config: Config = toml::from_str(toml)?;
        let StorageConfig::S3 { bucket, .. } = &config.auth_storage else {
            anyhow::bail!("expected S3 auth_storage");
        };
        assert_eq!(bucket, "auth-bucket");
        Ok(())
    }

    #[test]
    fn test_missing_auth_storage_fails() {
        let toml = r#"
evals_server_url = "http://evals:8000"

[storage]
backend = "s3"
bucket = "data-bucket"
"#;
        let result: Result<Config, _> = toml::from_str(toml);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_parquet_zstd_level_override() -> anyhow::Result<()> {
        let config: Config = toml::from_str(&minimal_toml("parquet_zstd_level = 9"))?;
        assert_eq!(config.parquet_zstd_level.compression_level(), 9);
        assert_eq!(config.writer_opts().zstd_level.compression_level(), 9);
        Ok(())
    }

    #[test]
    fn test_writer_opts_carries_configured_level() -> anyhow::Result<()> {
        let config: Config = toml::from_str(&minimal_toml("parquet_zstd_level = 9"))?;
        assert_eq!(config.writer_opts().zstd_level.compression_level(), 9);
        Ok(())
    }

    // `0` and negatives pass parquet 60's own check, so only ours rejects them.
    #[rstest]
    #[case::zero(0)]
    #[case::negative(-1)]
    #[case::above_max(23)]
    fn test_parquet_zstd_level_rejected_at_parse(#[case] level: i32) -> anyhow::Result<()> {
        let err = toml::from_str::<Config>(&minimal_toml(&format!("parquet_zstd_level = {level}")))
            .err()
            .context("invalid parquet_zstd_level must reject the whole config")?;
        let msg = err.to_string();
        assert!(
            msg.contains(&format!("parquet_zstd_level {level}")),
            "got: {msg}"
        );
        Ok(())
    }

    #[test]
    fn test_missing_required_field() {
        let toml = r#"listen_addr = "0.0.0.0:3000""#;
        let result: Result<Config, _> = toml::from_str(toml);
        assert!(result.is_err());
    }

    #[test]
    fn test_todo_storage_defaults_to_none_and_falls_back_to_storage() -> anyhow::Result<()> {
        let toml = r#"
evals_server_url = "http://evals:8000"

[storage]
backend = "s3"
bucket = "data-bucket"

[auth_storage]
backend = "local_fs"
"#;
        let config: Config = toml::from_str(toml)?;
        assert!(config.todo_storage.is_none());
        // todo_storage() must return the [storage] config when unset
        let StorageConfig::S3 { bucket, .. } = config.todo_storage() else {
            anyhow::bail!("expected S3 config from fallback");
        };
        assert_eq!(bucket, "data-bucket");
        Ok(())
    }

    #[test]
    fn test_todo_storage_explicit() -> anyhow::Result<()> {
        let toml = r#"
evals_server_url = "http://evals:8000"

[storage]
backend = "s3"
bucket = "data-bucket"

[auth_storage]
backend = "local_fs"

[todo_storage]
backend = "s3"
bucket = "todo-bucket--use1-az4--x-s3"
region = "us-east-1"
"#;
        let config: Config = toml::from_str(toml)?;
        assert!(config.todo_storage.is_some());
        let StorageConfig::S3 { bucket, .. } = config.todo_storage() else {
            anyhow::bail!("expected S3 todo_storage");
        };
        assert_eq!(bucket, "todo-bucket--use1-az4--x-s3");
        Ok(())
    }

    #[test]
    fn test_score_chunk_size_zero_rejected() {
        let result: Result<Config, _> = toml::from_str(&minimal_toml("score_chunk_size = 0"));
        assert!(
            result.is_err(),
            "score_chunk_size = 0 must be rejected at parse time"
        );
    }

    #[test]
    fn test_mutate_lock_ttl_secs_zero_rejected() {
        let result: Result<Config, _> = toml::from_str(&minimal_toml("mutate_lock_ttl_secs = 0"));
        assert!(
            result.is_err(),
            "mutate_lock_ttl_secs = 0 must be rejected at parse time — a zero lease \
             would make every lock instantly stale and defeat the mutex"
        );
    }
}
