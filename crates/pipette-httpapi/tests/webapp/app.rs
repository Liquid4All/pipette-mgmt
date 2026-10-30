//! The Svelte app's static routes (`/`, `/app`, `/app/` and every page under it,
//! `/app/assets/{file}`): the page and its built assets, served without a session.

use anyhow::Context;
use axum::body::Body;
use axum::http::StatusCode;
use rstest::rstest;

use pipette_web_testkit::{
    CLERK_FRONTEND_API, CLERK_ORIGIN, ClerkInstance, approved_client, make_state_with_clerk,
    make_state_with_clerk_section, unauthed_get, unauthed_get_with_headers,
};

use crate::support::{body_text, location};

/// The root and the app's base land on `/app/`, the query kept so a Clerk
/// return's marker survives.
#[rstest]
#[case::the_root("/", "/app/")]
#[case::the_root_with_a_query("/?clerk=1", "/app/?clerk=1")]
#[case::bare("/app", "/app/")]
#[case::clerk_return("/app?clerk=1", "/app/?clerk=1")]
#[tokio::test]
async fn test_the_root_and_the_app_base_land_on_the_app(
    #[case] asked: &str,
    #[case] landed: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, _client_id) = approved_client(dir.path()).await?;

    let resp = unauthed_get(&state, asked).await?;
    assert_eq!(resp.status(), StatusCode::PERMANENT_REDIRECT, "{asked}");
    assert_eq!(location(&resp)?, landed, "{asked}");
    Ok(())
}

/// The app's page is public, since it holds no data, and loads nothing from
/// another origin.
#[tokio::test]
async fn test_the_app_page_is_served_under_a_same_origin_policy() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, _client_id) = approved_client(dir.path()).await?;

    let resp = unauthed_get(&state, "/app/").await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let header = |resp: &axum::http::Response<Body>, name: &str| {
        resp.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    assert_eq!(
        header(&resp, "content-type").as_deref(),
        Some("text/html; charset=utf-8")
    );
    let csp = header(&resp, "content-security-policy").context("the page carries a CSP")?;
    assert!(csp.contains("default-src 'self'"), "{csp}");
    assert!(csp.contains("connect-src 'self'"), "{csp}");
    let html = body_text(resp).await?;
    assert!(html.contains("<title>Pipette</title>"), "{html}");
    assert!(
        !html.contains("__PIPETTE_VERSION__"),
        "the version is stamped in: {html}"
    );
    Ok(())
}

/// A page the app routes itself is the same page, so an address under `/app/`
/// survives a reload and a bookmark.
#[rstest]
#[case::page("/app/server/users")]
#[case::trailing_slash("/app/orgs/")]
#[case::unknown("/app/no/such/page")]
#[tokio::test]
async fn test_every_page_under_the_app_is_the_app(#[case] uri: &str) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, _client_id) = approved_client(dir.path()).await?;

    let resp = unauthed_get(&state, uri).await?;
    assert_eq!(resp.status(), StatusCode::OK, "{uri}");
    let html = body_text(resp).await?;
    assert!(html.contains("<title>Pipette</title>"), "{uri}: {html}");
    Ok(())
}

/// Every asset the build wrote is served under its hashed name, cached for good.
/// Empty in a checkout that never built the app, which `console-ui` covers.
#[tokio::test]
async fn test_the_app_serves_every_built_asset() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, _client_id) = approved_client(dir.path()).await?;

    for name in pipette_httpapi::ui::asset_names() {
        let resp = unauthed_get(&state, &format!("/app/assets/{name}")).await?;
        assert_eq!(resp.status(), StatusCode::OK, "{name}");
        let header = |header: &str| resp.headers().get(header).and_then(|v| v.to_str().ok());
        assert_eq!(
            header("cache-control"),
            Some("public, max-age=31536000, immutable"),
            "{name}"
        );
        assert_ne!(
            header("content-type"),
            Some("application/octet-stream"),
            "{name}"
        );
    }
    Ok(())
}

/// A name the build did not write is a 404 in the envelope.
#[rstest]
#[case::unknown("/app/assets/other.js")]
#[case::traversal("/app/assets/..%2Fbuild.rs")]
#[tokio::test]
async fn test_the_app_serves_nothing_else(#[case] uri: &str) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, _client_id) = approved_client(dir.path()).await?;

    let resp = unauthed_get(&state, uri).await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{uri}");
    assert!(
        body_text(resp).await?.contains(r#""error""#),
        "in the envelope"
    );
    Ok(())
}

/// The page admits ClerkJS's origin only at a host `[clerk] authorized_parties`
/// names; elsewhere it stays same-origin.
#[tokio::test]
async fn test_the_app_page_admits_clerk_only_at_an_authorized_host() -> anyhow::Result<()> {
    let clerk = ClerkInstance::new()?;
    let dir = tempfile::tempdir()?;
    let state = make_state_with_clerk(dir.path(), &clerk, &[]).await?;
    let csp_at = |host: &'static str| {
        let state = state.clone();
        async move {
            let resp =
                unauthed_get_with_headers(&state, "/app/", &[("host", host.to_string())]).await?;
            let csp = resp
                .headers()
                .get("content-security-policy")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
                .context("the page carries a CSP")?;
            let html = body_text(resp).await?;
            assert!(
                !html.contains("__PIPETTE_CLERK__"),
                "the placeholder is filled: {html}"
            );
            anyhow::Ok(csp)
        }
    };

    let authorized = csp_at(CLERK_ORIGIN.trim_start_matches("https://")).await?;
    assert!(
        authorized.contains(&format!("script-src 'self' https://{CLERK_FRONTEND_API}")),
        "{authorized}"
    );
    assert!(authorized.contains(&format!("connect-src 'self' https://{CLERK_FRONTEND_API}")));

    let elsewhere = csp_at("elsewhere.example.test").await?;
    assert!(elsewhere.contains("script-src 'self';"), "{elsewhere}");
    assert!(!elsewhere.contains(CLERK_FRONTEND_API), "{elsewhere}");
    Ok(())
}

/// A server answering at several hosts offers Clerk only at those
/// `authorized_parties` names, since a token issued to any other is refused.
#[rstest]
#[case::the_authorized_host("https://console.example.test", Some("console.example.test"), true)]
#[case::in_capitals("https://console.example.test", Some("CONSOLE.example.test"), true)]
#[case::a_trailing_slash("https://console.example.test/", Some("console.example.test"), true)]
#[case::with_its_port("http://localhost:3000", Some("localhost:3000"), true)]
#[case::another_host("https://console.example.test", Some("pipette.example.test"), false)]
#[case::another_port("http://localhost:3000", Some("localhost:3001"), false)]
#[case::no_host("https://console.example.test", None, false)]
#[tokio::test]
async fn test_the_app_offers_clerk_only_at_an_authorized_host(
    #[case] authorized: &str,
    #[case] host: Option<&str>,
    #[case] offered: bool,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut section = ClerkInstance::new()?.config(&[])?;
    section.authorized_parties = vec![authorized.to_string()];
    let state = make_state_with_clerk_section(dir.path(), section).await?;

    let headers: Vec<(&str, String)> = host.map(|h| ("host", h.to_string())).into_iter().collect();
    let resp = unauthed_get_with_headers(&state, "/app/", &headers).await?;
    let csp = resp
        .headers()
        .get("content-security-policy")
        .and_then(|v| v.to_str().ok())
        .context("the page carries a CSP")?;
    assert_eq!(csp.contains(CLERK_FRONTEND_API), offered, "{host:?}: {csp}");
    Ok(())
}
