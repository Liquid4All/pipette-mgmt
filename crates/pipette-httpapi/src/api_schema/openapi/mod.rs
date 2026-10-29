//! The operator API, every route under `/api/v1`, as an OpenAPI 3.1 document,
//! served at `GET /api/v1/openapi.json`. An answer's schema is the serialize
//! side's, a request's and a query's the deserialize side's.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::header::{
    ACCESS_CONTROL_ALLOW_ORIGIN, CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, ETAG,
};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use schemars::generate::SchemaSettings;
use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde_json::Value;

use pipette_core::canonical_json::sort_keys;
use pipette_plan_types::ErrorBody;

use self::table::operations;
use crate::auth::TIMESTAMP_TOLERANCE_SECS;
use crate::error::AppError;
use crate::etag;
use crate::gate::Gate::{self, Active, AllOf, AnyOf, Needs};
use crate::state::AppState;

mod route_docs;
mod site;
mod table;

/// `GET /api/v1/openapi.json`: the document, to any caller and any origin, as
/// it describes routes and no data.
pub async fn openapi_document(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let (body, etag) = state.openapi.served();
    let shared = [
        (ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
        (CACHE_CONTROL, "no-cache"),
        (ETAG, etag.as_str()),
    ];
    if etag::matched(&headers, etag) {
        return (StatusCode::NOT_MODIFIED, shared).into_response();
    }
    let json = [(CONTENT_TYPE, "application/json")];
    (shared, json, body.clone()).into_response()
}

/// `sandbox` gives the page an origin apart from the app's signing keys;
/// `default-src 'none'` runs no script.
const SITE_CSP: &str = "sandbox; default-src 'none'; style-src 'self'; base-uri 'none'; \
                        form-action 'none'; frame-ancestors 'none'";

/// `GET /docs`: the page's stylesheet resolves against `/docs/`.
pub async fn site_root() -> Redirect {
    Redirect::permanent("/docs/")
}

/// `GET /docs/`: the API reference, the document as one static page.
pub async fn site_page(State(state): State<AppState>) -> Result<Response, AppError> {
    let (page, _) = state.openapi.served_site()?;
    Ok(site_file("text/html; charset=utf-8", page.clone()))
}

/// `GET /docs/style.css`: the page's stylesheet.
pub async fn site_style(State(state): State<AppState>) -> Result<Response, AppError> {
    let (_, style) = state.openapi.served_site()?;
    Ok(site_file("text/css; charset=utf-8", style.clone()))
}

fn site_file(content_type: &'static str, body: Bytes) -> Response {
    let headers = [
        (CONTENT_TYPE, content_type),
        (CACHE_CONTROL, "no-cache"),
        (CONTENT_SECURITY_POLICY, SITE_CSP),
    ];
    (headers, body).into_response()
}

/// The document, built once as the server starts (`AppState::new`): it changes
/// only with the binary.
pub struct OpenApi {
    document: Value,
    /// The body the route serves and its `ETag`, rendered at the first
    /// request: most states are a test's, which serves none.
    served: OnceLock<(Bytes, String)>,
    /// The API reference page and its stylesheet, rendered at the first
    /// request as [`Self::served`] is.
    site: OnceLock<Result<(Bytes, Bytes), String>>,
}

impl OpenApi {
    pub fn build() -> anyhow::Result<Self> {
        Ok(Self {
            document: document()?,
            served: OnceLock::new(),
            site: OnceLock::new(),
        })
    }

    pub fn document(&self) -> &Value {
        &self.document
    }

    /// The API reference as a static page and its stylesheet, by file name.
    pub fn site(&self) -> anyhow::Result<[(&'static str, String); 2]> {
        site::files(&self.document)
    }

    /// The summary of the operation `method` on the route pattern `route`.
    pub fn summary(&self, method: &str, route: &str) -> Option<&str> {
        self.document
            .get("paths")?
            .get(route)?
            .get(method.to_ascii_lowercase().as_str())?
            .get("summary")?
            .as_str()
    }

    fn served(&self) -> &(Bytes, String) {
        self.served.get_or_init(|| {
            let body = Bytes::from(self.document.to_string());
            let etag = etag::of(&body);
            (body, etag)
        })
    }

    fn served_site(&self) -> Result<&(Bytes, Bytes), AppError> {
        self.site
            .get_or_init(|| {
                let [(_, page), (_, style)] = self.site().map_err(|e| format!("{e:#}"))?;
                Ok((Bytes::from(page), Bytes::from(style)))
            })
            .as_ref()
            .map_err(|e| AppError::Internal(e.clone()))
    }
}

/// The OpenAPI document of every operation [`operations`] lists. Fails on a
/// schema name two types share, as the app's bundle does.
fn document() -> anyhow::Result<Value> {
    let mut answers = generator(SchemaSettings::draft2020_12().for_serialize());
    let mut requests = generator(SchemaSettings::draft2020_12().for_deserialize());
    // A query's fields become parameters, so its schema is wanted whole, not
    // as a reference into the components.
    let mut queries = SchemaSettings::draft2020_12()
        .for_deserialize()
        .with(|s| s.inline_subschemas = true)
        .into_generator();

    let paths = operations().into_iter().fold(
        BTreeMap::<&str, Vec<(String, Value)>>::new(),
        |mut paths, operation| {
            let described = operation.describe(&mut answers, &mut requests, &mut queries);
            paths
                .entry(operation.path)
                .or_default()
                .push((operation.method.as_str().to_ascii_lowercase(), described));
            paths
        },
    );
    let error = answers.subschema_for::<ErrorBody>().to_value();
    let mut schemas = super::merged(
        answers.take_definitions(true),
        requests.take_definitions(true),
    )?;
    schemas.values_mut().for_each(plain_descriptions);

    let mut document = object([
        ("openapi", "3.1.1".into()),
        ("info", info()),
        // The paths are absolute, so the server is whichever serves this.
        ("servers", vec![object([("url", "/".into())])].into()),
        (
            "tags",
            <Tag as strum::IntoEnumIterator>::iter()
                .map(|tag| {
                    object([
                        ("name", tag.name().into()),
                        ("description", tag.description().into()),
                    ])
                })
                .collect(),
        ),
        (
            "paths",
            object(
                paths
                    .into_iter()
                    .map(|(path, methods)| (path, object(methods))),
            ),
        ),
        (
            "components",
            object([
                ("schemas", Value::Object(schemas)),
                (
                    "responses",
                    object(
                        std::iter::once(("Error", error_response(ERROR_DESCRIPTION, &error)))
                            .chain(ERRORS.iter().map(|(_, name, description)| {
                                (*name, error_response(description, &error))
                            })),
                    ),
                ),
                ("securitySchemes", security_schemes()),
            ]),
        ),
    ]);
    for_renderers(&mut document);
    Ok(sort_keys(document))
}

/// Docs renderers read less JSON Schema than a validator; what each schema
/// admits is unchanged. Not for the app's bundle, whose types take titles as names.
fn for_renderers(document: &mut Value) {
    match document {
        Value::Object(fields) => fields
            .iter_mut()
            .for_each(|(key, value)| match key.as_str() {
                "schema" => renderable(value),
                "schemas" => {
                    if let Value::Object(schemas) = value {
                        schemas.values_mut().for_each(renderable);
                    }
                }
                _ => for_renderers(value),
            }),
        Value::Array(items) => items.iter_mut().for_each(for_renderers),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

/// Renderers drop keywords beside a `$ref`, and label an untitled union arm by
/// its type alone.
fn renderable(schema: &mut Value) {
    if *schema == Value::Bool(true) {
        *schema = object(std::iter::empty::<(&str, Value)>());
        return;
    }
    let Value::Object(keywords) = schema else {
        return;
    };
    if keywords.len() > 1
        && let Some(reference) = keywords.remove("$ref")
    {
        let arm = object([("$ref", reference)]);
        match keywords.get_mut("allOf").and_then(Value::as_array_mut) {
            Some(arms) => arms.insert(0, arm),
            None => {
                keywords.insert("allOf".into(), vec![arm].into());
            }
        }
    }
    keywords
        .iter_mut()
        .for_each(|(keyword, value)| match keyword.as_str() {
            "properties" | "patternProperties" | "$defs" => {
                if let Value::Object(schemas) = value {
                    schemas.values_mut().for_each(renderable);
                }
            }
            "items" | "additionalProperties" | "propertyNames" | "contains" | "not" => {
                renderable(value);
            }
            "allOf" | "anyOf" | "oneOf" | "prefixItems" => value
                .as_array_mut()
                .into_iter()
                .flatten()
                .for_each(renderable),
            _ => {}
        });
    for union in ["oneOf", "anyOf"] {
        if let Some(arms) = keywords.get_mut(union).and_then(Value::as_array_mut)
            && arms.iter().filter(|arm| !is_null(arm)).count() > 1
        {
            arms.iter_mut().for_each(title_arm);
        }
    }
}

fn is_null(schema: &Value) -> bool {
    schema.get("type").and_then(Value::as_str) == Some("null")
}

/// Titled after what tells the arm apart: its values, its tags, or its one property.
fn title_arm(arm: &mut Value) {
    let Value::Object(keywords) = arm else {
        return;
    };
    if keywords.contains_key("title") || keywords.contains_key("$ref") {
        return;
    }
    let text = |value: &Value| {
        value
            .as_str()
            .map_or_else(|| value.to_string(), str::to_owned)
    };
    let tags: Vec<&str> = keywords
        .get("properties")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter_map(|(_, property)| property.get("const").and_then(Value::as_str))
        .collect();
    let required: Vec<&str> = keywords
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let title = if let Some(value) = keywords.get("const") {
        Some(text(value))
    } else if let Some(values) = keywords.get("enum").and_then(Value::as_array) {
        Some(values.iter().map(text).collect::<Vec<_>>().join(" | "))
    } else if !tags.is_empty() {
        Some(tags.join(" / "))
    } else if let [only] = required.as_slice() {
        Some((*only).to_owned())
    } else {
        keywords
            .get("format")
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    if let Some(title) = title {
        keywords.insert("title".into(), title.into());
    }
}

/// A generator whose references point into the document's components.
fn generator(settings: SchemaSettings) -> SchemaGenerator {
    settings
        .with(|s| s.definitions_path = "/components/schemas".into())
        .into_generator()
}

/// The manifest's license, which the document repeats; a manifest without one
/// fails the build here rather than publishing an empty license.
const LICENSE: &str = env!("CARGO_PKG_LICENSE");
const _: () = assert!(!LICENSE.is_empty());

fn info() -> Value {
    object([
        ("title", "pipette-mgmt operator API".into()),
        ("version", "1".into()),
        ("description", INFO_DESCRIPTION.into()),
        (
            "license",
            object([("name", LICENSE.into()), ("identifier", LICENSE.into())]),
        ),
    ])
}

const INFO_DESCRIPTION: &str = "What the app or a script calls on behalf of a user: \
accounts, organizations, the fleet, campaigns, plans and the vault. A device's own routes \
(registration, the catalog, the queue and results) are not described here. Every error carries \
the `ErrorBody` envelope. A TOML body's schema is of its parsed data: a plan document is a \
`SchedulerPlan`.\n\n\
**Getting a key.** Every request but registration and this document's own is signed by one of \
the account's Ed25519 keys, as the `userSignature` scheme describes. A caller makes a keypair and \
registers its public key at `POST /api/v1/users/register`, unsigned, with one of:\n\n\
- `user_preauth_key`: the invite an administrator of the account minted at \
`POST /api/v1/users/{user_id}/preauth-keys` and handed over. The key joins that account, \
normally `active`.\n\
- `contact_email`: a request for a new account. It starts `pending`, holding no permissions, \
and its key reads `GET /api/v1/users/me` alone until an operator approves it.\n\n\
The app registers a browser's key through a Clerk sign-in instead, at \
`POST /api/v1/users/register/clerk`. The answer's `key_id` is the `keyid` each signature names, \
and a signing account adds keys at `POST /api/v1/users/me/keys`. The [Users](#tag/users) section \
walks through all of it, with a script that signs a request, and through what an account may do.\n\n\
**Reading results.** `GET /api/v1/plans/{plan_id}/jobs?results=true` answers a page of the \
plan's jobs, each with its `result`: the latest scored run's `rows`, one per metric, or why there \
is none. `all=true` answers every job in one page, and the page's `results_read` says how the \
read went. `GET /api/v1/plans/{plan_id}/results.csv` answers every row as one file. Both read the \
results warehouse, which refreshes every five minutes by default, so a run scored since its last \
refresh appears after the next. The file also lists a plan's jobs from the warehouse, so it is \
empty for a plan filed since then.\n\n\
**Paging.** `GET /api/v1/plans`, `/campaigns`, `/vault/resources`, `/clients`, \
`/descriptor-catalog` and the audit logs (newest first) page by cursor and answer `items`, `next_cursor` and `unreadable`: omit `cursor` for the \
first page, then send each `next_cursor` unchanged, with the same filters and `order`, until it \
is `null`; a full page can be the last. Their `limit` is 50 by default, at most 200 (the \
server's `list_default_limit` and `list_max_limit`), and `0` or more than that is a `400`, not \
clamped. `GET /api/v1/plans/{plan_id}/jobs` pages by `offset` instead and answers `total`: step \
`offset` by `limit` until it reaches `total`, or send `all=true`. Its `limit` is 100 by default, \
at most 500, and more than that is clamped to 500. Every other listing answering a `...Page` \
schema answers whole: its `next_cursor` is always `null`.\n\n\
**Names.** A body names accounts and organizations by id. A read that takes `names=true` adds \
`names`, an object from each owner id its body carries to a readable name: an account's \
address, an organization's handle. An id whose record is gone or did not read is left out, so \
show the id. A campaign's overview always carries `names`. A plan's jobs with `results=true` \
carry `names` for the devices (at full fleet detail only) and vault resources they name, and \
leave it out when there are none.\n\n\
**Size limits.** A body over its route's cap is a `413`, never parsed. A body carrying a plan \
document (the plan and campaign routes that file or check one) may be 128 MiB by default (the \
server's `plan_body_limit_bytes`), a vault blob 128 GiB by default (`vault_blob_limit_bytes`), \
streamed; every other body 64 KiB by default (`default_body_limit_bytes`).\n\n\
**Write answers.** A write that edits one record answers the record as stored after it, \
plus `changed`. `changed: false` is a success that wrote nothing, since the record already \
held what was asked, so a retry is safe.\n\n\
**Links.** A link `#tag/<tag>` names a tag's description and `#tag/<tag>/<operationId>` the \
operation with that `operationId`, where `<tag>` is `users` (Users), `orgs` (Organizations), \
`devices` (Devices), `model-params` (Model parameters), \
`descriptor-catalog` (Descriptor catalog), `campaigns` (Campaigns), \
`plans` (Plans), `vault` (Vault) or `document` (API description). `#tag/document`, the tag of this \
document's own route, points to these shared rules.";

const ERROR_DESCRIPTION: &str = "An error. A `4xx` says what the caller got wrong and is safe \
to show a user; a `5xx` is a fixed string, `\"internal server error\"` (`500`) or \
`\"upstream request failed\"` (`502`), with no detail of the failure.";

/// The errors an operation lists besides `default`: each status, its
/// component's name, and what it means.
const ERRORS: &[(u16, &str, &str)] = &[
    (
        400,
        "BadRequest",
        "The request is malformed: a path, query or body value the route refuses.",
    ),
    (
        401,
        "Unauthorized",
        "The request is unsigned, or its signature or token does not verify.",
    ),
    (
        403,
        "Forbidden",
        "The caller may not: it lacks the permission, its account is not active, or the \
         object is out of its reach.",
    ),
    (
        404,
        "NotFound",
        "No such object, or none the caller may see.",
    ),
    (
        408,
        "RequestTimeout",
        "The body sent nothing for the server's read timeout.",
    ),
    (409, "Conflict", "The object's state refuses the change."),
    (
        412,
        "PreconditionFailed",
        "The `If-Match` or `If-None-Match` the request sets does not hold.",
    ),
    (
        413,
        "PayloadTooLarge",
        "The body is over the route's size limit.",
    ),
    (
        415,
        "UnsupportedMediaType",
        "The body's `Content-Type` is not one the route reads.",
    ),
    (
        429,
        "TooManyRequests",
        "The route is at its concurrency limit: HuggingFace import permits, or vault uploads \
         in flight. Or HuggingFace rate-limited the requests of the token; the message gives \
         the wait if HuggingFace sent one. Or too many registrations without a pre-auth key; \
         `Retry-After` gives the wait. Nothing was done; retry later.",
    ),
    (
        502,
        "BadGateway",
        "An upstream failed: HuggingFace during an import, or pipette-duckdb.",
    ),
];

fn error_response(description: &str, schema: &Value) -> Value {
    object([
        ("description", description.into()),
        ("content", json_content(schema.clone())),
    ])
}

/// OpenAPI has no scheme for a signature over the request, so this one only
/// describes it; a gate's permissions are a requirement's roles.
fn security_schemes() -> Value {
    object([
        (
            USER_SIGNATURE,
            object([
                ("type", "apiKey".into()),
                ("in", "header".into()),
                ("name", "Signature".into()),
                ("description", signature_description().into()),
            ]),
        ),
        (
            CLERK,
            object([
                ("type", "http".into()),
                ("scheme", "bearer".into()),
                ("bearerFormat", "JWT".into()),
                (
                    "description",
                    "A Clerk session token, from the JWT template the deployment names, \
                     which registers the key the body carries to the account its verified \
                     address names. Each token signs in once."
                        .into(),
                ),
            ]),
        ),
    ])
}

const USER_SIGNATURE: &str = "userSignature";
const CLERK: &str = "clerk";

/// The RFC 9421 profile the server verifies, with a request signed under it.
fn signature_description() -> String {
    format!(
        "An Ed25519 signature by one of the account's keys, as an RFC 9421 HTTP Message \
         Signature. The server reads one profile of the RFC and refuses anything outside it \
         with `401`:\n\n\
         - **Covered components:** `\"@method\" \"@authority\" \"@path\" \"@query\"` in any \
         order, and `\"content-digest\"` when the request has a body; no other component, and \
         no component parameters. `@authority` is the `Host` the request goes to, lowercased, \
         and `@query` is `?` and the query as sent, or `?` alone.\n\
         - **Parameters:** `created` (Unix seconds, within {TIMESTAMP_TOLERANCE_SECS} seconds \
         of the server's clock), `nonce` (1 to 256 visible ASCII characters, unique per \
         request), `keyid` (the key's `key_id`), `alg=\"ed25519\"` and `tag=\"pipette\"`. \
         `expires` is optional, and any other parameter is refused.\n\
         - **Body:** an RFC 9530 `Content-Digest` header with one `sha-256` member. A body \
         that is not the signed digest is refused with `400`.\n\
         - **Headers:** one signature under any label, the same in `Signature-Input` and \
         `Signature`, each on one header line.\n\n\
         `POST /api/v1/orgs?dry_run=true` to the host `mgmt.example`, with the body \
         `{{\"slug\":\"acme\"}}`, signed at `created=1760000000` with the nonce \
         `4f1c0de5a9b7` by RFC 9421's `test-key-ed25519` (Appendix B.1.4), whose `key_id` is \
         `userkey_` and the lowercase hex SHA-256 of its public key:\n\n\
         ```\n{SIGNATURE_EXAMPLE}\n```\n\n\
         The roles a requirement lists are the permissions its route needs."
    )
}

/// The headers [`signature_description`] shows, which its test signs again.
const SIGNATURE_EXAMPLE: &str = "\
Content-Digest: sha-256=:Dmcct3wNRuKmSGlN9J7lDIw9DvE0Xp6DUXXIZHWJ65k=:
Signature-Input: sig=(\"@method\" \"@authority\" \"@path\" \"@query\" \"content-digest\");\
created=1760000000;nonce=\"4f1c0de5a9b7\";\
keyid=\"userkey_b16c2d1bead1262639764fdb0ee4d3774599336bd493404cda4b1136c59f2062\";\
alg=\"ed25519\";tag=\"pipette\"
Signature: sig=:xErs0vVcglQO4jeofP53oNks9pyROP4E0OOttcbMlHt5CAanP85tQV21Sy4jOZEzJg3Bm9gRrGXvqDssOFy+Bg==:";

/// The document's sections, one per kind of record the routes administer.
#[derive(Clone, Copy, strum::EnumIter)]
enum Tag {
    Users,
    Organizations,
    Devices,
    Models,
    DescriptorCatalog,
    Campaigns,
    Plans,
    Vault,
    Document,
}

impl Tag {
    fn name(self) -> &'static str {
        match self {
            Self::Users => "Users",
            Self::Organizations => "Organizations",
            Self::Devices => "Devices",
            Self::Models => "Model parameters",
            Self::DescriptorCatalog => "Descriptor catalog",
            Self::Campaigns => "Campaigns",
            Self::Plans => "Plans",
            Self::Vault => "Vault",
            Self::Document => "API description",
        }
    }

    /// The tag's directory under `docs/api/`, and its intro's file name.
    fn slug(self) -> &'static str {
        match self {
            Self::Users => "users",
            Self::Organizations => "orgs",
            Self::Devices => "devices",
            Self::Models => "model-params",
            Self::DescriptorCatalog => "descriptor-catalog",
            Self::Campaigns => "campaigns",
            Self::Plans => "plans",
            Self::Vault => "vault",
            Self::Document => "document",
        }
    }

    fn by_slug(slug: &str) -> Option<Self> {
        <Self as strum::IntoEnumIterator>::iter().find(|tag| tag.slug() == slug)
    }

    /// The tag's section of the page [`site`] renders; an operation's is this
    /// and `/<operationId>`.
    fn fragment(self) -> String {
        format!("#tag/{}", self.slug())
    }

    /// The tag's intro under `docs/api/`; `Document` has none.
    fn description(self) -> String {
        if let Self::Document = self {
            return "This document. Its `info.description` holds the rules every route shares: \
                    getting a key, reading results, paging, names, size limits and write \
                    answers."
                .to_owned();
        }
        route_docs::tag_doc(self.slug()).unwrap_or_default()
    }
}

/// Who may call an operation.
enum Auth {
    /// Anyone: registration, which a token in the body may vouch for, and this
    /// document.
    Open,
    Clerk,
    /// A signed request from any account, active or not.
    Signed,
    /// A signed request the gate `router.rs` declares admits, with the gate's
    /// source text, which the tests hold to the router's.
    Gated(
        Gate,
        #[cfg_attr(
            not(test),
            expect(dead_code, reason = "read by the tests, which hold it to `router.rs`")
        )]
        &'static str,
    ),
}

type SchemaFn = fn(&mut SchemaGenerator) -> Schema;

fn of<T: JsonSchema>() -> SchemaFn {
    SchemaGenerator::subschema_for::<T>
}

/// What an operation reads from its body. A TOML body's schema is of its
/// parsed data, as for JSON.
enum Body {
    Json(SchemaFn),
    Toml(SchemaFn),
    /// JSON or TOML, chosen by `Content-Type`, the one shape either way.
    JsonOrToml(SchemaFn),
    Bytes,
}

fn json_body<T: JsonSchema>() -> Body {
    Body::Json(of::<T>())
}

fn toml_body<T: JsonSchema>() -> Body {
    Body::Toml(of::<T>())
}

/// One way an answer's body may come.
enum Payload {
    /// JSON, one of these shapes.
    Json(Vec<SchemaFn>),
    Ndjson(SchemaFn),
    Toml(SchemaFn),
    /// Comma-separated values, with what its columns are.
    Csv(fn() -> String),
}

fn json<T: JsonSchema>() -> Payload {
    Payload::Json(vec![of::<T>()])
}

fn either<A: JsonSchema, B: JsonSchema>() -> Payload {
    Payload::Json(vec![of::<A>(), of::<B>()])
}

struct Reply {
    status: u16,
    description: &'static str,
    content: Vec<Payload>,
}

/// One operation, as `router.rs` serves it and the document describes it.
struct Operation {
    method: Method,
    path: &'static str,
    /// The handler `router.rs` names, which is also the `operationId`.
    handler: &'static str,
    tag: Tag,
    auth: Auth,
    summary: &'static str,
    query: Vec<SchemaFn>,
    /// Repeatable query keys no struct names, each with what it does.
    repeated: &'static [(&'static str, &'static str)],
    body: Option<Body>,
    replies: Vec<Reply>,
    /// Errors past the ones its shape implies ([`Operation::error_statuses`]).
    errors: &'static [u16],
    /// Errors its shape implies that it never answers.
    never: &'static [u16],
    /// The handler takes a person, so a service account is refused.
    human_only: bool,
}

fn op(
    method: Method,
    path: &'static str,
    handler: &'static str,
    tag: Tag,
    auth: Auth,
) -> Operation {
    Operation {
        method,
        path,
        handler,
        tag,
        auth,
        summary: "",
        query: Vec::new(),
        repeated: &[],
        body: None,
        replies: Vec::new(),
        errors: &[],
        never: &[],
        human_only: false,
    }
}

impl Operation {
    fn summary(mut self, summary: &'static str) -> Self {
        self.summary = summary;
        self
    }

    fn query<T: JsonSchema>(mut self) -> Self {
        self.query.push(of::<T>());
        self
    }

    fn repeated(mut self, keys: &'static [(&'static str, &'static str)]) -> Self {
        self.repeated = keys;
        self
    }

    fn body(mut self, body: Body) -> Self {
        self.body = Some(body);
        self
    }

    fn reply(
        mut self,
        status: u16,
        description: &'static str,
        content: impl IntoIterator<Item = Payload>,
    ) -> Self {
        self.replies.push(Reply {
            status,
            description,
            content: content.into_iter().collect(),
        });
        self
    }

    /// An answer with no body.
    fn empty(self, status: u16, description: &'static str) -> Self {
        self.reply(status, description, [])
    }

    fn errors(mut self, errors: &'static [u16]) -> Self {
        self.errors = errors;
        self
    }

    fn never(mut self, never: &'static [u16]) -> Self {
        self.never = never;
        self
    }

    fn human_only(mut self) -> Self {
        self.human_only = true;
        self
    }

    /// The errors its shape implies: a signature, a gate, a path id, a body.
    fn implied_statuses(&self) -> BTreeSet<u16> {
        let path_ids = self.path.contains('{');
        let inputs = path_ids || !self.query.is_empty() || !self.repeated.is_empty();
        [
            (!matches!(self.auth, Auth::Open), 401),
            (matches!(self.auth, Auth::Gated(..)) || self.human_only, 403),
            (inputs || self.body.is_some(), 400),
            (path_ids, 404),
            (self.body.is_some(), 413),
            (
                matches!(self.body, Some(Body::Toml(_) | Body::JsonOrToml(_))),
                415,
            ),
        ]
        .into_iter()
        .filter_map(|(implied, status)| implied.then_some(status))
        .collect()
    }

    /// The errors this operation lists: the implied ones less those it never
    /// answers, and its own, unless a reply of its own declares the status.
    fn error_statuses(&self) -> BTreeSet<u16> {
        self.implied_statuses()
            .into_iter()
            .filter(|status| !self.never.contains(status))
            .chain(self.errors.iter().copied())
            .filter(|status| self.replies.iter().all(|reply| reply.status != *status))
            .collect()
    }

    #[cfg(test)]
    fn statuses(&self) -> BTreeSet<u16> {
        self.replies
            .iter()
            .map(|reply| reply.status)
            .chain(self.error_statuses())
            .collect()
    }

    fn describe(
        &self,
        answers: &mut SchemaGenerator,
        requests: &mut SchemaGenerator,
        queries: &mut SchemaGenerator,
    ) -> Value {
        let doc = route_docs::route_doc(self.handler);
        let when = |status: u16| {
            doc.as_ref()?
                .statuses
                .iter()
                .find(|(listed, _)| *listed == status)
                .map(|(_, when)| when.as_str())
        };
        let error = answers.subschema_for::<ErrorBody>().to_value();
        let mut responses: Vec<(String, Value)> = self
            .replies
            .iter()
            .map(|reply| {
                (
                    reply.status.to_string(),
                    reply.describe(answers, when(reply.status)),
                )
            })
            .collect();
        responses.extend(self.error_statuses().into_iter().map(|status| {
            let response = when(status).map_or_else(
                || {
                    let name = ERRORS
                        .iter()
                        .find(|(code, ..)| *code == status)
                        .map_or("Error", |(_, name, _)| *name);
                    object([("$ref", format!("#/components/responses/{name}").into())])
                },
                |when| error_response(when, &error),
            );
            (status.to_string(), response)
        }));
        responses.push((
            "default".to_owned(),
            object([("$ref", "#/components/responses/Error".into())]),
        ));
        let refused = if self.human_only {
            " A service account is refused."
        } else {
            ""
        };
        object([
            ("operationId", self.handler.into()),
            ("summary", self.summary.into()),
            (
                "description",
                format!(
                    "{}{refused}\n\n{}",
                    self.auth.needs(),
                    doc.as_ref().map_or("", |doc| doc.description.as_str())
                )
                .trim_end()
                .into(),
            ),
            ("x-human-only", self.human_only.then_some(true).into()),
            ("tags", vec![self.tag.name()].into()),
            ("parameters", self.parameters(queries)),
            (
                "requestBody",
                self.body.as_ref().map_or(Value::Null, |body| {
                    body.describe(requests, route_docs::examples(self.handler))
                }),
            ),
            ("responses", object(responses)),
            ("security", self.auth.requirements()),
        ])
    }

    fn parameters(&self, queries: &mut SchemaGenerator) -> Value {
        let parameters: Vec<Value> = path_names(self.path)
            .map(|name| parameter(name, "path", true, path_schema(self.path, name)))
            .chain(
                self.query
                    .iter()
                    .flat_map(|query| query_parameters(&query(queries).to_value())),
            )
            .chain(self.repeated.iter().map(|(name, description)| {
                let schema = object([
                    ("type", "array".into()),
                    ("items", string_schema()),
                    ("description", (*description).into()),
                ]);
                parameter(name, "query", false, schema)
            }))
            .collect();
        if parameters.is_empty() {
            Value::Null
        } else {
            parameters.into()
        }
    }
}

impl Reply {
    /// Described by `when`, its route's own line for the status, if it has one.
    fn describe(&self, answers: &mut SchemaGenerator, when: Option<&str>) -> Value {
        let content: Vec<(&str, Value)> = self
            .content
            .iter()
            .map(|payload| match payload {
                Payload::Json(shapes) => (
                    "application/json",
                    object([("schema", any_of(shapes, answers))]),
                ),
                Payload::Ndjson(line) => (
                    "application/x-ndjson",
                    object([("schema", line(answers).to_value())]),
                ),
                Payload::Toml(shape) => (
                    "application/toml",
                    object([("schema", shape(answers).to_value())]),
                ),
                Payload::Csv(columns) => (
                    "text/csv",
                    object([(
                        "schema",
                        object([("type", "string".into()), ("description", columns().into())]),
                    )]),
                ),
            })
            .collect();
        object([
            ("description", when.unwrap_or(self.description).into()),
            (
                "content",
                if content.is_empty() {
                    Value::Null
                } else {
                    object(content)
                },
            ),
        ])
    }
}

impl Body {
    /// With `examples`, `(media type, text)`, under their media types; a JSON
    /// one as the value it spells.
    fn describe<'a>(
        &self,
        requests: &mut SchemaGenerator,
        examples: impl Iterator<Item = (&'a str, &'a str)> + Clone,
    ) -> Value {
        let example = |media: &str| {
            examples
                .clone()
                .find(|(listed, _)| *listed == media)
                .map_or(Value::Null, |(_, text)| {
                    if media == "application/json" {
                        serde_json::from_str(text).unwrap_or_else(|_| text.into())
                    } else {
                        text.into()
                    }
                })
        };
        let media = |name: &'static str, schema: Value| {
            (
                name,
                object([("schema", schema), ("example", example(name))]),
            )
        };
        let content = match self {
            Self::Json(shape) => object([media("application/json", shape(requests).to_value())]),
            Self::Toml(shape) => object([media("application/toml", shape(requests).to_value())]),
            Self::JsonOrToml(shape) => object([
                media("application/json", shape(requests).to_value()),
                media("application/toml", shape(requests).to_value()),
            ]),
            Self::Bytes => object([(
                "application/octet-stream",
                object(Vec::<(&str, Value)>::new()),
            )]),
        };
        object([("required", true.into()), ("content", content)])
    }
}

