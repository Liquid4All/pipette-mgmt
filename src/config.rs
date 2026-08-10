use std::num::{NonZeroU64, NonZeroUsize};
use std::path::PathBuf;

use parquet::basic::ZstdLevel;
use serde::{Deserialize, Deserializer};

use crate::parquet_utils::WriterOpts;

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "backend", rename_all = "snake_case")]
pub enum StorageConfig {
    LocalFs {
        #[serde(default = "default_data_dir")]
        data_dir: PathBuf,
    },
    S3 {
        bucket: String,
        #[serde(default)]
        prefix: String,
        /// AWS region. Also reads `AWS_REGION` / `AWS_DEFAULT_REGION` env vars.
        region: Option<String>,
        /// Custom S3-compatible endpoint (e.g. MinIO, Cloudflare R2).
        endpoint: Option<String>,
        /// Cap on concurrent S3 requests issued by any fan-out operation
        /// (today: lookup of jobs by `client_id`+`job_id`). Defaults to 32
        /// — well under per-prefix rate limits and the local connection
        /// pool, large enough to keep latency low for realistic benchmark
        /// counts. Tune up for very high parallelism, down for
        /// rate-limited or constrained environments.
        #[serde(default = "default_max_concurrent_requests")]
        max_concurrent_requests: usize,
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

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub evals_server_url: String,
    #[serde(default = "default_listen_addr")]
    pub listen_addr: String,
    /// How many seconds to cache the benchmark catalog before re-reading from
    /// disk. Defaults to 180.
    #[serde(default = "default_catalog_ttl_secs")]
    pub catalog_ttl_secs: u64,
    /// How many days back a `GET /jobs/{job_id}` metrics lookup scans,
    /// across both new `day=` partitions and legacy `month=` partitions
    /// whose range overlaps the window. This is a hard cap: a job scored
    /// longer ago than this is reported without metrics (`metrics: null`)
    /// rather than scanned for — its rows remain in the warehouse for bulk
    /// queries. Defaults to 14.
    #[serde(default = "default_warehouse_read_days")]
    pub warehouse_read_days: u32,
    /// Maximum rows per Parquet part file in a warehouse partition. Writes
    /// append to the tail part and roll to a new one at this size, so it also
    /// bounds the rows read+rewritten per write. Defaults to 1000.
    #[serde(default = "default_warehouse_max_rows_per_part")]
    pub warehouse_max_rows_per_part: usize,
    /// Outbound HTTP client request timeout in seconds — the scoring/evals
    /// server calls, not inbound requests. Defaults to 600, sized for a slow
    /// scorer rather than for interactive latency.
    #[serde(default = "default_http_timeout_secs")]
    pub http_timeout_secs: u64,
    /// zstd compression level for Parquet writes. Valid range 1..=22.
    /// Defaults to 3 (zstd's general-purpose default — strong ratio at low CPU cost).
    /// Stored as a `ZstdLevel` so the type itself carries the
    /// "validated" invariant — every consumer can use it directly
    /// without re-checking the bound.
    #[serde(
        default = "default_parquet_zstd_level",
        deserialize_with = "deserialize_parquet_zstd_level"
    )]
    pub parquet_zstd_level: ZstdLevel,
    /// Number of incoming submissions listed per scoring iteration.
    /// `run_score` drains the entire backlog in chunks of this size, so the
    /// invocation handles all pending submissions; the chunk size only
    /// bounds per-iteration LIST cost on S3 (`ceil(score_chunk_size / 1000)`
    /// LIST requests per iteration). Defaults to 50.
    #[serde(default = "default_score_chunk_size")]
    pub score_chunk_size: NonZeroUsize,
    /// Lease duration, in seconds, for the storage mutate lock — the
    /// advisory lock that serializes `process-submissions`, the `fix-*`
    /// commands, and `requeue-eval`
    /// so they never interleave read-modify-write on the same Parquet
    /// partitions or submission bodies. Applies to the `s3` backend
    /// only; on `local_fs` the kernel manages lock lifetime via
    /// `flock(2)`. An `s3` command that crashes leaves the lease
    /// object behind; the next command run past this many seconds
    /// treats the lease as stale and takes it over. Set it comfortably
    /// above the longest expected `score` / `fix-*` run — a run that
    /// outlives its lease can have the lock taken over mid-write.
    /// Must be non-zero. Defaults to 1800 (30 minutes).
    #[serde(default = "default_mutate_lock_ttl_secs")]
    pub mutate_lock_ttl_secs: NonZeroU64,
    #[serde(default)]
    pub storage: StorageConfig,
    /// Storage backend for client identities (keys, registration data).
    /// Must be configured explicitly to ensure auth data is stored securely.
    pub auth_storage: StorageConfig,
    /// Path to the model params mapping TOML file. When unset, defaults to
    /// `model_params_mapping.toml` at the storage backend root —
    /// `{data_dir}/model_params_mapping.toml` for `local_fs`,
    /// `{prefix}/model_params_mapping.toml` for `s3`. When set, the value is
    /// used as-is: a filesystem path for `local_fs` (relative paths are
    /// resolved against the process cwd, not `data_dir`) and an object key
    /// for `s3` (the `prefix` is not prepended).
    #[serde(default)]
    pub model_params_mapping_path: Option<PathBuf>,
    /// Unverified-submission archive settings. Off by default — when
    /// disabled, unauthenticated `POST /benchmarks` requests are
    /// rejected with `401`. See `docs/storage.md` §4.1.
    #[serde(default)]
    pub unverified_submissions: UnverifiedSubmissionsConfig,
    /// Auto-approve rules matched against a client's `contact_email` at
    /// registration. Off by default. NOT a security control — email is
    /// self-reported and unverified. See `docs/authentication.md` §3.1.
    #[serde(default)]
    pub auto_approve: AutoApproveConfig,
    /// When `true`, `POST /clients/register` requires a valid pre-auth key —
    /// keyless registrations are rejected with `403`. Off by default, so
    /// registration stays open (governed only by `auto_approve`). See
    /// `docs/authentication.md` §6.
    #[serde(default)]
    pub require_preauth_key: bool,
    /// When `true`, a signature over the bare `X-Timestamp` value is accepted
    /// in addition to one over the `v1` signed payload. Every acceptance is
    /// logged at `warn` with the client id, so the log identifies which clients
    /// still sign the timestamp-only form — once those lines stop appearing,
    /// this can be set to `false`. A timestamp-only signature is replayable
    /// against any endpoint within the freshness window, so it is a migration
    /// aid rather than a supported mode. Defaults to `true`. See
    /// `docs/authentication.md` §2.3.
    #[serde(default = "default_accept_legacy_signatures")]
    pub accept_legacy_signatures: bool,
    /// Storage backend for the `todo/` job-queue tree. Must be an S3 Express
    /// One Zone bucket in production — the queue relies on atomic
    /// `RenameObject`, which is only available on Express One Zone. Defaults
    /// to `[storage]` when omitted, which is acceptable for `local_fs` /
    /// development use only. See `docs/storage.md §9`.
    #[serde(default)]
    pub todo_storage: Option<StorageConfig>,
    /// Lease duration, in seconds, granted on a successful
    /// `POST /plans/claim` and extended by each
    /// `PUT /plans/{job_id}/heartbeat`. Clients heartbeat at half this
    /// interval. Must be non-zero. Defaults to 300 (5 minutes).
    #[serde(default = "default_plan_lease_duration_secs")]
    pub plan_lease_duration_secs: NonZeroU64,
    /// Age, in seconds, past which `queue-maintenance` deletes a partial job
    /// file under `todo/tmp/` (left behind by a crashed planner). Set it
    /// comfortably above the planner's longest write-then-rename window. On
    /// S3, a lifecycle rule on the `todo/tmp/` prefix can substitute for this
    /// pass — see `docs/operations.md` §3.1. Must be non-zero. Defaults to
    /// 86400 (24 hours).
    #[serde(default = "default_todo_tmp_max_age_secs")]
    pub todo_tmp_max_age_secs: NonZeroU64,
}

