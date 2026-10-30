//! The server under test: its config, its state, and the on-disk fixtures it
//! reads at startup.

use std::num::NonZeroU64;
use std::time::Duration;

use anyhow::Context;
use axum::Router;

use pipette_core::config::{Config, DuckDbConfig, DuckDbPassword};
use pipette_pipeline::model_params::{self, ModelParamTable};
use pipette_pipeline::plan_results::ResultsDb;
use pipette_plan_types::OrgId;
use pipette_store_fs::build_local_fs_stores;
use pipette_store_sql::scratch::TestDb;

use pipette_httpapi::state::AppState;

/// The HuggingFace endpoint of [`test_config`]: a closed local port, so a test
/// that files a gated model reaches no real Hub.
pub const NO_HUB: &str = "http://127.0.0.1:9/";

pub fn test_config(dir: &std::path::Path) -> Config {
    test_config_with_evals_url(dir, "http://localhost:9999")
}

pub fn test_config_with_evals_url(dir: &std::path::Path, evals_url: &str) -> Config {
    Config {
        evals_server_url: evals_url.to_string(),
        listen_addr: "127.0.0.1:0".to_string(),
        catalog_ttl_secs: 0,
        http_timeout_secs: 10,
        storage: pipette_core::config::StorageConfig::local_fs(dir.to_path_buf()),
        auth_storage: pipette_core::config::StorageConfig::local_fs(dir.to_path_buf()),
        hf_import_endpoint: NO_HUB.to_string(),
        authorities: vec![crate::user_auth::TEST_AUTHORITY.to_string()],
        ..Config::default()
    }
}

/// The served route table, so a test exercises the routes and body limits the
/// binary serves rather than a second copy of them. Each `/api/v1` answer is
/// held to the OpenAPI document on its way out (`crate::contract`).
pub fn test_app(state: AppState) -> Router {
    let openapi = std::sync::Arc::clone(&state.openapi);
    pipette_httpapi::app(state).route_layer(axum::middleware::from_fn_with_state(
        openapi,
        crate::contract::conform,
    ))
}

/// Write the model-params rows the scoring tests expect, including one MoE row
/// to exercise the `total != active` path.
pub async fn setup_models(dir: &std::path::Path) -> anyhow::Result<()> {
    let rows = ModelParamTable::from_toml(
        r#""LFM2-700M" = 700
"llama-3.2-1b" = 1000
"LFM2-8B-A1B" = { total = 8340, active = 1500 }
"LFM2.5-8B-A1B" = { total = 8340, active = 1500 }
"#,
    )?;
    let stores = build_local_fs_stores(&test_config(dir))?;
    model_params::import(&*stores.model_params, &rows, false).await?;
    Ok(())
}

pub fn setup_benchmarks(dir: &std::path::Path) -> anyhow::Result<()> {
    let bm_dir = dir.join("benchmarks");
    std::fs::create_dir_all(&bm_dir)?;
    std::fs::write(
        bm_dir.join("prefill_throughput_256.toml"),
        r#"benchmark_type = "prefill_throughput"
parameter_prefill_tokens = 256"#,
    )?;
    std::fs::write(
        bm_dir.join("max_memory_usage_256.toml"),
        r#"benchmark_type = "max_memory_usage"
parameter_prefill_tokens = 256"#,
    )?;
    std::fs::write(
        bm_dir.join("vl_throughput_384x384_32_64.toml"),
        r#"benchmark_type = "vl_throughput"
parameter_image_width = 384
parameter_image_height = 384
parameter_text_tokens = 32
parameter_decode_tokens = 64"#,
    )?;
    std::fs::write(
        bm_dir.join("eval_test.toml"),
        r#"benchmark_type = "eval"
parameter_eval_id = "test_eval"
parameter_dataset_name = "test_ds"
parameter_max_tokens = 100"#,
    )?;
    Ok(())
}

/// Write the two benchmarks that the plans in `examples/plans/scheduler/`
/// name, so a test can post one of those documents as it is. Call this before
/// [`make_state`], as with [`setup_benchmarks`].
pub fn setup_scheduler_benchmarks(dir: &std::path::Path) -> anyhow::Result<()> {
    let bm_dir = dir.join("benchmarks");
    std::fs::create_dir_all(&bm_dir)?;
    std::fs::write(
        bm_dir.join("decode_throughput_512_100.toml"),
        r#"benchmark_type = "decode_throughput"
parameter_prefill_tokens = 512
parameter_decode_tokens = 100"#,
    )?;
    std::fs::write(
        bm_dir.join("end_to_end_latency_512_256.toml"),
        r#"benchmark_type = "end_to_end_latency"
parameter_prefill_tokens = 512
parameter_decode_tokens = 256"#,
    )?;
    Ok(())
}