impl Auth {
    /// Who may call, in words: a viewer may not show a requirement's roles.
    fn needs(&self) -> String {
        let names = |permissions: &[pipette_plan_types::UserPermission], joint: &str| {
            permissions
                .iter()
                .map(|permission| format!("`{}`", permission.as_ref()))
                .collect::<Vec<_>>()
                .join(joint)
        };
        match self {
            Self::Open => "Unsigned.".to_owned(),
            Self::Clerk => "Authenticated by a Clerk session token, unsigned.".to_owned(),
            Self::Signed => "Any account's signature, pending accounts' included.".to_owned(),
            Self::Gated(gate, _) => match gate {
                Active => "Any active account.".to_owned(),
                Needs(permission) => format!("Needs `{}`.", permission.as_ref()),
                AllOf(permissions) => format!("Needs {}.", names(permissions, " and ")),
                AnyOf(permissions, _) => format!("Needs {}.", names(permissions, " or ")),
            },
        }
    }

    /// The operation's security requirements: any one of them admits.
    fn requirements(&self) -> Value {
        let signed =
            |roles: &[&str]| -> Value { object([(USER_SIGNATURE, roles.to_vec().into())]) };
        match self {
            Self::Open => Value::Array(Vec::new()),
            Self::Clerk => vec![object([(CLERK, Value::Array(Vec::new()))])].into(),
            Self::Signed => vec![signed(&[])].into(),
            Self::Gated(gate, _) => match gate {
                Active => vec![signed(&[])].into(),
                Needs(permission) => vec![signed(&[permission.as_ref()])].into(),
                AllOf(permissions) => {
                    let names: Vec<&str> = permissions.iter().map(AsRef::as_ref).collect();
                    vec![signed(&names)].into()
                }
                AnyOf(permissions, _) => permissions
                    .iter()
                    .map(|permission| signed(&[permission.as_ref()]))
                    .collect(),
            },
        }
    }
}

