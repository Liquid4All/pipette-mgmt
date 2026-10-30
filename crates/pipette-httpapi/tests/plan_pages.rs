//! What the app's plan pages read beyond the plan routes' own bodies: the
//! listing newest first with its submitters named, a plan's submitter, the jobs
//! page with its results and offers (`results=true`), every job with its
//! request in document order (`all=true`), and the one-file export of its
//! results.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Context;
use axum::body::Body;
use axum::http::{HeaderName, Response, StatusCode, header};
use http_body_util::BodyExt;
use httpmock::Method::POST;
use httpmock::MockServer;
use rstest::rstest;
use serde_json::{Value, json};

use pipette_core::plan::PlanSource;
use pipette_httpapi::state::AppState;
use pipette_pipeline::plan_cells::PlanCell;
use pipette_pipeline::plan_document::expand_document;
use pipette_pipeline::plan_results::DEVICE_COLUMNS;
use pipette_plan_types::canonical_json as canonicalize;
use pipette_plan_types::{ClientId, ClientRunSpec, JobId, PlanId, PlanStatus, UserPermission};
use pipette_web_testkit::{
    DUCKDB_BASIC_AUTH, NAMED_CLIENT_ID, ONE_CELL_PLAN, UserSigner, approved_client, body_json,
    created_campaign_from, file_job_bodies, grant_user, json_str, plan_ids, post_plan_document,
    put_idle_campaign, put_loose_plan, registered_user, registered_user_with_detail, run_mocked,
    seed_org, set_user_org, signed_empty, submit_plan_as, user_address, with_duckdb,
};

async fn read(
    state: &AppState,
    signer: &UserSigner,
    uri: &str,
) -> anyhow::Result<(StatusCode, Value)> {
    let resp = signed_empty(state, signer, "GET", uri).await?;
    let status = resp.status();
    Ok((status, body_json(resp).await?))
}

/// [`read`], which must answer `200`.
async fn read_ok(state: &AppState, signer: &UserSigner, uri: &str) -> anyhow::Result<Value> {
    let (status, body) = read(state, signer, uri).await?;
    anyhow::ensure!(status == StatusCode::OK, "{uri}: {status} {body}");
    Ok(body)
}

/// A plan of `jobs` jobs filed by `signer` into a campaign of its own.
async fn filed(
    state: &AppState,
    signer: &UserSigner,
    client_id: &ClientId,
    name: &str,
    jobs: usize,
) -> anyhow::Result<PlanId> {
    let (key, user_id, key_id) = signer;
    submit_plan_as(state, key, user_id, key_id, client_id, name, jobs).await
}

// ── GET /api/v1/plans?order=newest&names=true ──────────────────────────────────────

/// Newest first pages from the newest plan down, its cursor bounding the next
/// page even once that plan stops matching the filter; the default stays
/// ascending.
#[rstest]
#[case::full(true)]
#[case::withheld(false)]
#[tokio::test]
async fn test_the_newest_plans_list_first(#[case] full: bool) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let filer = registered_user_with_detail(&state, full).await?;
    let mut filed_ids = Vec::new();
    for name in ["first", "second", "third"] {
        filed_ids.push(
            filed(&state, &filer, &client_id, name, 1)
                .await?
                .to_string(),
        );
    }
    let newest: Vec<String> = filed_ids.iter().rev().cloned().collect();

    let ascending = read_ok(&state, &filer, "/api/v1/plans").await?;
    assert_eq!(plan_ids(&ascending)?, filed_ids);

    let first = read_ok(&state, &filer, "/api/v1/plans?order=newest&limit=1").await?;
    crate::api_schema::assert_conforms("NamedPlanViewPage", &first)?;
    assert_eq!(plan_ids(&first)?, newest[..1]);
    let cursor = json_str(&first, "next_cursor")?.to_string();
    assert_eq!(cursor, newest[0]);
    let status = json_str(&first["items"][0], "status")?.to_string();

    // The cursor's plan leaves the filter; the bound it names still holds.
    let mut manifest = state
        .plan_store
        .get_plan(&PlanId::try_new(cursor.as_str())?)
        .await?
        .context("the plan is filed")?;
    manifest.status = PlanStatus::Cancelled;
    state.plan_store.put_plan(&manifest).await?;
    let rest = read_ok(
        &state,
        &filer,
        &format!("/api/v1/plans?order=newest&status={status}&cursor={cursor}"),
    )
    .await?;
    assert_eq!(plan_ids(&rest)?, newest[1..]);
    assert!(rest["next_cursor"].is_null(), "{rest}");
    Ok(())
}

/// An unknown order or names value is refused rather than ignored; an empty
/// one reads as absent.
#[rstest]
#[case::bogus_order("order=bogus", StatusCode::BAD_REQUEST)]
#[case::bogus_names("names=maybe", StatusCode::BAD_REQUEST)]
#[case::empty_order("order=", StatusCode::OK)]
#[case::empty_names("names=", StatusCode::OK)]
#[case::oldest("order=oldest", StatusCode::OK)]
#[tokio::test]
async fn test_the_listing_reads_its_order_and_names(
    #[case] query: &str,
    #[case] status: StatusCode,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, _client_id) = approved_client(dir.path()).await?;
    let reader = registered_user_with_detail(&state, false).await?;
    let (answered, body) = read(&state, &reader, &format!("/api/v1/plans?{query}")).await?;
    assert_eq!(answered, status, "{body}");
    if status == StatusCode::OK {
        assert!(body.get("names").is_none(), "{body}");
    }
    Ok(())
}

/// `names=true` names the submitter of every plan served and no other: a
/// `submitted_by` filter outside the reader's reach names nobody.
#[tokio::test]
async fn test_the_listing_names_the_submitters_it_serves() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let alice = registered_user_with_detail(&state, false).await?;
    let bob = registered_user_with_detail(&state, false).await?;
    filed(&state, &alice, &client_id, "alice", 1).await?;
    filed(&state, &bob, &client_id, "bob", 1).await?;
    let auditor = registered_user(&state, &[UserPermission::ReadAnyPlans]).await?;
    let (alice_id, bob_id) = (alice.1.to_string(), bob.1.to_string());

    let every = read_ok(&state, &auditor, "/api/v1/plans?order=newest&names=true").await?;
    crate::api_schema::assert_conforms("NamedPlanViewPage", &every)?;
    assert_eq!(
        every["names"],
        json!({
            alice_id.clone(): user_address(&state, &alice.1).await?,
            bob_id.clone(): user_address(&state, &bob.1).await?,
        })
    );

    let narrowed = read_ok(
        &state,
        &auditor,
        &format!("/api/v1/plans?names=true&submitted_by={alice_id}"),
    )
    .await?;
    assert_eq!(
        narrowed["names"],
        json!({ alice_id.clone(): user_address(&state, &alice.1).await? })
    );

    // Bob's plans are outside Alice's reach, so naming him names nobody.
    let outside = read_ok(
        &state,
        &alice,
        &format!("/api/v1/plans?names=true&submitted_by={bob_id}"),
    )
    .await?;
    assert_eq!(plan_ids(&outside)?, Vec::<String>::new());
    assert_eq!(outside["names"], json!({}));
    Ok(())
}