/// Email allow rules that approve a client at registration. See
/// `docs/authentication.md` §3.1.
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

/// Config for the write-only unverified submission archive. When
/// `enabled`, `POST /benchmarks` and `POST /benchmarks/batch` accept
/// requests with all three auth headers absent and route them to
/// `submissions/unverified/`. See `docs/storage.md` §4.1 and
/// `docs/httpapi.md` §2.7.3.
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

fn default_parquet_zstd_level() -> ZstdLevel {
    // 3 is zstd's general-purpose default and well inside the 1..=22
    // range accepted by `ZstdLevel::try_new`, so the unwrap cannot fire.
    ZstdLevel::try_new(3).expect("3 is in valid zstd range 1..=22")
}

/// Validate `parquet_zstd_level` at config-parse time by building the
/// typed `ZstdLevel` directly. Invalid values reject the whole config
/// at load instead of failing lazily on the first warehouse write.
fn deserialize_parquet_zstd_level<'de, D: Deserializer<'de>>(d: D) -> Result<ZstdLevel, D::Error> {
    let raw = i32::deserialize(d)?;
    ZstdLevel::try_new(raw)
        .map_err(|e| serde::de::Error::custom(format!("invalid parquet_zstd_level {raw}: {e}")))
}

fn default_score_chunk_size() -> NonZeroUsize {
    NonZeroUsize::new(50).expect("50 is non-zero")
}

