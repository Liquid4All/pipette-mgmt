//! HTTP contract with the scoring service.
//!
//! See `docs/scoring-service.md` for the contract. Holds typed wire DTOs and
//! thin `reqwest` wrappers so call sites (`score.rs`, `handlers.rs`) don't
//! hand-build URLs or parse `serde_json::Value`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::validated::NonEmptyTrimmedString;

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

/// One completion submitted to mgmt by a pipette client and stored in
/// the incoming submission JSON. The `failed` / `failed_reason` fields
/// (Liquid4All/pipette-clients#103) and `stop_reason` / `stop_detail` /
/// `completion_tokens` are mgmt-internal metadata about the client-side
/// run; they default to absent so submissions from
/// pre-feature clients deserialize
/// cleanly, and they `skip_serializing_if` when unset to keep stored
/// incoming JSON tidy. **They are not part of the scoring service
/// contract** — see `ScoreRequestSample` for the stripped wire shape used
/// upstream.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SampleCompletion {
    /// Sample identifier — required, non-empty. Used for warehouse
    /// row identity and the per-submission completion-id uniqueness
    /// check in `SuccessInput::validate`.
    pub id: NonEmptyTrimmedString,
    /// `""` is meaningful (client reports a sample it ran but
    /// produced no output, typically paired with `failed: true`).
    /// Kept as `String` so the empty case can round-trip cleanly.
    pub completion: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub failed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_reason: Option<String>,
    /// Client-captured stop reason for this sample, one of the canonical
    /// `stop_reason` enum values (`eos` | `truncated` | `doom_loop` |
    /// `failure` | `unknown`); see `docs/scoring-service.md`. `None` when
    /// the client didn't report one.
    ///
    /// Deliberately typed as a free-form `String`, not a validated enum:
    /// detection lives in the producer, and mgmt persists the reported
    /// value as-is so a new enum member added client-side lands in the
    /// warehouse without a mgmt release. Downstream readers own any
    /// bucketing of unexpected values.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    /// Free-form observation behind `stop_reason` — the *why / raw signal*
    /// the client recorded: the crash/transport detail for
    /// `failure`, the unclassified `stop_type` for `unknown`, the trigger
    /// for `doom_loop`; normally empty for a clean `eos` / `truncated`.
    /// Generalizes `failed_reason`. Persisted as-is; `None` when the client
    /// didn't report one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_detail: Option<String>,
    /// Number of completion (output) tokens the client generated for this
    /// sample. Paired with `stop_reason` to distinguish `eos` (< cap) from
    /// `truncated` (== cap). `None` when the client didn't report it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<i64>,
}

/// Wire-only DTO for the `POST /score` request. Intentionally narrower
/// than [`SampleCompletion`]: mgmt strips the client-side metadata
/// (`failed` / `failed_reason` / `stop_reason` / `stop_detail` /
/// `completion_tokens`)
/// because the scoring service has no contract for it — `/score` decides
/// `is_correct` from the completion text alone, and how generation ended
/// is orthogonal to that verdict. Empty-`completion` failed samples are
/// still forwarded so `/score` decides their `is_correct` (almost always
/// `false`); mgmt re-injects the stripped fields when building the
/// per-sample parquet rows.
#[derive(Serialize)]
pub struct ScoreRequestSample<'a> {
    pub id: &'a str,
    pub completion: &'a str,
}

/// Body of `POST /score`.
#[derive(Serialize)]
pub struct ScoreRequest<'a> {
    pub eval_id: &'a str,
    pub dataset_name: &'a str,
    pub completions: &'a [ScoreRequestSample<'a>],
}

/// Response of `POST /score`.
#[derive(Deserialize, Serialize)]
pub struct ScoreResponse {
    pub runtime_version: String,
    pub scored_samples: Vec<ScoredSample>,
    /// Eval-specific aggregate metrics (free-form flat key→value map).
    /// Produced by the scorer; logged by mgmt. `BTreeMap` so log key order
    /// is deterministic across runs.
    #[serde(default)]
    pub context: BTreeMap<String, serde_json::Value>,
}

