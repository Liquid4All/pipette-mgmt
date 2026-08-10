#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use axum::routing::{get, post};
use ed25519_dalek::{Signature, Signer, SigningKey};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use pipette_mgmt::config::Config;
use pipette_mgmt::handlers::AppState;
use pipette_mgmt::stores::{TodoStore, build_local_fs_stores};
use pipette_mgmt::todo_filename::avail_filename;
use pipette_mgmt::types::{ClientId, ExpiresAt, JobId};

/// Shared `NonZeroUsize` for `list_incoming` test calls — large enough to
/// return any test fixture's full backlog without paging.
pub const TEST_LIST_LIMIT: std::num::NonZeroUsize = std::num::NonZeroUsize::new(100).unwrap();

/// Build a validated [`JobId`] from a known-safe test literal. Integration
/// tests can't see the `#[cfg(test)]`-only `new_unchecked`, so fixtures go
/// through [`JobId::try_new`] via this helper. A tiny infallible test helper,
/// so `.expect` is fine here.
pub fn job(s: impl Into<String>) -> JobId {
    JobId::try_new(s).expect("valid test job id")
}

/// Write a job body straight into `todo/avail/{job_id}.{expires_at}.json`,
/// bypassing the server. The `TodoStore` trait has no `write_avail` (job
/// creation is a separate, not-yet-built concern), and the local_fs layout is
/// stable. Returns the `JobId`.
pub fn seed_avail(
    dir: &std::path::Path,
    job_id: &str,
    expires_at: ExpiresAt,
    body: &Value,
) -> anyhow::Result<JobId> {
    let job = job(job_id);
    let name = avail_filename(&job, expires_at);
    std::fs::write(
        dir.join("todo").join("avail").join(name),
        serde_json::to_vec(body)?,
    )?;
    Ok(job)
}

// ---------------------------------------------------------------------------
// Config & app builder
// ---------------------------------------------------------------------------

pub fn test_config(dir: &std::path::Path) -> Config {
    test_config_with_evals_url(dir, "http://localhost:9999")
}

pub fn test_config_with_evals_url(dir: &std::path::Path, evals_url: &str) -> Config {
    Config {
        evals_server_url: evals_url.to_string(),
        listen_addr: "127.0.0.1:0".to_string(),
        catalog_ttl_secs: 0,
        http_timeout_secs: 10,
        storage: pipette_mgmt::config::StorageConfig::local_fs(dir.to_path_buf()),
        auth_storage: pipette_mgmt::config::StorageConfig::local_fs(dir.to_path_buf()),
        ..Config::default()
    }
}

/// The served route table, so a test exercises the routes and body limits the
/// binary serves rather than a second copy of them.
pub fn test_app(state: AppState) -> Router {
    pipette_mgmt::router::app(state)
}

