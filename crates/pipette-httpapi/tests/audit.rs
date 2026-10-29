//! The audit log as the HTTP layer writes and reads it: one event per request
//! to a write route, naming the caller, what it acted on, and the organizations
//! it concerns. SQL alone keeps the log, so these run on a scratch database and
//! skip without one, except the case of a server that has none.

use std::collections::BTreeSet;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::http::StatusCode;
use futures::{StreamExt, TryStreamExt, stream};
use rstest::rstest;
use serde_json::json;

use pipette_core::model_params::ModelParams;
use pipette_httpapi::state::AppState;
use pipette_plan_types::ModelKey;
use pipette_plan_types::{
    AuditAction, AuditActor, AuditEvent, AuditOutcome, AuditTarget, ClientId, ClientOwner, OrgId,
    UserPermission,
};
use pipette_store::{AuditFilter, ModelParamsPage, ModelParamsStore, Stores};
use pipette_store_fs::build_local_fs_stores;
use pipette_store_testkit::CapturingAuditStore;
use pipette_web_testkit::*;

const ROUTER: &str = include_str!("../src/router.rs");

/// [`seeded_sql_state`]'s state and the scratch database it keeps alive, or a
/// return of `Ok(())` from the test where no database server is configured.
macro_rules! sql_state_or_skip {
    ($dir:expr) => {{
        let Some(sql) = seeded_sql_state($dir).await? else {
            return Ok(());
        };
        (sql.state.clone(), sql)
    }};
}

/// Every event, newest first.
async fn events(state: &AppState) -> anyhow::Result<Vec<AuditEvent>> {
    let limit = NonZeroUsize::new(1000).context("a page size")?;
    let store = state.audit_store.as_deref().context("an audit store")?;
    Ok(store
        .list_events(&AuditFilter::default(), None, limit)
        .await?
        .items)
}

/// How many events `send` adds to the log.
async fn events_added(
    state: &AppState,
    send: impl std::future::Future<Output = anyhow::Result<()>> + Send,
) -> anyhow::Result<usize> {
    let before = events(state).await?.len();
    send.await?;
    Ok(events(state).await?.len() - before)
}

async fn latest_event(state: &AppState) -> anyhow::Result<AuditEvent> {
    events(state).await?.into_iter().next().context("an event")
}

/// The newest event, which a streamed answer writes after its body ends.
async fn settled_event(state: &AppState, count: usize) -> anyhow::Result<AuditEvent> {
    for _ in 0..100 {
        if events(state).await?.len() == count {
            return latest_event(state).await;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    anyhow::bail!("no event settled: {:#?}", events(state).await?)
}

/// A device's signed reclaim of a job, the device write these tests record.
async fn device_reclaim(
    state: &AppState,
    key: &ed25519_dalek::SigningKey,
    client_id: &ClientId,
) -> anyhow::Result<axum::http::Response<axum::body::Body>> {
    let uri = format!("/plans/{}/reclaim", sample("job_id"));
    let headers = auth_headers(key, client_id, "POST", &uri)?;
    request_with_headers(state, "POST", &uri, &headers).await
}

fn route_of(event: &AuditEvent) -> (&str, &str, u16) {
    match &event.action {
        AuditAction::Http {
            method,
            route,
            status,
            ..
        } => (method.as_str(), route.as_str(), *status),
        AuditAction::Cli { .. } | AuditAction::FirstClerkSignIn { .. } => ("", "", 0),
    }
}

fn orgs(ids: &[&OrgId]) -> BTreeSet<OrgId> {
    ids.iter().copied().cloned().collect()
}

/// A well-formed value for each path parameter `router.rs` names.
fn sample(param: &str) -> &'static str {
    match param {
        "client_id" => "ev1_0123456789abcdef",
        "user_id" | "principal_id" => "user_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e",
        "org_id" => "org_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e",
        "campaign_id" => "campaign_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e",
        "plan_id" => "plan-0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e",
        "job_id" => "job-0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e",
        "resource_id" => "vault_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e",
        "sha256" => "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        "model_key" => "acmelm-4.5b-text",
        "benchmark_id" => "gsm8k",
        _ => "0195c3e2a4f37b8c",
    }
}

fn concrete(pattern: &str) -> String {
    pattern
        .split('/')
        .map(
            |segment| match segment.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
                Some(param) => sample(param).to_owned(),
                None => segment.to_owned(),
            },
        )
        .collect::<Vec<_>>()
        .join("/")
}

/// The id that a request claims when it proves nothing. The claim makes its
/// refusal an event.
const CLAIMED: &str = "ev1_0123456789abcdef";