#[derive(Deserialize, Serialize)]
pub struct ScoredSample {
    /// Sample id echoed back from the scorer — must match a request
    /// id. Non-empty by construction (`NonEmptyTrimmedString`) so a
    /// blank id from upstream is rejected at deserialize, not when
    /// the per-sample warehouse row gets built with a blank
    /// identity later.
    pub id: NonEmptyTrimmedString,
    pub messages: Vec<ChatMessage>,
    pub completion: String,
    pub is_correct: bool,
}

/// Chat message with forward-compatible extra fields — any keys we don't
/// model (e.g. `tool_calls`, `name`) are preserved and re-emitted on serialize
/// so the audit log stores exactly what the scorer sent.
#[derive(Deserialize, Serialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
    /// Forward-compat for fields we don't model (e.g. `tool_calls`, `name`).
    /// `BTreeMap` so the re-serialized Parquet audit log is byte-stable.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// Response of `GET /evals/{eval_id}/datasets/{dataset_name}/samples`.
#[derive(Deserialize)]
pub struct SamplesResponse {
    pub samples: Vec<EvalSample>,
}

#[derive(Deserialize, Serialize)]
pub struct EvalSample {
    /// Sample id supplied by the evals server. Non-empty by
    /// construction so a blank id can't make it into the eval
    /// catalog or downstream payloads.
    pub id: NonEmptyTrimmedString,
    pub messages: Vec<ChatMessage>,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Failure of a scoring-service HTTP call.
///
/// The distinction matters to the scoring cron: an [`Unreachable`] error means
/// the service is *down* (connection refused, DNS failure, or the request timed
/// out), so the cron skips the rest of the run and retries the whole batch on
/// its next invocation rather than burning a full HTTP timeout per submission
/// against a service that isn't answering. Every other failure — a non-success
/// status, a malformed response body — is a per-request [`Other`] error that
/// fails just that submission.
///
/// [`Unreachable`]: ScoringError::Unreachable
/// [`Other`]: ScoringError::Other
#[derive(Debug, thiserror::Error)]
pub enum ScoringError {
    /// The scoring service could not be reached at all. Carries the
    /// underlying transport error for logging.
    #[error("scoring service unreachable: {0}")]
    Unreachable(#[source] reqwest::Error),
    /// The service was reached but the call failed (bad status, malformed
    /// body, contract violation). The message carries the detail; there's no
    /// machine-readable structure callers need beyond the [`Unreachable`]
    /// distinction.
    ///
    /// [`Unreachable`]: ScoringError::Unreachable
    #[error("{0}")]
    Other(String),
}

impl ScoringError {
    /// True when the service was unreachable — the signal the cron uses to
    /// pause the run and retry later instead of marking the submission failed.
    pub fn is_unreachable(&self) -> bool {
        matches!(self, ScoringError::Unreachable(_))
    }
}

/// Classify a `reqwest` send error: a connect or timeout failure means the
/// service is unreachable; anything else is a generic per-request failure.
fn classify_send_error(e: reqwest::Error) -> ScoringError {
    if e.is_connect() || e.is_timeout() {
        ScoringError::Unreachable(e)
    } else {
        ScoringError::Other(format!("failed to reach evals server: {e}"))
    }
}

// ---------------------------------------------------------------------------
// HTTP calls
// ---------------------------------------------------------------------------

/// Max **bytes** of an error response body to echo into mgmt errors — enough
/// to carry the scorer's `detail` message, short enough to keep logs sane.
/// Truncation respects UTF-8 char boundaries, so the visible cutoff may be a
/// few bytes below this for multi-byte text.
const ERROR_BODY_MAX: usize = 500;

async fn read_error_body(resp: reqwest::Response) -> String {
    let body = resp.text().await.unwrap_or_default();
    if body.len() <= ERROR_BODY_MAX {
        return body;
    }
    // Walk backward from ERROR_BODY_MAX to the previous char boundary so
    // slicing doesn't panic on multi-byte scripts. 0 is always a boundary,
    // so this never fails.
    let cut = (0..=ERROR_BODY_MAX)
        .rev()
        .find(|&i| body.is_char_boundary(i))
        .expect("0 is always a char boundary");
    format!("{}… ({} bytes truncated)", &body[..cut], body.len() - cut)
}

/// `POST /score` — fire the scoring request and return the typed response.
///
/// Returns [`ScoringError::Unreachable`] when the service can't be reached so
/// the cron can pause and retry; all other failures are [`ScoringError::Other`].
pub async fn score(
    http_client: &reqwest::Client,
    base_url: &str,
    req: &ScoreRequest<'_>,
) -> Result<ScoreResponse, ScoringError> {
    let url = format!("{base_url}/score");
    let resp = http_client
        .post(&url)
        .json(req)
        .send()
        .await
        .map_err(classify_send_error)?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = read_error_body(resp).await;
        return Err(ScoringError::Other(format!(
            "evals server POST /score returned {status}: {body}"
        )));
    }

