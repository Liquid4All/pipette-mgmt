//! The Svelte app (`ui/` at the workspace root), served at `/app/`: it signs
//! every request in the browser, so it holds no session. `build.rs` embeds its
//! build, or a page saying the app was not built.

use std::sync::LazyLock;

use axum::extract::{Path, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, HOST};
use axum::http::{HeaderMap, Uri};
use axum::response::{IntoResponse, Redirect, Response};
use serde::Serialize;

use crate::clerk::ClerkSignIn;
use crate::error::AppError;
use crate::state::AppState;

/// The page with this build's version stamped in, for its footer.
static INDEX: LazyLock<String> = LazyLock::new(|| {
    include_str!(concat!(env!("OUT_DIR"), "/index.html"))
        .replace("__PIPETTE_VERSION__", pipette_core::BUILD_VERSION)
});
/// Every file the build wrote under `assets/`, by name. Vite hashes each name,
/// so a deploy changes the names rather than the files behind them.
static ASSETS: &[(&str, &[u8])] = include!(concat!(env!("OUT_DIR"), "/ui_assets.rs"));

/// The browser holds the signing key, so the page loads nothing from elsewhere
/// and talks to no other origin.
const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; \
                   connect-src 'self'; img-src 'self' data:; base-uri 'none'; \
                   form-action 'none'; frame-ancestors 'none'";

/// [`CSP`] widened for ClerkJS, at a host that offers Clerk: its frontend API
/// serves the script and answers it, and the script injects its own styles,
/// starts a worker and may load Cloudflare's bot check.
fn clerk_csp(frontend_api: &str) -> String {
    let api = format!("https://{frontend_api}");
    format!(
        "default-src 'self'; script-src 'self' {api} https://challenges.cloudflare.com; \
         style-src 'self' 'unsafe-inline'; connect-src 'self' {api}; \
         img-src 'self' data: https://img.clerk.com; worker-src 'self' blob:; \
         frame-src https://challenges.cloudflare.com; base-uri 'none'; form-action 'none'; \
         frame-ancestors 'none'"
    )
}

/// What the page needs to load ClerkJS.
#[derive(Serialize)]
struct ClerkPage<'a> {
    publishable_key: &'a str,
    frontend_api: &'a str,
    template: &'a str,
}

/// The page names this build's assets, so a browser revalidates it.
const NO_CACHE: &str = "no-cache";

/// An asset's name carries its content hash, so it never changes.
const IMMUTABLE: &str = "public, max-age=31536000, immutable";

/// `GET /` and `GET /app`: the app's base is `/app/`, which its asset paths resolve against.
pub async fn root(uri: Uri) -> Redirect {
    Redirect::permanent(&with_query("/app/", &uri))
}

/// `path` with `uri`'s query, so a marker such as Clerk's `clerk=1` survives a redirect.
fn with_query(path: &str, uri: &Uri) -> String {
    match uri.query().filter(|query| !query.is_empty()) {
        Some(query) => format!("{path}?{query}"),
        None => path.to_string(),
    }
}

/// `GET /app/` and every page under it, offering Clerk only at a host its
/// `authorized_parties` names.
pub async fn index(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let host = headers.get(HOST).and_then(|host| host.to_str().ok());
    let clerk = state
        .clerk
        .as_deref()
        .filter(|clerk| host.is_some_and(|host| clerk.authorizes_host(host)));
    let csp = clerk.map_or_else(|| CSP.to_string(), |c| clerk_csp(c.frontend_api()));
    let page = INDEX.replace(
        "__PIPETTE_CLERK__",
        &clerk.map(clerk_attribute).unwrap_or_default(),
    );
    (
        [
            (CONTENT_TYPE, "text/html; charset=utf-8".to_string()),
            (CACHE_CONTROL, NO_CACHE.to_string()),
            (CONTENT_SECURITY_POLICY, csp),
        ],
        page,
    )
        .into_response()
}

/// The Clerk settings as JSON, escaped for the attribute it is written into.
fn clerk_attribute(clerk: &ClerkSignIn) -> String {
    let page = ClerkPage {
        publishable_key: clerk.publishable_key(),
        frontend_api: clerk.frontend_api(),
        template: clerk.jwt_template(),
    };
    serde_json::to_string(&page)
        .unwrap_or_default()
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// `GET /app/assets/{file}`.
pub async fn asset(Path(file): Path<String>) -> Response {
    let Some((name, body)) = ASSETS.iter().find(|(name, _)| *name == file) else {
        return AppError::NotFound("no such asset".into()).into_response();
    };
    (
        [
            (CONTENT_TYPE, content_type(name)),
            (CACHE_CONTROL, IMMUTABLE),
        ],
        *body,
    )
        .into_response()
}

/// The names of the assets this binary embeds, empty when the app was not built.
pub fn asset_names() -> impl Iterator<Item = &'static str> {
    ASSETS.iter().map(|(name, _)| *name)
}

fn content_type(name: &str) -> &'static str {
    match name.rsplit_once('.').map(|(_, ext)| ext) {
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    }
}