/// Call `route` unsigned, with a claim. Return `None` if its event is the
/// newest and says what happened. Otherwise, return what went wrong.
async fn unrecorded(state: &AppState, route: &RegisteredRoute) -> anyhow::Result<Option<String>> {
    let before = events(state).await?.len();
    let uri = concrete(&route.path);
    let claim = [("X-Client-Id", CLAIMED.to_owned())];
    let resp = json_request(state, &route.method, &uri, &claim, &json!({})).await?;
    let events = events(state).await?;
    let recorded = events.first().filter(|_| events.len() == before + 1);
    let matches = recorded.is_some_and(|event| {
        let (method, pattern, status) = route_of(event);
        method == route.method
            && pattern == route.path
            && status == resp.status().as_u16()
            && event.outcome != AuditOutcome::Succeeded
            && matches!(event.actor, AuditActor::Unverified { .. })
    });
    Ok((!matches).then(|| format!("{} {} ({})", route.method, route.path, resp.status())))
}

/// The middleware is a route layer, so no write route can be added outside it.
/// The test sends an unsigned request that claims an id to each write route.
/// The audit log records each one under the pattern of its route, except on
/// the routes in `UNAUDITED`.
#[tokio::test]
async fn every_write_route_records_an_unsigned_attempt() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _db) = sql_state_or_skip!(dir.path());
    let writes: Vec<RegisteredRoute> = registered_routes(ROUTER)?
        .into_iter()
        .filter(|route| !matches!(route.method.as_str(), "GET" | "HEAD"))
        .filter(|route| !pipette_httpapi::audit::UNAUDITED.contains(&route.path.as_str()))
        .collect();
    assert!(writes.len() > 50, "{writes:#?}");

    let state = &state;
    let missing: Vec<String> = stream::iter(&writes)
        .then(|route| unrecorded(state, route))
        .try_filter_map(|missed| async move { Ok(missed) })
        .try_collect()
        .await?;
    anyhow::ensure!(missing.is_empty(), "not recorded: {missing:#?}");
    Ok(())
}

/// A read writes nothing.
#[tokio::test]
async fn a_read_records_nothing() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _db) = sql_state_or_skip!(dir.path());
    let reader = registered_user(&state, &[UserPermission::ReadAnyOrgs]).await?;
    let before = events(&state).await?.len();
    let resp = signed_empty(&state, &reader, "GET", "/api/v1/orgs").await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(events(&state).await?.len(), before);
    Ok(())
}

/// A create names the account that signed, its key, and the record it made,
/// and the new organization's log lists it.
#[tokio::test]
async fn a_create_names_its_caller_and_what_it_made() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _db) = sql_state_or_skip!(dir.path());
    let admin = registered_user(&state, &[UserPermission::ManageAnyOrgs]).await?;
    let org_id = OrgId::try_new(create_org(&state, &admin, "acme").await?)?;

    let event = latest_event(&state).await?;
    assert_eq!(
        event.actor,
        AuditActor::User {
            user_id: admin.1.clone(),
            key_id: admin.2.clone(),
        }
    );
    assert_eq!(route_of(&event), ("POST", "/api/v1/orgs", 201));
    assert_eq!(event.outcome, AuditOutcome::Succeeded);
    assert_eq!(
        event.targets,
        BTreeSet::from([AuditTarget::Org {
            org_id: org_id.clone()
        }])
    );
    assert_eq!(event.org_ids, orgs(&[&org_id]));
    Ok(())
}

/// A device move names the device, so the organization that the device leaves
/// is in the log, with the one it is moved to and the mover's own. That holds
/// for a move a fleet administrator makes, and for a move refused to an
/// organization's administrator who reaches into another organization.
#[rstest]
#[case::handed_over(
    &[UserPermission::ManageAnyOrgs, UserPermission::ManageAnyClients],
    false,
    StatusCode::OK,
    AuditOutcome::Succeeded
)]
#[case::refused_outside_the_scope(
    &[UserPermission::ManageClients],
    true,
    StatusCode::FORBIDDEN,
    AuditOutcome::Refused
)]
#[tokio::test]
async fn a_device_move_is_in_the_owner_s_log(
    #[case] permissions: &[UserPermission],
    #[case] mover_in_target: bool,
    #[case] status: StatusCode,
    #[case] outcome: AuditOutcome,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _db) = sql_state_or_skip!(dir.path());
    let (from, to, home) = (
        seed_org(&state, "from").await?.org_id,
        seed_org(&state, "to").await?.org_id,
        seed_org(&state, "home").await?.org_id,
    );
    let mover = registered_user(&state, permissions).await?;
    let mover_org = if mover_in_target { &to } else { &home };
    set_user_org(&state, &mover.1, Some(mover_org.clone())).await?;
    let (_, client_id) = register_and_approve(&state).await?;
    own_client_by_org(&state, &client_id, &from).await?;

    let resp = signed_json(
        &state,
        &mover,
        "PUT",
        &format!("/api/v1/orgs/{to}/clients"),
        &json!({ "client_ids": [client_id] }),
    )
    .await?;
    assert_eq!(resp.status(), status);
    let event = latest_event(&state).await?;
    assert_eq!(event.outcome, outcome);
    assert!(event.targets.contains(&AuditTarget::Client { client_id }));
    assert_eq!(event.org_ids, orgs(&[&from, &to, mover_org]));
    Ok(())
}