/// One shape as itself, several as the `anyOf` that admits each.
fn any_of(shapes: &[SchemaFn], answers: &mut SchemaGenerator) -> Value {
    match shapes {
        [shape] => shape(answers).to_value(),
        shapes => object([(
            "anyOf",
            shapes
                .iter()
                .map(|shape| shape(answers).to_value())
                .collect(),
        )]),
    }
}

fn json_content(schema: Value) -> Value {
    object([("application/json", object([("schema", schema)]))])
}

fn string_schema() -> Value {
    object([("type", "string".into())])
}

/// A path parameter's rule: the canonical form its handler's id type parses,
/// so a caller can build the segment without a round trip.
struct PathRule {
    /// The route fragment it governs, ending in `{name}`: `key_id` names three
    /// kinds, told apart by the segment before it.
    at: &'static str,
    /// Anchored, as JSON Schema's `pattern` is not.
    pattern: &'static str,
    example: &'static str,
    /// What the segment names; a shape the type does not check says "as minted".
    description: &'static str,
}

/// One per route fragment; `test_every_path_parameter_states_its_rule` holds
/// each pattern to its type's parser. Patterns take the canonical form, so a
/// padded or upper-case value the parser trims or lowercases fails them.
const PATH_RULES: &[PathRule] = &[
    PathRule {
        at: "/{campaign_id}",
        pattern: "^campaign_[A-Za-z0-9_-]*$",
        example: "campaign_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e",
        description: "The campaign: `campaign_` and, as minted, a UUIDv7. A plan id is \
                      refused.",
    },
    PathRule {
        at: "/{plan_id}",
        pattern: "^[A-Za-z0-9-]+$",
        example: "plan-0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e",
        description: "The plan: `plan-` and a UUIDv7, as minted. Not its campaign's id.",
    },
    PathRule {
        at: "/{job_id}",
        pattern: "^[A-Za-z0-9-]+$",
        example: "job-0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e",
        description: "The job: `job-` and a UUIDv7, as minted.",
    },
    PathRule {
        at: "/{client_id}",
        pattern: "^[A-Za-z0-9_-]+$",
        example: "ev1_e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        description: "The client: as minted, `ev1_` and the hex `sha256` of its public \
                      key; one registered before that prefix has none.",
    },
    PathRule {
        at: "/{user_id}",
        pattern: "^user_[A-Za-z0-9_-]*$",
        example: "user_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e",
        description: "The user account: `user_` and, as minted, a UUIDv7.",
    },
    PathRule {
        at: "/{org_id}",
        pattern: "^(org_[A-Za-z0-9_-]*|[a-z0-9_-]{1,64})$",
        example: "acme",
        description: "The organization, by id (`org_` and, as minted, a UUIDv7) or by \
                      its lower-case handle.",
    },
    PathRule {
        at: "/orgs/{org_id}/descriptor-catalog",
        pattern: "^org_[A-Za-z0-9_-]*$",
        example: "org_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e",
        description: "The organization ID, not its handle.",
    },
    PathRule {
        at: "/descriptor-catalog/{name}",
        pattern: "^[A-Za-z0-9][A-Za-z0-9._-]{0,199}$",
        example: "Fork-1",
        description: "The case-preserving catalog entry name in this organization.",
    },
    PathRule {
        at: "/keys/{key_id}",
        pattern: "^userkey_[A-Za-z0-9_-]*$",
        example: "userkey_e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        description: "The user key: `userkey_` and, as minted, the hex `sha256` of the \
                      public key.",
    },
    PathRule {
        at: "/preauth-keys/{key_id}",
        pattern: "^[A-Za-z0-9_-]+$",
        example: "9f86d081884c7d65",
        description: "The pre-auth key, 16 hex characters as minted: its token's part \
                      between `user_preauth_` and the `.`.",
    },
    PathRule {
        at: "/client-preauth-keys/{key_id}",
        pattern: "^[A-Za-z0-9_-]+$",
        example: "9f86d081884c7d65",
        description: "The pre-auth key, 16 hex characters as minted: its token's part \
                      between `client_preauth_` and the `.`.",
    },
    PathRule {
        at: "/{benchmark_id}",
        pattern: "^[A-Za-z0-9][A-Za-z0-9_.-]*$",
        example: "eval_ifbench_2026.06.1",
        description: "The benchmark, as its catalog entry names it.",
    },
    PathRule {
        at: "/{model_key}",
        pattern: "^[a-z0-9_-][a-z0-9._-]{0,199}$",
        example: "lfm2-8b",
        description: "The model key: a model name as table lookup normalizes it, or a \
                      vault resource id.",
    },
    PathRule {
        at: "/{resource_id}",
        pattern: "^vault_[A-Za-z0-9_-]*$",
        example: "vault_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e",
        description: "The vault resource: `vault_` and, as minted, a UUIDv7.",
    },
    PathRule {
        at: "/{principal_id}",
        pattern: "^(org|user)_[A-Za-z0-9_-]*$",
        example: "org_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e",
        description: "The owning principal: an organization id or a user id, never a \
                      handle.",
    },
    PathRule {
        at: "/{sha256}",
        pattern: "^[0-9a-f]{64}$",
        example: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        description: "The blob's SHA-256: 64 lower-case hex characters.",
    },
];

