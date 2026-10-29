//! The progress stream of the routes that file a plan, opted into with
//! `Accept: application/x-ndjson`: `200` at once, then one [`IngestEvent`] per
//! line. A dropped body drops the ingest at its current await.

use std::collections::HashMap;
use std::convert::Infallible;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::http::header::{ACCEPT, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use serde::Serialize;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing::Instrument;

use pipette_pipeline::hf_import::{ImportProgress, Progress};
use pipette_plan_types::IngestEvent;

use crate::audit::{Audit, CALLER_LEFT};
use crate::error::AppError;

const NDJSON: &str = "application/x-ndjson";

/// The longest time between two lines: under a proxy's idle timeout (60 s on an
/// AWS load balancer).
const KEEPALIVE_EVERY: Duration = Duration::from_secs(15);

/// The least time between two `progress` lines for one file.
const PROGRESS_EVERY: Duration = Duration::from_secs(1);

/// Whether one `Accept` media range is `application/x-ndjson`; `*/*` asks for
/// the plain answer.
pub(crate) fn requested(headers: &HeaderMap) -> bool {
    accepts(headers, NDJSON)
}

/// Whether one `Accept` media range names `media` itself. A wildcard range
/// such as `*/*` names no type, so it asks for a route's default answer.
pub(crate) fn accepts(headers: &HeaderMap, media: &str) -> bool {
    headers
        .get_all(ACCEPT)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|range| {
            let mut parts = range.split(';');
            parts
                .next()
                .unwrap_or_default()
                .trim()
                .eq_ignore_ascii_case(media)
                && parts.all(|parameter| {
                    let Some((name, value)) = parameter.trim().split_once('=') else {
                        return true;
                    };
                    !name.trim().eq_ignore_ascii_case("q")
                        || value
                            .trim()
                            .parse::<f32>()
                            .is_ok_and(|quality| quality > 0.0)
                })
        })
}

pub(crate) async fn answer<T, F>(
    stream: bool,
    audit: Audit,
    work: impl FnOnce(Progress) -> F,
) -> Result<Response, AppError>
where
    T: Serialize + Send + 'static,
    F: Future<Output = Result<(StatusCode, T), AppError>> + Send + 'static,
{
    if stream {
        return Ok(respond(audit, work));
    }
    let (status, body) = work(Progress::Silent).await?;
    Ok((status, axum::Json(body)).into_response())
}

/// Answer `200` with the stream, and run `work` on its own task. The audit event
/// takes the status of the last line of the stream, unless the writes that
/// `work` detached report their own status.
pub(crate) fn respond<T, F>(audit: Audit, work: impl FnOnce(Progress) -> F) -> Response
where
    T: Serialize + Send + 'static,
    F: Future<Output = Result<(StatusCode, T), AppError>> + Send + 'static,
{
    let (progress, events) = mpsc::channel(64);
    let (lines, body) = mpsc::channel(16);
    let work = work(Progress::Channel(progress));
    tracing::info!("ingest answers as an ndjson stream");
    audit.stream();
    let spawner = audit.clone();
    let task = async move {
        let status = write_lines(work, events, lines).await;
        audit.settle(status);
    };
    drop(spawner.spawn(task.instrument(tracing::Span::current())));
    // Fused: the gzip layer polls the body again after it ends.
    let body = futures::stream::unfold(body, |mut body: mpsc::Receiver<Bytes>| async move {
        body.recv()
            .await
            .map(|line| (Ok::<_, Infallible>(line), body))
    })
    .fuse();
    (
        [(CONTENT_TYPE, HeaderValue::from_static(NDJSON))],
        Body::from_stream(body),
    )
        .into_response()
}

/// The status of the last line, or [`CALLER_LEFT`] when there was none.
async fn write_lines<T, F>(
    work: F,
    mut events: mpsc::Receiver<ImportProgress>,
    lines: mpsc::Sender<Bytes>,
) -> u16
where
    T: Serialize,
    F: Future<Output = Result<(StatusCode, T), AppError>>,
{
    let mut work = std::pin::pin!(work);
    let mut throttle = Throttle::default();
    let mut last_line = Instant::now();
    let mut events_open = true;
    let answer = loop {
        tokio::select! {
            biased;
            () = lines.closed() => {
                tracing::info!("ingest stream closed by the caller; the ingest stopped");
                return CALLER_LEFT;
            }
            event = events.recv(), if events_open => match event {
                Some(event) => {
                    if throttle.due(&event, Instant::now()) && put_progress(&lines, event).await {
                        last_line = Instant::now();
                    }
                }
                None => events_open = false,
            },
            answer = &mut work => break answer,
            () = tokio::time::sleep_until(last_line + KEEPALIVE_EVERY) => {
                // A full channel holds lines that the caller did not read yet,
                // so the connection is not idle. The timer restarts either way.
                offer(&lines, &IngestEvent::<()>::Keepalive);
                last_line = Instant::now();
            }
        }
    };
    // The work can complete with events still in the channel, such as the
    // event at which the last file completes.
    while let Ok(event) = events.try_recv() {
        if throttle.due(&event, Instant::now()) {
            put_progress(&lines, event).await;
        }
    }

    let last = match answer {
        Ok((status, body)) => encode(&IngestEvent::Result {
            status: status.as_u16(),
            body,
        })
        .map(|line| (status, line))
        .map_err(|e| {
            AppError::internal(anyhow::Error::new(e).context("failed to encode the ingest result"))
        }),
        Err(e) => Err(e),
    };
    let last = last.or_else(|e| {
        e.log_if_server_error();
        encode(&IngestEvent::<()>::Error {
            status: e.status().as_u16(),
            error: e.message().to_owned(),
        })
        .map(|line| (e.status(), line))
    });
    match last {
        Ok((status, line)) => {
            if lines.send(line).await.is_err() {
                tracing::info!("ingest stream closed by the caller before the last line");
            }
            status.as_u16()
        }
        Err(e) => {
            tracing::error!(error = %e, "failed to encode the ingest error line");
            StatusCode::INTERNAL_SERVER_ERROR.as_u16()
        }
    }
}