/// A check and a dry run change nothing, so they write nothing; refused, the
/// same request is recorded.
#[tokio::test]
async fn a_request_that_changes_nothing_records_nothing() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _db) = sql_state_or_skip!(dir.path());
    let (_, client_id) = register_and_approve(&state).await?;
    let filer = registered_user_with_detail(&state, true).await?;
    let document = submittable_document(&client_id, "check", 1);
    let before = events(&state).await?.len();

    let resp = post_plan_document(&state, &filer, "/api/v1/plans/validate", &document).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let campaign = put_idle_campaign(&state, &filer.1, "dry").await?;
    let uri = format!("{}&dry_run=true", filing_uri(campaign.as_str(), "dry")?);
    let resp = post_plan_document(&state, &filer, &uri, &document).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(events(&state).await?.len(), before);

    let outsider = registered_user(&state, &[]).await?;
    let resp = post_plan_document(&state, &outsider, "/api/v1/plans/validate", &document).await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let event = latest_event(&state).await?;
    assert_eq!(event.outcome, AuditOutcome::Refused);
    assert!(matches!(&event.actor, AuditActor::User { user_id, .. } if user_id == &outsider.1));
    Ok(())
}

/// How a forged request is signed.
#[derive(Clone, Copy)]
enum Signing {
    /// The `X-Client-Id` scheme, which claims the account in a header.
    Legacy,
    /// The app's scheme, which claims the account through its key id.
    Rfc9421,
}

/// A signature that does not verify is recorded under the id it claimed, in
/// the server's log alone: a claim proves no organization.
#[rstest]
#[case::the_legacy_scheme(Signing::Legacy)]
#[case::rfc_9421(Signing::Rfc9421)]
#[tokio::test]
async fn a_forged_request_is_in_no_organization_s_log(
    #[case] signing: Signing,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _db) = sql_state_or_skip!(dir.path());
    let acme = seed_org(&state, "acme").await?.org_id;
    let (_, user_id, key_id) = registered_user(&state, &[UserPermission::ManageOrgs]).await?;
    set_user_org(&state, &user_id, Some(acme.clone())).await?;
    let forger = (signing_key()?, user_id.clone(), key_id);

    let created = json!({ "slug": "forged" });
    let resp = match signing {
        Signing::Legacy => signed_json(&state, &forger, "POST", "/api/v1/orgs", &created).await?,
        Signing::Rfc9421 => {
            rfc9421_request(&state, &forger, "POST", "/api/v1/orgs", Some(&created)).await?
        }
    };
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let event = latest_event(&state).await?;
    assert_eq!(
        event.actor,
        AuditActor::Unverified {
            claimed_id: Some(user_id.to_string())
        }
    );
    assert_eq!(event.outcome, AuditOutcome::Refused);
    assert!(event.org_ids.is_empty(), "{event:#?}");
    Ok(())
}

/// A refusal that names no caller and claims no id is no event.
#[tokio::test]
async fn an_anonymous_refusal_records_nothing() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _db) = sql_state_or_skip!(dir.path());
    let created = json!({ "slug": "anonymous" });
    let resp = json_request(&state, "POST", "/api/v1/orgs", &[], &created).await?;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(events(&state).await?.is_empty());
    Ok(())
}

/// A device's writes name it, and its owner's organization lists them; a claim
/// that finds no job and a heartbeat write nothing.
#[tokio::test]
async fn a_device_write_names_the_device() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _db) = sql_state_or_skip!(dir.path());
    let acme = seed_org(&state, "acme").await?.org_id;
    let (key, client_id) = register_and_approve(&state).await?;
    own_client(
        &state,
        &client_id,
        ClientOwner::Org {
            org_id: acme.clone(),
        },
    )
    .await?;
    let before = events(&state).await?.len();

    let headers = auth_headers(&key, &client_id, "POST", "/plans/claim")?;
    let resp = request_with_headers(&state, "POST", "/plans/claim", &headers).await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert_eq!(events(&state).await?.len(), before);

    let uri = format!("/plans/{}/heartbeat", sample("job_id"));
    let headers = auth_headers(&key, &client_id, "PUT", &uri)?;
    request_with_headers(&state, "PUT", &uri, &headers).await?;
    assert_eq!(events(&state).await?.len(), before);

    let resp = device_reclaim(&state, &key, &client_id).await?;
    let event = latest_event(&state).await?;
    assert_eq!(
        event.actor,
        AuditActor::Client {
            client_id: client_id.clone()
        }
    );
    assert_eq!(
        route_of(&event),
        ("POST", "/plans/{job_id}/reclaim", resp.status().as_u16())
    );
    assert_eq!(event.org_ids, orgs(&[&acme]));
    Ok(())
}

