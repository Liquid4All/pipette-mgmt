//! Typed DTOs and thin `reqwest` wrappers for the scoring service's HTTP
//! contract, which `docs/scoring-service.md` describes.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use pipette_plan_types::{ChatMessage, NonEmptyTrimmedString};

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

/// Wire-only DTO for a `POST /score` sample. Of the client metadata only
/// `stop_reason` is forwarded: with `enable_thinking`, it tells a sample cut
/// mid-reasoning at the token cap from a finished answer.
#[derive(Serialize)]
pub struct ScoreRequestSample<'a> {
    pub id: &'a str,
    pub completion: &'a str,
    /// Omitted when the client reported none, keeping the body `{id, completion}`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<&'a str>,
}

/// Body of `POST /score`.
#[derive(Serialize)]
pub struct ScoreRequest<'a> {
    pub eval_id: &'a str,
    pub dataset_name: &'a str,
    pub completions: &'a [ScoreRequestSample<'a>],
    /// The `enable_thinking` the job's model flags asked for; omitted when unknown,
    /// and `/score` then grades as it did before the key existed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enable_thinking: Option<bool>,
    /// A vlm_eval's or an asr_eval's prompt profile: the scored samples carry
    /// the prompts the device was sent (an asr_eval's also picks its answer
    /// cleanup). Omitted when the entry names none; an asr_eval always does.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_profile: Option<&'a str>,
    /// The digest of the samples a vlm_eval's or an asr_eval's corpus holds
    /// (`sha256:<hex>`): `/score` refuses (`409`) when the evals server no
    /// longer serves those samples. Omitted for a text eval.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub samples_digest: Option<String>,
}

/// Response of `POST /score`.
#[derive(Deserialize, Serialize)]
pub struct ScoreResponse {
    pub runtime_version: String,
    pub scored_samples: Vec<ScoredSample>,
    /// Eval-specific aggregate metrics, logged by mgmt; `BTreeMap` so the log key
    /// order is deterministic.
    #[serde(default)]
    pub context: BTreeMap<String, serde_json::Value>,
}

#[derive(Deserialize, Serialize)]
pub struct ScoredSample {
    /// Must match a request id; non-empty so a blank upstream id fails at
    /// deserialize, not when the warehouse row is built.
    pub id: NonEmptyTrimmedString,
    pub messages: Vec<ChatMessage>,
    pub completion: String,
    pub is_correct: bool,
    /// An asr_eval sample's error counts and normalized texts; absent from the
    /// accuracy evals' scorers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measures: Option<serde_json::Value>,
    /// The sample's chat-template arguments as the evals server serves them
    /// (an asr_eval Voxtral sample's transcription mode and language), so the
    /// stored row shows the templating request; absent when it has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_template_kwargs: Option<serde_json::Map<String, serde_json::Value>>,
}

/// Response of `GET /evals/{eval_id}/datasets/{dataset_name}/samples`.
#[derive(Deserialize)]
pub struct SamplesResponse {
    pub samples: Vec<EvalSample>,
}

#[derive(Deserialize, Serialize)]
pub struct EvalSample {
    /// Non-empty, so a blank id cannot reach the eval catalog or payloads.
    pub id: NonEmptyTrimmedString,
    pub messages: Vec<ChatMessage>,
    /// Per-sample template arguments (an asr_eval Voxtral sample's language).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_template_kwargs: Option<serde_json::Map<String, serde_json::Value>>,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Failure of a scoring-service HTTP call. [`ScoringError::Unreachable`] and
/// [`ScoringError::Unavailable`] make the scoring cron stop and retry the batch
/// later, rather than spend a timeout per submission; any other failure fails
/// just that submission.
#[derive(Debug, thiserror::Error)]
pub enum ScoringError {
    #[error("scoring service unreachable: {0}")]
    Unreachable(#[source] reqwest::Error),
    /// A `502`, `503` or `504`: the service, or a gateway in front of it, is
    /// down or overloaded.
    #[error("scoring service unavailable: {message}")]
    Unavailable {
        status: reqwest::StatusCode,
        message: String,
    },
    #[error("{0}")]
    Other(String),
}

impl ScoringError {
    pub fn is_unreachable(&self) -> bool {
        matches!(
            self,
            ScoringError::Unreachable(_) | ScoringError::Unavailable { .. }
        )
    }

