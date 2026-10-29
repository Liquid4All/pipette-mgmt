//! Every `/api/v1` answer a test receives, held to the OpenAPI document. Not
//! requests: tests send ones the server refuses on purpose. Runs outside the
//! router's layers, so it sees the matched route but not a compressed body.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{MatchedPath, Request, State};
use axum::http::header::{CONTENT_ENCODING, CONTENT_TYPE};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use http_body_util::BodyExt as _;
use serde_json::Value;

use pipette_httpapi::api_schema::openapi::OpenApi;

use crate::schema_check::refusals;

/// Where the document describes every error: the envelope.
const ERROR_REPLY: &str = "/components/responses/Error";

/// What an answer did that the document does not allow.
#[derive(Debug, thiserror::Error)]
enum ContractError {
    #[error("is served, and the document has no such operation")]
    NoOperation,
    #[error("answered {0}, which the document does not declare")]
    Undeclared(StatusCode),
    #[error("answered {status} with no content type, where `{reply}` lists one")]
    NoMedia { status: StatusCode, reply: String },
    #[error("answered {status} as `{media}`, which `{reply}` does not list")]
    UnlistedMedia {
        status: StatusCode,
        media: String,
        reply: String,
    },
    #[error("answered {status} with a body that does not read: {source}")]
    Unreadable {
        status: StatusCode,
        source: ReadError,
    },
    #[error("answered {status} with a body `{schema}` refuses: {}\n{answer}", .refusals.join("; "))]
    Refused {
        status: StatusCode,
        schema: String,
        refusals: Vec<String>,
        answer: Value,
    },
}

/// Why an answer's body did not read.
#[derive(Debug, thiserror::Error)]
enum ReadError {
    #[error(transparent)]
    Body(#[from] axum::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Utf8(#[from] std::str::Utf8Error),
    #[error(transparent)]
    Toml(#[from] toml::de::Error),
}

/// The middleware [`crate::test_app`] puts on every route, holding answers to
/// the document the test's state built.
pub(crate) async fn conform(
    State(openapi): State<Arc<OpenApi>>,
    request: Request,
    next: Next,
) -> Response {
    let method = request.method().as_str().to_ascii_lowercase();
    let path = request
        .extensions()
        .get::<MatchedPath>()
        .map(|path| path.as_str().to_owned());
    let Some(path) = path.filter(|path| path.starts_with("/api/v1/")) else {
        return next.run(request).await;
    };
    let response = next.run(request).await;
    match check(openapi.document(), &method, &path, response).await {
        Ok(response) => response,
        Err(violation) => fail(&format!("{method} {path} {violation}")),
    }
}

#[expect(
    clippy::panic,
    reason = "a contract violation fails the test that met it, naming what it was"
)]
fn fail(violation: &str) -> Response {
    panic!("{violation}")
}

async fn check(
    document: &Value,
    method: &str,
    path: &str,
    response: Response,
) -> Result<Response, ContractError> {
    let status = response.status();
    let media = media(response.headers());
    let Some(schema) = expected(document, method, path, status, media.as_deref())? else {
        return Ok(response);
    };
    // Compressed by the router, outside this layer.
    if response.headers().contains_key(CONTENT_ENCODING) {
        return Ok(response);
    }
    let (parts, body) = response.into_parts();
    let bytes = body
        .collect()
        .await
        .map_err(|e| ContractError::Unreadable {
            status,
            source: e.into(),
        })?
        .to_bytes();
    conforms(document, schema, status, media.as_deref(), &bytes)?;
    Ok(Response::from_parts(parts, Body::from(bytes)))
}

/// A `Content-Type`'s media type, without its parameters.
fn media(headers: &HeaderMap) -> Option<String> {
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(|media| media.trim().to_owned())
}

/// Where the document describes the answer, as a pointer: the reply the
/// operation declares for `status`, or the shared one that reply refers to.
fn reply(
    document: &Value,
    method: &str,
    path: &str,
    status: StatusCode,
) -> Result<String, ContractError> {
    let at = |method: &str| format!("/paths/{}/{method}", escape(path));
    // axum answers a `HEAD` from the `GET` handler where the path registers none.
    let operation_at = if method == "head" && document.pointer(&at("head")).is_none() {
        at("get")
    } else {
        at(method)
    };
    if document.pointer(&operation_at).is_none() {
        // A method the path does not register: the router's own `405`.
        return if status == StatusCode::METHOD_NOT_ALLOWED {
            Ok(ERROR_REPLY.to_owned())
        } else {
            Err(ContractError::NoOperation)
        };
    }
    let declared_at = format!("{operation_at}/responses/{}", status.as_u16());
    match document.pointer(&declared_at) {
        // A shared error, such as `NotFound`, by the reference to it.
        Some(reply) => Ok(reply
            .get("$ref")
            .and_then(Value::as_str)
            .and_then(|reference| reference.strip_prefix('#'))
            .map_or(declared_at, str::to_owned)),
        None if status.is_server_error() => Ok(ERROR_REPLY.to_owned()),
        None => Err(ContractError::Undeclared(status)),
    }
}