/// Write a `model_params_mapping.toml` into the tempdir with the entries our scoring
/// tests expect. Call this before [`make_state`] when a test needs the
/// scorer's `model_params_*_millions` lookup to resolve. Includes one MoE
/// row to exercise the `total != active` path.
pub fn setup_models(dir: &std::path::Path) -> anyhow::Result<()> {
    std::fs::write(
        dir.join("model_params_mapping.toml"),
        r#""LFM2-700M" = 700
"llama-3.2-1b" = 1000
"LFM2-8B-A1B" = { total = 8340, active = 1500 }
"LFM2.5-8B-A1B" = { total = 8340, active = 1500 }
"#,
    )?;
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

pub async fn make_state(dir: &std::path::Path) -> anyhow::Result<AppState> {
    make_state_with_evals_url(dir, "http://localhost:9999").await
}

/// Build an [`AppState`] backed by local_fs stores under the config's data
/// dir. The `make_state_*` helpers differ only in which config fields they set,
/// so they all delegate here.
pub async fn make_state_from_config(config: Config) -> anyhow::Result<AppState> {
    let stores = build_local_fs_stores(&config)?;
    let catalog = stores.catalog.load_catalog().await?;
    let catalog_cache = Arc::new(pipette_mgmt::catalog_cache::CatalogCache::new(
        stores.catalog,
        catalog,
        std::time::Duration::from_secs(config.catalog_ttl_secs),
    ));
    Ok(AppState {
        config: Arc::new(config),
        catalog_cache,
        http_client: reqwest::Client::new(),
        replay_cache: Arc::new(pipette_mgmt::auth::ReplayCache::new()),
        migrated_clients: Arc::new(pipette_mgmt::auth::MigratedClients::new()),
        auth_store: stores.auth,
        submission_store: stores.submissions,
        warehouse_store: stores.warehouse,
        eval_sample_result_store: stores.eval_sample_results,
        todo_store: stores.todo,
    })
}

/// Build an [`AppState`] with `accept_legacy_signatures` toggled. Used by the
/// auth tests to exercise the timestamp-only signature on both settings.
pub async fn make_state_with_legacy_signatures(
    dir: &std::path::Path,
    accept: bool,
) -> anyhow::Result<AppState> {
    let mut config = test_config(dir);
    config.accept_legacy_signatures = accept;
    make_state_from_config(config).await
}

/// Build an [`AppState`] with `[unverified_submissions] enabled` toggled.
/// Used by the held-submission tests to exercise both the hold path and
/// the `403`-when-disabled path.
pub async fn make_state_with_unverified(
    dir: &std::path::Path,
    enabled: bool,
) -> anyhow::Result<AppState> {
    let mut config = test_config(dir);
    config.unverified_submissions.enabled = enabled;
    make_state_from_config(config).await
}

/// Build an [`AppState`] with `[auto_approve]` rules configured. Used by
/// the registration tests to exercise the email/domain auto-approve path.
pub async fn make_state_with_auto_approve(
    dir: &std::path::Path,
    emails: Vec<String>,
    domains: Vec<String>,
) -> anyhow::Result<AppState> {
    let mut config = test_config(dir);
    config.auto_approve = pipette_mgmt::config::AutoApproveConfig { emails, domains };
    make_state_from_config(config).await
}

/// Build an [`AppState`] with `require_preauth_key` toggled on, for the
/// preauth registration tests.
pub async fn make_state_require_preauth(dir: &std::path::Path) -> anyhow::Result<AppState> {
    let mut config = test_config(dir);
    config.require_preauth_key = true;
    make_state_from_config(config).await
}

pub async fn make_state_with_evals_url(
    dir: &std::path::Path,
    evals_url: &str,
) -> anyhow::Result<AppState> {
    make_state_from_config(test_config_with_evals_url(dir, evals_url)).await
}

// ---------------------------------------------------------------------------
// Auth helpers
// ---------------------------------------------------------------------------

/// Auth headers signing the given request at the current time.
pub fn auth_headers(
    signing_key: &SigningKey,
    client_id: &ClientId,
    method: &str,
    path: &str,
) -> Vec<(&'static str, String)> {
    let timestamp = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    auth_headers_at(signing_key, client_id, method, path, timestamp)
}

/// Auth headers whose signature covers only the timestamp — the payload the
/// server accepts while `accept_legacy_signatures` is set.
pub fn legacy_auth_headers(
    signing_key: &SigningKey,
    client_id: &ClientId,
) -> Vec<(&'static str, String)> {
    let timestamp = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let signature = signing_key.sign(timestamp.as_bytes());
    signed_headers(client_id, timestamp, signature)
}

/// Auth headers signing the given request at a caller-chosen timestamp, with a
/// nonce unique to this call.
///
/// The payload is spelled out here rather than shared with `src/auth.rs` on
/// purpose: this is a second, independent implementation of the wire format, so
/// a server-side change surfaces as a test failure instead of being mirrored
/// into the tests automatically. `test_signed_payload_wire_format` in
/// `tests/auth.rs` pins the exact bytes.
pub fn auth_headers_at(
    signing_key: &SigningKey,
    client_id: &ClientId,
    method: &str,
    path: &str,
    timestamp: String,
) -> Vec<(&'static str, String)> {
    let nonce = nonce();
    let signature = signing_key
        .sign(format!("v1\n{method}\n{path}\n{timestamp}\n{client_id}\n{nonce}").as_bytes());
    let mut headers = signed_headers(client_id, timestamp, signature);
    headers.push(("X-Nonce", nonce));
    headers
}

/// A nonce unique to one request, which is what makes its signature unique.
fn nonce() -> String {
    let mut bytes = [0u8; 16];
    rand_core::RngCore::fill_bytes(&mut rand_core::OsRng, &mut bytes);
    hex::encode(bytes)
}

/// The auth headers carrying an already-computed signature. Only the signed
/// bytes differ between the `v1` and timestamp-only payloads; the header
/// envelope is the same, so it lives here rather than in each signer. The
/// timestamp-only payload covers no nonce, so `X-Nonce` is added by the `v1`
/// signer rather than here.
fn signed_headers(
    client_id: &ClientId,
    timestamp: String,
    signature: Signature,
) -> Vec<(&'static str, String)> {
    vec![
        ("X-Client-Id", client_id.to_string()),
        ("X-Timestamp", timestamp),
        ("X-Signature", hex::encode(signature.to_bytes())),
    ]
}

pub async fn body_json(resp: axum::http::Response<Body>) -> anyhow::Result<Value> {
    let bytes = resp.into_body().collect().await?.to_bytes();
    Ok(serde_json::from_slice(&bytes)?)
}

/// Extract a string field from a JSON body, erroring (rather than
/// panicking) when it is missing or not a string — so tests can `?` it.
pub fn json_str<'a>(value: &'a Value, key: &str) -> anyhow::Result<&'a str> {
    value[key]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing or non-string field {key:?}"))
}

