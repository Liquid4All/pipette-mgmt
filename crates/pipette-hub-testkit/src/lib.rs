//! A mock HuggingFace Hub and CDN for the import tests. They listen on different
//! ports, so a redirect from the Hub to the CDN crosses origins as in production.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use reqwest::Url;
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::sync::{Notify, OwnedRwLockWriteGuard, RwLock};

pub const GOOD_TOKEN: &str = "hf_good_SECRET";
/// A second token that the Hub accepts, with the same access as
/// [`GOOD_TOKEN`].
pub const OTHER_GOOD_TOKEN: &str = "hf_other_SECRET";
pub const BAD_TOKEN: &str = "hf_bad_SECRET";
/// A revision name that has a `/`, which each mock repository resolves.
pub const PR_REF: &str = "refs/pr/1";
/// The seconds until the rate limit resets, in each `429`'s `RateLimit` header.
pub const RATE_LIMIT_RESET: u64 = 55;

#[derive(Clone)]
pub struct MockFile {
    pub path: String,
    pub bytes: Vec<u8>,
    pub lfs: bool,
    /// The listing gives no size.
    pub sizeless: bool,
    /// A download answers `403`.
    pub refused: bool,
    /// A download answers `500`.
    pub failing: bool,
    /// A download answers `429` with a `RateLimit` header.
    pub rate_limited: bool,
    /// A download gives these bytes, not `bytes`.
    pub served: Option<Vec<u8>>,
    /// A download waits while a test holds [`MockHub::hold_files`].
    pub held: bool,
}

impl MockFile {
    pub fn lfs(path: &str, bytes: &[u8]) -> Self {
        Self {
            path: path.to_owned(),
            bytes: bytes.to_vec(),
            lfs: true,
            sizeless: false,
            refused: false,
            failing: false,
            rate_limited: false,
            served: None,
            held: false,
        }
    }

    pub fn git(path: &str, bytes: &[u8]) -> Self {
        Self {
            lfs: false,
            ..Self::lfs(path, bytes)
        }
    }

    pub fn sizeless(self) -> Self {
        Self {
            sizeless: true,
            ..self
        }
    }

    pub fn refused(self) -> Self {
        Self {
            refused: true,
            ..self
        }
    }

    pub fn failing(self) -> Self {
        Self {
            failing: true,
            ..self
        }
    }

    pub fn rate_limited(self) -> Self {
        Self {
            rate_limited: true,
            ..self
        }
    }

    pub fn serving(self, bytes: &[u8]) -> Self {
        Self {
            served: Some(bytes.to_vec()),
            ..self
        }
    }

    pub fn held(self) -> Self {
        Self { held: true, ..self }
    }

    fn body(&self) -> Vec<u8> {
        self.served.clone().unwrap_or_else(|| self.bytes.clone())
    }

    /// The lowercase hex SHA-256 of the listed bytes.
    pub fn sha256(&self) -> String {
        hex::encode(Sha256::digest(&self.bytes))
    }
}

/// One mock repository at one commit. The commit resolves from `main`, from
/// [`PR_REF`] and from its own id.
#[derive(Clone)]
pub struct MockRepo {
    pub commit: String,
    pub files: Vec<MockFile>,
    /// A request with no token is answered; a bad token still gets `403`.
    pub public: bool,
    /// A revision request answers `429` with a `RateLimit` header.
    pub rate_limited: bool,
    /// The access rule that a revision response reports.
    pub gate: MockGate,
}