/// A plan's status names its submitter when asked, and only then.
#[rstest]
#[case::full(true)]
#[case::withheld(false)]
#[tokio::test]
async fn test_a_plan_names_its_submitter_when_asked(#[case] full: bool) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let filer = registered_user_with_detail(&state, full).await?;
    let plan_id = filed(&state, &filer, &client_id, "named", 1).await?;

    let named = read_ok(
        &state,
        &filer,
        &format!("/api/v1/plans/{plan_id}?names=true"),
    )
    .await?;
    crate::api_schema::assert_conforms("NamedPlanView", &named)?;
    assert_eq!(
        named["names"],
        json!({ filer.1.to_string(): user_address(&state, &filer.1).await? })
    );
    let bare = read_ok(&state, &filer, &format!("/api/v1/plans/{plan_id}")).await?;
    assert!(bare.get("names").is_none(), "{bare}");
    crate::api_schema::assert_conforms("PlanView", &bare)?;
    Ok(())
}

/// A plan read's flag that is not a boolean is refused rather than ignored.
#[rstest]
#[case::status_names("/api/v1/plans/{plan_id}?names=maybe")]
#[case::jobs_results("/api/v1/plans/{plan_id}/jobs?results=maybe")]
#[tokio::test]
async fn test_a_plan_read_refuses_a_flag_that_is_not_a_boolean(
    #[case] uri: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let filer = registered_user_with_detail(&state, true).await?;
    let plan_id = filed(&state, &filer, &client_id, "flagged", 1).await?;
    let uri = uri.replace("{plan_id}", plan_id.as_str());

    let (status, body) = read(&state, &filer, &uri).await?;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}: {body}");
    assert!(body["error"].is_string(), "{uri}: {body}");
    Ok(())
}

// ── GET /api/v1/plans/{plan_id}/jobs?results=true ─────────────────────────────

/// Whose cancel the jobs page offers: the filer's own campaign takes
/// `cancel_plans`, which it may lack, and a reader of every plan who
/// administers nothing needs `cancel_any_plans`.
#[rstest]
#[case::filer_without_cancel(false, false, "cancel_plans", false)]
#[case::filer_with_cancel(false, true, "cancel_plans", true)]
#[case::reader_of_every_plan(true, false, "cancel_any_plans", false)]
#[tokio::test]
async fn test_the_jobs_page_says_whose_cancel_it_offers(
    #[case] auditor: bool,
    #[case] cancel: bool,
    #[case] permission: &str,
    #[case] may_cancel: bool,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let filer = registered_user_with_detail(&state, false).await?;
    let plan_id = filed(&state, &filer, &client_id, "offered", 2).await?;
    if cancel {
        let grants = [
            UserPermission::SubmitPlans,
            UserPermission::ReadPlans,
            UserPermission::CancelPlans,
        ];
        grant_user(&state, &filer.1, &grants).await?;
    }
    let reader = match auditor {
        true => registered_user(&state, &[UserPermission::ReadAnyPlans]).await?,
        false => filer,
    };

    let body = read_ok(
        &state,
        &reader,
        &format!("/api/v1/plans/{plan_id}/jobs?results=true"),
    )
    .await?;
    crate::api_schema::assert_conforms("NamedPlanJobs", &body)?;
    assert_eq!(body["cancel_permission"], permission);
    assert_eq!(body["may_cancel"], may_cancel);
    assert_eq!(body["results_read"], "off");
    assert_eq!(body["total"], 2);
    assert_eq!(
        body["jobs"][0]["result"],
        json!({"status": "not_run"}),
        "{body}"
    );
    // Withheld: no device is named, so there are no names.
    assert_eq!(body["fleet_detail"], "withheld");
    assert!(body.get("names").is_none(), "{body}");

    // Unasked, the body is the jobs page it was.
    let bare = read_ok(&state, &reader, &format!("/api/v1/plans/{plan_id}/jobs")).await?;
    crate::api_schema::assert_conforms("NamedPlanJobs", &bare)?;
    assert!(bare.get("may_cancel").is_none(), "{bare}");
    Ok(())
}

/// What the stored document is, from what the store holds at the full tier, and
/// from the manifest alone below it: a withheld reader's document is not read.
#[rstest]
#[case::present_in_full(None, true, "present")]
#[case::collected(Some(None), true, "collected")]
#[case::replaced(Some(Some("benchmarks = []\n")), true, "mismatch")]
#[case::replaced_but_withheld(Some(Some("benchmarks = []\n")), false, "present")]
#[tokio::test]
async fn test_the_jobs_page_says_what_the_document_is(
    #[case] stored: Option<Option<&str>>,
    #[case] full: bool,
    #[case] expected: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let filer = registered_user_with_detail(&state, full).await?;
    let plan_id = filed(&state, &filer, &client_id, "documented", 1).await?;
    if let Some(stored) = stored {
        state.plan_store.delete_plan_source(&plan_id).await?;
        if let Some(text) = stored {
            let source = PlanSource::new(text.to_string())?;
            state.plan_store.put_plan_source(&plan_id, &source).await?;
        }
    }

    let body = read_ok(
        &state,
        &filer,
        &format!("/api/v1/plans/{plan_id}/jobs?results=true"),
    )
    .await?;
    crate::api_schema::assert_conforms("NamedPlanJobs", &body)?;
    assert_eq!(body["document_state"], expected);
    Ok(())
}

/// A plan filed as job bodies has no document, at either tier.
#[rstest]
#[case::full(true)]
#[case::withheld(false)]
#[tokio::test]
async fn test_a_plan_of_job_bodies_has_no_document(#[case] full: bool) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let filer = registered_user_with_detail(&state, full).await?;
    let campaign_id = put_idle_campaign(&state, &filer.1, "Bodies").await?;
    let plan_id =
        file_job_bodies(&state, &filer, campaign_id.as_str(), &client_id, "bodies").await?;

    let body = read_ok(
        &state,
        &filer,
        &format!("/api/v1/plans/{plan_id}/jobs?results=true"),
    )
    .await?;
    assert_eq!(body["document_state"], "none");
    Ok(())
}

/// Files a one-job plan and stores `record`, with the job's id added, as its
/// answer; the queue entry is dropped as scoring would.
async fn answered_job(
    state: &AppState,
    filer: &UserSigner,
    client_id: &ClientId,
    name: &str,
    mut record: Value,
) -> anyhow::Result<(PlanId, JobId)> {
    let plan_id = filed(state, filer, client_id, name, 1).await?;
    let job_id = state
        .plan_store
        .get_plan(&plan_id)
        .await?
        .context("the plan is filed")?
        .job_ids
        .first()
        .cloned()
        .context("the plan holds a job")?;
    record
        .as_object_mut()
        .context("the record is an object")?
        .insert("job_id".to_string(), json!(job_id.as_str()));
    state.todo_store.delete_avail_by_job(&job_id).await?;
    state
        .submission_store
        .write_incoming(&job_id, &record)
        .await?;
    Ok((plan_id, job_id))
}

