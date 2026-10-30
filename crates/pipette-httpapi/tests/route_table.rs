//! What every route shares. Authentication is an extractor per handler, not a
//! layer, so a route added without one is open: a new signed route joins
//! `signed_routes`, a new open one `OPEN_ROUTES`.

use std::collections::BTreeSet;

use anyhow::Context;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::{MethodRouter, get};
use futures::{StreamExt, TryStreamExt, stream};
use rstest::rstest;
use tower::ServiceExt;

use pipette_core::user_permission::UserPermissionGroup;
use pipette_httpapi::auth::{ActiveUser, HumanUser};
use pipette_httpapi::gate::{Gate, gate};
use pipette_httpapi::state::AppState;
use pipette_plan_types::UserPermission;

use pipette_web_testkit::*;

const CLIENT: &str = "ev1_0123456789abcdef";
const USER: &str = "user_0195c3e2a4f37b8c";
const USER_KEY: &str = "userkey_0195c3e2a4f37b8c";
const PREAUTH_KEY: &str = "0195c3e2a4f37b8c";
const CLIENT_PREAUTH_KEY: &str = "0195c3e2a4f37b8d";
const ORG: &str = "org_0195c3e2a4f37b8c";
const CAMPAIGN: &str = "campaign_0195c3e2a4f37b8c";
const PLAN: &str = "plan-0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e";
const JOB: &str = "job-0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e";
const RESOURCE: &str = "vault_0195c3e2a4f37b8c";
const BENCHMARK: &str = "gsm8k";
const MODEL_KEY: &str = "acmelm-4.5b-text";
const CATALOG_NAME: &str = "Catalog-1";
const SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// The ids above, which stand for a path parameter in `signed_routes`.
const IDS: &[&str] = &[
    CLIENT,
    USER,
    USER_KEY,
    PREAUTH_KEY,
    CLIENT_PREAUTH_KEY,
    ORG,
    CAMPAIGN,
    PLAN,
    JOB,
    RESOURCE,
    BENCHMARK,
    MODEL_KEY,
    CATALOG_NAME,
    SHA256,
];

const ROUTER: &str = include_str!("../src/router.rs");

