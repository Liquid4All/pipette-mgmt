use std::collections::HashMap;
use std::fmt;
use std::marker::PhantomData;
use std::str::FromStr;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use axum::Json;
use axum::body::{Body, Bytes, HttpBody as _};
use axum::extract::path::ErrorKind;
use axum::extract::rejection::{
    BytesRejection, JsonRejection, MissingJsonContentType, PathRejection, QueryRejection,
};
use axum::extract::{FromRequest, FromRequestParts, Path, Query, RawPathParams, Request};
use axum::http::header::CONTENT_TYPE;
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode};
use serde::de::{DeserializeOwned, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use sha2::Digest;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tower_http::timeout::{TimeoutBody, TimeoutError};

use pipette_plan_types::ClientId;

use crate::auth::SignedContentSha256;
use crate::error::AppError;
use crate::state::AppState;

const BODY_TOO_LARGE: &str = "request body is too large";

/// Larger bodies are hashed on a blocking thread so they don't stall the runtime.
const HASH_INLINE_MAX: usize = 1024 * 1024;

/// `axum::Json` for request bodies, but a failure is a `400` in the
/// `{"error": ...}` envelope rather than axum's plain-text `422`/`415`.
pub struct ApiJson<T>(pub T);

impl<S, T> FromRequest<S> for ApiJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let bytes = json_body(req, state).await?;
        parse_json(&bytes).map(ApiJson)
    }
}

async fn json_body<S: Send + Sync>(req: Request, state: &S) -> Result<Bytes, AppError> {
    // Before the body is read, as `axum::Json` checks it, so a caller
    // refused for its type costs no body.
    if PlanMediaType::of(req.headers()) != Some(PlanMediaType::Json) {
        return Err(AppError::BadRequest(
            MissingJsonContentType::default().body_text(),
        ));
    }
    buffer(req, state).await
}

fn parse_json<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, AppError> {
    Json::<T>::from_bytes(bytes)
        .map(|Json(value)| value)
        .map_err(|rejection: JsonRejection| AppError::BadRequest(rejection.body_text()))
}

/// Submission bodies and parsed trees are counted in 1/1024ths of
/// `submission_body_limit_bytes`.
const CEILING_SHARES: u32 = 1024;

/// Four ceilings, in shares: the most [`AppState::submission_bodies`] holds
/// for bodies, and [`AppState::submissions_in_flight`] for parsed trees.
pub const SUBMISSION_SHARES: usize = 4 * CEILING_SHARES as usize;

/// One ceiling, in shares: the most of [`SUBMISSION_SHARES`] that held bodies,
/// from devices nobody has approved, take at once ([`AppState::held_submission_bodies`]).
pub const HELD_SUBMISSION_SHARES: usize = CEILING_SHARES as usize;

/// A body that sends nothing for this long gives up its share.
const SUBMISSION_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// [`ApiJson`] for a benchmark submission, refused with `413` before its tree
/// is built when the tree would take more than `submission_body_limit_bytes`.
/// The handler binds the permit, the tree's share of the ceiling, for as long
/// as it holds the tree. Extract it after [`Submitter`], which names the
/// device in a [`SubmissionReader`].
///
/// [`Submitter`]: crate::client::benchmarks::submissions::Submitter
pub struct SubmissionJson(pub serde_json::Value, pub OwnedSemaphorePermit);

/// The device whose submission body [`SubmissionJson`] reads, and whether its
/// submission is held for want of approval.
#[derive(Clone)]
pub struct SubmissionReader {
    pub client_id: ClientId,
    pub held: bool,
}

impl FromRequest<AppState> for SubmissionJson {
    type Rejection = AppError;