/// The schema an answer's body must meet, by its pointer, or `None` when the
/// document asks nothing of the body: an empty answer, one neither JSON nor
/// TOML, or a `HEAD`'s.
fn expected<'d>(
    document: &'d Value,
    method: &str,
    path: &str,
    status: StatusCode,
    media: Option<&str>,
) -> Result<Option<(String, &'d Value)>, ContractError> {
    let reply_at = reply(document, method, path, status)?;
    // A `HEAD` carries the headers of a body it does not send.
    if method == "head" {
        return Ok(None);
    }
    let content = document
        .pointer(&format!("{reply_at}/content"))
        .and_then(Value::as_object);
    let Some(media) = media else {
        return match content {
            Some(_) => Err(ContractError::NoMedia {
                status,
                reply: reply_at,
            }),
            None => Ok(None),
        };
    };
    let Some(listed) = content.and_then(|content| content.get(media)) else {
        return Err(ContractError::UnlistedMedia {
            status,
            media: media.to_owned(),
            reply: reply_at,
        });
    };
    if media != JSON && media != TOML {
        return Ok(None);
    }
    let schema_at = format!("{reply_at}/content/{}/schema", escape(media));
    Ok(listed.get("schema").map(|schema| (schema_at, schema)))
}

fn conforms(
    document: &Value,
    (schema_at, schema): (String, &Value),
    status: StatusCode,
    media: Option<&str>,
    body: &[u8],
) -> Result<(), ContractError> {
    let answer =
        read(media, body).map_err(|source| ContractError::Unreadable { status, source })?;
    let found = refusals(document, schema, &answer);
    if found.is_empty() {
        Ok(())
    } else {
        Err(ContractError::Refused {
            status,
            schema: schema_at,
            refusals: found,
            answer,
        })
    }
}

const JSON: &str = "application/json";
const TOML: &str = "application/toml";

/// A body as the data its schema describes.
fn read(media: Option<&str>, body: &[u8]) -> Result<Value, ReadError> {
    if media == Some(TOML) {
        return Ok(crate::toml_document(std::str::from_utf8(body)?)?);
    }
    Ok(serde_json::from_slice(body)?)
}