/// The `{name}` parameters of route `path`, in order.
fn path_names(path: &str) -> impl Iterator<Item = &str> {
    path.split('/')
        .filter_map(|segment| segment.strip_prefix('{')?.strip_suffix('}'))
}

/// The rules governing path parameter `name` on `path`; the test wants one.
fn path_rules<'a>(path: &'a str, name: &'a str) -> impl Iterator<Item = &'static PathRule> + 'a {
    let braced = format!("{{{name}}}");
    let org_catalog = path.starts_with("/api/v1/orgs/{org_id}/descriptor-catalog");
    PATH_RULES.iter().filter(move |rule| {
        if org_catalog && name == "org_id" {
            return rule.at == "/orgs/{org_id}/descriptor-catalog";
        }
        rule.at.ends_with(&braced) && path.contains(rule.at)
    })
}

/// `name`'s rule on `path`, or a bare string if it has none (a test refuses
/// that, so the document need not fail to build).
fn path_schema(path: &str, name: &str) -> Value {
    path_rules(path, name)
        .next()
        .map_or_else(string_schema, |rule| {
            object([
                ("type", "string".into()),
                ("pattern", rule.pattern.into()),
                ("examples", vec![rule.example].into()),
                ("description", rule.description.into()),
            ])
        })
}

/// A query struct's fields, each its own parameter.
fn query_parameters(schema: &Value) -> Vec<Value> {
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    schema
        .get("properties")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .map(|(name, field)| {
            parameter(
                name,
                "query",
                required.contains(&name.as_str()),
                without_null(field.clone()),
            )
        })
        .collect()
}