    /// The call ran out of time, which may be the one job's size as much as
    /// the service: unlike a refused connection or a `503`, it is the job's to
    /// count.
    pub fn is_timeout(&self) -> bool {
        match self {
            ScoringError::Unreachable(e) => e.is_timeout(),
            ScoringError::Unavailable { status, .. } => {
                *status == reqwest::StatusCode::GATEWAY_TIMEOUT
            }
            ScoringError::Other(_) => false,
        }
    }
}

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

/// Max **bytes** of an error body echoed into mgmt errors: enough for the
/// scorer's `detail`. The cut lands on a char boundary, so may be a few short.
const ERROR_BODY_MAX: usize = 500;

async fn read_error_body(resp: reqwest::Response) -> String {
    let body = resp
        .text()
        .await
        .inspect_err(|e| {
            tracing::warn!(
                error = %e,
                "failed to read the evals server error body; reporting the status alone"
            );
        })
        .unwrap_or_default();
    if body.len() <= ERROR_BODY_MAX {
        return body;
    }
    // `get` refuses a cut inside a multi-byte char; 0 always succeeds.
    let head = (0..=ERROR_BODY_MAX)
        .rev()
        .find_map(|cut| body.get(..cut))
        .unwrap_or_default();
    format!("{head}… ({} bytes truncated)", body.len() - head.len())
}

/// `POST /score`, returning the typed response.
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
        let message = format!("evals server POST /score returned {status}: {body}");
        return Err(match status {
            reqwest::StatusCode::BAD_GATEWAY
            | reqwest::StatusCode::SERVICE_UNAVAILABLE
            | reqwest::StatusCode::GATEWAY_TIMEOUT => ScoringError::Unavailable { status, message },
            _ => ScoringError::Other(message),
        });
    }

    resp.json::<ScoreResponse>()
        .await
        .map_err(|e| ScoringError::Other(format!("failed to parse /score response: {e}")))
}

