//! The HuggingFace Hub calls an import makes. The client holds no token: each
//! call sends its [`HfRepo`]'s token as a bearer header, which `reqwest` drops
//! on a cross-host redirect. Permits, held until a body ends, cap concurrency.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use futures::{StreamExt, TryStreamExt};
use reqwest::header::{HeaderMap, RETRY_AFTER};
use reqwest::{RequestBuilder, Response, StatusCode, Url};
use serde::Deserialize;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use pipette_plan_types::{GitOid, HfRepo, Sha256};
use pipette_store::BlobBody;

/// The time to open a connection to the Hub or the CDN.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// The longest gap between two reads. No total timeout: a multi-gigabyte
/// download outlasts any.
const READ_TIMEOUT: Duration = Duration::from_secs(60);

/// The header in which HuggingFace gives the seconds until its rate limit
/// resets, as the `t` parameter: `"api";r=0;t=55`.
const RATE_LIMIT: &str = "ratelimit";

/// A client for one HuggingFace endpoint, such as `https://huggingface.co`.
#[derive(Debug, Clone)]
pub struct HubClient {
    http: reqwest::Client,
    endpoint: Url,
    /// One permit for each request that runs.
    requests: Arc<Semaphore>,
}

/// Why a [`HubClient`] could not be built.
#[derive(Debug, thiserror::Error)]
pub enum HubSetupError {
    #[error("the HuggingFace endpoint `{endpoint}` is not an absolute URL: {reason}")]
    BadEndpoint { endpoint: String, reason: String },
    #[error("the HuggingFace endpoint `{0}` cannot hold a path")]
    NotABase(Url),
    #[error("the HTTP client for HuggingFace: {0}")]
    Client(#[from] reqwest::Error),
}

/// A HuggingFace revision as one commit, with every file at that commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRepo {
    pub commit: GitOid,
    pub files: Vec<HubFile>,
    /// Whether the repository is private. `None` if the Hub did not say.
    pub private: Option<bool>,
    /// The access rule of the repository. `None` if the Hub did not say.
    pub gated: Option<HubGate>,
}

/// The access rule that the Hub reports in `gated`: `false`, or the approval
/// mode of a gated repository, such as `"auto"` or `"manual"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HubGate {
    Open,
    /// The mode as the Hub reports it.
    Gated(String),
}

impl<'de> Deserialize<'de> for HubGate {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Flag(bool),
            Mode(String),
        }
        Ok(match Raw::deserialize(deserializer)? {
            Raw::Flag(false) => Self::Open,
            Raw::Flag(true) => Self::Gated("true".to_owned()),
            Raw::Mode(mode) => Self::Gated(mode),
        })
    }
}

/// One file of a repository, as the Hub lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubFile {
    /// The path from the repository root.
    pub path: String,
    pub size: Option<u64>,
    /// The digest of the file's bytes. Only a file in LFS storage has one.
    pub lfs_sha256: Option<Sha256>,
}

/// A Hub status that refuses access to a repository, a revision or a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::Display)]
pub enum HubRefusal {
    #[strum(to_string = "401 Unauthorized")]
    Unauthorized,
    #[strum(to_string = "403 Forbidden")]
    Forbidden,
    #[strum(to_string = "404 Not Found")]
    NotFound,
}

impl HubRefusal {
    pub fn code(self) -> u16 {
        match self {
            Self::Unauthorized => 401,
            Self::Forbidden => 403,
            Self::NotFound => 404,
        }
    }

    fn of(status: StatusCode) -> Option<Self> {
        match status {
            StatusCode::UNAUTHORIZED => Some(Self::Unauthorized),
            StatusCode::FORBIDDEN => Some(Self::Forbidden),
            StatusCode::NOT_FOUND => Some(Self::NotFound),
            _ => None,
        }
    }
}

/// Why a Hub call failed. Each arm names the repository by its reference,
/// which holds no token.
#[derive(Debug, thiserror::Error)]
pub enum HubError {
    #[error("HuggingFace refused access to {repo}: {status}")]
    Refused { repo: String, status: HubRefusal },
    /// A retry after `retry_after_secs` can succeed.
    #[error("HuggingFace rate-limited the requests for {repo}. {}", retry_hint(.retry_after_secs))]
    RateLimited {
        repo: String,
        retry_after_secs: Option<u64>,
    },
    #[error("HuggingFace failed for {repo}: {reason}")]
    Upstream { repo: String, reason: String },
}

/// The part of `GET /api/models/{repo}/revision/{rev}?blobs=true` that the
/// import reads.
#[derive(Deserialize)]
struct ModelInfo {
    sha: String,
    siblings: Vec<Sibling>,
    #[serde(default)]
    private: Option<bool>,
    #[serde(default)]
    gated: Option<HubGate>,
}