/// A query string has no `null`: an absent key is the `None`. Query only, as
/// a body or answer's `null` is real.
fn without_null(schema: Value) -> Value {
    let Value::Object(mut keywords) = schema else {
        return schema;
    };
    // An inlined `Option<Enum>` is the enum, the field's description first.
    if let Some(Value::Array(arms)) = keywords.get_mut("anyOf") {
        arms.retain(|arm| !is_null(arm));
        if let [Value::Object(only)] = arms.as_slice() {
            let mut merged = only.clone();
            keywords.remove("anyOf");
            merged.extend(keywords);
            keywords = merged;
        }
    }
    if let Some(Value::Array(types)) = keywords.get_mut("type") {
        types.retain(|kind| kind != "null");
        if let [only] = types.as_slice() {
            let only = only.clone();
            keywords.insert("type".into(), only);
        }
    }
    if let Some(Value::Array(values)) = keywords.get_mut("enum") {
        values.retain(|value| !value.is_null());
    }
    Value::Object(keywords)
}

/// A parameter, carrying its field's description, which a viewer shows beside
/// the name rather than inside the schema.
fn parameter(name: &str, location: &str, required: bool, schema: Value) -> Value {
    let (description, schema) = if let Value::Object(mut fields) = schema {
        let description = fields.remove("description");
        (description, Value::Object(fields))
    } else {
        (None, schema)
    };
    let description = description.and_then(|text| text.as_str().map(plain_links));
    object([
        ("name", name.into()),
        ("in", location.into()),
        ("required", required.into()),
        ("description", description.map_or(Value::Null, Value::from)),
        ("schema", schema),
    ])
}

/// An object of `entries`, leaving out each whose value is `null`: an absent
/// field reads the same and keeps the document short.
fn object<K: Into<String>>(entries: impl IntoIterator<Item = (K, Value)>) -> Value {
    Value::Object(
        entries
            .into_iter()
            .filter(|(_, value)| !value.is_null())
            .map(|(key, value)| (key.into(), value))
            .collect(),
    )
}