fn default_mutate_lock_ttl_secs() -> NonZeroU64 {
    NonZeroU64::new(1800).expect("1800 is non-zero")
}

fn default_plan_lease_duration_secs() -> NonZeroU64 {
    NonZeroU64::new(300).expect("300 is non-zero")
}

fn default_todo_tmp_max_age_secs() -> NonZeroU64 {
    NonZeroU64::new(86_400).expect("86400 is non-zero")
}

fn default_accept_legacy_signatures() -> bool {
    true
}

fn default_listen_addr() -> String {
    "0.0.0.0:3000".to_string()
}

impl Default for Config {
    /// Convenience for test fixtures and the `..Default::default()` shorthand
    /// at struct-literal sites. `evals_server_url` has no natural default
    /// (it's required at runtime), so it's empty here — a `Config` built
    /// from `Default` is *not* directly usable as a runtime config.
    fn default() -> Self {
        Self {
            evals_server_url: String::new(),
            listen_addr: default_listen_addr(),
            catalog_ttl_secs: default_catalog_ttl_secs(),
            warehouse_read_days: default_warehouse_read_days(),
            warehouse_max_rows_per_part: default_warehouse_max_rows_per_part(),
            http_timeout_secs: default_http_timeout_secs(),
            parquet_zstd_level: default_parquet_zstd_level(),
            score_chunk_size: default_score_chunk_size(),
            mutate_lock_ttl_secs: default_mutate_lock_ttl_secs(),
            storage: StorageConfig::default(),
            auth_storage: StorageConfig::default(),
            model_params_mapping_path: None,
            unverified_submissions: UnverifiedSubmissionsConfig::default(),
            auto_approve: AutoApproveConfig::default(),
            require_preauth_key: false,
            accept_legacy_signatures: default_accept_legacy_signatures(),
            todo_storage: None,
            plan_lease_duration_secs: default_plan_lease_duration_secs(),
            todo_tmp_max_age_secs: default_todo_tmp_max_age_secs(),
        }
    }
}