/// `token` as one JSON pointer segment.
fn escape(token: &str) -> String {
    token.replace('~', "~0").replace('/', "~1")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// What a case expects the check to make of an answer.
    #[derive(Debug, PartialEq, Eq)]
    enum Verdict {
        Passes,
        NoOperation,
        Undeclared,
        NoMedia,
        UnlistedMedia,
        Unreadable,
        Refused,
    }

    fn verdict(
        document: &Value,
        (method, path, status, media, body): (&str, &str, StatusCode, Option<&str>, &[u8]),
    ) -> Verdict {
        let checked =
            expected(document, method, path, status, media).and_then(|schema| match schema {
                Some(schema) => conforms(document, schema, status, media, body),
                None => Ok(()),
            });
        match checked {
            Ok(()) => Verdict::Passes,
            Err(ContractError::NoOperation) => Verdict::NoOperation,
            Err(ContractError::Undeclared(_)) => Verdict::Undeclared,
            Err(ContractError::NoMedia { .. }) => Verdict::NoMedia,
            Err(ContractError::UnlistedMedia { .. }) => Verdict::UnlistedMedia,
            Err(ContractError::Unreadable { .. }) => Verdict::Unreadable,
            Err(ContractError::Refused { .. }) => Verdict::Refused,
        }
    }

    #[test]
    fn test_each_answer_is_held_to_the_document() -> anyhow::Result<()> {
        const JSON: Option<&str> = Some("application/json");
        let openapi = OpenApi::build()?;
        let orgs = "/api/v1/orgs";
        let blob = "/api/v1/vault/blobs/{principal_id}/{sha256}";
        let page = json!({ "items": [], "next_cursor": null, "unreadable": 0 });
        let cases = [
            (
                "a body its schema admits",
                ("get", orgs, StatusCode::OK, JSON, page.clone()),
                Verdict::Passes,
            ),
            (
                "a body its schema refuses",
                (
                    "get",
                    orgs,
                    StatusCode::OK,
                    JSON,
                    json!({ "items": "none", "next_cursor": null, "unreadable": 0 }),
                ),
                Verdict::Refused,
            ),
            (
                "a success it does not declare",
                ("get", orgs, StatusCode::CREATED, JSON, page),
                Verdict::Undeclared,
            ),
            (
                "a declared error in the envelope",
                (
                    "get",
                    "/api/v1/orgs/{org_id}",
                    StatusCode::NOT_FOUND,
                    JSON,
                    json!({ "error": "no such org" }),
                ),
                Verdict::Passes,
            ),
            (
                "an error outside the envelope",
                (
                    "get",
                    orgs,
                    StatusCode::BAD_REQUEST,
                    JSON,
                    json!({ "message": "no" }),
                ),
                Verdict::Refused,
            ),
            (
                "a client error it does not declare",
                (
                    "get",
                    orgs,
                    StatusCode::NOT_FOUND,
                    JSON,
                    json!({ "error": "no" }),
                ),
                Verdict::Undeclared,
            ),
            (
                "a server error falls to default",
                (
                    "get",
                    orgs,
                    StatusCode::BAD_GATEWAY,
                    JSON,
                    json!({ "error": "upstream" }),
                ),
                Verdict::Passes,
            ),
            (
                "the router's own 405",
                (
                    "put",
                    orgs,
                    StatusCode::METHOD_NOT_ALLOWED,
                    JSON,
                    json!({ "error": "method not allowed" }),
                ),
                Verdict::Passes,
            ),
            (
                "a content type it does not list",
                (
                    "get",
                    orgs,
                    StatusCode::OK,
                    Some("text/plain"),
                    json!("orgs"),
                ),
                Verdict::UnlistedMedia,
            ),
            (
                "a declared body sent with no content type",
                ("get", orgs, StatusCode::OK, None, json!(null)),
                Verdict::NoMedia,
            ),
            (
                "an empty answer with no content type",
                (
                    "delete",
                    "/api/v1/clients/{client_id}",
                    StatusCode::NO_CONTENT,
                    None,
                    json!(null),
                ),
                Verdict::Passes,
            ),
            (
                "a redirect it does not declare",
                ("get", orgs, StatusCode::FOUND, None, json!(null)),
                Verdict::Undeclared,
            ),
            (
                "a HEAD's declared status, whatever headers it carries",
                ("head", blob, StatusCode::NOT_FOUND, JSON, json!(null)),
                Verdict::Passes,
            ),
            (
                "a HEAD's status its operation does not declare",
                ("head", blob, StatusCode::CONFLICT, None, json!(null)),
                Verdict::Undeclared,
            ),
            (
                "a HEAD the GET handler answers, its body unread",
                ("head", orgs, StatusCode::OK, JSON, json!("unread")),
                Verdict::Passes,
            ),
            (
                "a route the document lacks",
                ("get", "/api/v1/nowhere", StatusCode::OK, JSON, json!({})),
                Verdict::NoOperation,
            ),
        ];
        let wrong: Vec<String> = cases
            .iter()
            .filter_map(|(name, (method, path, status, media, body), want)| {
                let body = body.to_string();
                let got = verdict(
                    openapi.document(),
                    (method, path, *status, *media, body.as_bytes()),
                );
                (got != *want).then(|| format!("{name}: {got:?}, not {want:?}"))
            })
            .collect();
        anyhow::ensure!(wrong.is_empty(), "{wrong:#?}");
        Ok(())
    }

    #[test]
    fn test_a_body_that_is_not_json_does_not_read() -> anyhow::Result<()> {
        let openapi = OpenApi::build()?;
        let document = openapi.document();
        let schema = expected(
            document,
            "get",
            "/api/v1/orgs",
            StatusCode::OK,
            Some("application/json"),
        )?
        .ok_or_else(|| anyhow::anyhow!("the listing's answer has a schema"))?;
        let read = conforms(
            document,
            schema,
            StatusCode::OK,
            Some("application/json"),
            b"{",
        );
        anyhow::ensure!(
            matches!(read, Err(ContractError::Unreadable { .. })),
            "{read:?}"
        );
        Ok(())
    }

    /// A TOML answer is read as its data, a datetime as its text, and held to
    /// its schema.
    #[test]
    fn test_a_toml_answer_is_held_to_its_schema() -> anyhow::Result<()> {
        const VARIANT: &str = r#"
[[variants]]
requires = ["os:ios"]
models = [{ type = "apple_foundation_text" }]
runtimes = [{ type = "apple_foundation" }]
"#;
        let openapi = OpenApi::build()?;
        let source = "/api/v1/plans/{plan_id}/document";
        let cases = [
            (
                "an instant as a TOML datetime",
                format!("expires_at = 2026-12-01T00:00:00Z\nbenchmarks = [\"b\"]\n{VARIANT}"),
                Verdict::Passes,
            ),
            (
                "a key no plan has",
                format!("colour = \"red\"\n{VARIANT}"),
                Verdict::Refused,
            ),
            ("no TOML", "[[variants".to_owned(), Verdict::Unreadable),
        ];
        let wrong: Vec<String> = cases
            .iter()
            .filter_map(|(name, body, want)| {
                let got = verdict(
                    openapi.document(),
                    ("get", source, StatusCode::OK, Some(TOML), body.as_bytes()),
                );
                (got != *want).then(|| format!("{name}: {got:?}, not {want:?}"))
            })
            .collect();
        anyhow::ensure!(wrong.is_empty(), "{wrong:#?}");
        Ok(())
    }
}