/// At the full tier each device a page names is named, beside the jobs and
/// their results; pipette-duckdb is asked for the page's jobs.
#[tokio::test]
async fn test_the_jobs_page_names_its_devices_at_the_full_tier() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let duckdb = MockServer::start_async().await;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let state = with_duckdb(state, &duckdb.base_url())?;
    let filer = registered_user_with_detail(&state, true).await?;
    let record = json!({
        "message_type": "success",
        "client_id": client_id.as_str(),
        "benchmark_id": "prefill_throughput_256",
    });
    let (plan_id, job_id) = answered_job(&state, &filer, &client_id, "answered", record).await?;
    let asked = duckdb
        .mock_async(|when, then| {
            when.method(POST)
                .path("/api/query")
                .header("authorization", DUCKDB_BASIC_AUTH)
                .body_includes(format!("('{job_id}', 0)"));
            then.status(200).json_body(json!({
                "columns": ["job_id", "benchmark_id", "benchmark_type", "metric", "value",
                            "value_stddev", "unit", "failed", "failed_reason", "model_descriptor",
                            "runtime_descriptor", "client_id", "device_name", "submitted_at",
                            "scored_at"],
                "rows": [[job_id.as_str(), "prefill_throughput_256", "perf", "tokens_per_second",
                          42.5, null, "tok/s", false, null, null, null, client_id.as_str(),
                          "test", null, "2026-09-29T10:05:00.000000Z"]],
            }));
        })
        .await;

    let body = read_ok(
        &state,
        &filer,
        &format!("/api/v1/plans/{plan_id}/jobs?results=true"),
    )
    .await?;
    crate::api_schema::assert_conforms("NamedPlanJobs", &body)?;
    assert_eq!(body["fleet_detail"], "full");
    assert_eq!(body["results_read"], "ok");
    assert_eq!(
        body["jobs"][0]["result"]["rows"][0]["client_id"],
        client_id.as_str()
    );
    assert_eq!(body["jobs"][0]["result"]["rows"][0]["value"], 42.5);
    // The device's details are its name where they hold no `user@host` word.
    assert_eq!(body["names"], json!({ client_id.as_str(): "test" }));
    asked.assert_async().await;
    Ok(())
}

/// A server-declared failure's reason can name clients, so it reaches a fleet
/// reader alone; below that only `expired` and `lease_lost` keep their source.
#[rstest]
#[case::lease_lost("lease_lost", true)]
#[case::expired("expired", true)]
#[case::all_clients_denied("all_clients_denied", false)]
#[case::unmatchable("unmatchable", false)]
fn test_the_jobs_page_names_a_server_declared_failure(
    #[case] source: &str,
    #[case] kept_when_withheld: bool,
    #[values(true, false)] full: bool,
) -> anyhow::Result<()> {
    run_mocked(assert_server_declared_failure(
        source,
        kept_when_withheld,
        full,
    ))
}

async fn assert_server_declared_failure(
    source: &str,
    kept_when_withheld: bool,
    full: bool,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let duckdb = MockServer::start_async().await;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let state = with_duckdb(state, &duckdb.base_url())?;
    let filer = registered_user_with_detail(&state, full).await?;
    let reason = format!("{source} (clients: {client_id})");
    let record = json!({
        "message_type": "failure",
        "client_id": "system",
        "benchmark_id": "prefill_throughput_256",
        "failure_reason": reason,
        "failure_source": source,
    });
    let (plan_id, job_id) = answered_job(&state, &filer, &client_id, source, record).await?;
    let asked = duckdb
        .mock_async(|when, then| {
            when.method(POST)
                .path("/api/query")
                .body_includes(format!("('{job_id}', 0)"))
                .body_includes("w.failure_source");
            then.status(200).json_body(json!({
                "columns": ["job_id", "benchmark_id", "benchmark_type", "metric", "value",
                            "value_stddev", "unit", "failed", "failed_reason", "model_descriptor",
                            "runtime_descriptor", "client_id", "device_name", "submitted_at",
                            "scored_at", "failure_source"],
                "rows": [[job_id.as_str(), "prefill_throughput_256", "perf",
                          "tokens_per_second", null, null, "tok/s", true, reason, null, null,
                          "system", null, null, "2026-09-29T10:05:00.000000Z", source]],
            }));
        })
        .await;

    let body = read_ok(
        &state,
        &filer,
        &format!("/api/v1/plans/{plan_id}/jobs?results=true"),
    )
    .await?;
    crate::api_schema::assert_conforms("NamedPlanJobs", &body)?;
    let shown = |value: Value| if full { value } else { Value::Null };
    let expected = [
        (
            "/fleet_detail",
            json!(if full { "full" } else { "withheld" }),
        ),
        ("/jobs/0/state", json!("failed")),
        // The reserved id names no machine, so neither the row nor `names` does.
        ("/jobs/0/queue", Value::Null),
        ("/jobs/0/result/rows/0/client_id", Value::Null),
        ("/jobs/0/result/rows/0/device_name", Value::Null),
        ("/jobs/0/result/rows/0/failed", json!(true)),
        (
            "/jobs/0/result/rows/0/failure_source",
            if full || kept_when_withheld {
                json!(source)
            } else {
                Value::Null
            },
        ),
        ("/jobs/0/result/rows/0/failed_reason", shown(json!(reason))),
    ];
    expected.iter().for_each(|(path, want)| {
        assert_eq!(body.pointer(path), Some(want), "{path}: {body}");
    });
    // `names` is absent below the full tier.
    assert_eq!(body.get("names"), full.then(|| json!({})).as_ref());
    asked.assert_async().await;
    Ok(())
}

/// Without `[duckdb]` a job's record settles its state, and the row names no
/// machine for a failure the server declared.
#[tokio::test]
async fn test_a_server_declared_failure_record_names_no_machine() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let filer = registered_user_with_detail(&state, true).await?;
    let record = json!({
        "message_type": "failure",
        "client_id": "system",
        "benchmark_id": "prefill_throughput_256",
        "failure_reason": "Lease lost without a result 3 times",
        "failure_source": "lease_lost",
    });
    let (plan_id, _) = answered_job(&state, &filer, &client_id, "lost", record).await?;

    let body = read_ok(&state, &filer, &format!("/api/v1/plans/{plan_id}/jobs")).await?;
    assert_eq!(body["fleet_detail"], "full");
    assert_eq!(body["jobs"][0]["state"], "failed");
    assert_eq!(body["jobs"][0]["planned"]["benchmark"], "eval_test");
    assert_eq!(body["jobs"][0]["queue"], Value::Null);
    Ok(())
}