/// Every signed route, by method and path. The ids are well formed and name
/// nothing, so a route that skipped its extractor would answer with a `404` or
/// a `400` from its own lookup instead of the `401`.
fn signed_routes() -> Vec<(&'static str, String)> {
    vec![
        // The device's own record.
        ("GET", "/clients/me".into()),
        ("PATCH", "/clients/me".into()),
        // The fleet and its administration.
        ("GET", "/api/v1/clients".into()),
        ("GET", "/api/v1/clients/capabilities".into()),
        ("GET", format!("/api/v1/clients/{CLIENT}")),
        ("PATCH", format!("/api/v1/clients/{CLIENT}")),
        ("DELETE", format!("/api/v1/clients/{CLIENT}")),
        ("POST", format!("/api/v1/clients/{CLIENT}/approve")),
        ("POST", format!("/api/v1/clients/{CLIENT}/reject")),
        ("POST", format!("/api/v1/clients/{CLIENT}/unsuspend")),
        ("PUT", format!("/api/v1/clients/{CLIENT}/owner")),
        ("GET", format!("/api/v1/clients/{CLIENT}/owners")),
        ("PUT", format!("/api/v1/clients/{CLIENT}/permissions")),
        ("PATCH", format!("/api/v1/clients/{CLIENT}/permissions")),
        // Accounts.
        ("GET", "/api/v1/users/me".into()),
        ("POST", "/api/v1/users/me/keys".into()),
        ("DELETE", format!("/api/v1/users/me/keys/{USER_KEY}")),
        ("GET", "/api/v1/users".into()),
        ("POST", "/api/v1/users".into()),
        ("GET", format!("/api/v1/users/{USER}")),
        ("PATCH", format!("/api/v1/users/{USER}")),
        ("PUT", format!("/api/v1/users/{USER}/org")),
        ("PUT", format!("/api/v1/users/{USER}/permissions")),
        ("PATCH", format!("/api/v1/users/{USER}/permissions")),
        ("GET", format!("/api/v1/users/{USER}/keys")),
        ("DELETE", format!("/api/v1/users/{USER}/keys/{USER_KEY}")),
        ("GET", format!("/api/v1/users/{USER}/preauth-keys")),
        ("POST", format!("/api/v1/users/{USER}/preauth-keys")),
        (
            "DELETE",
            format!("/api/v1/users/{USER}/preauth-keys/{PREAUTH_KEY}"),
        ),
        // Organizations and what hangs off them.
        ("GET", "/api/v1/orgs".into()),
        ("POST", "/api/v1/orgs".into()),
        ("GET", format!("/api/v1/orgs/{ORG}")),
        ("PATCH", format!("/api/v1/orgs/{ORG}")),
        ("GET", format!("/api/v1/orgs/{ORG}/users")),
        ("GET", format!("/api/v1/orgs/{ORG}/overview")),
        ("GET", format!("/api/v1/orgs/{ORG}/audit")),
        ("GET", "/api/v1/audit".into()),
        ("GET", "/api/v1/descriptor-catalog".into()),
        ("GET", "/api/v1/descriptor-catalog/import-runs".into()),
        (
            "GET",
            format!("/api/v1/descriptor-catalog/devices/{CLIENT}"),
        ),
        ("GET", "/api/v1/descriptor-catalog/names".into()),
        ("GET", "/api/v1/descriptor-catalog/by-digest".into()),
        ("POST", format!("/api/v1/orgs/{ORG}/descriptor-catalog")),
        (
            "POST",
            format!("/api/v1/orgs/{ORG}/descriptor-catalog/{CATALOG_NAME}/members"),
        ),
        (
            "POST",
            format!("/api/v1/orgs/{ORG}/descriptor-catalog/{CATALOG_NAME}/state"),
        ),
        ("GET", format!("/api/v1/orgs/{ORG}/clients")),
        ("PUT", format!("/api/v1/orgs/{ORG}/clients")),
        ("GET", format!("/api/v1/orgs/{ORG}/benchmarks")),
        ("GET", format!("/api/v1/orgs/{ORG}/benchmarks/{BENCHMARK}")),
        ("PUT", format!("/api/v1/orgs/{ORG}/benchmarks/{BENCHMARK}")),
        (
            "DELETE",
            format!("/api/v1/orgs/{ORG}/benchmarks/{BENCHMARK}"),
        ),
        ("GET", format!("/api/v1/orgs/{ORG}/client-preauth-keys")),
        ("POST", format!("/api/v1/orgs/{ORG}/client-preauth-keys")),
        (
            "DELETE",
            format!("/api/v1/orgs/{ORG}/client-preauth-keys/{CLIENT_PREAUTH_KEY}"),
        ),
        // Model params.
        ("GET", "/api/v1/model-params".into()),
        ("POST", "/api/v1/model-params".into()),
        ("PUT", format!("/api/v1/model-params/{MODEL_KEY}")),
        ("DELETE", format!("/api/v1/model-params/{MODEL_KEY}")),
        // Campaigns.
        ("GET", "/api/v1/campaigns".into()),
        ("POST", "/api/v1/campaigns".into()),
        ("GET", format!("/api/v1/campaigns/{CAMPAIGN}")),
        ("PATCH", format!("/api/v1/campaigns/{CAMPAIGN}")),
        ("DELETE", format!("/api/v1/campaigns/{CAMPAIGN}")),
        ("GET", format!("/api/v1/campaigns/{CAMPAIGN}/overview")),
        ("GET", format!("/api/v1/campaigns/{CAMPAIGN}/owners")),
        ("GET", format!("/api/v1/campaigns/{CAMPAIGN}/plan")),
        ("GET", format!("/api/v1/campaigns/{CAMPAIGN}/plans")),
        ("POST", format!("/api/v1/campaigns/{CAMPAIGN}/plans")),
        ("GET", format!("/api/v1/campaigns/{CAMPAIGN}/variants")),
        ("POST", format!("/api/v1/campaigns/{CAMPAIGN}/variants")),
        ("GET", format!("/api/v1/campaigns/{CAMPAIGN}/document")),
        ("PUT", format!("/api/v1/campaigns/{CAMPAIGN}/document")),
        ("POST", format!("/api/v1/campaigns/{CAMPAIGN}/rerun")),
        ("POST", format!("/api/v1/campaigns/{CAMPAIGN}/revert")),
        ("POST", format!("/api/v1/campaigns/{CAMPAIGN}/recalculate")),
        ("PUT", format!("/api/v1/campaigns/{CAMPAIGN}/owner")),
        // The vault.
        ("PUT", format!("/api/v1/vault/blobs/{USER}/{SHA256}")),
        ("HEAD", format!("/api/v1/vault/blobs/{USER}/{SHA256}")),
        ("GET", "/api/v1/vault/types".into()),
        ("GET", "/api/v1/vault/resources".into()),
        ("POST", "/api/v1/vault/resources".into()),
        ("GET", format!("/api/v1/vault/resources/{RESOURCE}")),
        ("DELETE", format!("/api/v1/vault/resources/{RESOURCE}")),
        ("GET", format!("/vault/resources/{RESOURCE}")),
        ("GET", format!("/vault/resources/{RESOURCE}/content")),
        // Results and the corpus a device runs on.
        ("POST", "/benchmarks".into()),
        ("POST", "/benchmarks/batch".into()),
        ("GET", format!("/datasets/{BENCHMARK}")),
        ("GET", format!("/jobs/{JOB}")),
        ("GET", format!("/jobs/{JOB}/eval-sample-results")),
        // Plans and the queue.
        ("GET", "/api/v1/plans".into()),
        ("POST", "/plans/claim".into()),
        ("POST", "/api/v1/plans/validate".into()),
        ("GET", format!("/api/v1/plans/{PLAN}")),
        ("PATCH", format!("/api/v1/plans/{PLAN}")),
        ("GET", format!("/api/v1/plans/{PLAN}/document")),
        ("GET", format!("/api/v1/plans/{PLAN}/variants")),
        ("GET", format!("/api/v1/jobs/{JOB}/plans")),
        ("GET", format!("/api/v1/plans/{PLAN}/jobs")),
        ("GET", format!("/api/v1/plans/{PLAN}/results.csv")),
        ("GET", format!("/api/v1/plans/{PLAN}/explain")),
        ("POST", format!("/api/v1/plans/{PLAN}/cancel")),
        ("PUT", format!("/plans/{JOB}/heartbeat")),
        ("POST", format!("/plans/{JOB}/reclaim")),
    ]
}

/// The signed routes that also take a signature naming only its key.
const KEY_ONLY_ROUTES: &[(&str, &str)] = &[("GET", "/api/v1/users/me")];

/// Every route that answers without a signature, with each path parameter as
/// `{}`.
const OPEN_ROUTES: &[(&str, &str)] = &[
    // A redirect to the app.
    ("GET", "/"),
    ("GET", "/health"),
    ("POST", "/clients/register"),
    ("POST", "/api/v1/users/register"),
    // A Clerk token is the credential, as the pre-auth key is above.
    ("POST", "/api/v1/users/register/clerk"),
    ("GET", "/benchmarks"),
    ("GET", "/benchmarks/{}"),
    ("GET", "/api/v2/benchmarks"),
    ("GET", "/api/v2/benchmarks/{}"),
    // The operator API's description, which names routes and no data.
    ("GET", "/api/v1/openapi.json"),
    // The same, as the API reference page.
    ("GET", "/docs"),
    ("GET", "/docs/"),
    ("GET", "/docs/style.css"),
    // The Svelte app's static files; it signs the API calls it makes itself.
    ("GET", "/app"),
    ("GET", "/app/"),
    ("GET", "/app/assets/{}"),
    // Every page the app routes itself, which is the same page.
    ("GET", "/app/{}"),
];

