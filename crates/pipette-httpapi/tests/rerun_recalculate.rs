//! What the app's re-run and recalculate controls send and read: the whole-plan
//! selectors with `base_plan_id`, and the offers the jobs reads carry.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use axum::http::StatusCode;
use httpmock::{Method::POST, Mock, MockServer};
use rstest::rstest;
use serde_json::{Value, json};

use pipette_httpapi::state::AppState;
use pipette_pipeline::plan_results::JOB_RESULTS_TIMEOUT;
use pipette_plan_types::{JobId, PlanId, PlanStatus, UserPermission};
use pipette_web_testkit::{
    FULL_EDITOR, MLX_700M_VARIANT, ONE_CELL_PLAN, UserSigner, approved_client, body_json,
    create_org, created_campaign, created_campaign_from, file_job_bodies, json_str,
    registered_user, run_mocked, seeded_state, set_user_org, signed_empty, signed_json,
    with_duckdb,
};

/// What the plan's one job holds before a request is sent.
#[derive(Clone, Copy, Debug)]
enum Recorded {
    Nothing,
    Failure,
    Success,
    Eval,
}

/// A one-job campaign holding `recorded`: its id, the plan's and the job's.
async fn campaign_with(
    state: &AppState,
    owner: &UserSigner,
    recorded: Recorded,
) -> anyhow::Result<(String, PlanId, JobId)> {
    let (campaign_id, first) = created_campaign(state, owner, &[("name", "Nightly")]).await?;
    let job = first_job(state, &first).await?;
    let (submissions, todo) = (state.submission_store.as_ref(), state.todo_store.as_ref());
    match recorded {
        Recorded::Nothing => {}
        Recorded::Failure => {
            pipette_store_testkit::finish_job_with_failure(submissions, todo, &job).await?;
        }
        Recorded::Success => pipette_store_testkit::finish_job(submissions, todo, &job).await?,
        Recorded::Eval => {
            pipette_store_testkit::finish_job_with_success(submissions, todo, &job, "eval").await?;
        }
    }
    Ok((campaign_id, first, job))
}

/// The runs server answers a job only once its run is mocked.
fn with_runs(state: AppState, server: &MockServer) -> anyhow::Result<AppState> {
    with_duckdb(state, &server.base_url())
}

async fn mock_run<'a>(
    server: &'a MockServer,
    job_id: &JobId,
    benchmark_id: &str,
    failed: bool,
    rescored_from: Option<&JobId>,
) -> Mock<'a> {
    let benchmark_type = if benchmark_id.starts_with("eval") {
        "eval"
    } else {
        "prefill_throughput"
    };
    let body = json!({
        "columns": ["job_id", "benchmark_id", "benchmark_type", "metric", "value", "unit",
                    "failed", "scored_at", "rescored_from"],
        "rows": [[job_id.as_str(), benchmark_id, benchmark_type, "accuracy", 0.5, "ratio",
                  failed, "2026-09-29T10:05:00.000000Z", rescored_from.map(JobId::as_str)]],
    });
    let named = format!("'{job_id}'");
    server
        .mock_async(|when, then| {
            when.method(POST).path("/api/query").body_includes(named);
            then.status(200).json_body(body);
        })
        .await
}

/// One eval run per `(job, rescored_from, value)`, answered only to a query
/// naming them all.
async fn mock_runs<'a>(server: &'a MockServer, runs: &[(&JobId, Option<&JobId>, f64)]) -> Mock<'a> {
    mock_runs_after(server, runs, Duration::ZERO).await
}

/// [`mock_runs`], answered `delay` after the query arrives.
async fn mock_runs_after<'a>(
    server: &'a MockServer,
    runs: &[(&JobId, Option<&JobId>, f64)],
    delay: Duration,
) -> Mock<'a> {
    let rows: Vec<Value> = runs
        .iter()
        .map(|(job_id, rescored_from, value)| {
            json!([
                job_id.as_str(),
                "eval_test",
                "eval",
                "accuracy",
                value,
                "ratio",
                false,
                "2026-09-29T10:05:00.000000Z",
                rescored_from.map(JobId::as_str),
                "ev1_a"
            ])
        })
        .collect();
    let named: Vec<&JobId> = runs.iter().map(|(job_id, ..)| *job_id).collect();
    mock_query_after(server, &named, rows, delay).await
}

/// `rows` answered only to a query naming every one of `named`.
async fn mock_query<'a>(server: &'a MockServer, named: &[&JobId], rows: Vec<Value>) -> Mock<'a> {
    mock_query_after(server, named, rows, Duration::ZERO).await
}

/// [`mock_query`], answered `delay` after the query arrives.
async fn mock_query_after<'a>(
    server: &'a MockServer,
    named: &[&JobId],
    rows: Vec<Value>,
    delay: Duration,
) -> Mock<'a> {
    let body = json!({
        "columns": ["job_id", "benchmark_id", "benchmark_type", "metric", "value", "unit",
                    "failed", "scored_at", "rescored_from", "client_id"],
        "rows": rows,
    });
    let named: Vec<String> = named.iter().map(|job_id| format!("'{job_id}'")).collect();
    server
        .mock_async(|when, then| {
            let when = when.method(POST).path("/api/query");
            named
                .into_iter()
                .fold(when, |when, named| when.body_includes(named));
            then.delay(delay).status(200).json_body(body);
        })
        .await
}

async fn first_job(state: &AppState, plan_id: &PlanId) -> anyhow::Result<JobId> {
    state
        .plan_store
        .get_plan(plan_id)
        .await?
        .context("the plan is filed")?
        .job_ids
        .first()
        .cloned()
        .context("the plan holds a job")
}

async fn head(state: &AppState, campaign_id: &str) -> anyhow::Result<Option<PlanId>> {
    Ok(state
        .campaign_store
        .get_campaign(&campaign_id.parse()?)
        .await?
        .context("the campaign exists")?
        .current_plan_id)
}