/// A plan outside the reader's scope is a `404` with results asked too.
#[tokio::test]
async fn test_the_jobs_page_of_another_tenant_is_not_found() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let filer = registered_user_with_detail(&state, true).await?;
    let stranger = registered_user_with_detail(&state, true).await?;
    let plan_id = filed(&state, &filer, &client_id, "private", 1).await?;
    let (status, body) = read(
        &state,
        &stranger,
        &format!("/api/v1/plans/{plan_id}/jobs?results=true"),
    )
    .await?;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    Ok(())
}

// ── GET /api/v1/plans/{plan_id}/jobs?all=true ────────────────────────────────

/// Every job of `plan_id` as `signer` reads them: the status and the body's bytes.
async fn read_all(
    state: &AppState,
    signer: &UserSigner,
    plan_id: &str,
) -> anyhow::Result<(StatusCode, String)> {
    let uri = format!("/api/v1/plans/{plan_id}/jobs?all=true");
    let resp = signed_empty(state, signer, "GET", &uri).await?;
    let status = resp.status();
    let bytes = resp.into_body().collect().await?.to_bytes();
    Ok((status, String::from_utf8(bytes.to_vec())?))
}

fn listed(body: &Value) -> anyhow::Result<&Vec<Value>> {
    body.get("jobs")
        .and_then(Value::as_array)
        .with_context(|| format!("a `jobs` array: {body}"))
}

/// Each listed job's `planned`, canonical, by job id.
fn requests(body: &Value) -> anyhow::Result<BTreeMap<String, String>> {
    listed(body)?
        .iter()
        .map(|job| {
            let planned = job.get("planned").context("a job's plan cell")?;
            Ok((json_str(job, "job_id")?.to_owned(), canonicalize(planned)))
        })
        .collect()
}

/// The request of each job `document` files, canonical.
async fn document_requests(document: &str) -> anyhow::Result<BTreeSet<String>> {
    // The public catalog's types, which its ids' prefixes name.
    let types = pipette_planner::prefix_types(document);
    expand_document(
        std::num::NonZeroUsize::MAX,
        types.clone(),
        None,
        document.to_owned(),
    )
    .await
    .map_err(|e| anyhow::anyhow!("{e}"))?
    .jobs
    .iter()
    .map(|job| {
        let spec = job.get("spec").context("a job body holds its spec")?;
        let spec: ClientRunSpec = serde_json::from_value(spec.clone())?;
        Ok(canonicalize(&serde_json::to_value(PlanCell::of(
            &spec, &types,
        ))?))
    })
    .collect()
}

/// [`ONE_CLIENT_PLAN`](pipette_web_testkit::ONE_CLIENT_PLAN) on llama.cpp, its
/// flags forwarding a planted secret beside a ggml knob.
const PLANTED_PLAN: &str = r#"
benchmarks = ["prefill_throughput_256"]

[[variants]]
requires = ["os:macos"]
clients  = ["ev1_named"]
models   = [{ type = "gguf_text", source = "huggingface", org = "LiquidAI", repo_name = "LFM2.5-1.2B-Instruct-GGUF", path = "LFM2.5-1.2B-Instruct-Q4_0.gguf" }]
runtimes = [{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b9050", flavor = "macos-arm64" }]
runtime_flags = [{ benchmark_type = "prefill_throughput", runtime_type = "llamacpp_cli_stock_tools", model_type = "gguf_text", threads = 4, envs = ["LLAMA_API_KEY=sk-planted", "GGML_METAL=1"] }]
"#;

/// A request holds only what a job's `spec` holds, as a record carries it, so a
/// teammate below the full tier reads the requests its full-tier filer does,
/// and neither names the client, the requirements or an env value but `GGML_*`'s.
#[tokio::test]
async fn test_a_request_reads_alike_at_both_tiers() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, _client_id) = approved_client(dir.path()).await?;
    let org = seed_org(&state, "acme").await?;
    let filer = registered_user(
        &state,
        &[
            UserPermission::SubmitPlans,
            UserPermission::ManageOrgs,
            UserPermission::ReadClients,
        ],
    )
    .await?;
    set_user_org(&state, &filer.1, Some(org.org_id.clone())).await?;
    let (_, plan_id) = created_campaign_from(
        &state,
        &filer,
        &[("name", "Team nightly"), ("owner", org.org_id.as_str())],
        PLANTED_PLAN,
    )
    .await?;
    let teammate = registered_user(&state, &[UserPermission::ReadPlans]).await?;
    set_user_org(&state, &teammate.1, Some(org.org_id.clone())).await?;

    let (status, full) = read_all(&state, &filer, plan_id.as_str()).await?;
    assert_eq!(status, StatusCode::OK, "{full}");
    let (status, withheld) = read_all(&state, &teammate, plan_id.as_str()).await?;
    assert_eq!(status, StatusCode::OK, "{withheld}");
    let body: Value = serde_json::from_str(&withheld)?;
    crate::api_schema::assert_conforms("NamedPlanJobs", &body)?;
    assert_eq!(requests(&body)?, requests(&serde_json::from_str(&full)?)?);

    let [job] = listed(&body)?.as_slice() else {
        anyhow::bail!("one job, got {body}");
    };
    let cell = &job["planned"];
    assert_eq!(cell["benchmark"], "prefill_throughput_256");
    assert_eq!(cell["benchmark_type"], "prefill_throughput");
    assert_eq!(cell["model"]["repo_name"], "LFM2.5-1.2B-Instruct-GGUF");
    assert_eq!(
        cell["runtime_flags"],
        json!({
            "runtime_type": "llamacpp_cli_stock_tools",
            "model_type": "gguf_text",
            "benchmark_type": "prefill_throughput",
            "threads": 4,
            "envs": ["LLAMA_API_KEY", "GGML_METAL=1"],
        })
    );
    [
        NAMED_CLIENT_ID,
        "ev1_",
        "os:macos",
        "auth_token",
        "sk-planted",
    ]
    .into_iter()
    .for_each(|hidden| assert!(!withheld.contains(hidden), "{hidden} in {withheld}"));
    Ok(())
}

/// A plan filed as job bodies has no document, and lists each job's request
/// as its body was filed, at either tier.
#[rstest]
#[case::full(true)]
#[case::withheld(false)]
#[tokio::test]
async fn test_a_plan_of_job_bodies_lists_its_requests(#[case] full: bool) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let filer = registered_user_with_detail(&state, full).await?;
    let campaign_id = put_idle_campaign(&state, &filer.1, "Bodies").await?;
    let plan_id =
        file_job_bodies(&state, &filer, campaign_id.as_str(), &client_id, "bodies").await?;

    let body = read_ok(
        &state,
        &filer,
        &format!("/api/v1/plans/{plan_id}/jobs?all=true"),
    )
    .await?;
    let [job] = listed(&body)?.as_slice() else {
        anyhow::bail!("one job, got {body}");
    };
    assert_eq!(job["planned"]["benchmark"], "eval_test", "{body}");
    Ok(())
}