    async fn from_request(req: Request, state: &AppState) -> Result<Self, Self::Rejection> {
        let limit = state.config.submission_body_limit_bytes.get();
        let SubmissionReader { client_id, held } = req
            .extensions()
            .get::<SubmissionReader>()
            .cloned()
            .ok_or_else(|| AppError::Internal("a submission body without its device".into()))?;
        let device = state.submission_readers.turn(client_id).await?;
        let shares = body_shares(req.body().size_hint().upper(), limit);
        // Pending devices are cheap to register, so held bodies first queue for
        // their own ceiling and never take more than one of the four.
        let held = if held {
            Some(reserve(&state.held_submission_bodies, shares).await?)
        } else {
            None
        };
        let reserved = (held, reserve(&state.submission_bodies, shares).await?);
        let req = req.map(|body| Body::new(TimeoutBody::new(SUBMISSION_IDLE_TIMEOUT, body)));
        let bytes = json_body(req, state).await?;
        let inline = bytes.len() <= HASH_INLINE_MAX;
        // The read's permits move in, so a dropped request frees neither while its bytes live.
        let read = (device, reserved);
        let (spent, bytes, read) = off_the_runtime(inline, move || {
            tree_cost(&bytes, limit).map(|spent| (spent, bytes, read))
        })
        .await?;
        let tree = Arc::clone(&state.submissions_in_flight)
            .acquire_many_owned(ceiling_shares(spent, limit))
            .await
            .map_err(|e| AppError::Internal(format!("waiting to parse the body failed: {e}")))?;
        off_the_runtime(inline, move || {
            let _read = read;
            parse_json(&bytes).map(|body| SubmissionJson(body, tree))
        })
        .await
    }
}

async fn reserve(pool: &Arc<Semaphore>, shares: u32) -> Result<OwnedSemaphorePermit, AppError> {
    Arc::clone(pool)
        .acquire_many_owned(shares)
        .await
        .map_err(|e| AppError::Internal(format!("waiting to read the body failed: {e}")))
}

async fn off_the_runtime<T: Send + 'static>(
    inline: bool,
    work: impl FnOnce() -> Result<T, AppError> + Send + 'static,
) -> Result<T, AppError> {
    if inline {
        return work();
    }
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| AppError::Internal(format!("parsing the request body failed: {e}")))?
}

/// The devices reading a submission body, one body at a time each, so one
/// device cannot take every share of [`AppState::submission_bodies`].
#[derive(Default)]
pub struct SubmissionReaders(Mutex<HashMap<ClientId, Arc<Semaphore>>>);

impl SubmissionReaders {
    fn devices(&self) -> MutexGuard<'_, HashMap<ClientId, Arc<Semaphore>>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    async fn turn(self: &Arc<Self>, client_id: ClientId) -> Result<ReaderTurn, AppError> {
        let device = Arc::clone(
            self.devices()
                .entry(client_id.clone())
                .or_insert_with(|| Arc::new(Semaphore::new(1))),
        );
        let permit = device
            .acquire_owned()
            .await
            .map_err(|e| AppError::Internal(format!("waiting to read the body failed: {e}")))?;
        Ok(ReaderTurn {
            readers: Arc::clone(self),
            client_id,
            permit: Some(permit),
        })
    }
}

struct ReaderTurn {
    readers: Arc<SubmissionReaders>,
    client_id: ClientId,
    permit: Option<OwnedSemaphorePermit>,
}

impl Drop for ReaderTurn {
    fn drop(&mut self) {
        let mut devices = self.readers.devices();
        self.permit.take();
        // Only the map's handle left, so no request waits on it.
        if devices
            .get(&self.client_id)
            .is_some_and(|device| Arc::strong_count(device) == 1)
        {
            devices.remove(&self.client_id);
        }
    }
}

/// The [`AppState::submission_bodies`] permits a body of `declared` bytes
/// takes; one of no declared length may run to the whole ceiling.
fn body_shares(declared: Option<u64>, limit: usize) -> u32 {
    let len = declared
        .and_then(|n| usize::try_from(n).ok())
        .map_or(limit, |n| n.min(limit));
    ceiling_shares(len, limit)
}

/// `bytes`' share of a ceiling of `limit` bytes, rounded up.
fn ceiling_shares(bytes: usize, limit: usize) -> u32 {
    let shares = bytes
        .min(limit)
        .saturating_mul(CEILING_SHARES as usize)
        .div_ceil(limit);
    u32::try_from(shares).unwrap_or(CEILING_SHARES)
}