/// Each doc comment schemars copied in, with rustdoc's links as plain text.
fn plain_descriptions(schema: &mut Value) {
    match schema {
        Value::Object(map) => map.iter_mut().for_each(|(key, value)| {
            if let Value::String(text) = value
                && key == "description"
            {
                *text = plain_links(text);
            } else {
                plain_descriptions(value);
            }
        }),
        Value::Array(items) => items.iter_mut().for_each(plain_descriptions),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

/// `text` with rustdoc's intra-doc links as their text alone: a reader of the
/// document has no crate to follow them into. A link to the web stays a link.
fn plain_links(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some((before, after)) = rest.split_once('[') {
        out.push_str(before);
        let Some((label, tail)) = after.split_once(']') else {
            out.push('[');
            rest = after;
            continue;
        };
        if let Some((target, more)) = tail
            .strip_prefix('(')
            .and_then(|inner| inner.split_once(')'))
        {
            if target.starts_with("http") {
                out.push_str(&format!("[{label}]({target})"));
            } else {
                out.push_str(label);
            }
            rest = more;
            continue;
        }
        if label.starts_with('`') && label.ends_with('`') {
            out.push_str(label);
        } else {
            out.push_str(&format!("[{label}]"));
        }
        rest = tail;
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use anyhow::Context;
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use ed25519_dalek::SigningKey;
    use rstest::rstest;
    use sha2::{Digest, Sha256};

    use pipette_core::config::Config;
    use pipette_httpsig::{Nonce, Outgoing, sign};
    use pipette_plan_types::UserKeyId;

    use super::*;
    use crate::api_v1::plans::reads::{JOBS_PAGE_DEFAULT, JOBS_PAGE_MAX};
    use crate::api_v1::users::own::MAX_KEY_LIFETIME;

    #[test]
    fn test_every_listed_error_has_its_response() -> anyhow::Result<()> {
        operations()
            .iter()
            .flat_map(|operation| {
                operation
                    .error_statuses()
                    .into_iter()
                    .map(move |status| (operation, status))
            })
            .try_for_each(|(operation, status)| -> anyhow::Result<()> {
                anyhow::ensure!(
                    ERRORS.iter().any(|(code, ..)| *code == status),
                    "{} {} lists {status}, which `ERRORS` does not name",
                    operation.method,
                    operation.path
                );
                Ok(())
            })
    }

    /// A row's own `errors` are ones its shape does not imply, and each status
    /// its `never` rules out is one its shape does: any other entry changes
    /// nothing, or contradicts a reply or another entry.
    #[test]
    fn test_each_listed_or_ruled_out_error_changes_the_row() -> anyhow::Result<()> {
        let idle: Vec<String> = operations()
            .iter()
            .filter_map(|operation| {
                let implied = operation.implied_statuses();
                let replied = |status: &u16| {
                    operation
                        .replies
                        .iter()
                        .any(|reply| reply.status == *status)
                };
                let errors: Vec<&u16> = operation
                    .errors
                    .iter()
                    .filter(|status| implied.contains(status) || replied(status))
                    .collect();
                let never: Vec<&u16> = operation
                    .never
                    .iter()
                    .filter(|status| !implied.contains(status) || replied(status))
                    .collect();
                (!errors.is_empty() || !never.is_empty()).then(|| {
                    format!(
                        "{} {}: `errors` {errors:?}, `never` {never:?}",
                        operation.method, operation.path
                    )
                })
            })
            .collect();
        anyhow::ensure!(
            idle.is_empty(),
            "`errors` already implied or replied, or `never` not implied or replied:\n{}",
            idle.join("\n")
        );
        Ok(())
    }

    /// The signed request the `userSignature` scheme shows is what the server's
    /// own signer makes of it, under the `key_id` its key registers as.
    #[test]
    fn test_the_signing_example_is_what_the_signer_makes() -> anyhow::Result<()> {
        // RFC 9421 Appendix B.1.4's `test-key-ed25519`, by its JWK `d`.
        let d: [u8; 32] = URL_SAFE_NO_PAD
            .decode("n4Ni-HpISpVObnQMW0wOhCKROaIKqKtW_2ZYb2p9KcU")?
            .try_into()
            .map_err(|_| anyhow::anyhow!("a 32-byte key"))?;
        let key = SigningKey::from_bytes(&d);
        let key_id = UserKeyId::try_new(format!(
            "{}{}",
            UserKeyId::PREFIX,
            hex::encode(Sha256::digest(key.verifying_key().as_bytes()))
        ))?;
        let signed = sign(
            &key,
            &key_id,
            &Nonce::try_new("4f1c0de5a9b7")?,
            1_760_000_000,
            &Outgoing {
                method: "POST",
                authority: "mgmt.example",
                path_and_query: "/api/v1/orgs?dry_run=true",
                body: Some(br#"{"slug":"acme"}"#),
            },
        )?;
        let made = format!(
            "Content-Digest: {}\nSignature-Input: {}\nSignature: {}",
            signed.content_digest.unwrap_or_default(),
            signed.signature_input,
            signed.signature
        );
        assert_eq!(made, SIGNATURE_EXAMPLE);
        anyhow::ensure!(signature_description().contains(SIGNATURE_EXAMPLE));
        Ok(())
    }

    /// The document alone tells a caller how to get a key and read results:
    /// each query parameter of those routes and each field of their bodies is
    /// described, and the limits the text states are the code's.
    #[test]
    fn test_the_key_and_results_routes_describe_every_field() -> anyhow::Result<()> {
        const ROUTES: &[(&str, &str)] = &[
            ("post", "/api/v1/users/register"),
            ("post", "/api/v1/users/register/clerk"),
            ("post", "/api/v1/users/me/keys"),
            ("post", "/api/v1/users/{user_id}/preauth-keys"),
            ("get", "/api/v1/plans/{plan_id}/jobs"),
            ("get", "/api/v1/plans/{plan_id}/results.csv"),
        ];
        const BODIES: &[&str] = &[
            "RegisterUserKeyRequest",
            "RegisterClerkUserKeyRequest",
            "AddUserKeyRequest",
            "UserKeyRegistered",
            "MintUserPreauthRequest",
            "UserPreauthKeyMinted",
            "ResultRow",
            "NamedPlanJobs",
            "PlanJob",
            "JobInPlan",
            "ParentJob",
            "ParentRun",
        ];
        const ENUMS: &[&str] = &["JobResult", "ResultsRead"];
        let document = document()?;
        let text = |pointer: &str| {
            document
                .pointer(pointer)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        let operation =
            |method: &str, path: &str| format!("/paths/{}/{method}", path.replace('/', "~1"));
        let query_parameters = |method: &str, path: &str| {
            document
                .pointer(&format!("{}/parameters", operation(method, path)))
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|parameter| parameter.get("in").and_then(Value::as_str) == Some("query"))
        };
        let parameters = ROUTES.iter().flat_map(|(method, path)| {
            query_parameters(method, path).map(move |parameter| {
                let name = parameter.get("name").and_then(Value::as_str);
                (format!("{method} {path} {name:?}"), parameter)
            })
        });
        let fields = BODIES.iter().flat_map(|body| {
            document
                .pointer(&format!("/components/schemas/{body}/properties"))
                .and_then(Value::as_object)
                .into_iter()
                .flatten()
                .map(move |(field, schema)| (format!("{body}.{field}"), schema))
        });
        let one_of = |schema: &str| {
            document
                .pointer(&format!("/components/schemas/{schema}/oneOf"))
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
        };
        let arms = ENUMS.iter().flat_map(|schema| {
            one_of(schema)
                .enumerate()
                .map(move |(arm, item)| (format!("{schema} arm {arm}"), item))
        });
        let undescribed: Vec<String> = parameters
            .chain(fields)
            .chain(arms)
            .filter(|(_, item)| {
                item.get("description")
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty)
            })
            .map(|(name, _)| name)
            .collect();
        let absent: Vec<&&str> = BODIES
            .iter()
            .filter(|body| {
                document
                    .pointer(&format!("/components/schemas/{body}"))
                    .is_none()
            })
            .collect();
        // An enum rendered without `oneOf` would pass the arm check unread.
        let armless: Vec<&&str> = ENUMS
            .iter()
            .filter(|schema| one_of(schema).next().is_none())
            .collect();
        anyhow::ensure!(
            undescribed.is_empty() && absent.is_empty() && armless.is_empty(),
            "undescribed: {undescribed:#?}\nno such schema: {absent:?}\nno arms: {armless:?}"
        );

        let csv = text(&format!(
            "{}/responses/200/content/text~1csv/schema/description",
            operation("get", "/api/v1/plans/{plan_id}/results.csv")
        ));
        let limit = query_parameters("get", "/api/v1/plans/{plan_id}/jobs")
            .find(|parameter| parameter.get("name").and_then(Value::as_str) == Some("limit"))
            .and_then(|parameter| parameter.get("description"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let lifetime =
            text("/components/schemas/AddUserKeyRequest/properties/expires_at/description");
        anyhow::ensure!(csv.contains("`job_id`"), "{csv}");
        anyhow::ensure!(
            limit.contains(&format!(
                "{JOBS_PAGE_DEFAULT} by default, at most {JOBS_PAGE_MAX}"
            )),
            "{limit}"
        );
        anyhow::ensure!(
            lifetime.contains(&format!("{} days", MAX_KEY_LIFETIME.num_days())),
            "{lifetime}"
        );
        Ok(())
    }

    /// The paging, names and size rules the description states are the
    /// code's, and every query parameter says what it does.
    #[test]
    fn test_the_paging_and_size_rules_are_the_codes() -> anyhow::Result<()> {
        let config = Config::default();
        let cursor_limit = format!(
            "{} by default, at most {}",
            config.list_default_limit, config.list_max_limit
        );
        let stated = [
            cursor_limit.clone(),
            format!(
                "{JOBS_PAGE_DEFAULT} by default, at most {JOBS_PAGE_MAX}, and more than that is \
                 clamped to {JOBS_PAGE_MAX}"
            ),
            format!(
                "{} MiB by default",
                config.plan_body_limit_bytes.get() >> 20
            ),
            format!(
                "{} GiB by default",
                config.vault_blob_limit_bytes.get() >> 30
            ),
            format!(
                "every other body {} KiB by default",
                config.default_body_limit_bytes.get() >> 10
            ),
        ];
        let unstated: Vec<&String> = stated
            .iter()
            .filter(|text| !INFO_DESCRIPTION.contains(text.as_str()))
            .collect();
        anyhow::ensure!(unstated.is_empty(), "{unstated:?}");

        let document = document()?;
        let wrong: Vec<String> = every_query_parameter(&document)
            .into_iter()
            .filter(|(path, parameter)| {
                let description = parameter
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let cursor_paged = parameter.get("name").and_then(Value::as_str) == Some("limit")
                    && !path.ends_with("/jobs");
                description.is_empty() || (cursor_paged && !description.contains(&cursor_limit))
            })
            .map(|(path, parameter)| format!("{path} {parameter}"))
            .collect();
        anyhow::ensure!(wrong.is_empty(), "{wrong:#?}");
        Ok(())
    }

    /// Every section of the document heads an operation, so a viewer shows no
    /// empty one.
    #[test]
    fn test_every_tag_heads_an_operation() -> anyhow::Result<()> {
        let used: BTreeSet<&str> = operations()
            .iter()
            .map(|operation| operation.tag.name())
            .collect();
        let empty: Vec<&str> = <Tag as strum::IntoEnumIterator>::iter()
            .map(Tag::name)
            .filter(|name| !used.contains(name))
            .collect();
        anyhow::ensure!(empty.is_empty(), "sections with no operation: {empty:?}");
        Ok(())
    }

    /// Every link in the document is one its reader can follow: to a section
    /// of the document that exists, or to the web. None leads into the
    /// private repository.
    #[test]
    fn test_every_link_lands_in_the_document_or_the_web() -> anyhow::Result<()> {
        fn strings<'a>(value: &'a Value, key: &str, out: &mut Vec<(bool, &'a str)>) {
            match value {
                Value::String(text) => out.push((key == "description", text)),
                Value::Array(items) => items.iter().for_each(|item| strings(item, key, out)),
                Value::Object(map) => map.iter().for_each(|(key, item)| strings(item, key, out)),
                Value::Null | Value::Bool(_) | Value::Number(_) => {}
            }
        }
        let document = document()?;
        let mut texts = Vec::new();
        strings(&document, "", &mut texts);
        let sections: BTreeSet<String> = <Tag as strum::IntoEnumIterator>::iter()
            .map(Tag::fragment)
            .chain(
                operations()
                    .iter()
                    .map(|operation| format!("{}/{}", operation.tag.fragment(), operation.handler)),
            )
            .collect();
        let wrong: Vec<&str> = texts
            .iter()
            .flat_map(|&(description, text)| {
                let targets = description
                    .then_some(text)
                    .into_iter()
                    .flat_map(|text| {
                        text.match_indices("](")
                            .filter_map(move |(at, _)| text.get(at + 2..))
                    })
                    .map(|after| after.split_once(')').map_or(after, |(target, _)| target))
                    .filter(|target| {
                        !(sections.contains(*target)
                            || target.starts_with("https://")
                            || target.starts_with("http://")
                            || target.starts_with("mailto:"))
                    });
                text.contains("github.com/Liquid4All")
                    .then_some(text)
                    .into_iter()
                    .chain(targets)
            })
            .collect();
        anyhow::ensure!(wrong.is_empty(), "{wrong:#?}");
        let (_, links) = INFO_DESCRIPTION
            .split_once("**Links.**")
            .context("no Links paragraph")?;
        let missing: Vec<String> = <Tag as strum::IntoEnumIterator>::iter()
            .map(|tag| format!("`{}` ({})", tag.slug(), tag.name()))
            .filter(|spelled| !links.contains(spelled.as_str()))
            .collect();
        anyhow::ensure!(missing.is_empty(), "Links leaves out {missing:?}");
        Ok(())
    }

    /// Both revision routes describe the plans a revision takes runs from, and
    /// a variant edit the plans it takes variants from.
    #[rstest]
    #[case::variants(
        "/api/v1/campaigns/{campaign_id}/variants",
        &["carry_earlier", "inherit_from", "add_from", "restore"]
    )]
    #[case::document(
        "/api/v1/campaigns/{campaign_id}/document",
        &["carry_earlier", "inherit_from"]
    )]
    fn test_the_revision_routes_describe_what_they_inherit(
        #[case] path: &str,
        #[case] names: &[&str],
    ) -> anyhow::Result<()> {
        let document = document()?;
        let operation = document
            .pointer(&format!("/paths/{}", path.replace('/', "~1")))
            .and_then(Value::as_object)
            .and_then(|operations| operations.get("post").or_else(|| operations.get("put")))
            .with_context(|| format!("{path} is described"))?;
        let parameters = operation
            .get("parameters")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        names.iter().try_for_each(|name| {
            let described = parameters
                .iter()
                .find(|parameter| parameter.get("name").and_then(Value::as_str) == Some(*name))
                .and_then(|parameter| parameter.get("description"))
                .and_then(Value::as_str)
                .is_some_and(|text| !text.is_empty());
            anyhow::ensure!(described, "{path} describes `{name}`");
            Ok(())
        })
    }

    /// Every path parameter has exactly one rule, and each rule's pattern takes
    /// what its handler's type parses: a probe set and the rule's own example.
    #[test]
    fn test_every_path_parameter_states_its_rule() -> anyhow::Result<()> {
        let unruled: Vec<String> = operations()
            .iter()
            .flat_map(|operation| path_names(operation.path).map(move |name| (operation, name)))
            .filter_map(|(operation, name)| {
                let rules = path_rules(operation.path, name).count();
                (rules != 1).then(|| {
                    format!(
                        "{} {}: `{name}` has {rules} rules in PATH_RULES",
                        operation.method, operation.path
                    )
                })
            })
            .collect();
        anyhow::ensure!(unruled.is_empty(), "{unruled:#?}");
        // Canonical values only: a parser that trims or lowercases takes ` A`,
        // which the pattern rightly refuses.
        let long = "a".repeat(201);
        let long65 = "a".repeat(65);
        let hex63 = HEX64.get(1..).unwrap_or_default();
        let probes = [
            "",
            "a",
            "a/b",
            "..",
            ".x",
            "-a",
            "_a",
            "x.y",
            "x_y",
            "a-1",
            "org",
            "acme",
            "campaign_",
            "campaign_x",
            "plan-x",
            "user_x",
            "org_x",
            "userkey_x",
            "vault_x",
            &long65,
            &long,
            HEX64,
            hex63,
        ];
        PATH_RULES
            .iter()
            .try_for_each(|rule| -> anyhow::Result<()> {
                let schema = serde_json::json!({ "type": "string", "pattern": rule.pattern });
                let validator = jsonschema::validator_for(&schema)?;
                probes
                    .iter()
                    .copied()
                    .chain([rule.example])
                    .try_for_each(|raw| {
                        anyhow::ensure!(
                            validator.is_valid(&raw.into()) == parses(rule.at, raw)?,
                            "{}: `{}` and its type disagree on {raw:?}",
                            rule.at,
                            rule.pattern
                        );
                        Ok(())
                    })
            })
    }

    const HEX64: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    /// Whether the type the handler at `at` parses its segment into takes `raw`.
    fn parses(at: &str, raw: &str) -> anyhow::Result<bool> {
        use pipette_plan_types as types;
        Ok(match at {
            "/{campaign_id}" => raw.parse::<types::CampaignId>().is_ok(),
            "/{plan_id}" => raw.parse::<types::PlanId>().is_ok(),
            "/{job_id}" => raw.parse::<types::JobId>().is_ok(),
            "/{client_id}" => raw.parse::<types::ClientId>().is_ok(),
            "/{user_id}" => raw.parse::<types::UserId>().is_ok(),
            "/{org_id}" => raw.parse::<crate::api_v1::orgs::OrgPath>().is_ok(),
            "/orgs/{org_id}/descriptor-catalog" => raw.parse::<types::OrgId>().is_ok(),
            "/descriptor-catalog/{name}" => raw.parse::<types::CatalogName>().is_ok(),
            "/keys/{key_id}" => raw.parse::<types::UserKeyId>().is_ok(),
            "/preauth-keys/{key_id}" => raw.parse::<types::UserPreauthKeyId>().is_ok(),
            "/client-preauth-keys/{key_id}" => raw.parse::<types::ClientPreauthKeyId>().is_ok(),
            "/{benchmark_id}" => raw.parse::<types::BenchmarkId>().is_ok(),
            "/{model_key}" => raw.parse::<types::ModelKey>().is_ok(),
            "/{resource_id}" => raw.parse::<types::VaultResourceId>().is_ok(),
            "/{principal_id}" => raw.parse::<types::VaultPrincipalId>().is_ok(),
            // No `FromStr`: the vault route calls `try_new` itself.
            "/{sha256}" => types::Sha256::try_new(raw.to_owned()).is_ok(),
            other => anyhow::bail!("no parser for {other}; add one beside its PATH_RULES row"),
        })
    }

    /// No operation lists a parameter twice, which OpenAPI forbids: two query
    /// structs on one route sharing a field would.
    #[test]
    fn test_no_operation_repeats_a_parameter() -> anyhow::Result<()> {
        let document = document()?;
        let paths = document
            .pointer("/paths")
            .and_then(Value::as_object)
            .context("the document has paths")?;
        paths
            .iter()
            .flat_map(|(path, operations)| {
                operations
                    .as_object()
                    .into_iter()
                    .flatten()
                    .map(move |(method, operation)| (path, method, operation))
            })
            .try_for_each(|(path, method, operation)| -> anyhow::Result<()> {
                let parameters: Vec<(&str, &str)> = operation
                    .get("parameters")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|parameter| {
                        Some((
                            parameter.get("name")?.as_str()?,
                            parameter.get("in")?.as_str()?,
                        ))
                    })
                    .collect();
                let distinct: BTreeSet<&(&str, &str)> = parameters.iter().collect();
                anyhow::ensure!(
                    distinct.len() == parameters.len(),
                    "{method} {path} repeats a parameter: {parameters:?}"
                );
                Ok(())
            })
    }

    #[rstest]
    #[case::an_item("See [`OrgSlug`].", "See `OrgSlug`.")]
    #[case::a_path_and_a_file(
        "Read by [`list`](crate::list) and [the guide](docs/x.md).",
        "Read by `list` and the guide."
    )]
    #[case::a_web_link_stays(
        "[RFC 9421](https://www.rfc-editor.org/rfc/rfc9421)",
        "[RFC 9421](https://www.rfc-editor.org/rfc/rfc9421)"
    )]
    #[case::brackets_that_are_no_link("a [b] c [", "a [b] c [")]
    fn test_rustdoc_links_read_as_their_text(#[case] text: &str, #[case] plain: &str) {
        assert_eq!(plain_links(text), plain);
    }

    #[rstest]
    #[case::true_schema("true", "{}")]
    #[case::ref_beside_a_description(
        r##"{"$ref": "#/x", "description": "d"}"##,
        r##"{"allOf": [{"$ref": "#/x"}], "description": "d"}"##
    )]
    #[case::a_lone_ref(r##"{"$ref": "#/x"}"##, r##"{"$ref": "#/x"}"##)]
    #[case::arms_by_constant_tag_and_property(
        r#"{"oneOf": [
            {"const": "a"},
            {"enum": ["b", "c"]},
            {"type": "object", "properties": {"kind": {"const": "d"}}},
            {"type": "object", "required": ["model"]},
            {"type": "null"}
        ]}"#,
        r#"{"oneOf": [
            {"const": "a", "title": "a"},
            {"enum": ["b", "c"], "title": "b | c"},
            {"type": "object", "properties": {"kind": {"const": "d"}}, "title": "d"},
            {"type": "object", "required": ["model"], "title": "model"},
            {"type": "null"}
        ]}"#
    )]
    #[case::a_nullable_alone_is_no_union(
        r#"{"anyOf": [{"const": "a"}, {"type": "null"}]}"#,
        r#"{"anyOf": [{"const": "a"}, {"type": "null"}]}"#
    )]
    fn test_schemas_are_reshaped_for_renderers(
        #[case] schema: &str,
        #[case] expected: &str,
    ) -> anyhow::Result<()> {
        let mut schema: Value = serde_json::from_str(schema)?;
        renderable(&mut schema);
        assert_eq!(schema, serde_json::from_str::<Value>(expected)?);
        Ok(())
    }

    #[rstest]
    #[case::a_nullable_type(r#"{"type": ["boolean", "null"]}"#, r#"{"type": "boolean"}"#)]
    #[case::a_nullable_enum(
        r#"{"enum": ["a", "b", null], "type": ["string", "null"]}"#,
        r#"{"enum": ["a", "b"], "type": "string"}"#
    )]
    #[case::an_optional_enum_lifts_its_description(
        r#"{"anyOf": [{"description": "d", "oneOf": [{"const": "a"}]}, {"type": "null"}]}"#,
        r#"{"description": "d", "oneOf": [{"const": "a"}]}"#
    )]
    #[case::the_field_description_wins(
        r#"{"description": "f", "anyOf": [{"description": "d", "const": "a"}, {"type": "null"}]}"#,
        r#"{"description": "f", "const": "a"}"#
    )]
    #[case::a_plain_string_stays(r#"{"type": "string"}"#, r#"{"type": "string"}"#)]
    fn test_query_parameters_admit_no_null(
        #[case] schema: &str,
        #[case] expected: &str,
    ) -> anyhow::Result<()> {
        let schema = without_null(serde_json::from_str(schema)?);
        assert_eq!(schema, serde_json::from_str::<Value>(expected)?);
        Ok(())
    }

    /// No query parameter in the document admits `null`, which a query string
    /// cannot carry.
    #[test]
    fn test_no_query_parameter_admits_null() -> anyhow::Result<()> {
        let document = document()?;
        let admits_null = |schema: &Value| {
            is_null(schema)
                || schema
                    .get("type")
                    .and_then(Value::as_array)
                    .is_some_and(|types| types.iter().any(|kind| kind == "null"))
                || schema
                    .get("enum")
                    .and_then(Value::as_array)
                    .is_some_and(|values| values.iter().any(Value::is_null))
        };
        let nullable: Vec<String> = every_query_parameter(&document)
            .into_iter()
            .filter(|(_, parameter)| every_schema(parameter).into_iter().any(admits_null))
            .map(|(path, parameter)| format!("{path} {parameter}"))
            .collect();
        anyhow::ensure!(nullable.is_empty(), "{nullable:#?}");
        Ok(())
    }

    /// Each operation's query parameters, beside the path that takes them.
    fn every_query_parameter(document: &Value) -> Vec<(&String, &Value)> {
        document
            .pointer("/paths")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .flat_map(|(path, operations)| {
                operations
                    .as_object()
                    .into_iter()
                    .flat_map(serde_json::Map::values)
                    .filter_map(|operation| operation.get("parameters").and_then(Value::as_array))
                    .flatten()
                    .filter(|parameter| {
                        parameter.get("in").and_then(Value::as_str) == Some("query")
                    })
                    .map(move |parameter| (path, parameter))
            })
            .collect()
    }

    fn every_schema(document: &Value) -> Vec<&Value> {
        fn under<'a>(schema: &'a Value, found: &mut Vec<&'a Value>) {
            found.push(schema);
            let Value::Object(keywords) = schema else {
                return;
            };
            keywords
                .iter()
                .for_each(|(keyword, value)| match keyword.as_str() {
                    "properties" | "patternProperties" | "$defs" => value
                        .as_object()
                        .into_iter()
                        .flat_map(serde_json::Map::values)
                        .for_each(|schema| under(schema, found)),
                    "items" | "additionalProperties" | "propertyNames" | "contains" | "not" => {
                        under(value, found);
                    }
                    "allOf" | "anyOf" | "oneOf" | "prefixItems" => value
                        .as_array()
                        .into_iter()
                        .flatten()
                        .for_each(|schema| under(schema, found)),
                    _ => {}
                });
        }
        fn roots<'a>(document: &'a Value, found: &mut Vec<&'a Value>) {
            match document {
                Value::Object(fields) => {
                    fields.iter().for_each(|(key, value)| match key.as_str() {
                        "schema" => under(value, found),
                        "schemas" => value
                            .as_object()
                            .into_iter()
                            .flat_map(serde_json::Map::values)
                            .for_each(|schema| under(schema, found)),
                        _ => roots(value, found),
                    })
                }
                Value::Array(items) => items.iter().for_each(|item| roots(item, found)),
                Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
            }
        }
        let mut found = Vec::new();
        roots(document, &mut found);
        found
    }

    /// Nothing a renderer skips: a `true` schema, or keywords beside a `$ref`.
    #[test]
    fn test_renderers_read_every_schema_whole() -> anyhow::Result<()> {
        let openapi = OpenApi::build()?;
        let hidden: Vec<String> = every_schema(openapi.document())
            .into_iter()
            .filter(|schema| {
                **schema == Value::Bool(true)
                    || schema
                        .as_object()
                        .is_some_and(|keywords| keywords.len() > 1 && keywords.contains_key("$ref"))
            })
            .map(Value::to_string)
            .collect();
        anyhow::ensure!(hidden.is_empty(), "{hidden:#?}");
        Ok(())
    }

    /// The arms of a union render under distinct labels: a title, a
    /// reference, or a type no other arm shares.
    #[test]
    fn test_union_arms_read_apart() -> anyhow::Result<()> {
        let openapi = OpenApi::build()?;
        let label = |arm: &Value| {
            arm.get("title")
                .or_else(|| arm.get("$ref"))
                .or_else(|| arm.get("type"))
                .map(Value::to_string)
                .unwrap_or_default()
        };
        let alike: Vec<String> = every_schema(openapi.document())
            .into_iter()
            .flat_map(|schema| {
                ["oneOf", "anyOf"]
                    .into_iter()
                    .filter_map(|union| schema.get(union).and_then(Value::as_array))
            })
            .filter_map(|arms| {
                let labels: Vec<String> = arms.iter().map(label).collect();
                let distinct: BTreeSet<&String> = labels.iter().collect();
                (distinct.len() < labels.len()).then(|| labels.join(", "))
            })
            .collect();
        anyhow::ensure!(alike.is_empty(), "unions whose arms read alike: {alike:#?}");
        Ok(())
    }

    /// A `summary` is plain text in OpenAPI; Markdown belongs in `description`.
    #[test]
    fn test_summaries_are_plain_text() {
        let marked: Vec<&str> = operations()
            .iter()
            .map(|operation| operation.summary)
            .filter(|summary| summary.contains('`'))
            .collect();
        assert!(marked.is_empty(), "{marked:?}");
    }

    /// Names the rule leaves alone: field types, and the body every refusal shares.
    const VOCABULARY: &[&str] = &[
        "DeviceTimeTotal",
        "ErrorBody",
        "FleetDetail",
        "FleetStanding",
        "UserAccountType",
    ];

    /// No published schema, nor any query the route table names, takes a name
    /// the `pipette_plan_types` rule refuses.
    #[test]
    fn test_every_wire_type_is_named_by_subject_and_role() -> anyhow::Result<()> {
        let document = document()?;
        let schemas = document
            .pointer("/components/schemas")
            .and_then(Value::as_object)
            .context("the document publishes schemas")?;
        // The document inlines queries as parameters; a plain generator refers to them.
        // An inlined query fails here rather than skipping the check.
        let queries = operations()
            .iter()
            .flat_map(|operation| &operation.query)
            .map(|query| {
                query(&mut SchemaGenerator::default())
                    .get("$ref")
                    .and_then(Value::as_str)
                    .and_then(|reference| reference.rsplit('/').next())
                    .map(str::to_owned)
                    .context("a query schema is a reference")
            })
            .collect::<anyhow::Result<Vec<String>>>()?;
        anyhow::ensure!(!queries.is_empty(), "the route table names no query type");
        let broken: Vec<String> = schemas
            .iter()
            .map(|(name, schema)| (name.as_str(), Some(schema)))
            .chain(queries.iter().map(|name| (name.as_str(), None)))
            .filter(|(name, _)| !VOCABULARY.contains(name))
            .flat_map(|(name, schema)| {
                naming_problems(name, schema)
                    .into_iter()
                    .map(move |problem| format!("{name}: {problem}"))
            })
            .collect();
        anyhow::ensure!(broken.is_empty(), "{}", broken.join("\n"));
        Ok(())
    }

    fn naming_problems(name: &str, schema: Option<&Value>) -> Vec<&'static str> {
        let words = camel_words(name);
        // `ModelParams` is the record's own name, not a role.
        let refused_suffix = ["Response", "Record", "Params", "Body"]
            .iter()
            .any(|suffix| name.ends_with(suffix))
            && !matches!(name, "ModelParams" | "ChangedModelParams");
        let refused_subject = words.iter().any(|word| {
            ["Account", "Membership", "Device", "Fleet"].contains(&word.trim_end_matches('s'))
        });
        // Only an untagged enum is an `anyOf`; schemars makes a tagged one a `oneOf`.
        let lone_answer = words.last() == Some(&"Answer")
            && schema.and_then(|schema| schema.get("anyOf")).is_none();
        [
            (
                refused_suffix,
                "a Response, Record, Params or Body suffix: name its role",
            ),
            (
                words.contains(&"Api"),
                "an Api word: name a query for its answer",
            ),
            (
                refused_subject,
                "Account, Membership, Device or Fleet: name the record",
            ),
            (
                lone_answer,
                "Answer names only an untagged enum of a route's answers",
            ),
        ]
        .into_iter()
        .filter_map(|(broken, problem)| broken.then_some(problem))
        .collect()
    }

    fn camel_words(name: &str) -> Vec<&str> {
        let starts: Vec<usize> = name
            .char_indices()
            .filter(|(_, c)| c.is_ascii_uppercase())
            .map(|(at, _)| at)
            .chain([name.len()])
            .collect();
        starts
            .windows(2)
            .filter_map(|pair| match pair {
                [from, to] => name.get(*from..*to),
                _ => None,
            })
            .collect()
    }
}