/// The requests come off the jobs, so a document collected, or replaced by one
/// that does not parse, costs the list only its order, and no parser text
/// reaches it.
#[rstest]
#[case::collected(false)]
#[case::unreadable(true)]
#[tokio::test]
async fn test_the_jobs_outlive_their_document(#[case] replaced: bool) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let filer = registered_user_with_detail(&state, true).await?;
    let plan_id = filed(&state, &filer, &client_id, "documented", 2).await?;
    state.plan_store.delete_plan_source(&plan_id).await?;
    let mut manifest = state
        .plan_store
        .get_plan(&plan_id)
        .await?
        .context("the plan was filed")?;
    if replaced {
        let unreadable = PlanSource::new("plan_id = \"hf_quoted\"\n".to_string())?;
        state
            .plan_store
            .put_plan_source(&plan_id, &unreadable)
            .await?;
        manifest.source_sha256 = Some(unreadable.sha256().clone());
        state.plan_store.put_plan(&manifest).await?;
    }

    let (status, text) = read_all(&state, &filer, plan_id.as_str()).await?;
    assert_eq!(status, StatusCode::OK, "{text}");
    let body: Value = serde_json::from_str(&text)?;
    let listed: Vec<&str> = listed(&body)?
        .iter()
        .map(|job| json_str(job, "job_id"))
        .collect::<anyhow::Result<_>>()?;
    let filed_order: Vec<&str> = manifest.job_ids.iter().map(JobId::as_str).collect();
    assert_eq!(listed, filed_order, "{body}");
    assert!(
        requests(&body)?.values().all(|request| request != "null"),
        "{body}"
    );
    assert!(!text.contains("hf_quoted"), "{text}");
    Ok(())
}

/// The document orders the jobs as written, benchmark then model, where the
/// planner's sort by body puts `max_memory_usage_256` first and `LFM2-700M`
/// before `LFM2.5-350M`. A page is a window on that order.
#[tokio::test]
async fn test_the_document_orders_the_jobs() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, _client_id) = approved_client(dir.path()).await?;
    let filer = registered_user_with_detail(&state, true).await?;
    let document = ONE_CELL_PLAN
        .replace(
            r#"["prefill_throughput_256"]"#,
            r#"["prefill_throughput_256", "max_memory_usage_256"]"#,
        )
        .replace(
            r#"repo_name = "LFM2.5-350M-MLX-4bit" }]"#,
            r#"repo_name = "LFM2.5-350M-MLX-4bit" }, { type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "LFM2-700M-MLX-4bit" }]"#,
        );
    let (_, plan_id) =
        created_campaign_from(&state, &filer, &[("name", "Ordered")], &document).await?;

    let body = read_ok(
        &state,
        &filer,
        &format!("/api/v1/plans/{plan_id}/jobs?all=true"),
    )
    .await?;
    let drawn = listed(&body)?
        .iter()
        .map(|job| {
            let planned = &job["planned"];
            Ok((
                json_str(planned, "benchmark")?.to_owned(),
                json_str(&planned["model"], "repo_name")?.to_owned(),
            ))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let cell = |benchmark: &str, repo: &str| (benchmark.to_owned(), repo.to_owned());
    assert_eq!(
        drawn,
        [
            cell("prefill_throughput_256", "LFM2.5-350M-MLX-4bit"),
            cell("prefill_throughput_256", "LFM2-700M-MLX-4bit"),
            cell("max_memory_usage_256", "LFM2.5-350M-MLX-4bit"),
            cell("max_memory_usage_256", "LFM2-700M-MLX-4bit"),
        ]
    );
    let ids = |body: &Value| -> anyhow::Result<Vec<String>> {
        listed(body)?
            .iter()
            .map(|job| Ok(json_str(job, "job_id")?.to_owned()))
            .collect()
    };
    let all = ids(&body)?;
    // The manifest lists the same jobs the other way round.
    let manifest = state
        .plan_store
        .get_plan(&plan_id)
        .await?
        .context("the plan was filed")?;
    let mut filed_order: Vec<String> = manifest.job_ids.iter().map(ToString::to_string).collect();
    filed_order.reverse();
    assert_eq!(all, filed_order, "{body}");

    let page = read_ok(
        &state,
        &filer,
        &format!("/api/v1/plans/{plan_id}/jobs?offset=1&limit=2"),
    )
    .await?;
    assert_eq!(ids(&page)?, all.get(1..3).unwrap_or_default(), "{page}");
    Ok(())
}

/// A queued job with no stored source, as one filed before the server stored
/// them, is named by its queued body.
#[tokio::test]
async fn test_a_queued_job_without_a_source_reads_its_body() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let filer = registered_user_with_detail(&state, true).await?;
    let plan_id = filed(&state, &filer, &client_id, "older", 1).await?;
    let manifest = state
        .plan_store
        .get_plan(&plan_id)
        .await?
        .context("the plan was filed")?;
    let [job_id] = manifest.job_ids.as_slice() else {
        anyhow::bail!("one job, got {:?}", manifest.job_ids);
    };
    pipette_store_testkit::local_fs_remove_job_source(dir.path(), job_id)?;

    let body = read_ok(
        &state,
        &filer,
        &format!("/api/v1/plans/{plan_id}/jobs?all=true"),
    )
    .await?;
    let [job] = listed(&body)?.as_slice() else {
        anyhow::bail!("one job, got {body}");
    };
    assert_eq!(job["planned"]["benchmark"], "eval_test", "{body}");
    Ok(())
}

/// Each job's request is a cell its document expands to, whether the job is
/// queued or has left the queue.
#[rstest]
#[case::queued(false)]
#[case::left_the_queue(true)]
#[tokio::test]
async fn test_a_jobs_request_is_a_cell_of_its_document(#[case] left: bool) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let filer = registered_user_with_detail(&state, false).await?;
    let plan_id = filed(&state, &filer, &client_id, "requested", 3).await?;
    if left {
        let manifest = state
            .plan_store
            .get_plan(&plan_id)
            .await?
            .context("the plan was filed")?;
        for job_id in &manifest.job_ids {
            state.todo_store.delete_avail_by_job(job_id).await?;
        }
    }
    let document = state
        .plan_store
        .get_plan_source(&plan_id)
        .await?
        .context("the plan has its document")?;

    let body = read_ok(
        &state,
        &filer,
        &format!("/api/v1/plans/{plan_id}/jobs?all=true"),
    )
    .await?;
    let requested: BTreeSet<String> = requests(&body)?.into_values().collect();
    assert_eq!(requested.len(), 3, "{body}");
    assert_eq!(requested, document_requests(document.text()).await?);
    Ok(())
}

/// Each shipped scheduler example's requests are bodies the `PlanCell` schema
/// takes.
#[tokio::test]
async fn test_the_examples_requests_conform() -> anyhow::Result<()> {
    let dir = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/plans/scheduler"
    );
    let mut examples = 0;
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("toml") {
            continue;
        }
        let document = std::fs::read_to_string(&path)?;
        document_requests(&document)
            .await?
            .iter()
            .try_for_each(|request| {
                crate::api_schema::assert_conforms("PlanCell", &serde_json::from_str(request)?)
            })
            .with_context(|| path.display().to_string())?;
        examples += 1;
    }
    assert_eq!(examples, 3, "the scheduler examples in {dir}");
    Ok(())
}