async fn revise(
    state: &AppState,
    signer: &UserSigner,
    campaign_id: &str,
    verb: &str,
    query: &str,
    body: &Value,
) -> anyhow::Result<(StatusCode, Value)> {
    let uri = format!("/api/v1/campaigns/{campaign_id}/{verb}{query}");
    let resp = signed_json(state, signer, "POST", &uri, body).await?;
    let status = resp.status();
    Ok((status, body_json(resp).await?))
}

async fn read_ok(state: &AppState, signer: &UserSigner, uri: &str) -> anyhow::Result<Value> {
    let resp = signed_empty(state, signer, "GET", uri).await?;
    let status = resp.status();
    let body = body_json(resp).await?;
    anyhow::ensure!(status == StatusCode::OK, "{uri}: {status} {body}");
    Ok(body)
}

async fn plan_jobs(
    state: &AppState,
    signer: &UserSigner,
    plan_id: &PlanId,
) -> anyhow::Result<Value> {
    let body = read_ok(
        state,
        signer,
        &format!("/api/v1/plans/{plan_id}/jobs?results=true"),
    )
    .await?;
    crate::api_schema::assert_conforms("NamedPlanJobs", &body)?;
    Ok(body)
}

// ── whole-plan selectors ────────────────────────────────────────────────────

/// `failed` takes the failures, `all` every answered job.
#[rstest]
#[case::failed(Recorded::Failure, "failed")]
#[case::all_over_a_failure(Recorded::Failure, "all")]
#[case::all_over_a_success(Recorded::Success, "all")]
#[tokio::test]
async fn test_answered_re_runs_the_jobs_it_takes(
    #[case] recorded: Recorded,
    #[case] which: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let owner = registered_user(&state, FULL_EDITOR).await?;
    let (campaign_id, first, job) = campaign_with(&state, &owner, recorded).await?;

    let body = json!({"answered": which, "note": "again"});
    crate::api_schema::assert_conforms("RerunJobsRequest", &body)?;
    let query = format!("?base_plan_id={first}");
    let (status, revised) = revise(&state, &owner, &campaign_id, "rerun", &query, &body).await?;
    assert_eq!(status, StatusCode::CREATED, "{revised}");
    crate::api_schema::assert_conforms("CampaignRevised", &revised)?;
    assert_eq!(revised["previous_plan_id"], json!(first.as_str()));
    assert_eq!(revised["rerun_job_count"], 1);
    assert_eq!(revised["plan"]["note"], "again");
    assert_eq!(
        revised["plan"]["change"],
        json!({"type": "rerun", "answered": which, "job_count": 1})
    );
    let revision = PlanId::try_new(json_str(&revised["plan"], "plan_id")?)?;
    assert_eq!(head(&state, &campaign_id).await?, Some(revision.clone()));
    assert_ne!(first_job(&state, &revision).await?, job, "a new job");
    assert!(state.plan_store.has_cancel_marker(&first).await?);
    Ok(())
}

#[tokio::test]
async fn test_all_evals_recalculates_the_finished_evals() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let owner = registered_user(&state, FULL_EDITOR).await?;
    let (campaign_id, first, _) = campaign_with(&state, &owner, Recorded::Eval).await?;

    let body = json!({"all_evals": true});
    crate::api_schema::assert_conforms("RecalculateJobsRequest", &body)?;
    let query = format!("?base_plan_id={first}");
    let (status, revised) =
        revise(&state, &owner, &campaign_id, "recalculate", &query, &body).await?;
    assert_eq!(status, StatusCode::CREATED, "{revised}");
    crate::api_schema::assert_conforms("CampaignRevised", &revised)?;
    assert_eq!(revised["recalculated_job_count"], 1);
    assert_eq!(revised["rerun_job_count"], 0);
    assert!(revised["plan"].get("note").is_none(), "{revised}");
    assert_eq!(
        revised["plan"]["change"],
        json!({"type": "recalculate", "all_evals": true, "job_count": 1})
    );
    let revision = PlanId::try_new(json_str(&revised["plan"], "plan_id")?)?;
    assert_eq!(head(&state, &campaign_id).await?, Some(revision));
    Ok(())
}

#[rstest]
#[case::rerun("rerun", Recorded::Success)]
#[case::recalculate("recalculate", Recorded::Eval)]
#[tokio::test]
async fn test_named_jobs_take_the_base_too(
    #[case] verb: &str,
    #[case] recorded: Recorded,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let owner = registered_user(&state, FULL_EDITOR).await?;
    let (campaign_id, first, job) = campaign_with(&state, &owner, recorded).await?;

    let body = json!({"job_ids": [job.as_str()]});
    let def = match verb {
        "rerun" => "RerunJobsRequest",
        _ => "RecalculateJobsRequest",
    };
    crate::api_schema::assert_conforms(def, &body)?;
    let query = format!("?base_plan_id={first}");
    let (status, revised) = revise(&state, &owner, &campaign_id, verb, &query, &body).await?;
    assert_eq!(status, StatusCode::CREATED, "{revised}");
    assert_ne!(head(&state, &campaign_id).await?, Some(first));
    Ok(())
}