/// A streamed filing is recorded once it ends, with the status of its last
/// line and the plan it filed.
#[tokio::test]
async fn a_streamed_filing_records_its_result() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _db) = sql_state_or_skip!(dir.path());
    let acme = seed_org(&state, "acme").await?.org_id;
    let (_, client_id) = register_and_approve(&state).await?;
    let filer = registered_user_with_detail(&state, true).await?;
    set_user_org(&state, &filer.1, Some(acme.clone())).await?;
    let campaign = put_idle_campaign(&state, &filer.1, "streamed").await?;
    let before = events(&state).await?.len();

    let uri = filing_uri(campaign.as_str(), "streamed")?;
    let document = submittable_document(&client_id, "streamed", 1);
    let response = stream_plan_document(&state, &filer, "POST", &uri, &document).await?;
    let lines = stream_lines(response).await?;
    let last = lines.last().context("a result line")?;
    assert_eq!(last["status"], 201, "{lines:?}");

    let event = settled_event(&state, before + 1).await?;
    assert_eq!(route_of(&event).2, 201);
    assert_eq!(event.outcome, AuditOutcome::Succeeded);
    let plan_id = pipette_plan_types::PlanId::try_new(json_str(&last["body"], "plan_id")?)?;
    assert!(
        event.targets.contains(&AuditTarget::Plan { plan_id }),
        "{event:#?}"
    );
    assert!(event.targets.contains(&AuditTarget::Campaign {
        campaign_id: campaign
    }));
    assert_eq!(event.org_ids, orgs(&[&acme]));
    Ok(())
}

/// An organization's administrator reads its log and no other; the whole log
/// is a server administrator's.
#[tokio::test]
async fn an_org_admin_reads_its_own_log() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _db) = sql_state_or_skip!(dir.path());
    let (acme, rival) = (
        seed_org(&state, "acme").await?.org_id,
        seed_org(&state, "rival").await?.org_id,
    );
    let admin = registered_user(&state, &[UserPermission::ManageOrgs]).await?;
    set_user_org(&state, &admin.1, Some(acme.clone())).await?;
    // Probing one outside the scope must not name it in the log.
    for (org, name) in [(&acme, "Acme"), (&rival, "Rival")] {
        let renamed = json!({ "display_name": name });
        signed_json(
            &state,
            &admin,
            "PATCH",
            &format!("/api/v1/orgs/{org}"),
            &renamed,
        )
        .await?;
    }

    let page =
        body_json(signed_empty(&state, &admin, "GET", "/api/v1/orgs/acme/audit").await?).await?;
    let items = page["items"].as_array().context("items")?;
    assert_eq!(items.len(), 2, "{page:#}");
    assert!(
        items.iter().all(|event| event["org_ids"]
            .as_array()
            .is_some_and(|ids| ids.contains(&json!(acme)))),
        "{page:#}"
    );
    // Out of scope reads as absent on the wire, a `404`, and as a refusal in
    // the log.
    assert_eq!(
        items[0]["outcome"], "refused",
        "the rival rename, newest first"
    );
    assert_eq!(items[0]["action"]["status"], 404);
    assert_eq!(items[1]["outcome"], "succeeded");
    let rival_id = json!(rival);
    let names_rival = |event: &serde_json::Value| {
        let listed = event["org_ids"]
            .as_array()
            .is_some_and(|ids| ids.contains(&rival_id));
        let targeted = event["targets"]
            .as_array()
            .is_some_and(|targets| targets.iter().any(|target| target["org_id"] == rival_id));
        listed || targeted
    };
    assert!(!items.iter().any(names_rival), "{page:#}");

    let resp = signed_empty(
        &state,
        &admin,
        "GET",
        &format!("/api/v1/orgs/{rival}/audit"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let resp = signed_empty(&state, &admin, "GET", "/api/v1/audit").await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let server = registered_user(&state, &[UserPermission::ManageAnyOrgs]).await?;
    let all = body_json(signed_empty(&state, &server, "GET", "/api/v1/audit").await?).await?;
    let unredacted = all["items"].as_array().context("items")?;
    assert!(
        unredacted.iter().any(names_rival),
        "a server administrator sees every organization"
    );
    let every = events(&state).await?.len();
    assert_eq!(all["items"].as_array().map(Vec::len), Some(every));
    Ok(())
}

/// A page's cursor resumes past its last event.
#[tokio::test]
async fn the_log_pages_by_cursor() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _db) = sql_state_or_skip!(dir.path());
    let server = registered_user(&state, &[UserPermission::ManageAnyOrgs]).await?;
    for slug in ["one", "two", "three"] {
        create_org(&state, &server, slug).await?;
    }
    let expected: Vec<String> = events(&state)
        .await?
        .into_iter()
        .map(|event| event.event_id.to_string())
        .collect();

    let mut seen = Vec::new();
    let mut pages = 0;
    let mut uri = "/api/v1/audit?limit=2".to_string();
    loop {
        let page = body_json(signed_empty(&state, &server, "GET", &uri).await?).await?;
        let items = page["items"].as_array().context("items")?;
        assert!(items.len() <= 2, "{page:#}");
        pages += 1;
        assert!(
            pages <= expected.len(),
            "the cursor does not move: {page:#}"
        );
        seen.extend(
            items
                .iter()
                .filter_map(|event| event["event_id"].as_str().map(str::to_owned)),
        );
        match page["next_cursor"].as_str() {
            Some(cursor) => uri = format!("/api/v1/audit?limit=2&cursor={cursor}"),
            None => break,
        }
    }
    assert_eq!(seen, expected);
    assert!(pages >= 2, "{} events on {pages} page(s)", seen.len());
    Ok(())
}