/// `GET /evals/{eval_id}/datasets/{dataset_name}/samples`: a text eval's
/// prompts. A vlm_eval's reach a client as a corpus, packed by
/// `dataset-cache` from the same route with its prompt profile.
pub async fn fetch_samples(
    http_client: &reqwest::Client,
    base_url: &str,
    eval_id: &str,
    dataset_name: &str,
) -> Result<SamplesResponse, ScoringError> {
    let url = samples_url(base_url, eval_id, dataset_name)?;
    let resp = http_client
        .get(url)
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

fn samples_url(
    base_url: &str,
    eval_id: &str,
    dataset_name: &str,
) -> Result<reqwest::Url, ScoringError> {
    let invalid =
        |why: &str| ScoringError::Other(format!("invalid evals server samples url: {why}"));
    if let Some(bad) = [eval_id, dataset_name]
        .into_iter()
        .find(|s| !crate::http_client::is_plain_path_segment(s))
    {
        return Err(invalid(&format!("path segment {bad:?}")));
    }
    let mut url = reqwest::Url::parse(base_url).map_err(|e| invalid(&e.to_string()))?;
    url.path_segments_mut()
        .map_err(|()| invalid("base url cannot take a path"))?
        .pop_if_empty()
        .extend(["evals", eval_id, "datasets", dataset_name, "samples"]);
    Ok(url)
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use pipette_plan_types::SampleCompletion;

    use super::*;

    /// A submission without the new keys still deserializes, so old clients work.
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

    /// The `/score` body, byte for byte: without a stop reason,
    /// `enable_thinking` or a prompt profile it is the body sent before any of
    /// those keys existed.
    #[rstest]
    #[case::none_known(
        None,
        None,
        None,
        None,
        r#"{"eval_id":"math_500","dataset_name":"2026.06.1","completions":[{"id":"s1","completion":"B"}]}"#
    )]
    #[case::both_known(
        Some("truncated"),
        Some(true),
        None,
        None,
        r#"{"eval_id":"math_500","dataset_name":"2026.06.1","completions":[{"id":"s1","completion":"B","stop_reason":"truncated"}],"enable_thinking":true}"#
    )]
    #[case::thinking_off(
        Some("eos"),
        Some(false),
        None,
        None,
        r#"{"eval_id":"math_500","dataset_name":"2026.06.1","completions":[{"id":"s1","completion":"B","stop_reason":"eos"}],"enable_thinking":false}"#
    )]
    #[case::prompt_profile(
        None,
        None,
        Some("internvl"),
        None,
        r#"{"eval_id":"math_500","dataset_name":"2026.06.1","completions":[{"id":"s1","completion":"B"}],"prompt_profile":"internvl"}"#
    )]
    #[case::samples_digest(
        None,
        None,
        Some("internvl"),
        Some("sha256:1bcd"),
        r#"{"eval_id":"math_500","dataset_name":"2026.06.1","completions":[{"id":"s1","completion":"B"}],"prompt_profile":"internvl","samples_digest":"sha256:1bcd"}"#
    )]
    fn score_request_serializes_optional_keys_only_when_known(
        #[case] stop_reason: Option<&str>,
        #[case] enable_thinking: Option<bool>,
        #[case] prompt_profile: Option<&str>,
        #[case] samples_digest: Option<&str>,
        #[case] expected: &str,
    ) -> anyhow::Result<()> {
        let completions = [ScoreRequestSample {
            id: "s1",
            completion: "B",
            stop_reason,
        }];
        let req = ScoreRequest {
            eval_id: "math_500",
            dataset_name: "2026.06.1",
            completions: &completions,
            enable_thinking,
            prompt_profile,
            samples_digest: samples_digest.map(str::to_owned),
        };
        assert_eq!(serde_json::to_string(&req)?, expected);
        Ok(())
    }

    /// A vision sample is proxied to the device as the evals server wrote it.
    #[test]
    fn a_vision_sample_round_trips() -> anyhow::Result<()> {
        let sample = r#"{"id":"s1","messages":[{"role":"user","content":[{"type":"text","text":"Which shape is red?"},{"type":"image_url","image_url":{"url":"data:image/png;base64,iVBO"}}]}]}"#;
        let response: SamplesResponse =
            serde_json::from_str(&format!(r#"{{"samples":[{sample}]}}"#))?;
        assert_eq!(
            serde_json::to_string(&response.samples)?,
            format!("[{sample}]")
        );
        Ok(())
    }

    /// An asr_eval scored sample keeps its measures.
    #[test]
    fn a_scored_sample_keeps_its_measures() -> anyhow::Result<()> {
        let body = r#"{"runtime_version":"1","scored_samples":[{"id":"s1","messages":[],"completion":"hallo","is_correct":false,"measures":{"substitutions":1,"reference_units":2}}]}"#;
        let response: ScoreResponse = serde_json::from_str(body)?;
        assert_eq!(
            response.scored_samples[0].measures,
            Some(serde_json::json!({"substitutions": 1, "reference_units": 2}))
        );
        Ok(())
    }

    /// A catalog id cannot leave its path segment or add a query.
    #[rstest]
    #[case::plain(
        "http://evals:8000",
        "math_500",
        "2026.06.1",
        "http://evals:8000/evals/math_500/datasets/2026.06.1/samples"
    )]
    #[case::trailing_slash(
        "http://evals:8000/",
        "math_500",
        "2026.06.1",
        "http://evals:8000/evals/math_500/datasets/2026.06.1/samples"
    )]
    #[case::percent_dot_dot(
        "http://evals:8000",
        "%2e%2e",
        "%2E.",
        "http://evals:8000/evals/%252e%252e/datasets/%252E./samples"
    )]
    #[case::slashes_and_query(
        "http://evals:8000",
        "../admin?x=1#",
        "a/b",
        "http://evals:8000/evals/..%2Fadmin%3Fx=1%23/datasets/a%2Fb/samples"
    )]
    fn the_samples_url_names_the_eval_and_dataset(
        #[case] base_url: &str,
        #[case] eval_id: &str,
        #[case] dataset_name: &str,
        #[case] expected: &str,
    ) -> anyhow::Result<()> {
        let url = samples_url(base_url, eval_id, dataset_name)?;
        assert_eq!(url.as_str(), expected);
        Ok(())
    }

    #[rstest]
    #[case::dot_dot_eval("..", "ds")]
    #[case::dot_dataset("math_500", ".")]
    #[case::empty_eval("", "ds")]
    #[case::tab_dot_dot(".\t.", "ds")]
    #[case::newline_dot_dot("math_500", "..\n")]
    #[case::lone_tab("\t", "ds")]
    fn a_dot_or_empty_samples_url_segment_is_refused(
        #[case] eval_id: &str,
        #[case] dataset_name: &str,
    ) {
        assert!(samples_url("http://evals:8000", eval_id, dataset_name).is_err());
    }

    /// A part type no device could send fails the samples proxy and the
    /// `/score` parse alike.
    #[test]
    fn an_unknown_part_type_is_refused() {
        let messages =
            r#"[{"role":"user","content":[{"type":"input_video","input_video":{"data":""}}]}]"#;
        let samples = format!(r#"{{"samples":[{{"id":"s1","messages":{messages}}}]}}"#);
        assert!(serde_json::from_str::<SamplesResponse>(&samples).is_err());
        let scored = format!(
            r#"{{"runtime_version":"1","scored_samples":[{{"id":"s1","messages":{messages},"completion":"B","is_correct":true}}]}}"#
        );
        assert!(serde_json::from_str::<ScoreResponse>(&scored).is_err());
    }
}