/// Every property name `schema` reaches in `bundle`, its `$ref`s followed once.
fn reached_properties(
    bundle: &Value,
    schema: &Value,
    seen: &mut BTreeSet<String>,
    names: &mut BTreeSet<String>,
) -> anyhow::Result<()> {
    match schema {
        Value::Object(map) => {
            if let Some(reference) = map.get("$ref").and_then(Value::as_str) {
                let name = reference
                    .strip_prefix("#/$defs/")
                    .with_context(|| format!("a reference outside the bundle: {reference}"))?;
                if seen.insert(name.to_owned()) {
                    let definition = bundle
                        .pointer(&format!("/$defs/{name}"))
                        .with_context(|| format!("no definition {name}"))?;
                    reached_properties(bundle, definition, seen, names)?;
                }
            }
            if let Some(properties) = map.get("properties").and_then(Value::as_object) {
                names.extend(properties.keys().cloned());
            }
            map.values()
                .try_for_each(|inner| reached_properties(bundle, inner, seen, names))
        }
        Value::Array(items) => items
            .iter()
            .try_for_each(|inner| reached_properties(bundle, inner, seen, names)),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => Ok(()),
    }
}

/// No type a request reaches can carry eligibility: not the six fields a
/// withheld job source drops (`ServedJobSource`), a variant's
/// `client_versions`, nor a local plan's `allowed_clients`.
#[test]
fn test_a_requests_schema_has_no_eligibility() -> anyhow::Result<()> {
    let bundle = pipette_httpapi::api_schema::bundle()?;
    let mut names = BTreeSet::new();
    reached_properties(
        &bundle,
        &json!({ "$ref": "#/$defs/PlanCell" }),
        &mut BTreeSet::new(),
        &mut names,
    )?;
    for reached in [
        "benchmark",
        "benchmark_type",
        "repo_name",
        "threads",
        "readiness",
    ] {
        assert!(
            names.contains(reached),
            "the walk missed {reached}: {names:?}"
        );
    }
    let eligibility = [
        "requires",
        "any_of",
        "clients",
        "excludes",
        "not_all_of",
        "exclude_clients",
        "client_versions",
        "allowed_clients",
    ];
    for key in eligibility {
        assert!(!names.contains(key), "{key} under PlanCell");
    }
    Ok(())
}

// ── GET /api/v1/plans/{plan_id}/results.csv ─────────────────────────────────────

/// The rows of one job, as `/api/query` answers the export's one query.
fn export_rows(job_id: &str) -> Value {
    json!({
        "columns": ["job_id", "benchmark_id", "benchmark_type", "metric", "value", "value_stddev",
                    "unit", "failed", "failed_reason", "model_descriptor", "runtime_descriptor",
                    "client_extras", "client_id", "device_name", "submitted_at", "scored_at",
                    "device_time_ms", "failure_source", "parameter_prefill_tokens",
                    "client_version", "rescored_from", "run_submitted_at",
                    "score_runtime_version", "device_os_name", "device_chip_model",
                    "observation_max_swap_bytes"],
        "rows": [[job_id, "prefill_throughput_256", "perf", "tokens_per_second", 42.5, 1.5,
                  "tok/s", false, null, null, null, "{\"peak_rss_kib\":10}", "client-1",
                  "Pixel 9", null, "2026-09-29T10:05:00.000000Z", 184_250, null, 256, "1.4.0",
                  "job-0", "2026-09-01T08:00:00.000000Z", "1.2.3", "Android", "Tensor G4",
                  1_048_576],
                 [job_id, "prefill_throughput_256", "perf", "ttft", null, null, "ms", true,
                  "=out of memory", null, null, null, "client-1", "Pixel 9", null,
                  "2026-09-29T10:05:00.000000Z", null, "client", 256, "1.4.0", "job-0",
                  "2026-09-01T08:00:00.000000Z", null, "Android", null, null]],
    })
}

/// The export of `plan_id` as `reader` downloads it. The answer must be `200`.
async fn exported(
    state: &AppState,
    reader: &UserSigner,
    plan_id: &PlanId,
) -> anyhow::Result<Response<Body>> {
    let uri = format!("/api/v1/plans/{plan_id}/results.csv");
    let resp = signed_empty(state, reader, "GET", &uri).await?;
    anyhow::ensure!(resp.status() == StatusCode::OK, "{uri}: {}", resp.status());
    Ok(resp)
}

/// The value of the header `name` on `resp`. `None` if `resp` has no such header.
fn header_of(resp: &Response<Body>, name: HeaderName) -> anyhow::Result<Option<String>> {
    Ok(resp
        .headers()
        .get(name)
        .map(|value| value.to_str())
        .transpose()?
        .map(str::to_string))
}

/// The file holds every row from one unpaged query, as the console's download
/// writes it, with the fleet's columns empty below `read_clients`. The client,
/// observation and device columns follow the parameters.
#[rstest]
#[case::full(
    true,
    r#""{""peak_rss_kib"":10}""#,
    "client-1,Pixel 9",
    "1.4.0",
    "'=out of memory",
    "client"
)]
#[case::withheld(false, "", ",", "", "", "")]
fn test_the_export_holds_every_row(
    #[case] full: bool,
    #[case] extras: &str,
    #[case] fleet: &str,
    #[case] version: &str,
    #[case] reason: &str,
    #[case] source: &str,
) -> anyhow::Result<()> {
    // The device columns are there at either tier, empty when withheld.
    let device = |set: &[(&str, &str)]| {
        DEVICE_COLUMNS
            .iter()
            .map(|column| match set.iter().find(|(name, _)| name == column) {
                Some((_, cell)) if full => *cell,
                _ => "",
            })
            .collect::<Vec<_>>()
            .join(",")
    };
    let expected = format!(
        "job_id,benchmark_id,benchmark_type,metric,value,value_stddev,unit,failed,\
         failed_reason,model_descriptor,runtime_descriptor,model_vault_descriptor,\
         model_flags,runtime_flags,benchmark_flags,requested_model_flags,\
         requested_runtime_flags,requested_benchmark_flags,eval_metadata,client_extras,\
         client_id,device_name,submitted_at,scored_at,device_time_ms,failure_source,\
         parameter_prefill_tokens,client_version,rescored_from,run_submitted_at,\
         score_runtime_version,model_params_total_millions,model_params_active_millions,\
         runtime_catalog_name,model_catalog_name,observation_max_swap_bytes,{devices}\n\
         job-1,prefill_throughput_256,perf,tokens_per_second,42.5,1.5,tok/s,false,,,,,,,,,,,\
         ,{extras},{fleet},,2026-09-29T10:05:00Z,184250,,256,{version},job-0,2026-09-01T08:00:00Z,1.2.3,,,,,1048576,{first}\n\
         job-1,prefill_throughput_256,perf,ttft,,,ms,true,{reason},,,,,,,,,,,,{fleet},,\
         2026-09-29T10:05:00Z,,{source},256,{version},job-0,2026-09-01T08:00:00Z,,,,,,,{second}\n",
        devices = DEVICE_COLUMNS.join(","),
        first = device(&[
            ("device_os_name", "Android"),
            ("device_chip_model", "Tensor G4")
        ]),
        second = device(&[("device_os_name", "Android")]),
    );
    run_mocked(assert_the_export_holds_every_row(full, expected))
}