    resp.json::<ScoreResponse>()
        .await
        .map_err(|e| ScoringError::Other(format!("failed to parse /score response: {e}")))
}

/// `GET /evals/{eval_id}/datasets/{dataset_name}/samples` — fetch prompts.
///
/// Like [`score`], distinguishes an unreachable service from other failures.
pub async fn fetch_samples(
    http_client: &reqwest::Client,
    base_url: &str,
    eval_id: &str,
    dataset_name: &str,
) -> Result<SamplesResponse, ScoringError> {
    let url = format!("{base_url}/evals/{eval_id}/datasets/{dataset_name}/samples");
    let resp = http_client
        .get(&url)
        .send()
        .await
        .map_err(classify_send_error)?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = read_error_body(resp).await;
        return Err(ScoringError::Other(format!(
            "evals server GET samples returned {status}: {body}"
        )));
    }

    resp.json::<SamplesResponse>()
        .await
        .map_err(|e| ScoringError::Other(format!("failed to parse samples response: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    /// A client submitting the new keys must round-trip them; a pre-feature
    /// submission that omits them must still deserialize (fields → `None`),
    /// so old clients keep working.
    #[rstest]
    #[case(
        r#"{"id":"s1","completion":"B","stop_reason":"truncated","completion_tokens":8192}"#,
        Some("truncated"),
        None,
        Some(8192)
    )]
    #[case(
        r#"{"id":"s1","completion":"","stop_reason":"unknown","stop_detail":"stop_type=word"}"#,
        Some("unknown"),
        Some("stop_type=word"),
        None
    )]
    #[case(r#"{"id":"s1","completion":"B"}"#, None, None, None)]
    fn sample_completion_deserializes_stop_reason_fields(
        #[case] json: &str,
        #[case] expected_reason: Option<&str>,
        #[case] expected_detail: Option<&str>,
        #[case] expected_tokens: Option<i64>,
    ) -> anyhow::Result<()> {
        let c: SampleCompletion = serde_json::from_str(json)?;
        assert_eq!(c.stop_reason.as_deref(), expected_reason, "json: {json}");
        assert_eq!(c.stop_detail.as_deref(), expected_detail, "json: {json}");
        assert_eq!(c.completion_tokens, expected_tokens, "json: {json}");
        assert!(!c.failed);
        Ok(())
    }

    #[test]
    fn sample_completion_elides_unset_stop_reason_fields_on_serialize() -> anyhow::Result<()> {
        // Unset optional fields must not bloat stored incoming JSON.
        let c = SampleCompletion {
            id: NonEmptyTrimmedString::try_new("s1")?,
            completion: "B".to_string(),
            failed: false,
            failed_reason: None,
            stop_reason: None,
            stop_detail: None,
            completion_tokens: None,
        };
        let json = serde_json::to_string(&c)?;
        assert!(!json.contains("stop_reason"), "got: {json}");
        assert!(!json.contains("stop_detail"), "got: {json}");
        assert!(!json.contains("completion_tokens"), "got: {json}");
        Ok(())
    }
}