/// `{job}` and `{head}` stand for the plan's job and the plan.
#[rstest]
#[case::rerun_neither("rerun", "?base_plan_id={head}", json!({}), StatusCode::BAD_REQUEST, "name either job_ids or answered")]
#[case::rerun_both(
    "rerun",
    "?base_plan_id={head}",
    json!({"job_ids": ["{job}"], "answered": "all"}),
    StatusCode::BAD_REQUEST,
    "name either job_ids or answered"
)]
#[case::answered_without_base("rerun", "", json!({"answered": "all"}), StatusCode::BAD_REQUEST, "needs base_plan_id")]
#[case::answered_dry_run(
    "rerun",
    "?base_plan_id={head}&dry_run=true",
    json!({"answered": "all"}),
    StatusCode::BAD_REQUEST,
    "dry_run takes job_ids"
)]
#[case::answered_unknown(
    "rerun",
    "?base_plan_id={head}",
    json!({"answered": "some"}),
    StatusCode::BAD_REQUEST,
    "answered"
)]
#[case::answered_none_failed(
    "rerun",
    "?base_plan_id={head}",
    json!({"answered": "failed"}),
    StatusCode::CONFLICT,
    "holds no failed job"
)]
#[case::rerun_malformed_base(
    "rerun",
    "?base_plan_id=bad!id",
    json!({"job_ids": ["{job}"]}),
    StatusCode::BAD_REQUEST,
    "plan_id"
)]
#[case::recalculate_malformed_base(
    "recalculate",
    "?base_plan_id=bad!id",
    json!({"all_evals": true}),
    StatusCode::BAD_REQUEST,
    "plan_id"
)]
#[case::rerun_moved(
    "rerun",
    "?base_plan_id=plan-0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e",
    json!({"job_ids": ["{job}"]}),
    StatusCode::CONFLICT,
    "not plan-0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e"
)]
#[case::answered_moved(
    "rerun",
    "?base_plan_id=plan-0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e",
    json!({"answered": "all"}),
    StatusCode::CONFLICT,
    "not plan-0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e"
)]
#[case::recalculate_neither(
    "recalculate",
    "?base_plan_id={head}",
    json!({"all_evals": false}),
    StatusCode::BAD_REQUEST,
    "name either job_ids or all_evals"
)]
#[case::recalculate_both(
    "recalculate",
    "?base_plan_id={head}",
    json!({"job_ids": ["{job}"], "all_evals": true}),
    StatusCode::BAD_REQUEST,
    "name either job_ids or all_evals"
)]
#[case::all_evals_without_base("recalculate", "", json!({"all_evals": true}), StatusCode::BAD_REQUEST, "needs base_plan_id")]
#[case::all_evals_dry_run(
    "recalculate",
    "?dry_run=true&base_plan_id={head}",
    json!({"all_evals": true}),
    StatusCode::BAD_REQUEST,
    "dry_run takes job_ids"
)]
#[case::recalculate_moved(
    "recalculate",
    "?base_plan_id=plan-0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e",
    json!({"job_ids": ["{job}"]}),
    StatusCode::CONFLICT,
    "not plan-0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e"
)]
#[case::all_evals_moved(
    "recalculate",
    "?base_plan_id=plan-0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e",
    json!({"all_evals": true}),
    StatusCode::CONFLICT,
    "not plan-0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e"
)]
#[case::all_evals_none(
    "recalculate",
    "?base_plan_id={head}",
    json!({"all_evals": true}),
    StatusCode::CONFLICT,
    "holds no eval job"
)]
#[tokio::test]
async fn test_a_refused_revision_files_nothing(
    #[case] verb: &str,
    #[case] query: &str,
    #[case] body: Value,
    #[case] status: StatusCode,
    #[case] error: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let owner = registered_user(&state, FULL_EDITOR).await?;
    let (campaign_id, first, job) = campaign_with(&state, &owner, Recorded::Success).await?;
    let query = query.replace("{head}", first.as_str());
    let body: Value = serde_json::from_str(&body.to_string().replace("{job}", job.as_str()))?;

    let (answered, refusal) = revise(&state, &owner, &campaign_id, verb, &query, &body).await?;
    assert_eq!(answered, status, "{refusal}");
    let message = json_str(&refusal, "error")?;
    assert!(message.contains(error), "{message}");
    assert_eq!(head(&state, &campaign_id).await?, Some(first.clone()));
    assert!(!state.plan_store.has_cancel_marker(&first).await?);
    Ok(())
}

#[rstest]
#[case::rerun("rerun", json!({"answered": "all"}))]
#[case::recalculate("recalculate", json!({"all_evals": true}))]
#[tokio::test]
async fn test_a_stranger_is_refused_before_the_selector(
    #[case] verb: &str,
    #[case] body: Value,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let owner = registered_user(&state, FULL_EDITOR).await?;
    let (campaign_id, first, _) = campaign_with(&state, &owner, Recorded::Failure).await?;
    let stranger = registered_user(&state, FULL_EDITOR).await?;

    let (status, body) = revise(&state, &stranger, &campaign_id, verb, "", &body).await?;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(head(&state, &campaign_id).await?, Some(first));
    Ok(())
}

// ── the offers on the jobs reads ────────────────────────────────────────────

/// A finished eval is also offered as recalculable.
#[tokio::test]
async fn test_the_head_offers_its_editor_the_re_runs() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let server = MockServer::start_async().await;
    let state = with_runs(seeded_state(dir.path()).await?, &server)?;
    let owner = registered_user(&state, FULL_EDITOR).await?;
    let (campaign_id, first, job) = campaign_with(&state, &owner, Recorded::Eval).await?;
    mock_run(&server, &job, "eval_test", false, None).await;

    let body = plan_jobs(&state, &owner, &first).await?;
    assert_eq!(body["rerun_campaign_id"], json!(campaign_id));
    let result = &body["jobs"][0]["result"];
    assert_eq!(result["recalculable"], json!(true), "{body}");
    assert!(result["rescored_from"].is_null(), "{body}");
    Ok(())
}

#[rstest]
#[case::success(Recorded::Success)]
#[case::queued(Recorded::Nothing)]
#[case::failure(Recorded::Failure)]
fn test_only_a_finished_eval_is_recalculable(#[case] recorded: Recorded) -> anyhow::Result<()> {
    run_mocked(assert_recalculable_only_if_a_finished_eval(recorded))
}