async fn assert_the_export_holds_every_row(full: bool, expected: String) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let duckdb = MockServer::start_async().await;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let state = with_duckdb(state, &duckdb.base_url())?;
    let filer = registered_user_with_detail(&state, full).await?;
    let plan_id = filed(&state, &filer, &client_id, "exported", 1).await?;
    let mut grants = vec![UserPermission::SubmitPlans, UserPermission::ReadPlans];
    if full {
        grants.push(UserPermission::ReadClients);
    }
    grant_user(&state, &filer.1, &grants).await?;
    let asked = duckdb
        .mock_async(|when, then| {
            when.method(POST)
                .path("/api/query")
                .header("authorization", DUCKDB_BASIC_AUTH)
                .body_includes(format!("WHERE plan_id = '{plan_id}'"))
                .body_excludes("LIMIT");
            then.status(200).json_body(export_rows("job-1"));
        })
        .await;

    let resp = exported(&state, &filer, &plan_id).await?;
    assert_eq!(
        header_of(&resp, header::CONTENT_TYPE)?.as_deref(),
        Some("text/csv; charset=utf-8")
    );
    assert_eq!(
        header_of(&resp, header::CONTENT_DISPOSITION)?,
        Some(
            "attachment; filename=\"exported-plan-1.csv\"; filename*=UTF-8''exported-plan-1.csv"
                .to_string()
        )
    );
    let csv = String::from_utf8(resp.into_body().collect().await?.to_bytes().to_vec())?;
    assert_eq!(csv, expected);
    asked.assert_async().await;
    Ok(())
}

/// The reader of the export and the campaign that the plan is filed into.
#[derive(Clone, Copy, Debug)]
enum Naming {
    /// The campaign's owner reads the export. The file takes the campaign's name.
    Owner,
    /// A reader of every plan reads the export. The file takes the campaign's name.
    Auditor,
    /// The owner reads the export of the campaign's second plan. The file takes
    /// the number 2.
    Revised,
    /// The campaign record does not exist. The file takes the campaign's id.
    Vanished,
    /// The plan is outside every campaign. The file takes the plan's id.
    Loose,
}

/// A plan in a campaign takes `{campaign}-plan-{n}.csv`. `campaign` is the
/// campaign's name if the reader may read the campaign, and the campaign's id if
/// not. A plan outside every campaign takes `{plan_id}-results.csv`.
#[rstest]
#[case::owner(Naming::Owner)]
#[case::auditor(Naming::Auditor)]
#[case::revised(Naming::Revised)]
#[case::vanished(Naming::Vanished)]
#[case::loose(Naming::Loose)]
fn test_the_export_is_named_after_its_campaign(#[case] naming: Naming) -> anyhow::Result<()> {
    run_mocked(assert_the_export_is_named(naming))
}

async fn assert_the_export_is_named(naming: Naming) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let duckdb = MockServer::start_async().await;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let state = with_duckdb(state, &duckdb.base_url())?;
    let filer = registered_user(
        &state,
        &[UserPermission::SubmitPlans, UserPermission::ReadPlans],
    )
    .await?;
    let plan_id = match naming {
        Naming::Loose => {
            let (key, user_id, key_id) = &filer;
            put_loose_plan(&state, key, user_id, key_id, &client_id, "Nightly", 1).await?
        }
        Naming::Owner | Naming::Auditor | Naming::Vanished => {
            filed(&state, &filer, &client_id, "Nightly", 1).await?
        }
        Naming::Revised => {
            let first = filed(&state, &filer, &client_id, "Nightly", 1).await?;
            let campaign_id = state
                .plan_store
                .get_plan(&first)
                .await?
                .and_then(|manifest| manifest.campaign_id)
                .context("the first plan names its campaign")?;
            let resp = post_plan_document(
                &state,
                &filer,
                &format!("/api/v1/campaigns/{campaign_id}/plans"),
                ONE_CELL_PLAN,
            )
            .await?;
            assert_eq!(resp.status(), StatusCode::CREATED);
            state
                .campaign_store
                .get_campaign(&campaign_id)
                .await?
                .and_then(|campaign| campaign.current_plan_id)
                .context("the revision is the head")?
        }
    };
    let campaign_id = state
        .plan_store
        .get_plan(&plan_id)
        .await?
        .context("the filed plan exists")?
        .campaign_id;
    let reader = match naming {
        Naming::Owner | Naming::Revised | Naming::Loose => filer,
        Naming::Auditor => registered_user(&state, &[UserPermission::ReadAnyPlans]).await?,
        Naming::Vanished => {
            let campaign_id = campaign_id
                .as_ref()
                .context("the plan names its campaign")?;
            state.campaign_store.delete_campaign(campaign_id).await?;
            registered_user(
                &state,
                &[UserPermission::ReadAnyPlans, UserPermission::ReadPlans],
            )
            .await?
        }
    };
    let stem = match (naming, &campaign_id) {
        (Naming::Owner | Naming::Auditor, _) => "Nightly-plan-1".to_string(),
        (Naming::Revised, _) => "Nightly-plan-2".to_string(),
        (Naming::Vanished, Some(campaign_id)) => format!("{campaign_id}-plan-1"),
        (Naming::Vanished, None) => anyhow::bail!("the plan names no campaign"),
        (Naming::Loose, _) => format!("{plan_id}-results"),
    };
    duckdb
        .mock_async(|when, then| {
            when.method(POST).path("/api/query");
            then.status(200).json_body(export_rows("job-1"));
        })
        .await;

    let resp = exported(&state, &reader, &plan_id).await?;
    assert_eq!(
        header_of(&resp, header::CONTENT_DISPOSITION)?,
        Some(format!(
            "attachment; filename=\"{stem}.csv\"; filename*=UTF-8''{stem}.csv"
        ))
    );
    Ok(())
}

/// A failure the server declared leaves the export's `client_id` cell empty at
/// the full tier, as the JSON reads it `null`.
#[test]
fn test_the_export_names_no_machine_on_a_server_declared_failure() -> anyhow::Result<()> {
    run_mocked(assert_the_export_names_no_machine())
}