/// An eval definition as the TOML a catalog file holds.
pub const ACME_EVAL_TOML: &str = "benchmark_type = \"eval\"\nparameter_eval_id = \"acme_internal\"\n\
                                  parameter_dataset_name = \"v3\"\nparameter_max_tokens = 4096\n";

/// Write `body` as `org_id`'s entry `benchmark_id`, straight into its catalog
/// directory (`org-benchmarks/{org_id}/`) rather than through a write's checks.
pub fn write_private_entry(
    dir: &std::path::Path,
    org_id: &OrgId,
    benchmark_id: &str,
    body: &str,
) -> anyhow::Result<()> {
    let org_dir = dir.join("org-benchmarks").join(org_id.as_str());
    std::fs::create_dir_all(&org_dir)?;
    std::fs::write(org_dir.join(format!("{benchmark_id}.toml")), body)?;
    Ok(())
}

/// Write one eval entry into `org_id`'s own catalog directory, the way
/// `orgs benchmarks put` would. An eval, so the unreachable evals server the
/// tests point at shows whether the lookup ran before the samples proxy.
pub fn setup_private_benchmark(
    dir: &std::path::Path,
    benchmark_id: &str,
    org_id: &OrgId,
) -> anyhow::Result<()> {
    write_private_entry(
        dir,
        org_id,
        benchmark_id,
        r#"benchmark_type = "eval"
parameter_eval_id = "private_eval"
parameter_dataset_name = "private_ds"
parameter_max_tokens = 100"#,
    )
}

/// A state with the benchmark catalog seeded, for a test that needs no device:
/// a plan cannot be ingested without a catalog on disk.
pub async fn seeded_state(dir: &std::path::Path) -> anyhow::Result<AppState> {
    setup_benchmarks(dir)?;
    make_state(dir).await
}

/// A [`seeded_state`] whose record stores use a new PostgreSQL database, as a
/// `[database]` deployment does. The database drops when the value drops.
pub struct SqlState {
    pub state: AppState,
    _db: TestDb,
}

/// Use this function to test a feature that only the SQL store keeps. It
/// returns `None` if `TEST_DATABASE_URL` is not set and the run is not in CI.
/// The test then skips.
pub async fn seeded_sql_state(dir: &std::path::Path) -> anyhow::Result<Option<SqlState>> {
    seeded_sql_state_from(dir, test_config(dir)).await
}

/// A [`seeded_sql_state`] that reads Hugging Face at `endpoint`, for the tests
/// against a mock Hub.
pub async fn seeded_sql_state_with_hf_endpoint(
    dir: &std::path::Path,
    endpoint: &str,
) -> anyhow::Result<Option<SqlState>> {
    let mut config = test_config(dir);
    config.hf_import_endpoint = endpoint.to_string();
    seeded_sql_state_from(dir, config).await
}

async fn seeded_sql_state_from(
    dir: &std::path::Path,
    config: Config,
) -> anyhow::Result<Option<SqlState>> {
    let Some(db) = TestDb::new().await? else {
        return Ok(None);
    };
    setup_benchmarks(dir)?;
    let stores = pipette_store_sql::overlay(db.db(), build_local_fs_stores(&config)?);
    let state = AppState::new(config, stores, test_http_client()?).await?;
    Ok(Some(SqlState { state, _db: db }))
}

/// A client without the system proxy lookup, which on macOS lists the test
/// binary's directory, `target/debug/deps`, and costs each test about a second.
pub fn test_http_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder().no_proxy().build()
}

pub async fn make_state(dir: &std::path::Path) -> anyhow::Result<AppState> {
    make_state_with_evals_url(dir, "http://localhost:9999").await
}

/// Build an [`AppState`] over local_fs stores. Every `make_state_*` delegates
/// here, so a new store or `AppState` field is wired in one place.
pub async fn make_state_from_config(config: Config) -> anyhow::Result<AppState> {
    let stores = build_local_fs_stores(&config)?;
    AppState::new(config, stores, test_http_client()?).await
}

/// Build an [`AppState`] with `accept_legacy_signatures` toggled.
pub async fn make_state_with_legacy_signatures(
    dir: &std::path::Path,
    accept: bool,
) -> anyhow::Result<AppState> {
    let mut config = test_config(dir);
    config.accept_legacy_signatures = accept;
    make_state_from_config(config).await
}

/// Build an [`AppState`] with `[unverified_submissions] enabled` toggled.
pub async fn make_state_with_unverified(
    dir: &std::path::Path,
    enabled: bool,
) -> anyhow::Result<AppState> {
    let mut config = test_config(dir);
    config.unverified_submissions.enabled = enabled;
    make_state_from_config(config).await
}

/// Build an [`AppState`] with `[auto_approve]` rules configured.
pub async fn make_state_with_auto_approve(
    dir: &std::path::Path,
    emails: Vec<String>,
    domains: Vec<String>,
) -> anyhow::Result<AppState> {
    let mut config = test_config(dir);
    config.auto_approve = pipette_core::config::AutoApproveConfig { emails, domains };
    make_state_from_config(config).await
}