/// The heap the body's `serde_json::Value` tree would take, or `413` past `limit`.
fn tree_cost(bytes: &[u8], limit: usize) -> Result<usize, AppError> {
    let mut spent = 0;
    let costed = TreeCost {
        spent: &mut spent,
        max: limit,
    }
    .deserialize(&mut serde_json::Deserializer::from_slice(bytes));
    // A syntax error is left to the parse, which words it as `ApiJson` does.
    if costed.is_err() && spent > limit {
        return Err(AppError::PayloadTooLarge(format!(
            "request body would take more than {limit} bytes once parsed"
        )));
    }
    Ok(spent)
}

const VALUE: usize = size_of::<serde_json::Value>();

/// A `BTreeMap<String, Value>` internal node, the larger kind: a header, 11
/// entries and 12 edges.
const NODE: usize = 11 * (size_of::<String>() + VALUE) + 12 * size_of::<usize>() + 16;

/// The most an allocator adds to a small string's length.
const ALLOCATION: usize = 32;

/// Walks a JSON document without keeping it, adding up the heap its
/// `serde_json::Value` tree would take, and erring once past `max`.
struct TreeCost<'a> {
    spent: &'a mut usize,
    max: usize,
}

impl TreeCost<'_> {
    fn nested(&mut self) -> TreeCost<'_> {
        TreeCost {
            spent: self.spent,
            max: self.max,
        }
    }

    fn charge<E: serde::de::Error>(&mut self, bytes: usize) -> Result<(), E> {
        *self.spent = self.spent.saturating_add(bytes);
        if *self.spent > self.max {
            return Err(E::custom("too large once parsed"));
        }
        Ok(())
    }
}

impl<'de> DeserializeSeed<'de> for TreeCost<'_> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(mut self, deserializer: D) -> Result<(), D::Error> {
        self.charge::<D::Error>(VALUE)?;
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for TreeCost<'_> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_bool<E>(self, _: bool) -> Result<(), E> {
        Ok(())
    }

    fn visit_i64<E>(self, _: i64) -> Result<(), E> {
        Ok(())
    }

    fn visit_u64<E>(self, _: u64) -> Result<(), E> {
        Ok(())
    }

    fn visit_f64<E>(self, _: f64) -> Result<(), E> {
        Ok(())
    }

    fn visit_str<E: serde::de::Error>(mut self, s: &str) -> Result<(), E> {
        self.charge(s.len() + ALLOCATION)
    }

    fn visit_unit<E>(self) -> Result<(), E> {
        Ok(())
    }

    fn visit_seq<A: SeqAccess<'de>>(mut self, mut seq: A) -> Result<(), A::Error> {
        let mut len: usize = 0;
        while seq.next_element_seed(self.nested())?.is_some() {
            len += 1;
        }
        // Pushed one by one, a `Vec` holds four slots or the next power of two of its length.
        if len == 0 {
            return Ok(());
        }
        self.charge((len.next_power_of_two().max(4) - len) * VALUE)
    }

    fn visit_map<A: MapAccess<'de>>(mut self, mut map: A) -> Result<(), A::Error> {
        let mut len = 0;
        while map.next_key_seed(self.nested())?.is_some() {
            map.next_value_seed(self.nested())?;
            len += 1;
        }
        // A node is allocated whole, and any but the root holds at least five entries.
        if len == 0 {
            return Ok(());
        }
        self.charge(NODE + len * NODE / 5)
    }
}

/// `axum::extract::Query` with [`ApiJson`]'s `400` envelope.
pub struct ApiQuery<T>(pub T);

impl<S, T> FromRequestParts<S> for ApiQuery<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        Query::<T>::from_request_parts(parts, state)
            .await
            .map(|Query(value)| ApiQuery(value))
            .map_err(|rejection: QueryRejection| AppError::BadRequest(rejection.body_text()))
    }
}

/// `axum::extract::Path` with [`ApiJson`]'s `400` envelope. The ids it read
/// are recorded on the request span.
pub struct ApiPath<T>(pub T);

impl<S, T> FromRequestParts<S> for ApiPath<T>
where
    T: DeserializeOwned + Send,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let Path(value) = Path::<T>::from_request_parts(parts, state)
            .await
            .map_err(path_refusal)?;
        record_path_ids(parts, state).await;
        Ok(ApiPath(value))
    }
}