async fn assert_the_export_names_no_machine() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let duckdb = MockServer::start_async().await;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let state = with_duckdb(state, &duckdb.base_url())?;
    let filer = registered_user_with_detail(&state, true).await?;
    let plan_id = filed(&state, &filer, &client_id, "lost", 1).await?;
    let grants = [
        UserPermission::SubmitPlans,
        UserPermission::ReadPlans,
        UserPermission::ReadClients,
    ];
    grant_user(&state, &filer.1, &grants).await?;
    let asked = duckdb
        .mock_async(|when, then| {
            when.method(POST).path("/api/query");
            then.status(200).json_body(json!({
                "columns": ["job_id", "benchmark_id", "benchmark_type", "metric", "value",
                            "value_stddev", "unit", "failed", "failed_reason", "model_descriptor",
                            "runtime_descriptor", "client_id", "device_name", "submitted_at",
                            "scored_at", "failure_source"],
                "rows": [["job-1", "prefill_throughput_256", "perf", "tokens_per_second",
                          null, null, "tok/s", true, "lease lost", null, null, "system", null,
                          null, "2026-09-29T10:05:00.000000Z", "lease_lost"]],
            }));
        })
        .await;

    let resp = exported(&state, &filer, &plan_id).await?;
    let csv = String::from_utf8(resp.into_body().collect().await?.to_bytes().to_vec())?;
    let mut lines = csv.lines().map(|line| line.split(',').collect::<Vec<_>>());
    let (header, row) = (
        lines.next().unwrap_or_default(),
        lines.next().unwrap_or_default(),
    );
    let cell = |name: &str| {
        header
            .iter()
            .position(|column| *column == name)
            .and_then(|at| row.get(at).copied())
    };
    assert_eq!(cell("client_id"), Some(""), "{csv}");
    assert_eq!(cell("failed_reason"), Some("lease lost"), "{csv}");
    assert_eq!(cell("failure_source"), Some("lease_lost"), "{csv}");
    asked.assert_async().await;
    Ok(())
}

/// Who may not export, and a server that cannot: each answers in the JSON
/// envelope, in the gates' order, and the refusals ask pipette-duckdb nothing.
async fn assert_an_export_is_refused(
    reader: &[UserPermission],
    duckdb_answers: Option<u16>,
    status: StatusCode,
    message: &str,
    queries: usize,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let duckdb = MockServer::start_async().await;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let state = match duckdb_answers {
        Some(_) => with_duckdb(state, &duckdb.base_url())?,
        None => state,
    };
    let any = duckdb
        .mock_async(|when, then| {
            when.path("/api/query");
            then.status(duckdb_answers.unwrap_or(500));
        })
        .await;
    let filer = registered_user_with_detail(&state, true).await?;
    let plan_id = filed(&state, &filer, &client_id, "refused", 1).await?;
    let reader = registered_user(&state, reader).await?;

    let resp = signed_empty(
        &state,
        &reader,
        "GET",
        &format!("/api/v1/plans/{plan_id}/results.csv"),
    )
    .await?;
    assert_eq!(resp.status(), status);
    assert_eq!(
        resp.headers()
            .get(header::CONTENT_TYPE)
            .map(|value| value.to_str())
            .transpose()?,
        Some("application/json")
    );
    let error = json_str(&body_json(resp).await?, "error")?.to_string();
    assert!(error.contains(message), "{error}");
    any.assert_calls_async(queries).await;
    Ok(())
}

#[rstest]
#[case::without_read_plans(
    &[UserPermission::ReadClients],
    Some(200),
    StatusCode::FORBIDDEN,
    "user lacks the read_plans permission",
    0
)]
#[case::outside_the_scope(
    &[UserPermission::ReadPlans],
    Some(200),
    StatusCode::NOT_FOUND,
    "plan not found",
    0
)]
#[case::without_duckdb(
    &[UserPermission::ReadAnyPlans],
    None,
    StatusCode::NOT_FOUND,
    "no [duckdb] is configured",
    0
)]
#[case::duckdb_refuses(
    &[UserPermission::ReadAnyPlans],
    Some(500),
    StatusCode::BAD_GATEWAY,
    "upstream request failed",
    1
)]
fn test_an_export_is_refused(
    #[case] reader: &[UserPermission],
    #[case] duckdb_answers: Option<u16>,
    #[case] status: StatusCode,
    #[case] message: &str,
    #[case] queries: usize,
) -> anyhow::Result<()> {
    run_mocked(assert_an_export_is_refused(
        reader,
        duckdb_answers,
        status,
        message,
        queries,
    ))
}

/// The owning organization's `manage_orgs` administrator reads the campaign but
/// is refused its plan's export before pipette-duckdb is asked; granted
/// `read_plans`, it reads it withheld.
#[test]
fn test_a_team_plans_export_needs_read_plans_beyond_the_campaign_reach() -> anyhow::Result<()> {
    run_mocked(assert_a_team_plans_export_needs_read_plans())
}

async fn assert_a_team_plans_export_needs_read_plans() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let duckdb = MockServer::start_async().await;
    let (state, _key, _client_id) = approved_client(dir.path()).await?;
    let state = with_duckdb(state, &duckdb.base_url())?;
    let org = seed_org(&state, "acme").await?;
    let author = registered_user(
        &state,
        &[UserPermission::SubmitPlans, UserPermission::ManageOrgs],
    )
    .await?;
    set_user_org(&state, &author.1, Some(org.org_id.clone())).await?;
    let (campaign_id, plan_id) = created_campaign_from(
        &state,
        &author,
        &[("name", "Team nightly"), ("owner", org.org_id.as_str())],
        ONE_CELL_PLAN,
    )
    .await?;
    let reader = registered_user(&state, &[UserPermission::ManageOrgs]).await?;
    set_user_org(&state, &reader.1, Some(org.org_id.clone())).await?;
    let campaign = signed_empty(
        &state,
        &reader,
        "GET",
        &format!("/api/v1/campaigns/{campaign_id}"),
    )
    .await?;
    assert_eq!(
        campaign.status(),
        StatusCode::OK,
        "the campaign is the team's"
    );
    let asked = duckdb
        .mock_async(|when, then| {
            when.method(POST)
                .path("/api/query")
                .body_includes(format!("WHERE plan_id = '{plan_id}'"));
            then.status(200).json_body(export_rows("job-1"));
        })
        .await;
    let uri = format!("/api/v1/plans/{plan_id}/results.csv");

    let resp = signed_empty(&state, &reader, "GET", &uri).await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        body_json(resp).await?,
        json!({"error": "user lacks the read_plans permission"})
    );
    asked.assert_calls_async(0).await;

    grant_user(
        &state,
        &reader.1,
        &[UserPermission::ManageOrgs, UserPermission::ReadPlans],
    )
    .await?;
    let resp = signed_empty(&state, &reader, "GET", &uri).await?;
    assert_eq!(resp.status(), StatusCode::OK, "the refusal was the grant");
    let csv = String::from_utf8(resp.into_body().collect().await?.to_bytes().to_vec())?;
    let mut lines = csv.lines().map(|line| line.split(',').collect::<Vec<_>>());
    let (header, row) = (
        lines.next().unwrap_or_default(),
        lines.next().unwrap_or_default(),
    );
    let cell = |name: &str| {
        header
            .iter()
            .position(|column| *column == name)
            .and_then(|at| row.get(at).copied())
    };
    assert_eq!(cell("job_id"), Some("job-1"), "{csv}");
    assert_eq!(cell("client_id"), Some(""), "withheld: {csv}");
    asked.assert_async().await;
    Ok(())
}