/// Each filter narrows the log the server reads, and a value naming nothing is
/// refused rather than ignored.
#[tokio::test]
async fn the_log_filters_by_outcome_actor_and_record() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _db) = sql_state_or_skip!(dir.path());
    let acme = seed_org(&state, "acme").await?.org_id;
    let admin = registered_user(&state, &[UserPermission::ManageOrgs]).await?;
    set_user_org(&state, &admin.1, Some(acme.clone())).await?;
    let (key, client_id) = register_and_approve(&state).await?;
    own_client(
        &state,
        &client_id,
        ClientOwner::Org {
            org_id: acme.clone(),
        },
    )
    .await?;
    let rival = seed_org(&state, "rival").await?.org_id;
    let renamed = json!({ "display_name": "Acme Labs" });
    let (acme_uri, rival_uri) = (
        format!("/api/v1/orgs/{acme}"),
        format!("/api/v1/orgs/{rival}"),
    );
    signed_json(&state, &admin, "PATCH", &acme_uri, &renamed).await?;
    // Out of the admin's scope: a refusal its own organization's log lists.
    signed_json(&state, &admin, "PATCH", &rival_uri, &renamed).await?;
    // A forgery is in the server's log alone.
    let forger = (signing_key()?, admin.1.clone(), admin.2.clone());
    signed_json(&state, &forger, "PATCH", &acme_uri, &renamed).await?;
    device_reclaim(&state, &key, &client_id).await?;

    let read = |query: String| {
        let (state, admin) = (&state, &admin);
        async move {
            let uri = format!("/api/v1/orgs/acme/audit?{query}");
            let page = body_json(signed_empty(state, admin, "GET", &uri).await?).await?;
            let rows: Vec<(String, String)> = page["items"]
                .as_array()
                .context("items")?
                .iter()
                .map(|event| {
                    let kind = event["actor"]["type"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned();
                    let outcome = event["outcome"].as_str().unwrap_or_default().to_owned();
                    (kind, outcome)
                })
                .collect();
            anyhow::Ok(rows)
        }
    };
    let pair = |kind: &str, outcome: &str| (kind.to_owned(), outcome.to_owned());
    let reads = [
        ("outcome=refused", vec![pair("user", "refused")]),
        (
            "actor=user",
            vec![pair("user", "refused"), pair("user", "succeeded")],
        ),
        ("actor=unverified", Vec::new()),
    ];
    for (query, expected) in reads {
        assert_eq!(read(query.into()).await?, expected, "{query}");
    }
    let device = read("actor=client".into()).await?;
    assert_eq!(device.len(), 1, "{device:?}");
    let target = format!("target={client_id}");
    assert_eq!(
        read(target).await?,
        device,
        "a device's history is its own writes"
    );

    let rival_target = format!("target={rival}");
    for refused in [
        "outcome=done",
        "actor=robot",
        "target=a%20b",
        "colour=red",
        rival_target.as_str(),
    ] {
        let uri = format!("/api/v1/orgs/acme/audit?{refused}");
        let resp = signed_empty(&state, &admin, "GET", &uri).await?;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{refused}");
    }
    Ok(())
}

