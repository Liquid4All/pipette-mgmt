//! `GET /api/v1/openapi.json`: the operator API's description, open to a viewer
//! on any origin.

use anyhow::Context;
use axum::body::Body;
use axum::http::header::{
    ACCESS_CONTROL_ALLOW_ORIGIN, CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, ETAG,
    HeaderName, IF_NONE_MATCH, LOCATION,
};
use axum::http::{Response, StatusCode};
use http_body_util::BodyExt;
use rstest::rstest;

use pipette_httpapi::api_schema::openapi::OpenApi;
use pipette_web_testkit::{
    body_json, make_state, request_with_headers, setup_benchmarks, toml_document, unauthed_get,
};

const DOCUMENT: &str = "/api/v1/openapi.json";

fn header(resp: &Response<Body>, name: HeaderName) -> Option<&str> {
    resp.headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
}

#[tokio::test]
async fn the_document_is_served_unsigned_to_any_origin() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let resp = unauthed_get(&state, DOCUMENT).await?;

    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(header(&resp, ACCESS_CONTROL_ALLOW_ORIGIN), Some("*"));
    assert_eq!(header(&resp, CONTENT_TYPE), Some("application/json"));
    assert_eq!(header(&resp, CACHE_CONTROL), Some("no-cache"));
    assert_eq!(&body_json(resp).await?, state.openapi.document());
    Ok(())
}

/// A viewer holding the document revalidates it for a `304` and no body, in
/// each form a cache sends `If-None-Match`; another tag gets the document. The
/// answer carries the same `ETag` and headers either way.
#[rstest]
#[case::its_tag("{etag}", true)]
#[case::weak("W/{etag}", true)]
#[case::any("*", true)]
#[case::in_a_list("\"x\", {etag}", true)]
#[case::another_tag("\"nope\"", false)]
#[tokio::test]
async fn the_document_revalidates_by_its_etag(
    #[case] if_none_match: &str,
    #[case] matches: bool,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let first = unauthed_get(&state, DOCUMENT).await?;
    let etag = header(&first, ETAG)
        .context("the document carries an ETag")?
        .to_owned();

    let value = if_none_match.replace("{etag}", &etag);
    let again =
        request_with_headers(&state, "GET", DOCUMENT, &[(IF_NONE_MATCH.as_str(), value)]).await?;
    assert_eq!(header(&again, ETAG), Some(etag.as_str()));
    assert_eq!(header(&again, ACCESS_CONTROL_ALLOW_ORIGIN), Some("*"));
    assert_eq!(header(&again, CACHE_CONTROL), Some("no-cache"));
    if matches {
        assert_eq!(again.status(), StatusCode::NOT_MODIFIED);
        assert!(again.into_body().collect().await?.to_bytes().is_empty());
    } else {
        assert_eq!(again.status(), StatusCode::OK);
        assert_eq!(&body_json(again).await?, state.openapi.document());
    }
    Ok(())
}

/// The refusals of the TOML body schema `operation` declares, for `document`.
fn toml_body_refusals(operation: &str, document: &str) -> anyhow::Result<Vec<String>> {
    let mut schema = OpenApi::build()?.document().clone();
    let pointer = format!("/paths/{operation}/requestBody/content/application~1toml/schema");
    let body = schema
        .pointer(&pointer)
        .cloned()
        .with_context(|| format!("{pointer} is declared"))?;
    schema
        .as_object_mut()
        .context("the document is an object")?
        .extend(
            body.as_object()
                .cloned()
                .context("the body schema is an object")?,
        );
    let validator = jsonschema::validator_for(&schema)?;
    let instance = toml_document(document)?;
    Ok(validator
        .iter_errors(&instance)
        .map(|e| format!("{e} at {}", e.instance_path()))
        .collect())
}

const CREATE: &str = "~1api~1v1~1campaigns/post";

/// Each shipped scheduler example is a document the filing routes' schema
/// takes, so the schema describes what the server files.
#[rstest]
fn the_plan_document_schema_takes_each_example(
    #[files("../../examples/plans/scheduler/*.toml")] path: std::path::PathBuf,
) -> anyhow::Result<()> {
    let refusals = toml_body_refusals(CREATE, &std::fs::read_to_string(&path)?)?;
    anyhow::ensure!(refusals.is_empty(), "{}: {refusals:#?}", path.display());
    Ok(())
}

const VARIANT: &str = r#"
[[variants]]
requires = ["os:macos"]
models = [{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "LFM2-700M-MLX-4bit" }]
runtimes = [{ type = "mlx_macos_pipette", version = "0.31.3", flavor = "macos-arm64", source = { type = "pip_requirements_text", contents = "mlx-lm==0.31.3\n" } }]
"#;