/// The routes that administer accounts, organizations or devices, with each
/// path parameter as `{}`. Their handlers take `HumanUser`, so a service
/// account is refused whatever its set holds.
const HUMAN_ONLY_ROUTES: &[(&str, &str)] = &[
    ("PATCH", "/api/v1/clients/{}"),
    ("DELETE", "/api/v1/clients/{}"),
    ("POST", "/api/v1/clients/{}/approve"),
    ("POST", "/api/v1/clients/{}/reject"),
    ("POST", "/api/v1/clients/{}/unsuspend"),
    ("PUT", "/api/v1/clients/{}/owner"),
    ("GET", "/api/v1/clients/{}/owners"),
    ("PUT", "/api/v1/clients/{}/permissions"),
    ("PATCH", "/api/v1/clients/{}/permissions"),
    ("POST", "/api/v1/users"),
    ("PATCH", "/api/v1/users/{}"),
    ("PUT", "/api/v1/users/{}/org"),
    ("PUT", "/api/v1/users/{}/permissions"),
    ("PATCH", "/api/v1/users/{}/permissions"),
    ("GET", "/api/v1/users/{}/keys"),
    ("DELETE", "/api/v1/users/{}/keys/{}"),
    ("GET", "/api/v1/users/{}/preauth-keys"),
    ("POST", "/api/v1/users/{}/preauth-keys"),
    ("DELETE", "/api/v1/users/{}/preauth-keys/{}"),
    ("POST", "/api/v1/orgs"),
    ("PATCH", "/api/v1/orgs/{}"),
    ("GET", "/api/v1/orgs/{}/audit"),
    ("GET", "/api/v1/audit"),
    ("PUT", "/api/v1/orgs/{}/clients"),
    ("PUT", "/api/v1/orgs/{}/benchmarks/{}"),
    ("DELETE", "/api/v1/orgs/{}/benchmarks/{}"),
    ("GET", "/api/v1/orgs/{}/client-preauth-keys"),
    ("POST", "/api/v1/orgs/{}/client-preauth-keys"),
    ("DELETE", "/api/v1/orgs/{}/client-preauth-keys/{}"),
    ("POST", "/api/v1/model-params"),
    ("PUT", "/api/v1/model-params/{}"),
    ("DELETE", "/api/v1/model-params/{}"),
];

const READ_CAMPAIGN: &str =
    "reading campaigns needs read_plans, or manage_orgs for your organization's";
const ADMINISTER_CAMPAIGN: &str =
    "campaigns need submit_plans to own one, or manage_orgs to administer your organization's";
const EDIT_ORG_CATALOG: &str =
    "changing an organization's descriptor catalog needs edit_catalog or edit_any_catalog";