/// Register a client and leave it `pending` (unapproved). Returns
/// (signing_key, client_id).
pub async fn register_pending(state: &AppState) -> anyhow::Result<(SigningKey, ClientId)> {
    let mut csprng = rand_core::OsRng;
    let signing_key = SigningKey::generate(&mut csprng);
    let pk_hex = hex::encode(signing_key.verifying_key().as_bytes());

    let app = test_app(state.clone());
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/clients/register")
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_string(&json!({
                    "public_key": pk_hex,
                    "organization": "test-org",
                    "client_details": "test",
                    "contact_email": "t@t.com"
                }))?))?,
        )
        .await?;

    let body = body_json(resp).await?;
    let client_id = ClientId::try_new(
        body["client_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing client_id in register response"))?,
    )?;
    Ok((signing_key, client_id))
}

/// Register a client and approve it. Returns (signing_key, client_id).
pub async fn register_and_approve(state: &AppState) -> anyhow::Result<(SigningKey, ClientId)> {
    let mut csprng = rand_core::OsRng;
    let signing_key = SigningKey::generate(&mut csprng);
    let pk_hex = hex::encode(signing_key.verifying_key().as_bytes());
    let client_id = register_and_approve_public_key(state, &pk_hex).await?;
    Ok((signing_key, client_id))
}

/// Register and approve a caller-supplied public key, for the tests whose
/// subject is the key itself rather than a client holding one.
pub async fn register_and_approve_public_key(
    state: &AppState,
    pk_hex: &str,
) -> anyhow::Result<ClientId> {
    let app = test_app(state.clone());
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/clients/register")
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_string(&json!({
                    "public_key": pk_hex,
                    "organization": "test-org",
                    "client_details": "test",
                    "contact_email": "t@t.com"
                }))?))?,
        )
        .await?;

    let body = body_json(resp).await?;
    let client_id = ClientId::try_new(
        body["client_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing client_id in register response"))?,
    )?;

    let mut client = state
        .auth_store
        .get_client(&client_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("client {client_id} not found after registration"))?;
    client.status = pipette_mgmt::client::ClientStatus::Approved;
    state.auth_store.put_client(&client).await?;

    Ok(client_id)
}

/// Send a POST /benchmarks submission, return the job_id.
pub async fn submit_benchmark(
    state: &AppState,
    signing_key: &SigningKey,
    client_id: &ClientId,
    submission: &Value,
) -> anyhow::Result<String> {
    let headers = auth_headers(signing_key, client_id, "POST", "/benchmarks");
    let mut req = Request::builder()
        .method("POST")
        .uri("/benchmarks")
        .header("Content-Type", "application/json");
    for (k, v) in &headers {
        req = req.header(*k, v);
    }

    let app = test_app(state.clone());
    let resp = app
        .oneshot(req.body(Body::from(serde_json::to_string(submission)?))?)
        .await?;
    assert_eq!(resp.status(), axum::http::StatusCode::ACCEPTED);
    let body = body_json(resp).await?;
    let job_id = body["job_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing job_id in submit response"))?
        .to_string();
    Ok(job_id)
}

/// Send a GET request with auth headers, return the response.
pub async fn authed_get(
    state: &AppState,
    signing_key: &SigningKey,
    client_id: &ClientId,
    uri: &str,
) -> anyhow::Result<axum::http::Response<Body>> {
    let headers = auth_headers(signing_key, client_id, "GET", uri);
    request_with_headers(state, "GET", uri, &headers).await
}

/// Send a GET request with pre-built auth headers, return the response.
pub async fn authed_get_with_headers(
    state: &AppState,
    uri: &str,
    headers: &[(&str, String)],
) -> anyhow::Result<axum::http::Response<Body>> {
    request_with_headers(state, "GET", uri, headers).await
}