fn path_refusal(rejection: PathRejection) -> AppError {
    // `Parsed` fails inside its visitor, which axum files as a `DeserializeError`;
    // its message already names the id, so axum's `Invalid URL` lead is dropped.
    if let PathRejection::FailedToDeserializePathParams(failed) = &rejection
        && let ErrorKind::DeserializeError { message, .. } = failed.kind()
    {
        return AppError::BadRequest(message.clone());
    }
    // axum files a route whose pattern and type disagree as a `5xx`: that is
    // this service's bug, not the caller's, so it stays a `500`.
    if rejection.status().is_server_error() {
        AppError::Internal(rejection.body_text())
    } else {
        AppError::BadRequest(rejection.body_text())
    }
}

/// A path segment parsed by `T`'s validating `FromStr` and refused with its
/// message; `PlanId`'s and `JobId`'s `Deserialize` take any string.
pub struct Parsed<T>(pub T);

impl<'de, T> Deserialize<'de> for Parsed<T>
where
    T: FromStr,
    T::Err: fmt::Display,
{
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Segment<T>(PhantomData<T>);

        impl<T> Visitor<'_> for Segment<T>
        where
            T: FromStr,
            T::Err: fmt::Display,
        {
            type Value = Parsed<T>;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a path segment")
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                value.parse().map(Parsed).map_err(E::custom)
            }
        }

        deserializer.deserialize_str(Segment(PhantomData))
    }
}

/// Record the path ids the request span has fields for (`client_id` as
/// `target_client_id`). A value with non-id characters stays out of the log.
pub(crate) async fn record_path_ids<S: Send + Sync>(parts: &mut Parts, state: &S) {
    let Ok(params) = RawPathParams::from_request_parts(parts, state).await else {
        return;
    };
    let span = tracing::Span::current();
    params.iter().for_each(|(name, value)| {
        let field = match name {
            "client_id" => "target_client_id",
            "job_id" | "plan_id" | "campaign_id" | "benchmark_id" | "resource_id" => name,
            _ => return,
        };
        // `.` for a versioned benchmark id.
        if value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        {
            span.record(field, tracing::field::display(value));
        }
    });
}

/// The media types a [`DefinitionBody`] or a [`PlanDocument`] reads, by the
/// `Content-Type` essence: case and parameters are ignored.
#[derive(Debug, PartialEq, Eq)]
enum PlanMediaType {
    Json,
    Toml,
}

impl PlanMediaType {
    fn of(headers: &HeaderMap) -> Option<Self> {
        let value = headers.get(CONTENT_TYPE)?.to_str().ok()?;
        let essence = value
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        let (kind, subtype) = essence.split_once('/')?;
        match (kind, subtype) {
            ("application", "toml") => Some(Self::Toml),
            ("application", "json") => Some(Self::Json),
            ("application", subtype) if subtype.ends_with("+json") => Some(Self::Json),
            _ => None,
        }
    }
}

/// An `application/toml` plan document, buffered as UTF-8 text; any other
/// type, JSON included, is a `415`.
pub struct PlanDocument(pub String);

impl<S> FromRequest<S> for PlanDocument
where
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match PlanMediaType::of(req.headers()) {
            Some(PlanMediaType::Toml) => read_document(req, state).await.map(PlanDocument),
            Some(PlanMediaType::Json) | None => Err(AppError::UnsupportedMediaType(
                "send the plan as an application/toml plan document".into(),
            )),
        }
    }
}

/// The body of a benchmark definition write: a JSON definition, or the TOML a
/// catalog file holds. The `Content-Type` decides.
pub enum DefinitionBody<T> {
    Json(T),
    Toml(String),
}

impl<S, T> FromRequest<S> for DefinitionBody<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match PlanMediaType::of(req.headers()) {
            Some(PlanMediaType::Json) => ApiJson::<T>::from_request(req, state)
                .await
                .map(|ApiJson(value)| DefinitionBody::Json(value)),
            Some(PlanMediaType::Toml) => read_document(req, state).await.map(DefinitionBody::Toml),
            None => Err(AppError::UnsupportedMediaType(
                "send the definition as application/json or as the application/toml a catalog \
                 file holds"
                    .into(),
            )),
        }
    }
}