/// Every `403` an account holding nothing is answered, by its text, with each
/// path parameter as `{}`: the gate each user route declares in `router.rs`,
/// written a second time. A user route absent here refuses no active account
/// outright.
const REFUSALS: &[(&str, &[(&str, &str)])] = &[
    (
        EDIT_ORG_CATALOG,
        &[
            ("POST", "/api/v1/orgs/{}/descriptor-catalog"),
            ("POST", "/api/v1/orgs/{}/descriptor-catalog/{}/members"),
            ("POST", "/api/v1/orgs/{}/descriptor-catalog/{}/state"),
        ],
    ),
    (
        "user lacks the read_clients permission",
        &[
            ("GET", "/api/v1/clients"),
            ("GET", "/api/v1/clients/capabilities"),
            ("GET", "/api/v1/clients/{}"),
            ("GET", "/api/v1/clients/{}/owners"),
            ("GET", "/api/v1/descriptor-catalog/import-runs"),
            ("GET", "/api/v1/descriptor-catalog/devices/{}"),
        ],
    ),
    (
        "user lacks the manage_clients permission",
        &[
            ("PATCH", "/api/v1/clients/{}"),
            ("DELETE", "/api/v1/clients/{}"),
            ("POST", "/api/v1/clients/{}/approve"),
            ("POST", "/api/v1/clients/{}/reject"),
            ("POST", "/api/v1/clients/{}/unsuspend"),
            ("PUT", "/api/v1/clients/{}/owner"),
            ("PUT", "/api/v1/clients/{}/permissions"),
            ("PATCH", "/api/v1/clients/{}/permissions"),
            ("PUT", "/api/v1/orgs/{}/clients"),
            ("GET", "/api/v1/orgs/{}/client-preauth-keys"),
            ("POST", "/api/v1/orgs/{}/client-preauth-keys"),
            ("DELETE", "/api/v1/orgs/{}/client-preauth-keys/{}"),
        ],
    ),
    (
        "user lacks the read_orgs permission",
        &[
            ("GET", "/api/v1/users"),
            ("GET", "/api/v1/users/{}"),
            ("GET", "/api/v1/orgs"),
            ("GET", "/api/v1/orgs/{}"),
            ("GET", "/api/v1/orgs/{}/users"),
            ("GET", "/api/v1/orgs/{}/overview"),
            ("GET", "/api/v1/orgs/{}/clients"),
        ],
    ),
    (
        "user lacks the manage_orgs permission",
        &[
            ("POST", "/api/v1/users"),
            ("PATCH", "/api/v1/users/{}"),
            ("PUT", "/api/v1/users/{}/org"),
            ("PUT", "/api/v1/users/{}/permissions"),
            ("PATCH", "/api/v1/users/{}/permissions"),
            ("GET", "/api/v1/users/{}/keys"),
            ("DELETE", "/api/v1/users/{}/keys/{}"),
            ("GET", "/api/v1/users/{}/preauth-keys"),
            ("POST", "/api/v1/users/{}/preauth-keys"),
            ("DELETE", "/api/v1/users/{}/preauth-keys/{}"),
            ("PATCH", "/api/v1/orgs/{}"),
            ("GET", "/api/v1/orgs/{}/audit"),
            ("POST", "/api/v1/model-params"),
            ("PUT", "/api/v1/model-params/{}"),
            ("DELETE", "/api/v1/model-params/{}"),
            ("PUT", "/api/v1/orgs/{}/benchmarks/{}"),
            ("DELETE", "/api/v1/orgs/{}/benchmarks/{}"),
        ],
    ),
    (
        "user lacks the manage_any_orgs permission",
        &[("POST", "/api/v1/orgs"), ("GET", "/api/v1/audit")],
    ),
    (
        READ_CAMPAIGN,
        &[
            ("GET", "/api/v1/campaigns"),
            ("GET", "/api/v1/campaigns/{}"),
            ("GET", "/api/v1/campaigns/{}/overview"),
            ("GET", "/api/v1/campaigns/{}/plan"),
            ("GET", "/api/v1/campaigns/{}/plans"),
        ],
    ),
    (
        ADMINISTER_CAMPAIGN,
        &[
            ("PATCH", "/api/v1/campaigns/{}"),
            ("DELETE", "/api/v1/campaigns/{}"),
            ("GET", "/api/v1/campaigns/{}/owners"),
            ("PUT", "/api/v1/campaigns/{}/owner"),
        ],
    ),
    (
        "user lacks the read_plans permission",
        &[
            ("GET", "/api/v1/plans"),
            ("GET", "/api/v1/plans/{}"),
            ("GET", "/api/v1/plans/{}/document"),
            ("GET", "/api/v1/plans/{}/variants"),
            ("GET", "/api/v1/jobs/{}/plans"),
            ("GET", "/api/v1/plans/{}/jobs"),
            ("GET", "/api/v1/plans/{}/results.csv"),
            ("GET", "/api/v1/plans/{}/explain"),
        ],
    ),
    (
        "user lacks the read_any_plans permission",
        &[("GET", "/api/v1/model-params")],
    ),
    (
        "user lacks the submit_plans permission",
        &[
            ("POST", "/api/v1/plans/validate"),
            ("PATCH", "/api/v1/plans/{}"),
            ("POST", "/api/v1/campaigns"),
            ("POST", "/api/v1/campaigns/{}/plans"),
            ("GET", "/api/v1/campaigns/{}/variants"),
            ("POST", "/api/v1/campaigns/{}/variants"),
            ("GET", "/api/v1/campaigns/{}/document"),
            ("PUT", "/api/v1/campaigns/{}/document"),
            ("POST", "/api/v1/campaigns/{}/rerun"),
            ("POST", "/api/v1/campaigns/{}/revert"),
            ("POST", "/api/v1/campaigns/{}/recalculate"),
        ],
    ),
    (
        "user lacks the cancel_plans permission",
        &[("POST", "/api/v1/plans/{}/cancel")],
    ),
    // `Active`, refused by the handler's scope: the path names another account.
    (
        "you may only upload to the vault for yourself, your organization, or a member of an \
         organization you administer",
        &[("PUT", "/api/v1/vault/blobs/{}/{}")],
    ),
];

/// The gates `a_gate_admits_what_it_declares` probes, one of each shape.
const NEEDS: Gate = Gate::Needs(UserPermission::ReadPlans);
const ALL_OF: Gate = Gate::AllOf(&[UserPermission::ReadOrgs, UserPermission::ReadClients]);
const ANY_OF: Gate = Gate::AnyOf(
    &[UserPermission::ReadPlans, UserPermission::ReadClients],
    "needs read_plans or read_clients",
);