async fn assert_recalculable_only_if_a_finished_eval(recorded: Recorded) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let server = MockServer::start_async().await;
    let state = with_runs(seeded_state(dir.path()).await?, &server)?;
    let owner = registered_user(&state, FULL_EDITOR).await?;
    let (campaign_id, first, job) = campaign_with(&state, &owner, recorded).await?;
    match recorded {
        Recorded::Success => {
            mock_run(&server, &job, "prefill_throughput_256", false, None).await;
        }
        Recorded::Failure => {
            mock_run(&server, &job, "eval_test", true, None).await;
        }
        Recorded::Nothing | Recorded::Eval => {}
    }

    let body = plan_jobs(&state, &owner, &first).await?;
    assert_eq!(body.get("rerun_campaign_id"), Some(&json!(campaign_id)));
    assert_ne!(
        body.pointer("/jobs/0/result/recalculable"),
        Some(&json!(true)),
        "{body}"
    );
    Ok(())
}

/// Who reads the head in [`test_a_reader_who_may_not_revise_is_offered_nothing`].
#[derive(Clone, Copy, Debug)]
enum Reader {
    /// Reads every plan, and administers no campaign.
    OfEveryPlan,
    /// The owner, below the full tier.
    OwnerWithheld,
    /// The owner, who may not cancel the plan a revision replaces.
    OwnerWithoutCancel,
    /// The owner, of a head filed as job bodies, which recorded no digest.
    OwnerOfJobBodies,
}

#[rstest]
#[case::of_every_plan(Reader::OfEveryPlan)]
#[case::owner_withheld(Reader::OwnerWithheld)]
#[case::owner_without_cancel(Reader::OwnerWithoutCancel)]
#[case::owner_of_job_bodies(Reader::OwnerOfJobBodies)]
fn test_a_reader_who_may_not_revise_is_offered_nothing(
    #[case] reader: Reader,
) -> anyhow::Result<()> {
    run_mocked(assert_offered_nothing(reader))
}

async fn assert_offered_nothing(reader: Reader) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let server = MockServer::start_async().await;
    let (state, _client_key, client_id) = approved_client(dir.path()).await?;
    let state = with_runs(state, &server)?;
    let grants = match reader {
        Reader::OwnerWithheld => &[UserPermission::SubmitPlans, UserPermission::CancelPlans][..],
        Reader::OwnerWithoutCancel => &[UserPermission::SubmitPlans, UserPermission::ReadClients],
        Reader::OfEveryPlan | Reader::OwnerOfJobBodies => FULL_EDITOR,
    };
    let owner = registered_user(&state, grants).await?;
    let (campaign_id, mut head, mut job) = campaign_with(&state, &owner, Recorded::Eval).await?;
    if let Reader::OwnerOfJobBodies = reader {
        head = file_job_bodies(&state, &owner, &campaign_id, &client_id, "bodies").await?;
        job = first_job(&state, &head).await?;
        let (submissions, todo) = (state.submission_store.as_ref(), state.todo_store.as_ref());
        pipette_store_testkit::finish_job(submissions, todo, &job).await?;
    }
    mock_run(&server, &job, "eval_test", false, None).await;
    let signer = match reader {
        Reader::OfEveryPlan => registered_user(&state, &[UserPermission::ReadAnyPlans]).await?,
        Reader::OwnerWithheld | Reader::OwnerWithoutCancel | Reader::OwnerOfJobBodies => owner,
    };

    let body = plan_jobs(&state, &signer, &head).await?;
    assert!(body.get("rerun_campaign_id").is_none(), "{body}");
    assert_eq!(
        body.pointer("/jobs/0/result/recalculable"),
        Some(&json!(false)),
        "{body}"
    );
    Ok(())
}

/// Whoever filed it, with or without `manage_orgs`.
#[tokio::test]
async fn test_a_member_is_offered_the_team_s_re_runs() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let admin = registered_user(&state, &[UserPermission::ManageAnyOrgs]).await?;
    let acme = create_org(&state, &admin, "acme").await?;
    let team = [FULL_EDITOR, &[UserPermission::ManageOrgs]].concat();
    let author = registered_user(&state, &team).await?;
    let team_admin = registered_user(&state, &team).await?;
    let member = registered_user(&state, FULL_EDITOR).await?;
    // A revision ends the plan it replaces, which this one may not.
    let filer = registered_user(
        &state,
        &[UserPermission::SubmitPlans, UserPermission::ReadClients],
    )
    .await?;
    for signer in [&author, &team_admin, &member, &filer] {
        set_user_org(&state, &signer.1, Some(acme.parse()?)).await?;
    }
    let params = [("name", "Team"), ("owner", acme.as_str())];
    let (campaign_id, first) = created_campaign(&state, &author, &params).await?;

    for signer in [&team_admin, &member] {
        let body = plan_jobs(&state, signer, &first).await?;
        assert_eq!(body["rerun_campaign_id"], json!(campaign_id), "{body}");
    }
    let body = plan_jobs(&state, &filer, &first).await?;
    assert!(body.get("rerun_campaign_id").is_none(), "{body}");
    Ok(())
}

/// A recalculation names the parent in its run, a re-run in its plan. The replaced
/// plan, no longer the head, offers nothing.
#[rstest]
#[case::recalculated("recalculate")]
#[case::re_run("rerun")]
fn test_a_new_row_names_its_parent(#[case] verb: &str) -> anyhow::Result<()> {
    run_mocked(assert_the_new_row_names_its_parent(verb))
}