#[derive(Deserialize)]
struct Sibling {
    rfilename: String,
    size: Option<u64>,
    lfs: Option<Lfs>,
}

#[derive(Deserialize)]
struct Lfs {
    sha256: String,
}

impl HubClient {
    pub fn new(
        endpoint: Url,
        max_concurrent_requests: NonZeroUsize,
    ) -> Result<Self, HubSetupError> {
        if endpoint.cannot_be_a_base() {
            return Err(HubSetupError::NotABase(endpoint));
        }
        let http = crate::http_client::builder_for(endpoint.as_str())
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .build()?;
        Ok(Self {
            http,
            endpoint,
            requests: Arc::new(Semaphore::new(max_concurrent_requests.get())),
        })
    }

    /// The request permits that no request holds.
    #[cfg(test)]
    pub(super) fn available_permits(&self) -> usize {
        self.requests.available_permits()
    }

    /// The commit that `repo`'s revision names, and every file at that
    /// commit. A repository with no revision resolves `main`.
    pub async fn resolve(&self, repo: &HfRepo) -> Result<ResolvedRepo, HubError> {
        let revision = repo.revision.as_ref().map_or("main", AsRef::as_ref);
        let mut url = self.url(
            repo,
            [
                "api",
                "models",
                repo.org.as_ref(),
                repo.repo_name.as_ref(),
                "revision",
                revision,
            ],
        )?;
        url.set_query(Some("blobs=true"));
        let (response, permit) = self.send(self.http.get(url), repo).await?;
        let info: ModelInfo = response
            .json()
            .await
            .map_err(|e| upstream(repo, format!("{:#}", anyhow::Error::new(e))))?;
        drop(permit);
        let commit =
            GitOid::try_new(info.sha).map_err(|e| upstream(repo, format!("the commit id: {e}")))?;
        let files = info
            .siblings
            .into_iter()
            .map(|sibling| {
                let lfs_sha256 = sibling
                    .lfs
                    .map(|lfs| Sha256::try_new(lfs.sha256))
                    .transpose()
                    .map_err(|e| upstream(repo, format!("the LFS digest: {e}")))?;
                Ok(HubFile {
                    path: sibling.rfilename,
                    size: sibling.size,
                    lfs_sha256,
                })
            })
            .collect::<Result<_, HubError>>()?;
        Ok(ResolvedRepo {
            commit,
            files,
            private: info.private,
            gated: info.gated,
        })
    }

    /// The bytes of `path` in `repo` at `commit`. The status is checked before
    /// this returns; the body holds its request permit until it drops.
    pub async fn download(
        &self,
        repo: &HfRepo,
        commit: &GitOid,
        path: &str,
    ) -> Result<BlobBody, HubError> {
        let segments = [
            repo.org.as_ref(),
            repo.repo_name.as_ref(),
            "resolve",
            commit.as_ref(),
        ]
        .into_iter()
        .chain(path.split('/'));
        let url = self.url(repo, segments)?;
        let (response, permit) = self.send(self.http.get(url), repo).await?;
        let label = repo.reference();
        Ok(response
            .bytes_stream()
            .map_err(move |e| {
                // The closure owns the permit, so the body holds it.
                let _held = &permit;
                anyhow::Error::new(HubError::Upstream {
                    repo: label.clone(),
                    reason: format!("{:#}", anyhow::Error::new(e)),
                })
            })
            .boxed())
    }

    /// The endpoint with `segments` appended, each percent-encoded so a revision
    /// such as `refs/pr/1` stays one segment.
    fn url<'a>(
        &self,
        repo: &HfRepo,
        segments: impl IntoIterator<Item = &'a str>,
    ) -> Result<Url, HubError> {
        let mut url = self.endpoint.clone();
        // `new` refuses an endpoint that cannot hold a path.
        url.path_segments_mut()
            .map_err(|()| upstream(repo, "the endpoint cannot hold a path".to_owned()))?
            .pop_if_empty()
            .extend(segments);
        Ok(url)
    }

    /// Send `request` under a permit, which the caller holds until it reads the
    /// body.
    async fn send(
        &self,
        request: RequestBuilder,
        repo: &HfRepo,
    ) -> Result<(Response, OwnedSemaphorePermit), HubError> {
        let permit = self.permit(repo).await?;
        let request = match &repo.auth_token {
            Some(token) => request.bearer_auth(token.as_ref()),
            None => request,
        };
        let response = request
            .send()
            .await
            .map_err(|e| upstream(repo, format!("{:#}", anyhow::Error::new(e))))?;
        let status = response.status();
        if status.is_success() {
            return Ok((response, permit));
        }
        if status == StatusCode::TOO_MANY_REQUESTS {
            return Err(HubError::RateLimited {
                repo: repo.reference(),
                retry_after_secs: retry_after(response.headers()),
            });
        }
        Err(match HubRefusal::of(status) {
            Some(refusal) => HubError::Refused {
                repo: repo.reference(),
                status: refusal,
            },
            None => upstream(repo, format!("status {status}")),
        })
    }

    /// A request permit, logging any wait for one.
    async fn permit(&self, repo: &HfRepo) -> Result<OwnedSemaphorePermit, HubError> {
        if let Ok(permit) = Arc::clone(&self.requests).try_acquire_owned() {
            return Ok(permit);
        }
        let started = tokio::time::Instant::now();
        // The client never closes its semaphore, so this error does not occur.
        let permit = Arc::clone(&self.requests)
            .acquire_owned()
            .await
            .map_err(|_closed| upstream(repo, "the request permits are closed".to_owned()))?;
        tracing::info!(
            waited_ms = crate::queue_maintenance::millis(started.elapsed()),
            "hf request waited for a permit"
        );
        Ok(permit)
    }
}