/// `routes` less the `HEAD`s, whose answer carries no body to name a refusal.
fn without_head(
    routes: Vec<(&'static str, String)>,
) -> impl Iterator<Item = (&'static str, String)> {
    routes.into_iter().filter(|(method, _)| *method != "HEAD")
}

/// `path` with each path parameter, or each id from `IDS`, as `{}`.
fn placeholders(path: &str) -> String {
    path.split('/')
        .map(|segment| {
            if IDS.contains(&segment) || (segment.starts_with('{') && segment.ends_with('}')) {
                "{}"
            } else {
                segment
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Both ways: every registered route is listed, and every listed one
/// registered.
#[test]
fn every_route_is_signed_or_listed_as_open() -> anyhow::Result<()> {
    let registered: BTreeSet<(String, String)> = registered_routes(ROUTER)?
        .into_iter()
        .map(|route| (route.method, placeholders(&route.path)))
        .collect();
    let listed: BTreeSet<(String, String)> = signed_routes()
        .into_iter()
        .map(|(method, uri)| (method.to_string(), placeholders(&uri)))
        .chain(
            OPEN_ROUTES
                .iter()
                .map(|(method, path)| (method.to_string(), path.to_string())),
        )
        .collect();

    let unlisted: Vec<String> = registered
        .difference(&listed)
        .map(|(method, path)| format!("{method} {path}"))
        .collect();
    assert!(
        unlisted.is_empty(),
        "routes in neither signed_routes nor OPEN_ROUTES:\n{}",
        unlisted.join("\n")
    );
    let stale: Vec<String> = listed
        .difference(&registered)
        .map(|(method, path)| format!("{method} {path}"))
        .collect();
    assert!(
        stale.is_empty(),
        "listed routes the router does not register:\n{}",
        stale.join("\n")
    );
    Ok(())
}

/// A request with no credentials is refused by every signed route, before the
/// route reads its path, its body or the store.
#[tokio::test]
async fn every_signed_route_refuses_an_unsigned_request() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let mut open = Vec::new();
    for (method, uri) in signed_routes() {
        let resp = request_with_headers(&state, method, &uri, &[]).await?;
        let status = resp.status();
        // A `HEAD` answer carries no body to read the envelope from.
        let error = if method == "HEAD" {
            None
        } else {
            body_json(resp).await?["error"].as_str().map(str::to_string)
        };
        // A route that may name only its key asks for the key first.
        let missing = if KEY_ONLY_ROUTES.contains(&(method, uri.as_str())) {
            "missing X-User-Key-Id header"
        } else {
            "missing X-Client-Id header"
        };
        let refused = status == StatusCode::UNAUTHORIZED
            && (method == "HEAD" || error.as_deref() == Some(missing));
        if !refused {
            open.push(format!("{method} {uri}: {status} {error:?}"));
        }
    }
    assert!(
        open.is_empty(),
        "signed routes that did not refuse an unsigned request:\n{}",
        open.join("\n")
    );
    Ok(())
}

/// Every user route declares its permission gate in `router.rs`: a route that
/// forgot one answers `500` to every user rather than serving it open, and no
/// route here answers `500` otherwise.
#[tokio::test]
async fn every_user_route_declares_a_gate() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let user = registered_user(&state, &[]).await?;

    let ungated: Vec<String> = stream::iter(without_head(signed_routes()))
        .then(|(method, uri)| {
            let (state, user) = (&state, &user);
            async move {
                let status = signed_empty(state, user, method, &uri).await?.status();
                Ok::<_, anyhow::Error>(
                    (status == StatusCode::INTERNAL_SERVER_ERROR)
                        .then(|| format!("{method} {uri}")),
                )
            }
        })
        .try_filter_map(|row| async move { Ok(row) })
        .try_collect()
        .await?;
    assert!(
        ungated.is_empty(),
        "ungated user routes:\n{}",
        ungated.join("\n")
    );
    Ok(())
}

/// Each user route refuses an account holding nothing with the text
/// `REFUSALS` lists for it, whether its gate or its handler says so.
#[tokio::test]
async fn every_gate_requires_what_it_is_listed_with() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let user = registered_user(&state, &[]).await?;

    let refused: BTreeSet<(String, String, String)> = stream::iter(without_head(signed_routes()))
        .then(|(method, uri)| {
            let (state, user) = (&state, &user);
            async move {
                let resp = signed_empty(state, user, method, &uri).await?;
                if resp.status() != StatusCode::FORBIDDEN {
                    return Ok::<_, anyhow::Error>(None);
                }
                let error = json_str(&body_json(resp).await?, "error")?.to_string();
                Ok(Some((method.to_string(), placeholders(&uri), error)))
            }
        })
        .try_filter_map(|row| async move { Ok(row) })
        .try_collect()
        .await?;
    let expected: BTreeSet<(String, String, String)> = REFUSALS
        .iter()
        .flat_map(|(error, routes)| {
            routes.iter().map(move |(method, path)| {
                (method.to_string(), path.to_string(), error.to_string())
            })
        })
        .collect();
    let unlisted: Vec<_> = refused.difference(&expected).collect();
    let unrefused: Vec<_> = expected.difference(&refused).collect();
    assert!(
        unlisted.is_empty() && unrefused.is_empty(),
        "refused, not listed: {unlisted:#?}\nlisted, not refused: {unrefused:#?}"
    );
    Ok(())
}

/// The compound gates in `router.rs` list the names their refusals say: either
/// one opens a campaign route, and an organization's devices take both. The
/// ids name nothing, so an admitted caller is answered by the handler's lookup.
#[rstest]
#[case::any_of_by_its_first("GET", "/api/v1/campaigns", &[UserPermission::ReadPlans], StatusCode::OK, None)]
#[case::any_of_by_its_second("GET", "/api/v1/campaigns", &[UserPermission::ManageOrgs], StatusCode::OK, None)]
#[case::administer_by_its_first(
    "DELETE",
    "/api/v1/campaigns/campaign_0195c3e2a4f37b8c",
    &[UserPermission::SubmitPlans],
    StatusCode::NOT_FOUND,
    None
)]
#[case::administer_by_its_second(
    "DELETE",
    "/api/v1/campaigns/campaign_0195c3e2a4f37b8c",
    &[UserPermission::ManageOrgs],
    StatusCode::NOT_FOUND,
    None
)]
#[case::all_of_lacking_its_second(
    "GET",
    "/api/v1/orgs/org_0195c3e2a4f37b8c/clients",
    &[UserPermission::ReadOrgs],
    StatusCode::FORBIDDEN,
    Some("user lacks the read_clients permission")
)]
#[case::all_of_lacking_its_first(
    "GET",
    "/api/v1/orgs/org_0195c3e2a4f37b8c/clients",
    &[UserPermission::ReadClients],
    StatusCode::FORBIDDEN,
    Some("user lacks the read_orgs permission")
)]
#[case::all_of_holding_both(
    "GET",
    "/api/v1/orgs/org_0195c3e2a4f37b8c/clients",
    &[UserPermission::ReadOrgs, UserPermission::ReadClients],
    StatusCode::NOT_FOUND,
    None
)]
#[tokio::test]
async fn a_compound_route_admits_on_the_names_it_lists(
    #[case] method: &str,
    #[case] uri: &str,
    #[case] held: &[UserPermission],
    #[case] status: StatusCode,
    #[case] error: Option<&str>,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let user = registered_user(&state, held).await?;
    // A member, so `manage_orgs` administers an organization's campaigns.
    let org = seed_org(&state, "acme").await?;
    set_user_org(&state, &user.1, Some(org.org_id)).await?;

    let resp = signed_empty(&state, &user, method, uri).await?;
    assert_eq!(resp.status(), status);
    if let Some(error) = error {
        assert_eq!(json_str(&body_json(resp).await?, "error")?, error);
    }
    Ok(())
}