async fn assert_the_new_row_names_its_parent(verb: &str) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let server = MockServer::start_async().await;
    let state = with_runs(seeded_state(dir.path()).await?, &server)?;
    let owner = registered_user(&state, FULL_EDITOR).await?;
    let reader = registered_user(&state, &[UserPermission::ReadAnyPlans]).await?;
    let (campaign_id, first, job) = campaign_with(&state, &owner, Recorded::Eval).await?;
    mock_run(&server, &job, "eval_test", false, None).await;
    let (status, revised) = revise(
        &state,
        &owner,
        &campaign_id,
        verb,
        "",
        &json!({"job_ids": [job.as_str()]}),
    )
    .await?;
    assert_eq!(status, StatusCode::CREATED, "{revised}");
    let plan = revised.get("plan").context("the revision's plan")?;
    let revision = PlanId::try_new(json_str(plan, "plan_id")?)?;
    let fresh = first_job(&state, &revision).await?;
    let recalculated = verb == "recalculate";
    if recalculated {
        mock_run(&server, &fresh, "eval_test", false, Some(&job)).await;
    }

    let parent = json!({"job_id": job.as_str(), "plan_id": first.as_str()});
    for signer in [&owner, &reader] {
        let body = plan_jobs(&state, signer, &revision).await?;
        let (rescored_from, rerun_of) = if recalculated {
            (parent.clone(), Value::Null)
        } else {
            (Value::Null, parent.clone())
        };
        // A queued re-run has no run, so its result names no `rescored_from`.
        let field = |path: &str| {
            body.pointer(&format!("/jobs/0{path}"))
                .unwrap_or(&Value::Null)
        };
        assert_eq!(field("/result/rescored_from"), &rescored_from, "{body}");
        assert_eq!(field("/plan/rerun_of"), &rerun_of, "{body}");
    }
    let replaced = plan_jobs(&state, &owner, &first).await?;
    assert!(replaced.get("rerun_campaign_id").is_none(), "{replaced}");
    assert_eq!(
        replaced.pointer("/jobs/0/result/recalculable"),
        Some(&json!(false))
    );
    Ok(())
}

/// A recalculation carries its earlier runs, newest first, each with its plan
/// where one holds it.
#[test]
fn test_a_recalculation_carries_the_earlier_runs() -> anyhow::Result<()> {
    run_mocked(assert_a_recalculation_carries_the_earlier_runs())
}

async fn assert_a_recalculation_carries_the_earlier_runs() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let server = MockServer::start_async().await;
    let state = with_runs(seeded_state(dir.path()).await?, &server)?;
    let owner = registered_user(&state, FULL_EDITOR).await?;
    let (campaign_id, first, job) = campaign_with(&state, &owner, Recorded::Eval).await?;
    // `job` itself rescored `older`, which no plan of the campaign holds.
    let older = JobId::try_new("job-older")?;
    mock_runs(&server, &[(&older, None, 0.25)]).await;
    mock_runs(&server, &[(&job, Some(&older), 0.5)]).await;
    let body = json!({"job_ids": [job.as_str()]});
    let (status, revised) = revise(&state, &owner, &campaign_id, "recalculate", "", &body).await?;
    assert_eq!(status, StatusCode::CREATED, "{revised}");
    let plan = revised.get("plan").context("the revision's plan")?;
    let revision = PlanId::try_new(json_str(plan, "plan_id")?)?;
    let fresh = first_job(&state, &revision).await?;
    mock_runs(&server, &[(&fresh, Some(&job), 0.75)]).await;

    let body = plan_jobs(&state, &owner, &revision).await?;
    let earlier = body
        .pointer("/jobs/0/result/earlier_runs")
        .and_then(Value::as_array)
        .context("the result lists its earlier runs")?;
    let runs: Vec<(Option<&Value>, Option<&Value>)> = earlier
        .iter()
        .map(|run| (run.get("job"), run.pointer("/rows/0/value")))
        .collect();
    assert_eq!(
        runs,
        [
            (
                Some(&json!({"job_id": job.as_str(), "plan_id": first.as_str()})),
                Some(&json!(0.5))
            ),
            (
                Some(&json!({"job_id": older.as_str(), "plan_id": null})),
                Some(&json!(0.25))
            ),
        ],
        "{body}"
    );
    Ok(())
}

/// An unread earlier run empties the history, not the page.
#[test]
fn test_an_unread_earlier_run_ends_the_history() -> anyhow::Result<()> {
    run_mocked(assert_an_unread_earlier_run_ends_the_history())
}

async fn assert_an_unread_earlier_run_ends_the_history() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let server = MockServer::start_async().await;
    let state = with_runs(seeded_state(dir.path()).await?, &server)?;
    let owner = registered_user(&state, FULL_EDITOR).await?;
    let (campaign_id, _, job) = campaign_with(&state, &owner, Recorded::Eval).await?;
    let body = json!({"job_ids": [job.as_str()]});
    let (status, revised) = revise(&state, &owner, &campaign_id, "recalculate", "", &body).await?;
    assert_eq!(status, StatusCode::CREATED, "{revised}");
    let plan = revised.get("plan").context("the revision's plan")?;
    let revision = PlanId::try_new(json_str(plan, "plan_id")?)?;
    let fresh = first_job(&state, &revision).await?;
    mock_runs(&server, &[(&fresh, Some(&job), 0.75)]).await;
    let named = format!("'{job}'");
    server
        .mock_async(|when, then| {
            when.method(POST).path("/api/query").body_includes(named);
            then.status(500);
        })
        .await;

    let body = plan_jobs(&state, &owner, &revision).await?;
    assert_eq!(body.get("results_read"), Some(&json!("ok")), "{body}");
    assert_eq!(
        body.pointer("/jobs/0/result/rescored_from/job_id"),
        Some(&json!(job)),
        "{body}"
    );
    assert_eq!(
        body.pointer("/jobs/0/result/earlier_runs"),
        Some(&json!([])),
        "{body}"
    );
    Ok(())
}