/// The `gated` value of a revision response.
#[derive(Clone, Copy)]
pub enum MockGate {
    /// `"gated": false`.
    Open,
    /// `"gated"` holds this mode, such as `"auto"` or `"manual"`.
    Gated(&'static str),
    /// The response has neither `private` nor `gated`.
    Unreported,
}

struct HubState {
    /// Keyed by `org/repo_name`.
    repos: HashMap<String, MockRepo>,
    cdn: Url,
    /// The downloads of each file path, in every repository.
    downloads_of: HashMap<String, AtomicUsize>,
    revisions: AtomicUsize,
    cdn_authorized: AtomicUsize,
    /// Revision requests take a read lock; [`MockHub::hold`] takes the write lock.
    gate: Arc<RwLock<()>>,
    arrived: Notify,
    /// Held files' downloads take a read lock; [`MockHub::hold_files`] writes.
    file_gate: Arc<RwLock<()>>,
    file_arrived: Notify,
}

/// The running Hub and CDN. Both stop when the runtime that started them
/// stops.
pub struct MockHub {
    pub endpoint: Url,
    state: Arc<HubState>,
}

impl MockHub {
    pub async fn start(repos: HashMap<String, MockRepo>) -> anyhow::Result<Self> {
        let cdn_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let hub_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let cdn: Url = format!("http://{}/", cdn_listener.local_addr()?).parse()?;
        let endpoint: Url = format!("http://{}/", hub_listener.local_addr()?).parse()?;
        let downloads_of = repos
            .values()
            .flat_map(|repo| &repo.files)
            .map(|file| (file.path.clone(), AtomicUsize::new(0)))
            .collect();
        let state = Arc::new(HubState {
            repos,
            cdn,
            downloads_of,
            revisions: AtomicUsize::new(0),
            cdn_authorized: AtomicUsize::new(0),
            gate: Arc::new(RwLock::new(())),
            arrived: Notify::new(),
            file_gate: Arc::new(RwLock::new(())),
            file_arrived: Notify::new(),
        });
        let hub = Router::new()
            .route("/api/models/{org}/{repo}/revision/{rev}", get(revision))
            .route("/{org}/{repo}/resolve/{commit}/{*path}", get(resolve))
            .with_state(Arc::clone(&state));
        let cdn = Router::new()
            .route("/blob/{sha256}", get(blob))
            .with_state(Arc::clone(&state));
        tokio::spawn(async move { axum::serve(hub_listener, hub).await });
        tokio::spawn(async move { axum::serve(cdn_listener, cdn).await });
        Ok(Self { endpoint, state })
    }

    /// The download requests that the Hub answered with bytes or a redirect.
    pub fn downloads(&self) -> usize {
        self.state
            .downloads_of
            .values()
            .map(|count| count.load(Ordering::SeqCst))
            .sum()
    }

    pub fn revision_requests(&self) -> usize {
        self.state.revisions.load(Ordering::SeqCst)
    }

    /// Hold every revision request until the returned guard drops.
    pub async fn hold(&self) -> OwnedRwLockWriteGuard<()> {
        Arc::clone(&self.state.gate).write_owned().await
    }

    /// Wait until a revision request arrives at the Hub.
    pub async fn arrived(&self) {
        self.state.arrived.notified().await;
    }

    /// The download requests for `path` that the Hub answered with bytes or a
    /// redirect, in every repository.
    pub fn downloads_of(&self, path: &str) -> usize {
        self.state
            .downloads_of
            .get(path)
            .map_or(0, |count| count.load(Ordering::SeqCst))
    }

    /// The repository `repo` (`org/repo_name`) at its commit, as
    /// `org/repo_name@commit`. `None` if the Hub does not hold `repo`.
    pub fn reference(&self, repo: &str) -> Option<String> {
        let mock = self.state.repos.get(repo)?;
        Some(format!("{repo}@{}", mock.commit))
    }

    /// Hold the download of each held file until the returned guard drops.
    pub async fn hold_files(&self) -> OwnedRwLockWriteGuard<()> {
        Arc::clone(&self.state.file_gate).write_owned().await
    }

    /// Wait until a download of a held file arrives at the Hub.
    pub async fn file_arrived(&self) {
        self.state.file_arrived.notified().await;
    }