/// What a gate does on its own, behind a handler that checks nothing. A user
/// extractor answers `500` on a route that declares none, which is how
/// `every_user_route_declares_a_gate` finds one.
#[rstest]
#[case::active_ungated(get(async |_: ActiveUser| {}), &[], StatusCode::INTERNAL_SERVER_ERROR)]
#[case::human_ungated(get(async |_: HumanUser| {}), &[], StatusCode::INTERNAL_SERVER_ERROR)]
#[case::active(get(gate(Gate::Active, async |_: ActiveUser| {})), &[], StatusCode::OK)]
#[case::human(get(gate(Gate::Active, async |_: HumanUser| {})), &[], StatusCode::OK)]
#[case::needs_without(get(gate(NEEDS, async |_: ActiveUser| {})), &[], StatusCode::FORBIDDEN)]
#[case::needs_with(
    get(gate(NEEDS, async |_: ActiveUser| {})),
    &[UserPermission::ReadPlans],
    StatusCode::OK
)]
#[case::all_of_its_first_alone(
    get(gate(ALL_OF, async |_: ActiveUser| {})),
    &[UserPermission::ReadOrgs],
    StatusCode::FORBIDDEN
)]
#[case::all_of_its_second_alone(
    get(gate(ALL_OF, async |_: ActiveUser| {})),
    &[UserPermission::ReadClients],
    StatusCode::FORBIDDEN
)]
#[case::all_of_both(
    get(gate(ALL_OF, async |_: ActiveUser| {})),
    &[UserPermission::ReadOrgs, UserPermission::ReadClients],
    StatusCode::OK
)]
#[case::any_of_neither(get(gate(ANY_OF, async |_: ActiveUser| {})), &[], StatusCode::FORBIDDEN)]
#[case::any_of_its_first(
    get(gate(ANY_OF, async |_: ActiveUser| {})),
    &[UserPermission::ReadPlans],
    StatusCode::OK
)]
#[case::any_of_its_second(
    get(gate(ANY_OF, async |_: ActiveUser| {})),
    &[UserPermission::ReadClients],
    StatusCode::OK
)]
#[tokio::test]
async fn a_gate_admits_what_it_declares(
    #[case] route: MethodRouter<AppState>,
    #[case] held: &[UserPermission],
    #[case] status: StatusCode,
) -> anyhow::Result<()> {
    const URI: &str = "/probe";
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (key, user_id, key_id) = registered_user(&state, held).await?;

    let request = user_auth_headers(&key, &user_id, &key_id, "GET", URI)?
        .into_iter()
        .fold(Request::get(URI), |request, (name, value)| {
            request.header(name, value)
        })
        .body(Body::empty())?;
    let resp = Router::new()
        .route(URI, route)
        .with_state(state)
        .oneshot(request)
        .await?;
    assert_eq!(resp.status(), status);
    Ok(())
}

/// A service account is refused on exactly `HUMAN_ONLY_ROUTES`, even holding
/// every admin name from before the rule, and reaches every other handler.
#[tokio::test]
async fn a_service_account_is_refused_exactly_the_human_only_routes() -> anyhow::Result<()> {
    const REFUSAL: &str = "a service account cannot administer accounts, organizations or devices";
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let service = registered_user(&state, &[]).await?;
    let org = seed_org(&state, "acme").await?;
    make_service_account(&state, &service.1, Some(org.org_id)).await?;
    let every_admin_name = [
        UserPermissionGroup::OrgAdmin,
        UserPermissionGroup::ServerAdmin,
    ]
    .iter()
    .flat_map(|group| group.permissions().iter().copied())
    .collect::<Vec<_>>();
    grant_user(&state, &service.1, &every_admin_name).await?;

    let refused: BTreeSet<(String, String)> = stream::iter(without_head(signed_routes()))
        .then(|(method, uri)| {
            let (state, service) = (&state, &service);
            async move {
                let resp = signed_empty(state, service, method, &uri).await?;
                let human_only = resp.status() == StatusCode::FORBIDDEN
                    && body_json(resp).await?["error"].as_str() == Some(REFUSAL);
                Ok::<_, anyhow::Error>(human_only.then(|| (method.to_string(), placeholders(&uri))))
            }
        })
        .try_filter_map(|row| async move { Ok(row) })
        .try_collect()
        .await?;
    let expected: BTreeSet<(String, String)> = HUMAN_ONLY_ROUTES
        .iter()
        .map(|(method, path)| (method.to_string(), path.to_string()))
        .collect();
    assert_eq!(refused, expected);
    Ok(())
}