impl Config {
    /// Resolves the effective storage config for the `todo/` tree.
    /// Falls back to `[storage]` when `[todo_storage]` is not set.
    ///
    /// The queue's claim/renew/recycle race safety depends on atomic
    /// `RenameObject`, which only S3 Express One Zone (directory) buckets
    /// provide; commands that touch `todo/` verify this at startup via
    /// `TodoStore::validate_backend`, so a fallback to a regular `[storage]`
    /// S3 bucket is rejected before it can corrupt the queue.
    pub fn todo_storage(&self) -> &StorageConfig {
        self.todo_storage.as_ref().unwrap_or(&self.storage)
    }

    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("failed to read config file {}: {e}", path.display()))?;
        let config: Config = toml::from_str(&content)
            .map_err(|e| anyhow::anyhow!("failed to parse config file: {e}"))?;
        config.validate()?;
        Ok(config)
    }

    /// Bounds the field types cannot express. Both duration fields are capped
    /// at 10 years — far beyond any real value, and far below where their
    /// `chrono` arithmetic breaks: an oversized `plan_lease_duration_secs`
    /// would panic in [`Config::lease_expiry_from`]'s date math, and an
    /// oversized `todo_tmp_max_age_secs` would fail `list_stale_tmp`'s
    /// `chrono::Duration` conversion — erroring every `queue-maintenance`
    /// run at its final pass instead of being rejected at load.
    fn validate(&self) -> anyhow::Result<()> {
        const MAX_DURATION_SECS: u64 = 10 * 365 * 24 * 60 * 60;
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
        Ok(())
    }

    /// A lease expiry `plan_lease_duration_secs` after `now` — the single
    /// definition of "a fresh lease", shared by claim, heartbeat, reclaim, and
    /// the submit path's claim verification.
    ///
    /// Infallible because [`Config::validate`] bounds the duration at load;
    /// a hand-built `Config` (tests) must respect the same bound.
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
    use crate::config::*;

    #[test]
    fn test_validate_bounds_durations() -> anyhow::Result<()> {
        let toml = |extra_line: &str| {
            format!(
                r#"
evals_server_url = "http://evals:8000"
{extra_line}

[storage]
backend = "local_fs"
data_dir = "/var/lib/pipette-mgmt"

[auth_storage]
backend = "local_fs"
data_dir = "/var/lib/pipette-mgmt/auth"
"#
            )
        };

        let config: Config = toml::from_str(&toml(""))?;
        assert!(config.validate().is_ok(), "default durations are valid");

        // Oversized values would break chrono arithmetic downstream (panic in
        // `lease_expiry_from`, error in `list_stale_tmp`); validation must
        // reject them at load instead.
        let config: Config = toml::from_str(&toml("plan_lease_duration_secs = 99999999999999"))?;
        assert!(config.validate().is_err());
        let config: Config = toml::from_str(&toml("todo_tmp_max_age_secs = 99999999999999"))?;
        assert!(config.validate().is_err());
        Ok(())
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
        let toml = r#"
evals_server_url = "http://evals:8000"

[auth_storage]
backend = "local_fs"
"#;
        let config: Config = toml::from_str(toml)?;
        assert_eq!(config.evals_server_url, "http://evals:8000");
        assert_eq!(config.listen_addr, "0.0.0.0:3000");
        assert_eq!(config.catalog_ttl_secs, 180);
        assert_eq!(config.warehouse_read_days, 14);
        assert_eq!(config.warehouse_max_rows_per_part, 1_000);
        assert_eq!(config.http_timeout_secs, 600);
        assert_eq!(config.parquet_zstd_level.compression_level(), 3);
        assert_eq!(config.score_chunk_size.get(), 50);
        assert_eq!(config.mutate_lock_ttl_secs.get(), 1800);
        assert_eq!(config.storage.data_dir(), Some(&PathBuf::from("./data")));
        assert!(!config.unverified_submissions.enabled);
        Ok(())
    }

    #[test]
    fn test_unverified_submissions_defaults_disabled() -> anyhow::Result<()> {
        let toml = r#"
evals_server_url = "http://evals:8000"

[auth_storage]
backend = "local_fs"
"#;
        let config: Config = toml::from_str(toml)?;
        assert!(
            !config.unverified_submissions.enabled,
            "unverified submissions must be opt-in (off by default)"
        );
        Ok(())
    }

    #[test]
    fn test_unverified_submissions_enabled() -> anyhow::Result<()> {
        let toml = r#"
evals_server_url = "http://evals:8000"

[auth_storage]
backend = "local_fs"

[unverified_submissions]
enabled = true
"#;
        let config: Config = toml::from_str(toml)?;
        assert!(config.unverified_submissions.enabled);
        Ok(())
    }

    #[test]
    fn test_auto_approve_defaults_empty_and_approves_nothing() -> anyhow::Result<()> {
        let toml = r#"
evals_server_url = "http://evals:8000"

[auth_storage]
backend = "local_fs"
"#;
        let config: Config = toml::from_str(toml)?;
        assert!(config.auto_approve.emails.is_empty());
        assert!(config.auto_approve.domains.is_empty());
        assert!(!config.auto_approve.approves("anyone@example.org"));
        Ok(())
    }

    #[test]
    fn test_auto_approve_parse_and_match() -> anyhow::Result<()> {
        let toml = r#"
evals_server_url = "http://evals:8000"

[auth_storage]
backend = "local_fs"

[auto_approve]
emails = ["alice@example.com"]
domains = ["example.org"]
"#;
        let config: Config = toml::from_str(toml)?;
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
        } = &config.storage
        else {
            anyhow::bail!("expected S3 config");
        };
        assert_eq!(bucket, "my-bucket");
        assert_eq!(prefix, "v1/");
        assert_eq!(region.as_deref(), Some("us-west-2"));
        assert_eq!(endpoint.as_deref(), Some("https://s3.custom.example.com"));
        assert_eq!(*max_concurrent_requests, 32);
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
        match &config.auth_storage {
            StorageConfig::S3 { bucket, .. } => assert_eq!(bucket, "auth-bucket"),
            _ => anyhow::bail!("expected S3 auth_storage"),
        }
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
        let toml = r#"