/// Names follow what the reader may read elsewhere: the acting account's
/// address, the reader's organization and campaigns; never an account or a
/// campaign a refused probe named from outside the reader's reach.
#[tokio::test]
async fn a_name_discloses_nothing_the_reader_cannot_read() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _db) = sql_state_or_skip!(dir.path());
    let (acme, rival) = (
        seed_org(&state, "acme").await?.org_id,
        seed_org(&state, "rival").await?.org_id,
    );
    let admin = registered_user(&state, &[UserPermission::ManageOrgs]).await?;
    set_user_org(&state, &admin.1, Some(acme.clone())).await?;
    let stranger = registered_user(&state, &[]).await?;
    set_user_org(&state, &stranger.1, Some(rival.clone())).await?;
    let ours = put_idle_campaign_of(
        &state,
        pipette_plan_types::PrincipalOwner::Org {
            org_id: acme.clone(),
        },
        "Ours",
    )
    .await?;
    let theirs = put_idle_campaign_of(
        &state,
        pipette_plan_types::PrincipalOwner::Org {
            org_id: rival.clone(),
        },
        "Theirs",
    )
    .await?;
    let server = registered_user(&state, &[UserPermission::ManageAnyOrgs]).await?;

    let rename = json!({ "name": "Ours, renamed" });
    let uri = format!("/api/v1/campaigns/{}", ours.campaign_id);
    assert_eq!(
        signed_json(&state, &admin, "PATCH", &uri, &rename)
            .await?
            .status(),
        StatusCode::OK
    );
    let uri = format!("/api/v1/campaigns/{}", theirs.campaign_id);
    let probe = signed_json(&state, &admin, "PATCH", &uri, &rename).await?;
    assert!(probe.status().is_client_error(), "{}", probe.status());
    let uri = format!("/api/v1/users/{}", stranger.1);
    let probe = signed_json(
        &state,
        &admin,
        "PATCH",
        &uri,
        &json!({ "status": "disabled" }),
    )
    .await?;
    assert!(probe.status().is_client_error(), "{}", probe.status());
    let renamed = json!({ "display_name": "Acme Labs" });
    let uri = format!("/api/v1/orgs/{acme}");
    signed_json(&state, &server, "PATCH", &uri, &renamed).await?;

    let page =
        body_json(signed_empty(&state, &admin, "GET", "/api/v1/orgs/acme/audit?names=true").await?)
            .await?;
    let names = page["names"].as_object().context("names")?;
    let named = |id: &str| names.get(id).and_then(|name| name.as_str());
    assert_eq!(named(acme.as_str()), Some("acme"));
    assert_eq!(named(ours.campaign_id.as_str()), Some("Ours, renamed"));
    assert!(named(admin.1.as_str()).is_some_and(|address| address.contains('@')));
    assert!(
        named(server.1.as_str()).is_some(),
        "an outside actor is named as a submitter is"
    );
    assert_eq!(named(theirs.campaign_id.as_str()), None, "{page:#}");
    assert_eq!(named(stranger.1.as_str()), None, "{page:#}");
    assert_eq!(named(rival.as_str()), None, "{page:#}");
    let without =
        body_json(signed_empty(&state, &admin, "GET", "/api/v1/orgs/acme/audit").await?).await?;
    assert!(without.get("names").is_none());
    Ok(())
}

/// A row says what its route does in words: the route table's summary, or the
/// device protocol's.
#[tokio::test]
async fn an_event_reads_as_what_its_route_does() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _db) = sql_state_or_skip!(dir.path());
    let acme = seed_org(&state, "acme").await?.org_id;
    let server = registered_user(&state, &[UserPermission::ManageAnyOrgs]).await?;
    let renamed = json!({ "display_name": "Acme Labs" });
    let uri = format!("/api/v1/orgs/{acme}");
    signed_json(&state, &server, "PATCH", &uri, &renamed).await?;
    let (key, client_id) = register_and_approve(&state).await?;
    device_reclaim(&state, &key, &client_id).await?;

    let page = body_json(signed_empty(&state, &server, "GET", "/api/v1/audit").await?).await?;
    let summary_of = |route: &str| {
        page["items"].as_array().and_then(|items| {
            items
                .iter()
                .find(|event| event["action"]["route"] == route)
                .and_then(|event| event["summary"].as_str())
        })
    };
    let expected = state.openapi.summary("PATCH", "/api/v1/orgs/{org_id}");
    assert!(expected.is_some());
    assert_eq!(summary_of("/api/v1/orgs/{org_id}"), expected);
    assert_eq!(
        summary_of("/plans/{job_id}/reclaim"),
        Some("Take back a job's lease")
    );
    Ok(())
}

/// A write that set what was already set is no event; the first one is.
#[tokio::test]
async fn a_write_that_changes_nothing_records_nothing() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _db) = sql_state_or_skip!(dir.path());
    let acme = seed_org(&state, "acme").await?.org_id;
    let server = registered_user(
        &state,
        &[
            UserPermission::ManageAnyOrgs,
            UserPermission::ManageAnyClients,
        ],
    )
    .await?;
    let member = registered_user(&state, &[]).await?;
    let (key, client_id) = register_pending(&state).await?;
    let renamed = json!({ "display_name": "Acme Labs" });
    let org_uri = format!("/api/v1/orgs/{acme}");
    let member_uri = format!("/api/v1/users/{}/org", member.1);
    let joined = json!({ "org_id": acme });
    let approve = format!("/api/v1/clients/{client_id}/approve");
    let details = json!({ "client_details": "bench box" });

    let writes = async || {
        let resp = signed_json(&state, &server, "PATCH", &org_uri, &renamed).await?;
        anyhow::ensure!(resp.status() == StatusCode::OK, "rename: {}", resp.status());
        let resp = signed_json(&state, &server, "PUT", &member_uri, &joined).await?;
        anyhow::ensure!(resp.status() == StatusCode::OK, "join: {}", resp.status());
        let resp = signed_empty(&state, &server, "POST", &approve).await?;
        anyhow::ensure!(
            resp.status() == StatusCode::OK,
            "approve: {}",
            resp.status()
        );
        let headers = auth_headers(&key, &client_id, "PATCH", "/clients/me")?;
        let resp = json_request(&state, "PATCH", "/clients/me", &headers, &details).await?;
        anyhow::ensure!(
            resp.status() == StatusCode::OK,
            "details: {}",
            resp.status()
        );
        Ok(())
    };
    assert_eq!(
        events_added(&state, writes()).await?,
        4,
        "each first write is recorded"
    );
    assert_eq!(
        events_added(&state, writes()).await?,
        0,
        "the repeats changed nothing"
    );
    Ok(())
}