/// A method a path does not register is a `405` before any extractor runs, so
/// no handler is reached through a verb it was not written for.
#[tokio::test]
async fn a_method_a_path_does_not_register_is_a_405_before_authentication() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let unanswered: Vec<(&str, String)> = vec![
        ("POST", "/health".into()),
        ("PUT", "/clients/me".into()),
        ("POST", "/api/v1/clients".into()),
        ("DELETE", "/api/v1/clients/capabilities".into()),
        ("PUT", format!("/api/v1/clients/{CLIENT}")),
        ("GET", format!("/api/v1/clients/{CLIENT}/approve")),
        ("GET", format!("/api/v1/clients/{CLIENT}/owner")),
        ("GET", "/api/v1/users/register".into()),
        ("PATCH", "/api/v1/users/me".into()),
        ("DELETE", "/api/v1/users/me".into()),
        ("GET", "/api/v1/users/me/keys".into()),
        ("PUT", format!("/api/v1/users/{USER}")),
        // Offboarding is a `PATCH` to disabled; nothing deletes an account here.
        ("DELETE", format!("/api/v1/users/{USER}")),
        ("POST", format!("/api/v1/users/{USER}/permissions")),
        ("GET", format!("/api/v1/users/{USER}/org")),
        ("DELETE", format!("/api/v1/users/{USER}/org")),
        ("PUT", "/api/v1/orgs".into()),
        ("DELETE", "/api/v1/orgs".into()),
        // An organization is disabled, not deleted, over HTTP; the CLI deletes one.
        ("DELETE", format!("/api/v1/orgs/{ORG}")),
        ("POST", format!("/api/v1/orgs/{ORG}")),
        ("PUT", format!("/api/v1/orgs/{ORG}")),
        ("POST", format!("/api/v1/orgs/{ORG}/users")),
        ("POST", format!("/api/v1/orgs/{ORG}/clients")),
        ("DELETE", format!("/api/v1/orgs/{ORG}/clients")),
        ("PUT", format!("/api/v1/campaigns/{CAMPAIGN}/plan")),
        ("PUT", format!("/vault/resources/{RESOURCE}")),
        ("PATCH", format!("/vault/resources/{RESOURCE}")),
        ("POST", format!("/vault/resources/{RESOURCE}")),
        ("GET", format!("/api/v1/vault/blobs/{USER}/{SHA256}")),
        ("POST", format!("/api/v1/vault/blobs/{USER}/{SHA256}")),
        ("DELETE", format!("/api/v1/vault/blobs/{USER}/{SHA256}")),
        ("GET", "/benchmarks/batch".into()),
        ("PUT", format!("/benchmarks/{BENCHMARK}")),
        ("DELETE", format!("/benchmarks/{BENCHMARK}")),
        // A result meant for `POST /benchmarks` sent to the v2 catalog.
        ("POST", "/api/v2/benchmarks".into()),
        ("PUT", format!("/api/v2/benchmarks/{BENCHMARK}")),
        ("POST", format!("/datasets/{BENCHMARK}")),
        ("POST", "/api/v1/plans".into()),
        ("DELETE", "/api/v1/plans".into()),
        ("GET", "/plans/claim".into()),
        ("GET", "/api/v1/plans/validate".into()),
        ("POST", format!("/api/v1/plans/{PLAN}")),
        ("GET", format!("/api/v1/plans/{PLAN}/cancel")),
        ("GET", format!("/plans/{JOB}/heartbeat")),
        ("POST", format!("/plans/{JOB}/heartbeat")),
        ("GET", format!("/plans/{JOB}/reclaim")),
        ("PUT", format!("/plans/{JOB}/reclaim")),
    ];
    let mut answered = Vec::new();
    for (method, uri) in unanswered {
        let status = request_with_headers(&state, method, &uri, &[])
            .await?
            .status();
        if status != StatusCode::METHOD_NOT_ALLOWED {
            answered.push(format!("{method} {uri}: {status}"));
        }
    }
    assert!(
        answered.is_empty(),
        "methods answered with something other than 405:\n{}",
        answered.join("\n")
    );
    Ok(())
}

/// A path the table does not know is a `404`, signed or not, so a typo in a
/// client's URL is not mistaken for a refusal. A top-level unknown path is
/// `error_envelope.rs`'s.
#[tokio::test]
async fn an_unknown_path_is_a_404() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;

    for uri in ["/clients/me/no-such-route", "/v2/benchmarks"] {
        let resp = unauthed_get(&state, uri).await?;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "unsigned {uri}");
        let resp = authed_get(&state, &key, &client_id, uri).await?;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "signed {uri}");
        assert_eq!(
            json_str(&body_json(resp).await?, "error")?,
            "no such route",
            "{uri}"
        );
    }
    Ok(())
}

/// The `/api/v1` writes that read no body, so there is no digest to hold.
const BODYLESS_WRITES: &[(&str, &str)] = &[
    ("POST", "/api/v1/clients/{}/approve"),
    ("POST", "/api/v1/clients/{}/reject"),
    ("POST", "/api/v1/clients/{}/unsuspend"),
    ("POST", "/api/v1/plans/{}/cancel"),
];

/// The writes that take a plan document and nothing else.
const DOCUMENT_WRITES: &[(&str, &str)] = &[
    ("POST", "/api/v1/campaigns"),
    ("POST", "/api/v1/campaigns/{}/plans"),
    ("POST", "/api/v1/campaigns/{}/variants"),
    ("PUT", "/api/v1/campaigns/{}/document"),
    ("POST", "/api/v1/plans/validate"),
];