/// The seconds until HuggingFace accepts requests again: the first `t` of
/// `RateLimit`, else `Retry-After`.
fn retry_after(headers: &HeaderMap) -> Option<u64> {
    let header = |name| headers.get(name).and_then(|value| value.to_str().ok());
    let reset = header(RATE_LIMIT).and_then(|value| {
        value
            .split([';', ','])
            .find_map(|param| param.trim().strip_prefix("t="))
            .and_then(|secs| secs.trim().parse().ok())
    });
    reset.or_else(|| header(RETRY_AFTER.as_str()).and_then(|secs| secs.trim().parse().ok()))
}

/// The end of the message of a [`HubError::RateLimited`].
fn retry_hint(retry_after_secs: &Option<u64>) -> String {
    match retry_after_secs {
        Some(secs) => format!("Retry after {secs} seconds"),
        None => "Retry later".to_owned(),
    }
}

fn upstream(repo: &HfRepo, reason: String) -> HubError {
    HubError::Upstream {
        repo: repo.reference(),
        reason,
    }
}

#[cfg(test)]
mod tests {
    use reqwest::header::HeaderValue;
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::open(
        r#"{"sha": "x", "siblings": [], "private": false, "gated": false}"#,
        Some(false),
        Some(HubGate::Open)
    )]
    #[case::auto(r#"{"sha": "x", "siblings": [], "private": false, "gated": "auto"}"#, Some(false), Some(HubGate::Gated("auto".to_owned())))]
    #[case::manual(r#"{"sha": "x", "siblings": [], "private": false, "gated": "manual"}"#, Some(false), Some(HubGate::Gated("manual".to_owned())))]
    #[case::flag(r#"{"sha": "x", "siblings": [], "private": true, "gated": true}"#, Some(true), Some(HubGate::Gated("true".to_owned())))]
    #[case::unreported(r#"{"sha": "x", "siblings": []}"#, None, None)]
    fn the_visibility_comes_from_the_model_info(
        #[case] body: &str,
        #[case] private: Option<bool>,
        #[case] gated: Option<HubGate>,
    ) -> anyhow::Result<()> {
        let info: ModelInfo = serde_json::from_str(body)?;
        assert_eq!((info.private, info.gated), (private, gated));
        Ok(())
    }

    #[rstest]
    #[case::rate_limit(Some("\"api\";r=0;t=55"), None, Some(55))]
    #[case::two_policies(Some("\"api\";r=0;t=55, \"resolvers\";r=1;t=10"), None, Some(55))]
    #[case::retry_after(None, Some("30"), Some(30))]
    #[case::both(Some("\"api\";r=0;t=55"), Some("30"), Some(55))]
    #[case::neither(None, None, None)]
    #[case::rate_limit_without_reset(Some("\"api\";r=0"), Some("30"), Some(30))]
    #[case::malformed(Some("\"api\";t=soon"), Some("Wed, 21 Oct 2026 07:28:00 GMT"), None)]
    fn the_wait_comes_from_the_headers(
        #[case] rate_limit: Option<&'static str>,
        #[case] retry: Option<&'static str>,
        #[case] expected: Option<u64>,
    ) -> anyhow::Result<()> {
        let mut headers = HeaderMap::new();
        if let Some(value) = rate_limit {
            headers.insert(RATE_LIMIT, HeaderValue::from_str(value)?);
        }
        if let Some(value) = retry {
            headers.insert(RETRY_AFTER, HeaderValue::from_str(value)?);
        }
        assert_eq!(retry_after(&headers), expected);
        Ok(())
    }
}