/// A plan note set to what it already holds is no event; the change before it
/// is one.
#[tokio::test]
async fn a_plan_note_that_changes_nothing_records_nothing() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _db) = sql_state_or_skip!(dir.path());
    let owner = registered_user(&state, FULL_EDITOR).await?;
    let (_, plan_id) =
        created_campaign(&state, &owner, &[("name", "Nightly"), ("note", "first")]).await?;
    let uri = format!("/api/v1/plans/{plan_id}");
    let note = json!({ "note": "second" });
    let edit = async || {
        let resp = signed_json(&state, &owner, "PATCH", &uri, &note).await?;
        anyhow::ensure!(resp.status() == StatusCode::OK, "edit: {}", resp.status());
        Ok(())
    };
    assert_eq!(events_added(&state, edit()).await?, 1);
    assert_eq!(
        events_added(&state, edit()).await?,
        0,
        "the repeat changed nothing"
    );
    Ok(())
}

/// A model-params store whose rows do not read, and whose deletes succeed.
struct UnreadableModelParams;

#[async_trait::async_trait]
impl ModelParamsStore for UnreadableModelParams {
    async fn get_model_params(&self, _: &ModelKey) -> anyhow::Result<Option<ModelParams>> {
        anyhow::bail!("the row does not parse")
    }

    async fn put_model_params(&self, _: &ModelParams) -> anyhow::Result<()> {
        Ok(())
    }

    async fn list_model_params(
        &self,
        _: Option<&ModelKey>,
        _: NonZeroUsize,
    ) -> anyhow::Result<ModelParamsPage> {
        anyhow::bail!("the rows do not parse")
    }

    async fn delete_model_params(&self, _: &ModelKey) -> anyhow::Result<()> {
        Ok(())
    }
}

/// A model-params row that does not read still deletes, and the delete is
/// recorded.
#[tokio::test]
async fn an_unreadable_model_params_row_still_deletes() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let config = test_config(dir.path());
    let log = Arc::new(CapturingAuditStore::default());
    let stores = Stores {
        audit: Some(log.clone()),
        descriptor_catalog: None,
        model_params: Arc::new(UnreadableModelParams),
        ..build_local_fs_stores(&config)?
    };
    let state = AppState::new(config, stores, test_http_client()?).await?;
    let admin = registered_user(
        &state,
        &[UserPermission::ManageOrgs, UserPermission::ManageAnyOrgs],
    )
    .await?;

    let uri = "/api/v1/model-params/acmelm-4.5b-text";
    let resp = signed_empty(&state, &admin, "DELETE", uri).await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    state.audit_writes.close();
    state.audit_writes.wait().await;
    // Registering the admin wrote events too; the delete is the one asked about.
    let deletes: Vec<AuditEvent> = log
        .events()
        .into_iter()
        .filter(|event| route_of(event).0 == "DELETE")
        .collect();
    let [delete] = deletes.as_slice() else {
        anyhow::bail!("{} delete events", deletes.len());
    };
    assert_eq!(
        route_of(delete),
        ("DELETE", "/api/v1/model-params/{model_key}", 204)
    );
    assert_eq!(delete.outcome, AuditOutcome::Succeeded);
    Ok(())
}

/// Without a `[database]` there is no log: a write records nothing and both
/// reads say why.
#[tokio::test]
async fn a_server_without_a_database_keeps_no_log() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    assert!(state.audit_store.is_none());
    let acme = seed_org(&state, "acme").await?.org_id;
    let server = registered_user(&state, &[UserPermission::ManageAnyOrgs]).await?;
    let renamed = json!({ "display_name": "Acme Labs" });
    let uri = format!("/api/v1/orgs/{acme}");
    let resp = signed_json(&state, &server, "PATCH", &uri, &renamed).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    for uri in ["/api/v1/orgs/acme/audit", "/api/v1/audit"] {
        let resp = signed_empty(&state, &server, "GET", uri).await?;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{uri}");
        let error = json_str(&body_json(resp).await?, "error")?.to_owned();
        assert_eq!(error, pipette_pipeline::audit::NEEDS_DATABASE, "{uri}");
    }
    Ok(())
}