async fn read_document<S: Send + Sync>(req: Request, state: &S) -> Result<String, AppError> {
    let bytes = buffer(req, state).await?;
    String::from_utf8(bytes.into())
        .map_err(|e| AppError::BadRequest(format!("Request body didn't contain valid UTF-8: {e}")))
}

/// Buffer the body whole, holding it to the `Content-Digest` an RFC 9421
/// signature covered. Every buffered body passes here.
async fn buffer<S: Send + Sync>(req: Request, state: &S) -> Result<Bytes, AppError> {
    let signed = req.extensions().get::<SignedContentSha256>().cloned();
    let bytes = Bytes::from_request(req, state)
        .await
        .map_err(|rejection: BytesRejection| match rejection.status() {
            // A size problem, not a syntax one: a `400` would send the caller
            // looking for a malformed field.
            StatusCode::PAYLOAD_TOO_LARGE => AppError::PayloadTooLarge(BODY_TOO_LARGE.into()),
            _ if timed_out(&rejection) => AppError::RequestTimeout(format!(
                "the request body sent nothing for {} seconds",
                SUBMISSION_IDLE_TIMEOUT.as_secs()
            )),
            _ => AppError::BadRequest(rejection.body_text()),
        })?;
    let Some(SignedContentSha256(expected)) = signed else {
        return Ok(bytes);
    };
    let actual = if bytes.len() > HASH_INLINE_MAX {
        let hashed = bytes.clone();
        tokio::task::spawn_blocking(move || sha256_hex(&hashed))
            .await
            .map_err(|e| AppError::Internal(format!("hashing the request body failed: {e}")))?
    } else {
        sha256_hex(&bytes)
    };
    if actual != expected.as_ref() {
        // Logged because the client loses or resends the write; the span names it.
        tracing::warn!(
            expected = %expected,
            actual = %actual,
            "refused: the body does not hash to the signed Content-Digest"
        );
        return Err(AppError::BadRequest(format!(
            "the body hashes to sha-256 {actual}, not the signed Content-Digest sha-256 {expected}"
        )));
    }
    Ok(bytes)
}