/// Send an empty-bodied request with a caller-chosen method, URI, and
/// pre-built headers. Lets a test sign one request and send a different one.
pub async fn request_with_headers(
    state: &AppState,
    method: &str,
    uri: &str,
    headers: &[(&str, String)],
) -> anyhow::Result<axum::http::Response<Body>> {
    let req = headers
        .iter()
        .fold(Request::builder().method(method).uri(uri), |req, (k, v)| {
            req.header(*k, v)
        });
    let app = test_app(state.clone());
    Ok(app.oneshot(req.body(Body::empty())?).await?)
}

/// Send an unauthenticated GET request, return the response.
pub async fn unauthed_get(
    state: &AppState,
    uri: &str,
) -> anyhow::Result<axum::http::Response<Body>> {
    let app = test_app(state.clone());
    Ok(app
        .oneshot(Request::builder().uri(uri).body(Body::empty())?)
        .await?)
}

/// Send an unauthenticated GET request with extra headers, return the response.
pub async fn unauthed_get_with_headers(
    state: &AppState,
    uri: &str,
    headers: &[(&str, String)],
) -> anyhow::Result<axum::http::Response<Body>> {
    request_with_headers(state, "GET", uri, headers).await
}

/// Send an unauthenticated POST request with JSON body, return the response.
pub async fn unauthed_post(
    state: &AppState,
    uri: &str,
    body: &Value,
) -> anyhow::Result<axum::http::Response<Body>> {
    let app = test_app(state.clone());
    Ok(app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_string(body)?))?,
        )
        .await?)
}

/// Send an authenticated POST request with JSON body, return the response.
pub async fn authed_post(
    state: &AppState,
    signing_key: &SigningKey,
    client_id: &ClientId,
    uri: &str,
    body: &Value,
) -> anyhow::Result<axum::http::Response<Body>> {
    let headers = auth_headers(signing_key, client_id, "POST", uri);
    let mut req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("Content-Type", "application/json");
    for (k, v) in &headers {
        req = req.header(*k, v);
    }
    let app = test_app(state.clone());
    Ok(app
        .oneshot(req.body(Body::from(serde_json::to_string(body)?))?)
        .await?)
}

/// Send an authenticated PUT request with an empty body, return the response.
/// Used for endpoints like `PUT /plans/{job_id}/heartbeat` whose request body
/// is empty (httpapi.md §2.10.2).
pub async fn authed_put_empty(
    state: &AppState,
    signing_key: &SigningKey,
    client_id: &ClientId,
    uri: &str,
) -> anyhow::Result<axum::http::Response<Body>> {
    let headers = auth_headers(signing_key, client_id, "PUT", uri);
    request_with_headers(state, "PUT", uri, &headers).await
}

/// Send an authenticated PATCH request with JSON body, return the response.
pub async fn authed_patch(
    state: &AppState,
    signing_key: &SigningKey,
    client_id: &ClientId,
    uri: &str,
    body: &Value,
) -> anyhow::Result<axum::http::Response<Body>> {
    let headers = auth_headers(signing_key, client_id, "PATCH", uri);
    let mut req = Request::builder()
        .method("PATCH")
        .uri(uri)
        .header("Content-Type", "application/json");
    for (k, v) in &headers {
        req = req.header(*k, v);
    }
    let app = test_app(state.clone());
    Ok(app
        .oneshot(req.body(Body::from(serde_json::to_string(body)?))?)
        .await?)
}

// ---------------------------------------------------------------------------
// Mock evals server
// ---------------------------------------------------------------------------

async fn mock_get_samples() -> axum::Json<Value> {
    axum::Json(json!({
        "samples": [
            {"id": "s1", "messages": [{"role": "user", "content": "What is 2+2?"}]},
            {"id": "s2", "messages": [{"role": "user", "content": "Capital of France?"}]},
            {"id": "s3", "messages": [{"role": "user", "content": "Color of the sky?"}]}
        ]
    }))
}

async fn mock_post_score(axum::Json(body): axum::Json<Value>) -> axum::Json<Value> {
    // Honor the contract: echo each posted completion in the response, tag with
    // the messages from the "dataset" and a fixed is_correct verdict. Ids not
    // in the fixture are dropped (mirrors "scorer returned only known ids").
    let messages_by_id: std::collections::HashMap<&str, Value> = [
        ("s1", json!([{"role": "user", "content": "What is 2+2?"}])),
        (
            "s2",
            json!([{"role": "user", "content": "Capital of France?"}]),
        ),
        (
            "s3",
            json!([{"role": "user", "content": "Color of the sky?"}]),
        ),
    ]
    .into_iter()
    .collect();
    let correct_by_id: std::collections::HashMap<&str, bool> =
        [("s1", true), ("s2", false), ("s3", true)]
            .into_iter()
            .collect();

    let completions = body["completions"]
        .as_array()
        .expect("mock_post_score: request missing 'completions' array");

    let scored_samples: Vec<Value> = completions
        .iter()
        .filter_map(|c| {
            let id = c["id"].as_str()?;
            let completion = c["completion"].as_str()?;
            let messages = messages_by_id.get(id)?.clone();
            let is_correct = *correct_by_id.get(id)?;
            Some(json!({
                "id": id,
                "messages": messages,
                "completion": completion,
                "is_correct": is_correct,
            }))
        })
        .collect();

    axum::Json(json!({
        "runtime_version": "mock-v1.0.0",
        "context": {"accuracy_mock": 0.6667, "samples_seen": scored_samples.len()},
        "scored_samples": scored_samples,
    }))
}