evals_server_url = "http://evals:8000"
parquet_zstd_level = 9

[auth_storage]
backend = "local_fs"
"#;
        let config: Config = toml::from_str(toml)?;
        assert_eq!(config.parquet_zstd_level.compression_level(), 9);
        assert_eq!(config.writer_opts().zstd_level.compression_level(), 9);
        Ok(())
    }

    #[test]
    fn test_writer_opts_carries_configured_level() -> anyhow::Result<()> {
        let config: Config = toml::from_str(
            r#"
evals_server_url = "http://evals:8000"
parquet_zstd_level = 9

[auth_storage]
backend = "local_fs"
"#,
        )?;
        assert_eq!(config.writer_opts().zstd_level.compression_level(), 9);
        Ok(())
    }

    #[test]
    fn test_parquet_zstd_level_rejected_at_parse() {
        // 0 is outside the 1..=22 valid range — parse must fail.
        let err = toml::from_str::<Config>(
            r#"
evals_server_url = "http://evals:8000"
parquet_zstd_level = 0

[auth_storage]
backend = "local_fs"
"#,
        )
        .expect_err("invalid parquet_zstd_level must reject the whole config");
        let msg = err.to_string();
        assert!(msg.contains("parquet_zstd_level 0"), "got: {msg}");
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
        match config.todo_storage() {
            StorageConfig::S3 { bucket, .. } => assert_eq!(bucket, "data-bucket"),
            _ => anyhow::bail!("expected S3 config from fallback"),
        }
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
        match config.todo_storage() {
            StorageConfig::S3 { bucket, .. } => {
                assert_eq!(bucket, "todo-bucket--use1-az4--x-s3")
            }
            _ => anyhow::bail!("expected S3 todo_storage"),
        }
        Ok(())
    }

    #[test]
    fn test_model_params_mapping_path_defaults_to_none() -> anyhow::Result<()> {
        let toml = r#"
evals_server_url = "http://evals:8000"

[auth_storage]
backend = "local_fs"
"#;
        let config: Config = toml::from_str(toml)?;
        assert!(config.model_params_mapping_path.is_none());
        Ok(())
    }

    #[test]
    fn test_model_params_mapping_path_override() -> anyhow::Result<()> {
        let toml = r#"
evals_server_url = "http://evals:8000"
model_params_mapping_path = "/etc/pipette/models.toml"

[auth_storage]
backend = "local_fs"
"#;
        let config: Config = toml::from_str(toml)?;
        assert_eq!(
            config.model_params_mapping_path.as_deref(),
            Some(std::path::Path::new("/etc/pipette/models.toml"))
        );
        Ok(())
    }

    #[test]
    fn test_score_chunk_size_zero_rejected() {
        let toml = r#"
evals_server_url = "http://evals:8000"
score_chunk_size = 0

[auth_storage]
backend = "local_fs"
"#;
        let result: Result<Config, _> = toml::from_str(toml);
        assert!(
            result.is_err(),
            "score_chunk_size = 0 must be rejected at parse time"
        );
    }

    #[test]
    fn test_mutate_lock_ttl_secs_zero_rejected() {
        let toml = r#"
evals_server_url = "http://evals:8000"
mutate_lock_ttl_secs = 0

[auth_storage]
backend = "local_fs"
"#;
        let result: Result<Config, _> = toml::from_str(toml);
        assert!(
            result.is_err(),
            "mutate_lock_ttl_secs = 0 must be rejected at parse time — a zero lease \
             would make every lock instantly stale and defeat the mutex"
        );
    }
}