/// Whether a body timeout, the server's or a route's own, is in `error`'s chain.
pub(crate) fn timed_out(error: &(dyn std::error::Error + 'static)) -> bool {
    std::iter::successors(Some(error), |e| e.source()).any(|e| e.is::<TimeoutError>())
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(sha2::Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use anyhow::Context;
    use axum::Router;
    use axum::body::Body;
    use axum::http::HeaderValue;
    use axum::response::Response;
    use axum::routing::get;
    use rstest::rstest;
    use tower::ServiceExt;

    use pipette_plan_types::{BenchmarkId, JobId, PlanId};

    use super::*;

    /// A `GET` to `uri`; `/pairs` deliberately wants two segments where its pattern has one.
    async fn get_path(uri: &str) -> anyhow::Result<Response> {
        let app: Router = Router::new()
            .route("/items/{item_id}", get(|_: ApiPath<u32>| async {}))
            .route("/pairs/{item_id}", get(|_: ApiPath<(u32, u32)>| async {}))
            .route(
                "/plans/{plan_id}",
                get(|_: ApiPath<Parsed<PlanId>>| async {}),
            )
            .route(
                "/plans/{plan_id}/jobs/{job_id}",
                get(|_: ApiPath<(Parsed<PlanId>, Parsed<JobId>)>| async {}),
            )
            .route(
                "/benchmarks/{benchmark_id}",
                get(|_: ApiPath<BenchmarkId>| async {}),
            );
        let request = axum::http::Request::get(uri).body(Body::empty())?;
        Ok(app.oneshot(request).await?)
    }

    async fn error_of(response: Response) -> anyhow::Result<String> {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
        let body: serde_json::Value = serde_json::from_slice(&bytes)?;
        body.get("error")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .context("the body carries no error string")
    }

    #[rstest]
    #[case::unparsed("/items/nope", "nope")]
    #[case::undecodable("/items/%FF", "item_id")]
    // An id validated by its own `Deserialize`, not `Parsed`, keeps axum's lead.
    #[case::validated_without_parsed("/benchmarks/_x", "Invalid URL: ")]
    #[tokio::test]
    async fn a_bad_path_segment_is_a_bad_request(
        #[case] uri: &str,
        #[case] named: &str,
    ) -> anyhow::Result<()> {
        let response = get_path(uri).await?;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let error = error_of(response).await?;
        assert!(error.contains(named), "{error}");
        Ok(())
    }

    #[rstest]
    #[case::alone(
        "/plans/bad.id",
        "plan_id contains invalid character '.'; allowed: [A-Za-z0-9-]"
    )]
    #[case::in_a_pair(
        "/plans/plan-1/jobs/job_1",
        "job_id contains invalid character '_'; allowed: [A-Za-z0-9-]"
    )]
    #[tokio::test]
    async fn a_parsed_segment_is_refused_with_its_parse_message(
        #[case] uri: &str,
        #[case] message: &str,
    ) -> anyhow::Result<()> {
        let response = get_path(uri).await?;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(error_of(response).await?, message);
        Ok(())
    }

    #[tokio::test]
    async fn a_path_type_the_route_cannot_fill_is_a_server_error() -> anyhow::Result<()> {
        let response = get_path("/pairs/1").await?;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(error_of(response).await?, "internal server error");
        Ok(())
    }

    #[rstest]
    #[case::toml(Some("application/toml"), Some(PlanMediaType::Toml))]
    #[case::toml_with_charset(Some("Application/TOML; charset=utf-8"), Some(PlanMediaType::Toml))]
    #[case::json(Some("application/json"), Some(PlanMediaType::Json))]
    #[case::json_with_charset(Some("application/json; charset=utf-8"), Some(PlanMediaType::Json))]
    #[case::json_suffix(Some("application/vnd.pipette+json"), Some(PlanMediaType::Json))]
    #[case::text(Some("text/plain"), None)]
    #[case::text_toml(Some("text/toml"), None)]
    #[case::toml_prefix(Some("application/tomlx"), None)]
    #[case::missing(None, None)]
    fn media_type_reads_the_essence(
        #[case] content_type: Option<&'static str>,
        #[case] expected: Option<PlanMediaType>,
    ) {
        let mut headers = HeaderMap::new();
        if let Some(value) = content_type {
            headers.insert(CONTENT_TYPE, HeaderValue::from_static(value));
        }
        assert_eq!(PlanMediaType::of(&headers), expected);
    }

    /// `[1]` is two values and a four-slot `Vec`; one object of as many values
    /// costs a whole node more.
    #[rstest]
    #[case::at_the_budget("[1]", 5 * VALUE, None)]
    #[case::one_past_it("[1]", 5 * VALUE - 1, Some(StatusCode::PAYLOAD_TOO_LARGE))]
    #[case::an_object(r#"{"a": 1}"#, 5 * VALUE, Some(StatusCode::PAYLOAD_TOO_LARGE))]
    #[case::a_string(r#"["abc"]"#, 5 * VALUE + 3 + ALLOCATION, None)]
    #[case::malformed_within_it("[1, 2", NODE, Some(StatusCode::BAD_REQUEST))]
    #[case::malformed_at_the_budget("[1, 2", 3 * VALUE, Some(StatusCode::BAD_REQUEST))]
    fn a_submission_is_refused_past_its_parsed_size(
        #[case] body: &str,
        #[case] limit: usize,
        #[case] refused: Option<StatusCode>,
    ) {
        let status = tree_cost(body.as_bytes(), limit)
            .and_then(|_| parse_json::<serde_json::Value>(body.as_bytes()))
            .err()
            .map(|e| e.status());
        assert_eq!(status, refused, "{body}");
    }

    #[rstest]
    #[case::empty(Some(0), 0)]
    #[case::one_byte(Some(1), 1)]
    #[case::half(Some(512), CEILING_SHARES / 2)]
    #[case::the_ceiling(Some(1024), CEILING_SHARES)]
    #[case::past_it(Some(u64::MAX), CEILING_SHARES)]
    #[case::undeclared(None, CEILING_SHARES)]
    fn a_body_takes_its_share_of_the_ceiling(#[case] declared: Option<u64>, #[case] shares: u32) {
        assert_eq!(body_shares(declared, 1024), shares);
    }
}