/// Earlier runs are read in one query per hop, not per job, and withheld
/// below the full tier.
#[rstest]
#[case::full(true)]
#[case::withheld(false)]
fn test_a_page_reads_its_earlier_runs_in_one_query(#[case] full: bool) -> anyhow::Result<()> {
    run_mocked(assert_earlier_runs_read_in_one_query(full))
}

async fn assert_earlier_runs_read_in_one_query(full: bool) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let server = MockServer::start_async().await;
    let state = with_runs(seeded_state(dir.path()).await?, &server)?;
    let owner = registered_user(&state, FULL_EDITOR).await?;
    let document = format!("{ONE_CELL_PLAN}{MLX_700M_VARIANT}");
    let (campaign_id, first) =
        created_campaign_from(&state, &owner, &[("name", "Nightly")], &document).await?;
    let jobs = state
        .plan_store
        .get_plan(&first)
        .await?
        .context("the plan is filed")?
        .job_ids;
    let [a, b] = jobs.as_slice() else {
        anyhow::bail!("two jobs, not {jobs:?}");
    };
    let (submissions, todo) = (state.submission_store.as_ref(), state.todo_store.as_ref());
    futures::try_join!(
        pipette_store_testkit::finish_job_with_success(submissions, todo, a, "eval"),
        pipette_store_testkit::finish_job_with_success(submissions, todo, b, "eval"),
    )?;
    let body = json!({"job_ids": [a.as_str(), b.as_str()]});
    let (status, revised) = revise(&state, &owner, &campaign_id, "recalculate", "", &body).await?;
    assert_eq!(status, StatusCode::CREATED, "{revised}");
    let plan = revised.get("plan").context("the revision's plan")?;
    let revision = PlanId::try_new(json_str(plan, "plan_id")?)?;
    let fresh = state
        .plan_store
        .get_plan(&revision)
        .await?
        .context("the revision is filed")?
        .job_ids;
    let [fresh_a, fresh_b] = fresh.as_slice() else {
        anyhow::bail!("two jobs, not {fresh:?}");
    };
    mock_runs(
        &server,
        &[(fresh_a, Some(a), 0.75), (fresh_b, Some(b), 0.7)],
    )
    .await;
    let parents = mock_runs(&server, &[(a, None, 0.5), (b, None, 0.4)]).await;
    let reader = if full {
        owner
    } else {
        registered_user(&state, &[UserPermission::ReadAnyPlans]).await?
    };

    let body = plan_jobs(&state, &reader, &revision).await?;
    assert_eq!(parents.calls_async().await, 1, "{body}");
    let tier = if full { "full" } else { "withheld" };
    assert_eq!(body.get("fleet_detail"), Some(&json!(tier)));
    // The page keeps the document's order, not the revision's.
    let mut earlier: Vec<(Option<&str>, Option<&Value>)> = body
        .get("jobs")
        .and_then(Value::as_array)
        .context("a `jobs` array")?
        .iter()
        .map(|job| {
            let run = job.pointer("/result/earlier_runs/0");
            (
                run.and_then(|run| run.pointer("/job/job_id"))
                    .and_then(Value::as_str),
                run.and_then(|run| run.pointer("/rows/0/client_id")),
            )
        })
        .collect();
    earlier.sort_by_key(|(job_id, _)| *job_id);
    let client = if full { json!("ev1_a") } else { Value::Null };
    let mut expected = [
        (Some(a.as_str()), Some(&client)),
        (Some(b.as_str()), Some(&client)),
    ];
    expected.sort_by_key(|(job_id, _)| *job_id);
    assert_eq!(earlier, expected, "{body}");
    Ok(())
}

/// A second re-run names the runs before it and carries their values, newest
/// first, each with its plan and read once; a run with no rows is left out.
#[rstest]
#[case::all_ran(true)]
#[case::middle_lapsed(false)]
fn test_a_second_re_run_carries_the_runs_before(#[case] middle_ran: bool) -> anyhow::Result<()> {
    run_mocked(assert_a_second_re_run_carries_the_runs_before(middle_ran))
}

async fn assert_a_second_re_run_carries_the_runs_before(middle_ran: bool) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let server = MockServer::start_async().await;
    let state = with_runs(seeded_state(dir.path()).await?, &server)?;
    let owner = registered_user(&state, FULL_EDITOR).await?;
    let (campaign_id, first, original) = campaign_with(&state, &owner, Recorded::Success).await?;
    let rerun = async |job_id: &JobId| {
        let body = json!({"job_ids": [job_id.as_str()]});
        let (status, revised) = revise(&state, &owner, &campaign_id, "rerun", "", &body).await?;
        anyhow::ensure!(status == StatusCode::CREATED, "{revised}");
        let plan = revised.get("plan").context("the revision's plan")?;
        let revision = PlanId::try_new(json_str(plan, "plan_id")?)?;
        let job = first_job(&state, &revision).await?;
        anyhow::Ok((revision, job))
    };
    let (middle, again) = rerun(&original).await?;
    pipette_store_testkit::finish_job(
        state.submission_store.as_ref(),
        state.todo_store.as_ref(),
        &again,
    )
    .await?;
    let (last, latest) = rerun(&again).await?;
    mock_runs(&server, &[(&latest, None, 0.75)]).await;
    let parents = if middle_ran {
        mock_runs(&server, &[(&again, None, 0.5)]).await
    } else {
        mock_query(&server, &[&again], Vec::new()).await
    };
    let older = mock_runs(&server, &[(&original, None, 0.25)]).await;

    let body = plan_jobs(&state, &owner, &last).await?;
    assert_eq!(
        body.pointer("/jobs/0/plan/rerun_of"),
        Some(&json!({"job_id": again.as_str(), "plan_id": middle.as_str()})),
        "{body}"
    );
    assert_eq!(
        body.pointer("/jobs/0/plan/rerun_history"),
        Some(&json!([{"job_id": original.as_str(), "plan_id": first.as_str()}])),
        "{body}"
    );
    let earlier = body
        .pointer("/jobs/0/result/earlier_runs")
        .and_then(Value::as_array)
        .context("the result lists its earlier runs")?;
    let runs: Vec<(Option<&Value>, Option<&Value>)> = earlier
        .iter()
        .map(|run| (run.get("job"), run.pointer("/rows/0/value")))
        .collect();
    let again_run = json!({"job_id": again.as_str(), "plan_id": middle.as_str()});
    let original_run = json!({"job_id": original.as_str(), "plan_id": first.as_str()});
    let (again_value, original_value) = (json!(0.5), json!(0.25));
    let mut expected = vec![(Some(&original_run), Some(&original_value))];
    if middle_ran {
        expected.insert(0, (Some(&again_run), Some(&again_value)));
    }
    assert_eq!(runs, expected, "{body}");
    assert_eq!(
        (parents.calls_async().await, older.calls_async().await),
        (1, 1),
        "{body}"
    );
    Ok(())
}