/// Every `/api/v1` write that reads a body refuses one that does not match the
/// `Content-Digest` its RFC 9421 signature covers. The auth extractor only
/// records the digest, so a route reading its body some other way would skip
/// the check. The ids are real, so each write gets past its own lookups to the
/// body.
#[tokio::test]
async fn every_rfc9421_body_is_held_to_its_digest() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let admin = registered_user(&state, UserPermissionGroup::ServerAdmin.permissions()).await?;
    let org = create_org(&state, &admin, "acme").await?;
    let (campaign, head) = created_campaign(&state, &admin, &[("name", "Nightly")]).await?;
    let signed = b"the body the signature names";

    let mut unchecked = Vec::new();
    for (method, uri) in signed_routes() {
        let route = placeholders(&uri);
        let route = (method, route.as_str());
        if !matches!(method, "POST" | "PUT" | "PATCH")
            || !uri.starts_with("/api/v1/")
            || BODYLESS_WRITES.contains(&route)
        {
            continue;
        }
        let uri = uri
            .replace(CAMPAIGN, &campaign)
            .replace(ORG, &org)
            .replace(USER, admin.1.as_str());
        let uri = match route {
            ("POST", "/api/v1/campaigns") => format!("{uri}?name=Swept"),
            ("POST", "/api/v1/campaigns/{}/variants")
            | ("POST", "/api/v1/campaigns/{}/revert")
            | ("PUT", "/api/v1/campaigns/{}/document") => {
                format!("{uri}?base_plan_id={head}")
            }
            _ => uri,
        };
        let (content_type, body) = if DOCUMENT_WRITES.contains(&route) {
            ("application/toml", ONE_CELL_PLAN.as_bytes())
        } else {
            ("application/json", b"{}".as_slice())
        };
        let (key, _, key_id) = &admin;
        let mut headers = rfc9421_headers(key, key_id, method, &uri, Some(signed))?;
        headers.retain(|(name, _)| *name != "Content-Length");
        headers.push(("Content-Length", body.len().to_string()));
        let resp = raw_request(
            &state,
            method,
            &uri,
            &headers,
            Some(content_type),
            body.to_vec(),
        )
        .await?;
        let status = resp.status();
        let error = body_json(resp).await?["error"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let refused = status == StatusCode::BAD_REQUEST && error.contains("Content-Digest");
        if !refused {
            unchecked.push(format!("{method} {uri}: {status} {error}"));
        }
    }
    assert!(
        unchecked.is_empty(),
        "RFC 9421 writes that did not hold the body to its digest:\n{}",
        unchecked.join("\n")
    );
    Ok(())
}

/// Every `/api/v1` JSON body refuses a key it does not take, so a field sent to
/// the wrong route, such as an `owner` on a `PATCH`, is a `400` rather than
/// ignored. The ids are real for the writes that read their body after a lookup.
#[tokio::test]
async fn every_json_body_refuses_a_key_it_does_not_take() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let admin = registered_user(&state, UserPermissionGroup::ServerAdmin.permissions()).await?;
    let org = create_org(&state, &admin, "acme").await?;
    let (campaign, head) = created_campaign(&state, &admin, &[("name", "Nightly")]).await?;
    let stray = serde_json::json!({ "stray_key": 1 });

    let mut lenient = Vec::new();
    for (method, uri) in signed_routes() {
        let route = placeholders(&uri);
        let route = (method, route.as_str());
        if !matches!(method, "POST" | "PUT" | "PATCH")
            || !uri.starts_with("/api/v1/")
            || BODYLESS_WRITES.contains(&route)
            || DOCUMENT_WRITES.contains(&route)
            // Raw bytes, stored as sent.
            || route == ("PUT", "/api/v1/vault/blobs/{}/{}")
        {
            continue;
        }
        // These two check required keys first, so they send them.
        let body = match route {
            ("POST", "/api/v1/vault/resources") => {
                serde_json::json!({ "name": "x", "type": "gguf_vision", "stray_key": 1 })
            }
            ("PUT", "/api/v1/orgs/{}/benchmarks/{}") => serde_json::json!({
                "benchmark_type": "prefill_throughput",
                "parameter_prefill_tokens": 256,
                "stray_key": 1,
            }),
            _ => stray.clone(),
        };
        let uri = uri
            .replace(CAMPAIGN, &campaign)
            .replace(ORG, &org)
            .replace(USER, admin.1.as_str());
        // The query is read before the body.
        let uri = match route {
            ("POST", "/api/v1/campaigns/{}/revert") => format!("{uri}?base_plan_id={head}"),
            _ => uri,
        };
        let resp = signed_json(&state, &admin, method, &uri, &body).await?;
        lenient.extend(stray_kept(&format!("{method} {uri}"), resp).await?);
    }
    for uri in ["/api/v1/users/register", "/api/v1/users/register/clerk"] {
        let resp = unauthed_post(&state, uri, &stray).await?;
        lenient.extend(stray_kept(&format!("POST {uri}"), resp).await?);
    }
    assert!(
        lenient.is_empty(),
        "JSON writes that did not refuse a key they do not take:\n{}",
        lenient.join("\n")
    );
    Ok(())
}

/// `write`'s answer, unless it is the `400` naming `stray_key`.
async fn stray_kept(
    write: &str,
    resp: axum::http::Response<Body>,
) -> anyhow::Result<Option<String>> {
    let status = resp.status();
    let body = body_json(resp).await?;
    let error = body
        .pointer("/error")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    Ok(
        (status != StatusCode::BAD_REQUEST || !error.contains("stray_key"))
            .then(|| format!("{write}: {status} {error}")),
    )
}

/// The OpenAPI document marks exactly the routes [`HUMAN_ONLY_ROUTES`] lists as
/// refusing a service account.
#[test]
fn the_document_marks_each_route_that_refuses_a_service_account() -> anyhow::Result<()> {
    let openapi = pipette_httpapi::api_schema::openapi::OpenApi::build()?;
    let paths = openapi
        .document()
        .pointer("/paths")
        .and_then(serde_json::Value::as_object)
        .context("the document has paths")?;
    let marked: BTreeSet<(String, String)> = paths
        .iter()
        .flat_map(|(path, operations)| {
            operations
                .as_object()
                .into_iter()
                .flatten()
                .filter(|(_, operation)| operation.get("x-human-only") == Some(&true.into()))
                .map(move |(method, _)| (method.to_uppercase(), placeholders(path)))
        })
        .collect();
    let listed: BTreeSet<(String, String)> = HUMAN_ONLY_ROUTES
        .iter()
        .map(|(method, path)| ((*method).to_owned(), (*path).to_owned()))
        .collect();
    assert_eq!(marked, listed);
    Ok(())
}