/// Build an [`AppState`] whose router-wide body ceiling is `limit` bytes.
pub async fn make_state_with_default_body_limit(
    dir: &std::path::Path,
    limit: std::num::NonZeroUsize,
) -> anyhow::Result<AppState> {
    let mut config = test_config(dir);
    config.default_body_limit_bytes = limit;
    make_state_from_config(config).await
}

/// Build an [`AppState`] whose plan body ceiling is `limit` bytes, so a test
/// can exceed it without building a 128 MB body.
pub async fn make_state_with_plan_body_limit(
    dir: &std::path::Path,
    limit: std::num::NonZeroUsize,
) -> anyhow::Result<AppState> {
    let mut config = test_config(dir);
    config.plan_body_limit_bytes = limit;
    make_state_from_config(config).await
}

/// Build an [`AppState`] whose submission body ceiling is `limit` bytes, so a
/// test can exceed it without building a 500 MB body.
pub async fn make_state_with_submission_body_limit(
    dir: &std::path::Path,
    limit: std::num::NonZeroUsize,
) -> anyhow::Result<AppState> {
    let mut config = test_config(dir);
    config.submission_body_limit_bytes = limit;
    make_state_from_config(config).await
}

/// Build an [`AppState`] whose vault files may be at most `limit` bytes, for
/// the upload-ceiling tests, which could not otherwise exceed 128 GiB.
pub async fn make_state_with_vault_blob_limit(
    dir: &std::path::Path,
    limit: std::num::NonZeroU64,
) -> anyhow::Result<AppState> {
    let mut config = test_config(dir);
    config.vault_blob_limit_bytes = limit;
    make_state_from_config(config).await
}

/// Build an [`AppState`] that imports gated models from the Hub at `endpoint`,
/// for the import tests against a mock Hub.
pub async fn make_state_with_hf_endpoint(
    dir: &std::path::Path,
    endpoint: &str,
) -> anyhow::Result<AppState> {
    let mut config = test_config(dir);
    config.hf_import_endpoint = endpoint.to_string();
    make_state_from_config(config).await
}

/// Build an [`AppState`] whose plans may declare at most `limit` jobs, so a
/// test can exceed it with a handful of jobs.
pub async fn make_state_with_plan_max_jobs(
    dir: &std::path::Path,
    limit: std::num::NonZeroUsize,
) -> anyhow::Result<AppState> {
    let mut config = test_config(dir);
    config.plan_max_jobs = limit;
    make_state_from_config(config).await
}

/// Build an [`AppState`] with `require_client_preauth_key` toggled on, for the
/// client pre-auth registration tests.
pub async fn make_state_require_client_preauth(dir: &std::path::Path) -> anyhow::Result<AppState> {
    let mut config = test_config(dir);
    config.require_client_preauth_key = true;
    make_state_from_config(config).await
}

/// Build an [`AppState`] with `require_user_preauth_key` toggled on, for the
/// tests that close the keyless request for an account.
pub async fn make_state_require_user_preauth(dir: &std::path::Path) -> anyhow::Result<AppState> {
    let mut config = test_config(dir);
    config.require_user_preauth_key = true;
    make_state_from_config(config).await
}

pub async fn make_state_with_evals_url(
    dir: &std::path::Path,
    evals_url: &str,
) -> anyhow::Result<AppState> {
    make_state_from_config(test_config_with_evals_url(dir, evals_url)).await
}

/// The Basic header [`with_duckdb`]'s `mgmt:secret` credential sends.
pub const DUCKDB_BASIC_AUTH: &str = "Basic bWdtdDpzZWNyZXQ=";

/// [`with_duckdb`]'s `[duckdb] timeout_secs`.
pub const DUCKDB_TIMEOUT: Duration = Duration::from_secs(5);

/// `state` reading plan results from a pipette-duckdb at `url`, as
/// `mgmt:secret` against the `internal` database.
pub fn with_duckdb(mut state: AppState, url: &str) -> anyhow::Result<AppState> {
    state.results_db = Some(ResultsDb::new(
        state.http_client.clone(),
        DuckDbConfig {
            url: url.to_string(),
            user: "mgmt".to_string(),
            password: DuckDbPassword::new("secret"),
            database: "internal".to_string(),
            timeout_secs: NonZeroU64::new(DUCKDB_TIMEOUT.as_secs()).context("non-zero")?,
        },
    )?);
    Ok(state)
}

/// Runs `test` as `#[tokio::test]` would, for an `#[rstest]` case over a mock
/// pipette-duckdb: rstest keeps the async fn, which httpmock's `Rc` makes not `Send`.
pub fn run_mocked(test: impl Future<Output = anyhow::Result<()>>) -> anyhow::Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("test runtime")?
        .block_on(test)
}