/// `job`'s campaign revised by `verb` over `job`: the revision and its job.
async fn revised_over(
    state: &AppState,
    owner: &UserSigner,
    campaign_id: &str,
    verb: &str,
    job: &JobId,
) -> anyhow::Result<(PlanId, JobId)> {
    let body = json!({"job_ids": [job.as_str()]});
    let (status, revised) = revise(state, owner, campaign_id, verb, "", &body).await?;
    anyhow::ensure!(status == StatusCode::CREATED, "{revised}");
    let plan = revised.get("plan").context("the revision's plan")?;
    let revision = PlanId::try_new(json_str(plan, "plan_id")?)?;
    let fresh = first_job(state, &revision).await?;
    Ok((revision, fresh))
}

/// A re-run's history reads beside the page's runs, since the plan names the job
/// it ran again: two slow reads cost the page one of them.
#[test]
fn test_a_re_run_s_history_reads_beside_the_runs() -> anyhow::Result<()> {
    run_mocked(assert_a_re_run_s_history_reads_beside_the_runs())
}

async fn assert_a_re_run_s_history_reads_beside_the_runs() -> anyhow::Result<()> {
    const SLOW: Duration = Duration::from_millis(1500);
    let dir = tempfile::tempdir()?;
    let server = MockServer::start_async().await;
    let state = with_runs(seeded_state(dir.path()).await?, &server)?;
    let owner = registered_user(&state, FULL_EDITOR).await?;
    let (campaign_id, first, job) = campaign_with(&state, &owner, Recorded::Success).await?;
    let (revision, again) = revised_over(&state, &owner, &campaign_id, "rerun", &job).await?;
    mock_runs_after(&server, &[(&again, None, 0.75)], SLOW).await;
    mock_runs_after(&server, &[(&job, None, 0.5)], SLOW).await;

    let started = Instant::now();
    let body = plan_jobs(&state, &owner, &revision).await?;
    let waited = started.elapsed();
    // One after the other, the two reads would hold the page for `2 * SLOW`.
    assert!(waited >= SLOW && waited < SLOW + SLOW / 2, "{waited:?}");
    assert_eq!(body.get("results_read"), Some(&json!("ok")), "{body}");
    assert_eq!(
        body.pointer("/jobs/0/result/earlier_runs/0/job"),
        Some(&json!({"job_id": job.as_str(), "plan_id": first.as_str()})),
        "{body}"
    );
    Ok(())
}

/// A kept complete re-run reads neither its runs nor its history again: a
/// parent plan that changes after the fill shows in a cold read alone.
#[test]
fn test_a_kept_re_run_reads_its_history_once() -> anyhow::Result<()> {
    run_mocked(assert_a_kept_re_run_reads_its_history_once())
}

async fn assert_a_kept_re_run_reads_its_history_once() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let server = MockServer::start_async().await;
    let state = with_runs(seeded_state(dir.path()).await?, &server)?;
    let owner = registered_user(&state, FULL_EDITOR).await?;
    let (campaign_id, first, job) = campaign_with(&state, &owner, Recorded::Success).await?;
    let (revision, again) = revised_over(&state, &owner, &campaign_id, "rerun", &job).await?;
    let mut manifest = state
        .plan_store
        .get_plan(&revision)
        .await?
        .context("the revision is filed")?;
    manifest.status = PlanStatus::Complete;
    state.plan_store.put_plan(&manifest).await?;
    let runs = mock_runs(&server, &[(&again, None, 0.75)]).await;
    let parents = mock_runs(&server, &[(&job, None, 0.5)]).await;
    let uri = format!("/api/v1/plans/{revision}/jobs?results=true&all=true");
    let parent_of = |body: &Value| body.pointer("/jobs/0/plan/rerun_of").cloned();
    let filed_under = Some(json!({"job_id": job.as_str(), "plan_id": first.as_str()}));

    let filled = read_ok(&state, &owner, &uri).await?;
    assert_eq!(parent_of(&filled), filed_under, "{filled}");
    let mut parent = state
        .plan_store
        .get_plan(&first)
        .await?
        .context("the first plan is filed")?;
    parent.job_ids.retain(|job_id| job_id != &job);
    state.plan_store.put_plan(&parent).await?;

    let kept = read_ok(&state, &owner, &uri).await?;
    assert_eq!(kept, filled);
    assert_eq!(
        (runs.calls_async().await, parents.calls_async().await),
        (1, 1)
    );
    let mut cold = state.clone();
    cold.plan_results_cache = Arc::default();
    let cold = read_ok(&cold, &owner, &uri).await?;
    assert_eq!(
        parent_of(&cold),
        Some(json!({"job_id": job.as_str(), "plan_id": null})),
        "{cold}"
    );
    Ok(())
}