    /// The CDN requests that carried an `Authorization` header.
    pub fn cdn_authorized(&self) -> usize {
        self.state.cdn_authorized.load(Ordering::SeqCst)
    }
}

/// The repository that `org/repo` names, or the refusal for the request.
fn authorized<'a>(
    state: &'a HubState,
    headers: &HeaderMap,
    org: &str,
    repo: &str,
) -> Result<&'a MockRepo, StatusCode> {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let found = state.repos.get(&format!("{org}/{repo}"));
    match token {
        None => match found {
            Some(repo) if repo.public => Ok(repo),
            Some(_) | None => Err(StatusCode::UNAUTHORIZED),
        },
        Some(GOOD_TOKEN | OTHER_GOOD_TOKEN) => found.ok_or(StatusCode::NOT_FOUND),
        Some(_) => Err(StatusCode::FORBIDDEN),
    }
}

async fn revision(
    State(state): State<Arc<HubState>>,
    Path((org, repo, rev)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    state.revisions.fetch_add(1, Ordering::SeqCst);
    state.arrived.notify_one();
    let _open = state.gate.read().await;
    let repo = match authorized(&state, &headers, &org, &repo) {
        Ok(repo) => repo,
        Err(status) => return status.into_response(),
    };
    if repo.rate_limited {
        return rate_limited();
    }
    if rev != "main" && rev != PR_REF && rev != repo.commit {
        return StatusCode::NOT_FOUND.into_response();
    }
    let siblings: Vec<_> = repo
        .files
        .iter()
        .map(|file| {
            let mut sibling = serde_json::Map::new();
            sibling.insert("rfilename".to_owned(), json!(file.path));
            if !file.sizeless {
                sibling.insert("size".to_owned(), json!(file.bytes.len()));
            }
            if file.lfs {
                let lfs = json!({"sha256": file.sha256(), "size": file.bytes.len()});
                sibling.insert("lfs".to_owned(), lfs);
            }
            serde_json::Value::Object(sibling)
        })
        .collect();
    let mut info = json!({"sha": repo.commit, "siblings": siblings});
    let gated = match repo.gate {
        MockGate::Open => Some(json!(false)),
        MockGate::Gated(mode) => Some(json!(mode)),
        MockGate::Unreported => None,
    };
    if let (Some(gated), Some(object)) = (gated, info.as_object_mut()) {
        object.insert("private".to_owned(), json!(!repo.public));
        object.insert("gated".to_owned(), gated);
    }
    axum::Json(info).into_response()
}

async fn resolve(
    State(state): State<Arc<HubState>>,
    Path((org, repo, commit, path)): Path<(String, String, String, String)>,
    headers: HeaderMap,
) -> Response {
    let repo = match authorized(&state, &headers, &org, &repo) {
        Ok(repo) => repo,
        Err(status) => return status.into_response(),
    };
    let Some(file) = repo
        .files
        .iter()
        .find(|file| commit == repo.commit && file.path == path)
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if file.refused {
        return StatusCode::FORBIDDEN.into_response();
    }
    if file.failing {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    if file.rate_limited {
        return rate_limited();
    }
    if file.held {
        state.file_arrived.notify_one();
        let _open = state.file_gate.read().await;
    }
    if let Some(count) = state.downloads_of.get(&file.path) {
        count.fetch_add(1, Ordering::SeqCst);
    }
    if !file.lfs {
        return file.body().into_response();
    }
    match state.cdn.join(&format!("blob/{}", file.sha256())) {
        Ok(location) => (
            StatusCode::FOUND,
            [(header::LOCATION, location.to_string())],
        )
            .into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn blob(
    State(state): State<Arc<HubState>>,
    Path(sha256): Path<String>,
    headers: HeaderMap,
) -> Response {
    if headers.contains_key(header::AUTHORIZATION) {
        state.cdn_authorized.fetch_add(1, Ordering::SeqCst);
    }
    let file = state
        .repos
        .values()
        .flat_map(|repo| &repo.files)
        .find(|file| file.sha256() == sha256);
    match file {
        Some(file) => file.body().into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// A `429` with the `RateLimit` header that gives [`RATE_LIMIT_RESET`].
fn rate_limited() -> Response {
    let rate_limit = format!("\"resolvers\";r=0;t={RATE_LIMIT_RESET}");
    (StatusCode::TOO_MANY_REQUESTS, [("ratelimit", rate_limit)]).into_response()
}