fn progress(event: ImportProgress) -> IngestEvent<()> {
    IngestEvent::Progress {
        hf_ref: event.hf_ref,
        file: event.file,
        bytes_done: event.bytes_done,
        bytes_total: event.bytes_total,
    }
}

/// The line at which a file completes waits for room, so the caller sees each
/// file complete; an earlier one goes through [`offer`].
async fn put_progress(lines: &mpsc::Sender<Bytes>, event: ImportProgress) -> bool {
    if event.bytes_done < event.bytes_total {
        return offer(lines, &progress(event));
    }
    let Ok(line) = encode(&progress(event)) else {
        return false;
    };
    lines.send(line).await.is_ok()
}

/// Put `event` on `lines` if there is room: a slow reader loses the line for
/// good rather than stalling the task.
fn offer(lines: &mpsc::Sender<Bytes>, event: &IngestEvent<()>) -> bool {
    encode(event)
        .ok()
        .is_some_and(|line| lines.try_send(line).is_ok())
}

/// `event` as one line of JSON, with its newline.
fn encode<B: Serialize>(event: &IngestEvent<B>) -> Result<Bytes, serde_json::Error> {
    let mut line = serde_json::to_vec(event)?;
    line.push(b'\n');
    Ok(line.into())
}

/// When each file last got a `progress` line.
#[derive(Default)]
struct Throttle {
    last: HashMap<(String, String), Instant>,
}

impl Throttle {
    fn due(&mut self, event: &ImportProgress, now: Instant) -> bool {
        let key = (event.hf_ref.clone(), event.file.clone());
        let due = event.bytes_done >= event.bytes_total
            || self
                .last
                .get(&key)
                .is_none_or(|at| now.duration_since(*at) >= PROGRESS_EVERY);
        if due {
            self.last.insert(key, now);
        }
        due
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Context;
    use futures::TryStreamExt;
    use rstest::rstest;
    use serde_json::{Value, json};
    use tokio::sync::oneshot;

    use super::*;

    /// The lines of `response`, each parsed, until the body ends.
    async fn lines(response: Response) -> anyhow::Result<Vec<Value>> {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
        bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| Ok(serde_json::from_slice(line)?))
            .collect()
    }

    fn event(bytes_done: u64) -> ImportProgress {
        ImportProgress {
            hf_ref: "a/b@0123".into(),
            file: "m.gguf".into(),
            bytes_done,
            bytes_total: 100,
        }
    }

    #[rstest]
    #[case::exact(Some("application/x-ndjson"), true)]
    #[case::case_and_parameters(Some("Application/X-NDJSON; q=0.9"), true)]
    #[case::zero_quality(Some("application/x-ndjson;q=0"), false)]
    #[case::zero_quality_in_list(Some("application/x-ndjson;q=0, application/json"), false)]
    #[case::in_a_list(Some("application/json, application/x-ndjson"), true)]
    #[case::wildcard(Some("*/*"), false)]
    #[case::json(Some("application/json"), false)]
    #[case::absent(None, false)]
    fn the_accept_header_opts_in(#[case] accept: Option<&'static str>, #[case] expected: bool) {
        let headers: HeaderMap = accept
            .map(|value| (ACCEPT, HeaderValue::from_static(value)))
            .into_iter()
            .collect();
        assert_eq!(requested(&headers), expected);
    }