/// A page holding a re-run and a recalculation carries both histories, each
/// parent read once and named with the newest plan holding it.
#[test]
fn test_a_page_of_a_re_run_and_a_recalculation_carries_both_histories() -> anyhow::Result<()> {
    run_mocked(assert_a_page_carries_both_histories())
}

async fn assert_a_page_carries_both_histories() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let server = MockServer::start_async().await;
    let state = with_runs(seeded_state(dir.path()).await?, &server)?;
    let owner = registered_user(&state, FULL_EDITOR).await?;
    let document = format!("{ONE_CELL_PLAN}{MLX_700M_VARIANT}");
    let (campaign_id, first) =
        created_campaign_from(&state, &owner, &[("name", "Nightly")], &document).await?;
    let jobs_in = async |plan_id: &PlanId| {
        let manifest = state.plan_store.get_plan(plan_id).await?;
        anyhow::Ok(manifest.context("the plan is filed")?.job_ids)
    };
    let jobs = jobs_in(&first).await?;
    let [a, b] = jobs.as_slice() else {
        anyhow::bail!("two jobs, not {jobs:?}");
    };
    let (submissions, todo) = (state.submission_store.as_ref(), state.todo_store.as_ref());
    futures::try_join!(
        pipette_store_testkit::finish_job_with_success(submissions, todo, a, "eval"),
        pipette_store_testkit::finish_job_with_success(submissions, todo, b, "eval"),
    )?;
    // The re-run replaces `a` and carries `b`; the recalculation then replaces
    // `b` and carries the re-run's job.
    let (middle, _) = revised_over(&state, &owner, &campaign_id, "rerun", a).await?;
    let (last, _) = revised_over(&state, &owner, &campaign_id, "recalculate", b).await?;
    let held = jobs_in(&middle).await?;
    let fresh = jobs_in(&last).await?;
    let again = held
        .iter()
        .find(|job_id| fresh.contains(*job_id))
        .context("the re-run's job, carried")?;
    let rescored = fresh
        .iter()
        .find(|job_id| !held.contains(*job_id))
        .context("the recalculation's job")?;
    mock_runs(&server, &[(again, None, 0.75), (rescored, Some(b), 0.7)]).await;
    let reran = mock_runs(&server, &[(a, None, 0.5)]).await;
    let recalculated = mock_runs(&server, &[(b, None, 0.4)]).await;

    let body = plan_jobs(&state, &owner, &last).await?;
    let row = |job_id: &JobId| {
        body.get("jobs")
            .and_then(Value::as_array)
            .and_then(|jobs| {
                jobs.iter()
                    .find(|job| job.get("job_id") == Some(&json!(job_id)))
            })
            .with_context(|| format!("a row for {job_id}: {body}"))
    };
    let earlier = |row: &Value| {
        row.pointer("/result/earlier_runs")
            .and_then(Value::as_array)
            .map(|runs| {
                runs.iter()
                    .map(|run| {
                        (
                            run.get("job").cloned(),
                            run.pointer("/rows/0/value").cloned(),
                        )
                    })
                    .collect::<Vec<_>>()
            })
    };
    let (again_row, rescored_row) = (row(again)?, row(rescored)?);
    let a_run = json!({"job_id": a.as_str(), "plan_id": first.as_str()});
    // `b` is held by the first plan and, carried, by the re-run's: the newer names it.
    let b_run = json!({"job_id": b.as_str(), "plan_id": middle.as_str()});
    assert_eq!(again_row.pointer("/plan/rerun_of"), Some(&a_run), "{body}");
    assert_eq!(
        earlier(again_row),
        Some(vec![(Some(a_run.clone()), Some(json!(0.5)))]),
        "{body}"
    );
    assert_eq!(
        rescored_row.pointer("/result/rescored_from"),
        Some(&b_run),
        "{body}"
    );
    assert_eq!(
        earlier(rescored_row),
        Some(vec![(Some(b_run.clone()), Some(json!(0.4)))]),
        "{body}"
    );
    assert_eq!(
        (reran.calls_async().await, recalculated.calls_async().await),
        (1, 1),
        "{body}"
    );
    Ok(())
}

/// A recalculation's history reads after the page's runs, which name its parent,
/// and within the page's one wait: a parent not read by its end is left out.
#[test]
fn test_a_recalculation_s_history_reads_within_the_page_s_wait() -> anyhow::Result<()> {
    run_mocked(assert_a_recalculation_s_history_reads_within_the_page_s_wait())
}

async fn assert_a_recalculation_s_history_reads_within_the_page_s_wait() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let server = MockServer::start_async().await;
    let state = with_runs(seeded_state(dir.path()).await?, &server)?;
    let owner = registered_user(&state, FULL_EDITOR).await?;
    let (campaign_id, _, job) = campaign_with(&state, &owner, Recorded::Eval).await?;
    let (revision, fresh) = revised_over(&state, &owner, &campaign_id, "recalculate", &job).await?;
    // Each read alone fits the wait; the parent's, after the runs, ends past it.
    let slow = JOB_RESULTS_TIMEOUT * 2 / 3;
    mock_runs_after(&server, &[(&fresh, Some(&job), 0.75)], slow).await;
    mock_runs_after(&server, &[(&job, None, 0.5)], slow).await;

    let started = Instant::now();
    let body = plan_jobs(&state, &owner, &revision).await?;
    let waited = started.elapsed();
    assert!(
        waited >= JOB_RESULTS_TIMEOUT && waited < JOB_RESULTS_TIMEOUT + slow / 2,
        "{waited:?}"
    );
    assert_eq!(body.get("results_read"), Some(&json!("ok")), "{body}");
    assert_eq!(
        body.pointer("/jobs/0/result/rescored_from/job_id"),
        Some(&json!(job)),
        "{body}"
    );
    assert_eq!(
        body.pointer("/jobs/0/result/earlier_runs"),
        Some(&json!([])),
        "{body}"
    );
    Ok(())
}