/// Run the full pipeline an eval job traverses now that scoring is split:
/// fast route (`incoming/` → `score-queue/to_do/`), slow eval scoring
/// (`to_do` → `to_finalize`), then fast finalize (`to_finalize` → `processed/`).
/// Non-eval jobs are fully handled by the first `run_score`.
pub async fn run_full_score(config: &Config) -> anyhow::Result<()> {
    pipette_mgmt::score::run_process_submissions(config, build_local_fs_stores(config)?).await?;
    pipette_mgmt::score::run_score_eval(config, build_local_fs_stores(config)?).await?;
    pipette_mgmt::score::run_process_submissions(config, build_local_fs_stores(config)?).await?;
    Ok(())
}

/// Start a mock evals server on a random port, return the base URL.
pub async fn start_mock_evals_server() -> anyhow::Result<String> {
    let app = Router::new().route("/score", post(mock_post_score)).route(
        "/evals/{eval_id}/datasets/{dataset_name}/samples",
        get(mock_get_samples),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr: SocketAddr = listener.local_addr()?;
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    Ok(format!("http://{addr}"))
}

/// Mock evals server that returns 200 OK with a body missing the `samples`
/// field — exercises the mgmt-side contract deserializer.
pub async fn start_mock_evals_server_malformed_samples() -> anyhow::Result<String> {
    async fn malformed() -> axum::Json<Value> {
        axum::Json(json!({"not_samples": []}))
    }
    let app = Router::new().route(
        "/evals/{eval_id}/datasets/{dataset_name}/samples",
        get(malformed),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr: SocketAddr = listener.local_addr()?;
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    Ok(format!("http://{addr}"))
}

/// Mock evals server that returns 200 OK with a `/score` response missing the
/// `scored_samples` field — should trip the typed deserializer in mgmt.
pub async fn start_mock_evals_server_malformed_score() -> anyhow::Result<String> {
    async fn malformed() -> axum::Json<Value> {
        axum::Json(json!({"runtime_version": "x", "not_scored_samples": []}))
    }
    let app = Router::new().route("/score", post(malformed));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr: SocketAddr = listener.local_addr()?;
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    Ok(format!("http://{addr}"))
}

/// Drive `f` to completion with the line-oriented formatter writing into a
/// buffer, and return what it wrote, so a test can assert on the log a request
/// produced.
///
/// Takes a closure rather than a future because the dispatcher this installs is
/// thread-local: the runtime is built inside it, single-threaded, so every task
/// the closure drives stays on the thread that can see it. (The crate's own unit
/// tests carry a copy of this — a `#[cfg(test)]` helper is not visible across the
/// integration-test crate boundary.)
pub fn capture_logs<T>(f: impl FnOnce(&tokio::runtime::Runtime) -> T) -> (T, String) {
    #[derive(Clone)]
    struct Writer(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Writer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("capture lock").extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Writer {
        type Writer = Self;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    let buf = Arc::new(std::sync::Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(Writer(Arc::clone(&buf)))
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .finish();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let out = tracing::subscriber::with_default(subscriber, || f(&runtime));
    let text = String::from_utf8_lossy(&buf.lock().expect("capture lock")).into_owned();
    (out, text)
}

/// Delete every pending-reindex flag for `client_id` — the test stand-in for
/// the reindex pass lifting the gate. Production consumes flags by exact
/// captured key (`reindex_flagged_clients`); tests just need them all gone.
pub async fn clear_pending_reindex(
    todo: &dyn TodoStore,
    client_id: &ClientId,
) -> anyhow::Result<()> {
    for (_, key) in todo
        .list_pending_reindex()
        .await?
        .into_iter()
        .filter(|(flagged, _)| flagged == client_id)
    {
        todo.delete_pending_reindex(&key).await?;
    }
    Ok(())
}