    #[rstest]
    #[case::filed(
        Ok((StatusCode::CREATED, json!({"plan_id": "plan-x"}))),
        json!({"event": "result", "status": 201, "body": {"plan_id": "plan-x"}})
    )]
    #[case::rejected(
        Err(AppError::BadRequest("rejected job".into())),
        json!({"event": "error", "status": 400, "error": "rejected job"})
    )]
    #[case::redacted(
        Err(AppError::Internal("/srv/pipette: Permission denied".into())),
        json!({"event": "error", "status": 500, "error": "internal server error"})
    )]
    #[tokio::test]
    async fn the_last_line_is_the_plain_answer(
        #[case] answer: Result<(StatusCode, Value), AppError>,
        #[case] expected: Value,
    ) -> anyhow::Result<()> {
        let response = respond(Audit::default(), |_progress| async move { answer });
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(CONTENT_TYPE),
            Some(&HeaderValue::from_static(NDJSON))
        );
        assert_eq!(lines(response).await?, vec![expected]);
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn a_stalled_ingest_keeps_the_connection_open() -> anyhow::Result<()> {
        let response = respond(Audit::default(), |_progress| {
            std::future::pending::<Result<(StatusCode, ()), _>>()
        });
        let mut body = response.into_body().into_data_stream();
        let mut at = Instant::now();
        for _ in 0..3 {
            let line = body.next().await.context("the stream ended")??;
            assert_eq!(
                serde_json::from_slice::<Value>(&line)?,
                json!({"event": "keepalive"})
            );
            assert_eq!(at.elapsed(), KEEPALIVE_EVERY);
            at = Instant::now();
        }
        Ok(())
    }

    /// Many events for one file within `PROGRESS_EVERY` give one line, and the
    /// event at which the file completes gives one more.
    #[tokio::test(start_paused = true)]
    async fn progress_lines_are_thinned() -> anyhow::Result<()> {
        let response = respond(Audit::default(), |progress| async move {
            let Progress::Channel(sender) = progress else {
                return Err(AppError::Internal("no progress channel".into()));
            };
            futures::stream::iter((1..=100).map(event))
                .map(Ok)
                .try_for_each(|event| sender.send(event))
                .await
                .map_err(|e| AppError::internal(anyhow::Error::new(e)))?;
            Ok((StatusCode::CREATED, json!({})))
        });
        let events: Vec<Value> = lines(response)
            .await?
            .into_iter()
            .map(|line| json!([line["event"], line["bytes_done"]]))
            .collect();
        assert_eq!(
            events,
            vec![
                json!(["progress", 1]),
                json!(["progress", 100]),
                json!(["result", null]),
            ]
        );
        Ok(())
    }

    #[tokio::test]
    async fn each_file_completes_in_a_full_channel() -> anyhow::Result<()> {
        const FILES: usize = 40;
        let response = respond(Audit::default(), |progress| async move {
            let Progress::Channel(sender) = progress else {
                return Err(AppError::Internal("no progress channel".into()));
            };
            futures::stream::iter(0..FILES)
                .map(|n| {
                    Ok(ImportProgress {
                        hf_ref: "a/b@0123".into(),
                        file: format!("{n}.gguf"),
                        bytes_done: 100,
                        bytes_total: 100,
                    })
                })
                .try_for_each(|event| sender.send(event))
                .await
                .map_err(|e| AppError::internal(anyhow::Error::new(e)))?;
            Ok((StatusCode::CREATED, json!({})))
        });
        let completed = lines(response)
            .await?
            .iter()
            .filter(|line| line["event"] == "progress")
            .count();
        assert_eq!(completed, FILES);
        Ok(())
    }

    /// A gzipped line still arrives while the ingest runs: a browser always
    /// asks for gzip, so the console's progress lines depend on it.
    #[tokio::test(start_paused = true)]
    async fn a_gzipped_stream_delivers_each_line_at_once() -> anyhow::Result<()> {
        use std::io::Write;

        use axum::http::Request;
        use axum::http::header::{ACCEPT_ENCODING, CONTENT_ENCODING};
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        let app = axum::Router::new()
            .route(
                "/",
                axum::routing::post(|| async {
                    respond(Audit::default(), |_progress| {
                        std::future::pending::<Result<(StatusCode, ()), _>>()
                    })
                }),
            )
            .layer(crate::router::compression());
        let request = Request::post("/")
            .header(ACCEPT_ENCODING, "gzip")
            .body(Body::empty())?;
        let response = app.oneshot(request).await?;
        assert_eq!(
            response.headers().get(CONTENT_ENCODING),
            Some(&HeaderValue::from_static("gzip"))
        );

        let mut body = response.into_body();
        let mut unzipped = flate2::write::GzDecoder::new(Vec::new());
        while !unzipped.get_ref().ends_with(b"\n") {
            let frame = body.frame().await.context("the stream ended")??;
            if let Ok(data) = frame.into_data() {
                unzipped.write_all(&data)?;
                unzipped.flush()?;
            }
        }
        assert_eq!(
            serde_json::from_slice::<Value>(unzipped.get_ref())?,
            json!({"event": "keepalive"})
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_dropped_stream_drops_the_ingest() -> anyhow::Result<()> {
        let (held, dropped) = oneshot::channel::<()>();
        let response = respond(Audit::default(), |_progress| async move {
            let _held = held;
            std::future::pending::<Result<(StatusCode, ()), AppError>>().await
        });
        drop(response);
        assert!(dropped.await.is_err(), "the ingest still runs");
        Ok(())
    }
}