/// An organization's administrator reads a device moved out of its
/// organization as the route and the device: the path that named the receiving
/// organization is the server administrator's alone.
#[tokio::test]
async fn an_org_reader_sees_no_path_naming_another_org() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _db) = sql_state_or_skip!(dir.path());
    let (acme, rival) = (
        seed_org(&state, "acme").await?.org_id,
        seed_org(&state, "rival").await?.org_id,
    );
    let admin = registered_user(&state, &[UserPermission::ManageOrgs]).await?;
    set_user_org(&state, &admin.1, Some(acme.clone())).await?;
    let server = registered_user(
        &state,
        &[
            UserPermission::ManageAnyOrgs,
            UserPermission::ManageAnyClients,
        ],
    )
    .await?;
    let (_, client_id) = register_and_approve(&state).await?;
    own_client_by_org(&state, &client_id, &acme).await?;
    let moved = json!({ "client_ids": [client_id] });
    let uri = format!("/api/v1/orgs/{rival}/clients");
    assert_eq!(
        signed_json(&state, &server, "PUT", &uri, &moved)
            .await?
            .status(),
        StatusCode::OK
    );

    let scoped =
        body_json(signed_empty(&state, &admin, "GET", "/api/v1/orgs/acme/audit").await?).await?;
    let event = scoped["items"]
        .as_array()
        .and_then(|items| {
            items
                .iter()
                .find(|event| event["action"]["method"] == "PUT")
        })
        .context("the move in acme's log")?;
    assert_eq!(
        event["action"]["path"], event["action"]["route"],
        "{event:#}"
    );
    assert!(!event.to_string().contains(rival.as_str()), "{event:#}");

    let whole = body_json(signed_empty(&state, &server, "GET", "/api/v1/audit").await?).await?;
    let raw = whole["items"]
        .as_array()
        .and_then(|items| {
            items
                .iter()
                .find(|event| event["action"]["method"] == "PUT")
        })
        .context("the move in the whole log")?;
    assert_eq!(raw["action"]["path"], json!(uri));
    Ok(())
}

/// An edit refused while its body is read still names the vault resource its
/// path keys, so the owner's organization lists the attempt.
#[tokio::test]
async fn a_model_params_edit_refused_for_its_body_is_in_the_owner_s_log() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _db) = sql_state_or_skip!(dir.path());
    let acme = seed_org(&state, "acme").await?.org_id;
    let admin = registered_user(&state, &[UserPermission::ManageOrgs]).await?;
    set_user_org(&state, &admin.1, Some(acme.clone())).await?;
    let resource_id = upload_gguf_text(&state, &admin, Some(acme.as_ref()), VAULT_GGUF).await?;
    let rival = seed_org(&state, "rival").await?.org_id;
    let outsider = registered_user(&state, &[UserPermission::ManageOrgs]).await?;
    set_user_org(&state, &outsider.1, Some(rival.clone())).await?;

    let uri = format!("/api/v1/model-params/{resource_id}");
    let zero = json!({ "total_millions": 0 });
    let resp = signed_json(&state, &outsider, "PUT", &uri, &zero).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let event = latest_event(&state).await?;
    assert_eq!(
        event.targets,
        [AuditTarget::VaultResource {
            resource_id: resource_id.parse()?
        }]
        .into()
    );
    assert_eq!(event.org_ids, orgs(&[&acme, &rival]));
    Ok(())
}

/// A refused registration proves no device, so its actor is the id it claimed.
#[tokio::test]
async fn a_refused_registration_names_no_device() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _db) = sql_state_or_skip!(dir.path());
    let body = json!({
        "generate_key": true,
        "organization": "lab",
        "client_details": "bench box",
        "contact_email": "lab@example.test",
        "client_preauth_key": "client_preauth_0195c3e2a4f37b8c.not-the-secret",
    });
    let resp = json_request(&state, "POST", "/clients/register", &[], &body).await?;
    assert!(resp.status().is_client_error(), "{}", resp.status());
    let event = latest_event(&state).await?;
    assert!(
        matches!(&event.actor, AuditActor::Unverified { claimed_id: Some(id) } if id.starts_with("ev1_")),
        "{:?}",
        event.actor
    );
    Ok(())
}

/// A streamed filing whose caller leaves at once is still one event, and says
/// what happened: the plan it filed and a success, or a failure that filed none.
#[tokio::test]
async fn a_streamed_filing_left_early_records_what_happened() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _db) = sql_state_or_skip!(dir.path());
    let (_, client_id) = register_and_approve(&state).await?;
    let filer = registered_user_with_detail(&state, true).await?;
    let campaign = put_idle_campaign(&state, &filer.1, "left").await?;
    let before = events(&state).await?.len();

    let uri = filing_uri(campaign.as_str(), "left")?;
    let document = submittable_document(&client_id, "left", 1);
    let response = stream_plan_document(&state, &filer, "POST", &uri, &document).await?;
    assert_eq!(response.status(), StatusCode::OK);
    drop(response);

    let event = settled_event(&state, before + 1).await?;
    let head = state
        .campaign_store
        .get_campaign(&campaign)
        .await?
        .and_then(|campaign| campaign.current_plan_id);
    match head {
        Some(plan_id) => {
            assert_eq!(event.outcome, AuditOutcome::Succeeded, "{event:#?}");
            assert!(
                event.targets.contains(&AuditTarget::Plan { plan_id }),
                "{event:#?}"
            );
        }
        None => assert_eq!(route_of(&event).2, 499, "{event:#?}"),
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        events(&state).await?.len(),
        before + 1,
        "one event, not two"
    );
    Ok(())
}