/// The body schemas refuse what the server refuses: a key the type does not
/// have, a variant with no model, and an edit with a plan's own keys.
#[rstest]
#[case::a_plan(CREATE, format!("benchmarks = [\"b\"]\n{VARIANT}"), true)]
#[case::an_unknown_key(CREATE, format!("colour = \"red\"\n{VARIANT}"), false)]
#[case::no_variants(CREATE, "benchmarks = [\"b\"]".to_owned(), false)]
#[case::an_instant_as_a_toml_datetime(
    CREATE,
    format!("expires_at = 2026-12-01T00:00:00Z\nbenchmarks = [\"b\"]\n{VARIANT}"),
    true
)]
#[case::no_models(CREATE, VARIANT.replace("models = [{ type = \"mlx\"", "models = [] #"), false)]
#[case::an_edit("~1api~1v1~1campaigns~1{campaign_id}~1variants/post", format!("remove = [0]\n{VARIANT}"), true)]
#[case::an_edit_with_a_plan_key(
    "~1api~1v1~1campaigns~1{campaign_id}~1variants/post",
    format!("benchmarks = [\"b\"]\n{VARIANT}"),
    false
)]
fn a_toml_body_schema_holds_its_type(
    #[case] operation: &str,
    #[case] document: String,
    #[case] takes: bool,
) -> anyhow::Result<()> {
    let refusals = toml_body_refusals(operation, &document)?;
    assert_eq!(refusals.is_empty(), takes, "{refusals:#?}\n{document}");
    Ok(())
}

/// Every request body example the document shows is one its schema takes: a
/// JSON example as written, a TOML one as the data it parses to.
#[test]
fn each_request_example_is_a_body_the_schema_takes() -> anyhow::Result<()> {
    let document = OpenApi::build()?.document().clone();
    let examples: Vec<(String, &str, &serde_json::Value, &serde_json::Value)> = document
        .get("paths")
        .and_then(serde_json::Value::as_object)
        .context("the document has paths")?
        .iter()
        .flat_map(|(path, operations)| {
            operations
                .as_object()
                .into_iter()
                .flatten()
                .map(move |(method, operation)| (format!("{method} {path}"), operation))
        })
        .flat_map(|(route, operation)| {
            operation
                .pointer("/requestBody/content")
                .and_then(serde_json::Value::as_object)
                .into_iter()
                .flatten()
                .filter_map(move |(media, entry)| {
                    Some((
                        route.clone(),
                        media.as_str(),
                        entry.get("schema")?,
                        entry.get("example")?,
                    ))
                })
        })
        .collect();
    anyhow::ensure!(
        !examples.is_empty(),
        "the document shows no request example"
    );
    let refused = examples
        .iter()
        .map(
            |(route, media, body, example)| -> anyhow::Result<Vec<String>> {
                let mut schema = document.clone();
                schema
                    .as_object_mut()
                    .context("the document is an object")?
                    .extend(
                        body.as_object()
                            .cloned()
                            .context("the body schema is an object")?,
                    );
                let instance = match example.as_str() {
                    Some(text) if *media == "application/toml" => toml_document(text)?,
                    _ => (*example).clone(),
                };
                Ok(jsonschema::validator_for(&schema)?
                    .iter_errors(&instance)
                    .map(|e| format!("{route} {media}: {e} at {}", e.instance_path()))
                    .collect())
            },
        )
        .collect::<anyhow::Result<Vec<_>>>()?
        .concat();
    anyhow::ensure!(refused.is_empty(), "{refused:#?}");
    Ok(())
}

/// `/docs` lands on `/docs/`, which the page's stylesheet resolves against.
#[tokio::test]
async fn the_reference_root_redirects_to_its_page() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let resp = unauthed_get(&state, "/docs").await?;

    assert_eq!(resp.status(), StatusCode::PERMANENT_REDIRECT);
    assert_eq!(header(&resp, LOCATION), Some("/docs/"));
    Ok(())
}

/// Each file of the API reference is served sandboxed, so the page shares
/// nothing with the app's origin.
#[rstest]
#[case::page("/docs/", "index.html", "text/html; charset=utf-8")]
#[case::style("/docs/style.css", "style.css", "text/css; charset=utf-8")]
#[tokio::test]
async fn the_reference_is_served_sandboxed(
    #[case] path: &str,
    #[case] file: &str,
    #[case] content_type: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (_, body) = state
        .openapi
        .site()?
        .into_iter()
        .find(|(name, _)| *name == file)
        .context("no such file")?;

    let resp = unauthed_get(&state, path).await?;

    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(header(&resp, CONTENT_TYPE), Some(content_type));
    let csp = header(&resp, CONTENT_SECURITY_POLICY).context("no CSP")?;
    assert!(csp.starts_with("sandbox;"), "{csp}");
    assert!(csp.contains("default-src 'none'"), "{csp}");
    assert_eq!(resp.into_body().collect().await?.to_bytes(), body);
    Ok(())
}
