//! `GET /api/v1/plans/{plan_id}/jobs?results=true`: each job's result, read from a
//! mock pipette-duckdb.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use axum::body::Body;
use axum::http::{Response, StatusCode};
use ed25519_dalek::SigningKey;
use http_body_util::BodyExt;
use httpmock::Method::POST;
use httpmock::{Mock, MockServer};
use rstest::rstest;
use serde_json::{Value, json};
use tempfile::TempDir;

use pipette_core::plan::{ExpiredJob, PlanManifest, SettledOutcome};
use pipette_httpapi::state::AppState;
use pipette_pipeline::plan_results::JOB_RESULTS_TIMEOUT;
use pipette_plan_types::{
    CatalogScope, DescriptorEntryKind, DescriptorMember, JobId, NonEmptyVec, PlanId, PlanStatus,
    UserId, UserKeyId, UserPermission,
};
use pipette_store_testkit::{TodoStoreUnimplemented, a_descriptor_entry, a_member, a_model_member};
use pipette_web_testkit::{
    DUCKDB_TIMEOUT, ONE_CELL_PLAN, approved_client, authed_user_get, body_json, created_campaign,
    created_campaign_from, grant_user, register_and_approve, registered_user,
    registered_user_with_detail, run_mocked, seed_org, seeded_sql_state, set_user_org, with_duckdb,
};

/// [`ONE_CELL_PLAN`] over three models: a first plan of three jobs, to page.
const THREE_JOB_PLAN: &str = r#"
benchmarks = ["prefill_throughput_256"]

[[variants]]
requires = ["os:macos"]
models   = [
    { type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "LFM2.5-350M-MLX-4bit" },
    { type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "LFM2-700M-MLX-4bit" },
    { type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "LFM2-1.2B-MLX-4bit" },
]
runtimes = [{ type = "mlx_macos_pipette", version = "0.31.3", flavor = "macos-arm64", source = { type = "pip_requirements_text", contents = "mlx-lm==0.31.3\n" } }]
"#;

/// A campaign's first plan, filed by an account that reads its plans, and the
/// fleet when `detail`, on a server reading pipette-duckdb from `duckdb` when one
/// is given.
struct Filed {
    _dir: TempDir,
    state: AppState,
    owner: (SigningKey, UserId, UserKeyId),
    plan_id: PlanId,
}

async fn filed(duckdb: Option<&MockServer>, detail: bool) -> anyhow::Result<Filed> {
    filed_from(duckdb, detail, ONE_CELL_PLAN).await
}

/// [`filed`], with `document` as the campaign's first plan.
async fn filed_from(
    duckdb: Option<&MockServer>,
    detail: bool,
    document: &str,
) -> anyhow::Result<Filed> {
    let dir = tempfile::tempdir()?;
    let (state, _key, _client_id) = approved_client(dir.path()).await?;
    let state = match duckdb {
        Some(server) => with_duckdb(state, &server.base_url())?,
        None => state,
    };
    let owner = registered_user_with_detail(&state, detail).await?;
    let (_, plan_id) =
        created_campaign_from(&state, &owner, &[("name", "Nightly")], document).await?;
    Ok(Filed {
        _dir: dir,
        state,
        owner,
        plan_id,
    })
}

impl Filed {
    async fn get(&self, uri: &str) -> anyhow::Result<Response<Body>> {
        let (key, user_id, key_id) = &self.owner;
        authed_user_get(&self.state, key, user_id, key_id, uri).await
    }

    async fn get_json(&self, uri: &str) -> anyhow::Result<Value> {
        let resp = self.get(uri).await?;
        anyhow::ensure!(resp.status() == StatusCode::OK, "{uri}: {}", resp.status());
        body_json(resp).await
    }

    /// [`Self::get_json`] on a copy of the server with an empty results cache:
    /// the answer of a read that the cache does not serve.
    async fn get_json_cold(&self, uri: &str) -> anyhow::Result<Value> {
        let mut cold = self.state.clone();
        cold.plan_results_cache = Arc::default();
        let (key, user_id, key_id) = &self.owner;
        let resp = authed_user_get(&cold, key, user_id, key_id, uri).await?;
        anyhow::ensure!(resp.status() == StatusCode::OK, "{uri}: {}", resp.status());
        body_json(resp).await
    }

    async fn get_text(&self, uri: &str) -> anyhow::Result<String> {
        let resp = self.get(uri).await?;
        anyhow::ensure!(resp.status() == StatusCode::OK, "{uri}: {}", resp.status());
        Ok(String::from_utf8(
            resp.into_body().collect().await?.to_bytes().to_vec(),
        )?)
    }

    /// The plan's jobs page with each job's result, under `query`.
    fn results(&self, query: &str) -> String {
        format!("/api/v1/plans/{}/jobs?results=true{query}", self.plan_id)
    }

    async fn job_ids(&self) -> anyhow::Result<Vec<JobId>> {
        let body = self
            .get_json(&format!("/api/v1/plans/{}/jobs?all=true", self.plan_id))
            .await?;
        body.get("jobs")
            .and_then(Value::as_array)
            .context("a `jobs` array")?
            .iter()
            .map(|job| {
                let job_id = job
                    .get("job_id")
                    .and_then(Value::as_str)
                    .context("a job id")?;
                Ok(JobId::try_new(job_id)?)
            })
            .collect()
    }

    /// Take `job_id` out of the queue, holding a record of `message_type`, or
    /// none as teardown leaves it.
    async fn answer(&self, job_id: &JobId, message_type: Option<&str>) -> anyhow::Result<()> {
        self.state.todo_store.delete_avail_by_job(job_id).await?;
        if let Some(message_type) = message_type {
            self.state
                .submission_store
                .write_incoming(
                    job_id,
                    &json!({"job_id": job_id.as_str(), "message_type": message_type}),
                )
                .await?;
        }
        Ok(())
    }

    async fn job_id(&self) -> anyhow::Result<String> {
        let body = self
            .get_json(&format!("/api/v1/plans/{}/jobs", self.plan_id))
            .await?;
        body.pointer("/jobs/0/job_id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("the plan lists a job: {body}"))
    }
}

/// An `/api/query` body of one row of `job_id`: measured when `failed_reason`
/// is `None`.
fn rows_body(job_id: &str, failed_reason: Option<&str>) -> Value {
    query_body(vec![result_row(job_id, failed_reason)])
}

/// One result row of `job_id`, in the columns [`query_body`] names. A failure
/// is one its client reported.
fn result_row(job_id: &str, failed_reason: Option<&str>) -> Value {
    let (value, failed, source) = match failed_reason {
        None => (json!(42.5), false, Value::Null),
        Some(_) => (Value::Null, true, json!("client")),
    };
    json!([
        job_id,
        "prefill_throughput_256",
        "perf",
        "tokens_per_second",
        value,
        null,
        "tok/s",
        failed,
        failed_reason,
        null,
        null,
        "client-1",
        "Pixel 9",
        "2026-09-29T10:00:00.000000Z",
        "2026-09-29T10:05:00.000000Z",
        184_250,
        source
    ])
}

fn query_body(rows: Vec<Value>) -> Value {
    json!({
        "columns": ["job_id", "benchmark_id", "benchmark_type", "metric", "value", "value_stddev",
                    "unit", "failed", "failed_reason", "model_descriptor", "runtime_descriptor",
                    "client_id", "device_name", "submitted_at", "scored_at", "device_time_ms",
                    "failure_source"],
        "rowCount": rows.len(),
        "rows": rows,
    })
}

/// The query a jobs page sends, answered for `job_id` only.
async fn mock_rows<'a>(server: &'a MockServer, job_id: &str, body: Value) -> Mock<'a> {
    let named = format!("'{job_id}'");
    server
        .mock_async(|when, then| {
            when.method(POST)
                .path("/api/query")
                .query_param("db", "internal")
                .body_includes(named);
            then.status(200).json_body(body);
        })
        .await
}

/// The parameter counts on the first row of the plan's results file, its query
/// answered with `body`.
async fn csv_counts(
    filed: &Filed,
    server: &MockServer,
    body: Value,
) -> anyhow::Result<(String, String)> {
    let export = server
        .mock_async(|when, then| {
            when.method(POST)
                .path("/api/query")
                .body_includes(format!("WHERE plan_id = '{}'", filed.plan_id));
            then.status(200).json_body(body);
        })
        .await;
    let csv = filed
        .get_text(&format!("/api/v1/plans/{}/results.csv", filed.plan_id))
        .await?;
    export.delete_async().await;
    let mut reader = csv::Reader::from_reader(csv.as_bytes());
    let header = reader.headers()?.clone();
    let row = reader.records().next().context("a row")??;
    let cell = |name: &str| {
        header
            .iter()
            .position(|column| column == name)
            .and_then(|at| row.get(at))
            .map(str::to_string)
            .with_context(|| format!("no {name} in {csv}"))
    };
    Ok((
        cell("model_params_total_millions")?,
        cell("model_params_active_millions")?,
    ))
}

/// The page is the plan's job list with a `result` on each job. Without a
/// `[duckdb]` no job has a run, and the page says why.
#[tokio::test]
async fn test_results_are_the_job_list_with_a_result_each() -> anyhow::Result<()> {
    let filed = filed(None, true).await?;

    let listed = filed
        .get_json(&format!("/api/v1/plans/{}/jobs", filed.plan_id))
        .await?;
    crate::api_schema::assert_conforms("NamedPlanJobs", &listed)?;
    let mut body = filed.get_json(&filed.results("")).await?;
    crate::api_schema::assert_conforms("NamedPlanJobs", &body)?;
    let page = body.as_object_mut().context("the page is an object")?;
    assert_eq!(page.remove("results_read"), Some(json!("off")));
    for key in [
        "rerun_campaign_id",
        "may_cancel",
        "cancel_permission",
        "may_edit_note",
        "document_state",
        "names",
    ] {
        page.remove(key);
    }
    for job in page
        .get_mut("jobs")
        .and_then(Value::as_array_mut)
        .context("the page lists jobs")?
    {
        let job = job.as_object_mut().context("a job is an object")?;
        assert_eq!(job.remove("result"), Some(json!({"status": "not_run"})));
        ["planned_runtime_sha256", "planned_model_sha256"]
            .into_iter()
            .try_for_each(|key| {
                let digest = job
                    .remove(key)
                    .with_context(|| format!("a planned job carries {key}"))?;
                assert!(
                    digest
                        .as_str()
                        .is_some_and(pipette_store::is_content_digest),
                    "{key}: {digest} is a content digest"
                );
                Ok::<_, anyhow::Error>(())
            })?;
    }
    assert_eq!(body, listed);
    Ok(())
}

#[tokio::test]
async fn test_jobs_carry_their_result() -> anyhow::Result<()> {
    let server = MockServer::start_async().await;
    let filed = filed(Some(&server), true).await?;
    let job_id = filed.job_id().await?;
    let rows = mock_rows(&server, &job_id, rows_body(&job_id, None)).await;

    let body = filed.get_json(&filed.results("")).await?;
    crate::api_schema::assert_conforms("NamedPlanJobs", &body)?;
    let expected = [
        ("/plan_id", json!(filed.plan_id.as_str())),
        ("/results_read", json!("ok")),
        ("/total", json!(1)),
        ("/jobs/0/job_id", json!(job_id)),
        ("/jobs/0/result/status", json!("scored")),
        ("/jobs/0/result/rows/0/failed", json!(false)),
        ("/jobs/0/result/rows/0/metric", json!("tokens_per_second")),
        ("/jobs/0/result/rows/0/value", json!(42.5)),
        (
            "/jobs/0/result/rows/0/scored_at",
            json!("2026-09-29T10:05:00Z"),
        ),
        ("/jobs/0/result/rows/0/device_time_ms", json!(184_250)),
        // A row without the column reads as `null`.
        ("/jobs/0/result/rows/0/score_runtime_version", Value::Null),
    ];
    expected
        .iter()
        .for_each(|(path, want)| assert_eq!(body.pointer(path), Some(want), "{path}: {body}"));
    rows.assert_async().await;
    Ok(())
}

/// A job whose run pipette-duckdb holds is settled by it, its record unread:
/// this one left the queue with none, which alone would read `cancelled`. What
/// the run reports is its result's, beside the plan cell.
#[tokio::test]
async fn test_a_run_settles_its_job_without_its_record() -> anyhow::Result<()> {
    let server = MockServer::start_async().await;
    let filed = filed(Some(&server), true).await?;
    let job_id = filed.job_id().await?;
    filed
        .state
        .todo_store
        .delete_avail_by_job(&JobId::try_new(job_id.clone())?)
        .await?;
    let run = json!({
        "columns": ["job_id", "benchmark_id", "benchmark_type", "metric", "value", "value_stddev",
                    "unit", "failed", "failed_reason", "model_descriptor", "runtime_descriptor",
                    "client_id", "device_name", "submitted_at", "scored_at", "failure_source",
                    "rescored_from"],
        "rowCount": 1,
        "rows": [[job_id, "prefill_throughput_256", "prefill_throughput",
                  "tokens_per_second", 42.5, null, "tok/s", false, null,
                  "{\"org\":\"LiquidAI\",\"repo_name\":\"LFM2.5-350M-MLX-4bit\",\"source\":\"huggingface\",\"type\":\"mlx\"}",
                  "{\"type\":\"mlx_macos_pipette\"}", "ev1_runner", "Pixel 9",
                  "2026-09-29T10:00:00.000000Z", "2026-09-29T10:05:00.000000Z", null,
                  "job-parent"]],
    });
    mock_rows(&server, &job_id, run).await;

    let plain = filed
        .get_json(&format!("/api/v1/plans/{}/jobs", filed.plan_id))
        .await?;
    assert_eq!(plain["jobs"][0]["state"], "finished", "{plain}");
    for uri in [filed.results(""), filed.results("&all=true")] {
        let page = filed.get_json(&uri).await?;
        let job = &page["jobs"][0];
        assert_eq!(job["state"], "finished", "{uri}: {page}");
        assert_eq!(
            job["planned"]["benchmark"], "prefill_throughput_256",
            "{uri}"
        );
        let result = &job["result"];
        // A parent outside any plan of the campaign resolves to none.
        assert_eq!(
            result["rescored_from"],
            json!({"job_id": "job-parent", "plan_id": null}),
            "{uri}"
        );
        let row = &result["rows"][0];
        assert_eq!(
            row["model_descriptor"],
            json!({"org": "LiquidAI", "repo_name": "LFM2.5-350M-MLX-4bit",
                   "source": "huggingface", "type": "mlx"}),
            "{uri}"
        );
        // A shape this build does not read is wrapped, as stored.
        assert_eq!(
            row["runtime_descriptor"],
            json!({"type": "unknown", "value": {"type": "mlx_macos_pipette"}}),
            "{uri}"
        );
        assert_eq!(row["client_id"], "ev1_runner", "{uri}");
    }
    Ok(())
}

/// Each job's `result.status` on `body`, by job id.
fn statuses(body: &Value) -> anyhow::Result<Vec<(String, String, String)>> {
    body.get("jobs")
        .and_then(Value::as_array)
        .context("a `jobs` array")?
        .iter()
        .map(|job| {
            let field = |path: &str| {
                job.pointer(path)
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .with_context(|| format!("{path} on {job}"))
            };
            Ok((
                field("/job_id")?,
                field("/state")?,
                field("/result/status")?,
            ))
        })
        .collect()
}

async fn edit_manifest(filed: &Filed, edit: impl FnOnce(&mut PlanManifest)) -> anyhow::Result<()> {
    let mut manifest = filed
        .state
        .plan_store
        .get_plan(&filed.plan_id)
        .await?
        .context("the manifest was written")?;
    edit(&mut manifest);
    filed.state.plan_store.put_plan(&manifest).await
}

/// With the runs read, a job out of the queue with no run is settled by the
/// plan, its record unread: `unknown` on an active plan, though a record says
/// it finished, and `cancelled` once the plan is.
#[tokio::test]
async fn test_a_job_without_a_run_is_settled_by_its_plan() -> anyhow::Result<()> {
    let server = MockServer::start_async().await;
    let filed = filed_from(Some(&server), true, THREE_JOB_PLAN).await?;
    let job_ids = filed.job_ids().await?;
    let [answered, retired, queued] = job_ids.as_slice() else {
        anyhow::bail!("three jobs, got {job_ids:?}");
    };
    filed.answer(answered, Some("success")).await?;
    filed.answer(retired, None).await?;
    server
        .mock_async(|when, then| {
            when.method(POST).path("/api/query");
            then.status(200).json_body(query_body(Vec::new()));
        })
        .await;
    let entry = |job_id: &JobId, state: &str, status: &str| {
        (job_id.to_string(), state.to_owned(), status.to_owned())
    };

    let body = filed.get_json(&filed.results("&all=true")).await?;
    crate::api_schema::assert_conforms("NamedPlanJobs", &body)?;
    assert_eq!(body["results_read"], "ok", "{body}");
    assert_eq!(
        statuses(&body)?,
        [
            entry(answered, "unknown", "unknown"),
            entry(retired, "unknown", "unknown"),
            entry(queued, "available", "not_run"),
        ]
    );

    edit_manifest(&filed, |manifest| manifest.status = PlanStatus::Cancelled).await?;
    let body = filed.get_json(&filed.results("&all=true")).await?;
    assert_eq!(
        statuses(&body)?,
        [
            entry(answered, "cancelled", "not_run"),
            entry(retired, "cancelled", "not_run"),
            entry(queued, "available", "not_run"),
        ]
    );
    Ok(())
}

/// A job the plan records an outcome for reads as that outcome before its run
/// is published, where a complete plan would otherwise call it `cancelled`.
/// `unrecorded` is the state and result of a job the plan records nothing for.
#[rstest]
#[case::complete(PlanStatus::Complete, ("cancelled", "not_run"))]
#[case::active(PlanStatus::Active, ("unknown", "unknown"))]
fn test_a_recorded_outcome_settles_a_job_without_a_run(
    #[case] status: PlanStatus,
    #[case] unrecorded: (&'static str, &'static str),
) -> anyhow::Result<()> {
    run_mocked(assert_a_recorded_outcome_settles(status, unrecorded))
}

async fn assert_a_recorded_outcome_settles(
    status: PlanStatus,
    unrecorded: (&str, &str),
) -> anyhow::Result<()> {
    let server = MockServer::start_async().await;
    let filed = filed_from(Some(&server), true, THREE_JOB_PLAN).await?;
    let job_ids = filed.job_ids().await?;
    let [finished, failed, retired] = job_ids.as_slice() else {
        anyhow::bail!("three jobs, got {job_ids:?}");
    };
    filed.answer(finished, Some("success")).await?;
    filed.answer(failed, Some("failure")).await?;
    filed.answer(retired, None).await?;
    edit_manifest(&filed, |manifest| {
        manifest.status = status;
        manifest.settled_jobs = [
            (finished.clone(), SettledOutcome::Finished),
            (failed.clone(), SettledOutcome::Failed),
        ]
        .into();
    })
    .await?;
    server
        .mock_async(|when, then| {
            when.method(POST).path("/api/query");
            then.status(200).json_body(query_body(Vec::new()));
        })
        .await;

    let body = filed.get_json(&filed.results("&all=true")).await?;
    crate::api_schema::assert_conforms("NamedPlanJobs", &body)?;
    let entry = |job_id: &JobId, state: &str, status: &str| {
        (job_id.to_string(), state.to_owned(), status.to_owned())
    };
    assert_eq!(
        statuses(&body)?,
        [
            entry(finished, "finished", "unknown"),
            entry(failed, "failed", "unknown"),
            entry(retired, unrecorded.0, unrecorded.1),
        ]
    );
    Ok(())
}

/// A failed read settles nothing: on a complete plan a job with no run read
/// stays `unknown` rather than `cancelled`, its record unread.
#[tokio::test]
async fn test_a_failed_read_leaves_a_job_unknown() -> anyhow::Result<()> {
    let server = MockServer::start_async().await;
    let filed = filed(Some(&server), true).await?;
    let job_id = JobId::try_new(filed.job_id().await?)?;
    filed.answer(&job_id, Some("success")).await?;
    edit_manifest(&filed, |manifest| manifest.status = PlanStatus::Complete).await?;
    server
        .mock_async(|when, then| {
            when.method(POST).path("/api/query");
            then.status(500).body("boom");
        })
        .await;

    let body = filed.get_json(&filed.results("")).await?;
    assert_eq!(body["results_read"], "failed", "{body}");
    assert_eq!(
        statuses(&body)?,
        [(job_id.to_string(), "unknown".into(), "unknown".into())]
    );
    Ok(())
}

/// Without a `[duckdb]` no run is read, so a job answered, or lapsed for this
/// plan, may have one the page cannot show; a queued one has none.
#[tokio::test]
async fn test_unread_runs_leave_an_answer_unknown() -> anyhow::Result<()> {
    let filed = filed_from(None, true, THREE_JOB_PLAN).await?;
    let job_ids = filed.job_ids().await?;
    let [answered, lapsed, queued] = job_ids.as_slice() else {
        anyhow::bail!("three jobs, got {job_ids:?}");
    };
    filed.answer(answered, Some("failure")).await?;
    edit_manifest(&filed, |manifest| {
        manifest.expired_jobs.insert(
            lapsed.clone(),
            ExpiredJob {
                horizon: chrono::Utc::now() - chrono::Duration::hours(2),
                recorded_at: chrono::Utc::now(),
            },
        );
    })
    .await?;

    let body = filed.get_json(&filed.results("&all=true")).await?;
    assert_eq!(body["results_read"], "off", "{body}");
    let entry = |job_id: &JobId, state: &str, status: &str| {
        (job_id.to_string(), state.to_owned(), status.to_owned())
    };
    assert_eq!(
        statuses(&body)?,
        [
            entry(answered, "failed", "unknown"),
            entry(lapsed, "failed", "unknown"),
            entry(queued, "available", "not_run"),
        ]
    );
    Ok(())
}

/// Without a `[duckdb]` the outcome the plan records settles a job ahead of its
/// record: with no record left to read, the record alone would say `cancelled`.
#[tokio::test]
async fn test_a_recorded_outcome_settles_ahead_of_the_record() -> anyhow::Result<()> {
    let filed = filed_from(None, true, THREE_JOB_PLAN).await?;
    let job_ids = filed.job_ids().await?;
    let [recorded, unrecorded, queued] = job_ids.as_slice() else {
        anyhow::bail!("three jobs, got {job_ids:?}");
    };
    filed.answer(recorded, None).await?;
    filed.answer(unrecorded, None).await?;
    edit_manifest(&filed, |manifest| {
        manifest.settled_jobs = [(recorded.clone(), SettledOutcome::Finished)].into();
    })
    .await?;

    let body = filed.get_json(&filed.results("&all=true")).await?;
    assert_eq!(body["results_read"], "off", "{body}");
    let entry = |job_id: &JobId, state: &str, status: &str| {
        (job_id.to_string(), state.to_owned(), status.to_owned())
    };
    assert_eq!(
        statuses(&body)?,
        [
            entry(recorded, "finished", "unknown"),
            entry(unrecorded, "cancelled", "not_run"),
            entry(queued, "available", "not_run"),
        ]
    );
    Ok(())
}

/// `all=true` reads the whole plan's results in one query that names every
/// job.
#[tokio::test]
async fn test_all_reads_every_jobs_results_at_once() -> anyhow::Result<()> {
    let server = MockServer::start_async().await;
    let filed = filed_from(Some(&server), true, THREE_JOB_PLAN).await?;
    let job_ids = filed.job_ids().await?;
    let rows: Vec<Value> = job_ids
        .iter()
        .map(|job_id| result_row(job_id.as_str(), None))
        .collect();
    let named: Vec<String> = job_ids.iter().map(|job_id| format!("'{job_id}'")).collect();
    let query = server
        .mock_async(|when, then| {
            let when = when.method(POST).path("/api/query");
            named
                .into_iter()
                .fold(when, |when, id| when.body_includes(id));
            then.status(200).json_body(query_body(rows));
        })
        .await;

    let body = filed.get_json(&filed.results("&all=true&limit=1")).await?;
    crate::api_schema::assert_conforms("NamedPlanJobs", &body)?;
    assert_eq!((&body["offset"], &body["limit"]), (&json!(0), &json!(3)));
    let scored = statuses(&body)?
        .into_iter()
        .filter(|(_, _, status)| status == "scored")
        .count();
    assert_eq!(scored, 3, "{body}");
    query.assert_async().await;
    Ok(())
}

/// The page lists the plan's jobs in its document's order, the models as
/// written, and reads results for that window alone.
#[tokio::test]
async fn test_results_page_the_document_order_for_the_window_alone() -> anyhow::Result<()> {
    let server = MockServer::start_async().await;
    let filed = filed_from(Some(&server), true, THREE_JOB_PLAN).await?;
    let all = filed
        .get_json(&format!("/api/v1/plans/{}/jobs?all=true", filed.plan_id))
        .await?;
    let listed = |field: &str| -> anyhow::Result<Vec<String>> {
        all["jobs"]
            .as_array()
            .context("a `jobs` array")?
            .iter()
            .map(|job| {
                job.pointer(field)
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .with_context(|| format!("{field} on {job}"))
            })
            .collect()
    };
    assert_eq!(
        listed("/planned/model/repo_name")?,
        [
            "LFM2.5-350M-MLX-4bit",
            "LFM2-700M-MLX-4bit",
            "LFM2-1.2B-MLX-4bit"
        ]
    );
    let job_ids = listed("/job_id")?;
    let [skipped, second, third] = job_ids.as_slice() else {
        anyhow::bail!("the plan holds three jobs: {job_ids:?}");
    };
    let window = server
        .mock_async(|when, then| {
            when.method(POST)
                .path("/api/query")
                .query_param("db", "internal")
                .body_includes(format!("'{second}'"))
                .body_includes(format!("'{third}'"))
                .body_excludes(skipped.as_str());
            then.status(200).json_body(query_body(vec![
                result_row(second, None),
                result_row(third, None),
            ]));
        })
        .await;

    let uri = filed.results("&limit=2&offset=1");
    let body = filed.get_json(&uri).await?;
    assert_eq!(
        (&body["total"], &body["offset"], &body["limit"]),
        (&json!(3), &json!(1), &json!(2)),
        "{uri}"
    );
    assert_eq!(body["results_read"], "ok", "{uri}: {body}");
    let jobs = body["jobs"].as_array().context("a `jobs` array")?;
    let listed: Vec<&Value> = jobs.iter().map(|job| &job["job_id"]).collect();
    assert_eq!(listed, [second, third], "{uri}");
    jobs.iter()
        .for_each(|job| assert_eq!(job["result"]["status"], "scored", "{uri}: {job}"));
    window.assert_async().await;

    // A zero limit answers the count alone, and one past the ceiling is clamped.
    for (query, limit, listed) in [("&limit=0", 0, 0), ("&limit=9999", 500, 3)] {
        let uri = filed.results(query);
        let body = filed.get_json(&uri).await?;
        assert_eq!(body["total"], 3, "{uri}");
        assert_eq!(body["limit"], limit, "{uri}");
        assert_eq!(body["jobs"].as_array().map(Vec::len), Some(listed), "{uri}");
    }
    Ok(())
}

/// A client's failure reason is answered whole, and only to a reader of the
/// fleet, since it can list client ids. Its source is named there alone too.
async fn assert_reason_at_tier(detail: bool, tier: &str) -> anyhow::Result<()> {
    let server = MockServer::start_async().await;
    let filed = filed(Some(&server), detail).await?;
    let job_id = filed.job_id().await?;
    let reason = format!("{}out of memory", "x".repeat(8192));
    mock_rows(&server, &job_id, rows_body(&job_id, Some(&reason))).await;

    let body = filed.get_json(&filed.results("")).await?;
    crate::api_schema::assert_conforms("NamedPlanJobs", &body)?;
    let shown = |value: Value| if detail { value } else { Value::Null };
    assert_eq!(body.pointer("/fleet_detail"), Some(&json!(tier)));
    assert_eq!(
        body.pointer("/jobs/0/result/rows/0/failed"),
        Some(&json!(true))
    );
    assert_eq!(
        body.pointer("/jobs/0/result/rows/0/failed_reason"),
        Some(&shown(json!(reason)))
    );
    assert_eq!(
        body.pointer("/jobs/0/result/rows/0/failure_source"),
        Some(&shown(json!("client")))
    );
    Ok(())
}

#[tokio::test]
async fn test_a_failed_jobs_reason_is_answered_whole_at_read_clients() -> anyhow::Result<()> {
    assert_reason_at_tier(true, "full").await
}

#[tokio::test]
async fn test_a_failed_jobs_reason_is_withheld_below_read_clients() -> anyhow::Result<()> {
    assert_reason_at_tier(false, "withheld").await
}

/// An organization's administrator holding `manage_orgs` alone reads the team's
/// campaign but none of its plans: not the plan it points at, its status or its
/// jobs.
#[tokio::test]
async fn test_manage_orgs_alone_is_refused_every_plan_read_of_the_campaign() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, _client_id) = approved_client(dir.path()).await?;
    let org = seed_org(&state, "acme").await?;
    let author = registered_user(
        &state,
        &[UserPermission::SubmitPlans, UserPermission::ManageOrgs],
    )
    .await?;
    set_user_org(&state, &author.1, Some(org.org_id.clone())).await?;
    let (campaign_id, plan_id) = created_campaign(
        &state,
        &author,
        &[("name", "Team nightly"), ("owner", org.org_id.as_str())],
    )
    .await?;
    let (key, user_id, key_id) = &registered_user(&state, &[UserPermission::ManageOrgs]).await?;
    set_user_org(&state, user_id, Some(org.org_id.clone())).await?;

    let resp = authed_user_get(
        &state,
        key,
        user_id,
        key_id,
        &format!("/api/v1/campaigns/{campaign_id}"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK, "the campaign is the team's");

    // The export is plan_pages.rs's, which also proves duckdb is not asked.
    let uris = [
        format!("/api/v1/campaigns/{campaign_id}/plan"),
        format!("/api/v1/plans/{plan_id}"),
        format!("/api/v1/plans/{plan_id}/jobs?results=true"),
    ];
    for uri in &uris {
        let resp = authed_user_get(&state, key, user_id, key_id, uri).await?;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{uri}");
        assert_eq!(
            body_json(resp).await?["error"],
            "user lacks the read_plans permission",
            "{uri}"
        );
    }

    // Granted `read_plans`, the same account reads them, so the refusal was the grant.
    grant_user(
        &state,
        user_id,
        &[UserPermission::ManageOrgs, UserPermission::ReadPlans],
    )
    .await?;
    for uri in &uris {
        let resp = authed_user_get(&state, key, user_id, key_id, uri).await?;
        assert_eq!(resp.status(), StatusCode::OK, "{uri}");
        assert_eq!(body_json(resp).await?["plan_id"], plan_id.as_str(), "{uri}");
    }
    Ok(())
}

/// A failing pipette-duckdb costs the page its results, not its jobs.
#[tokio::test]
async fn test_jobs_answer_without_results_when_duckdb_fails() -> anyhow::Result<()> {
    let server = MockServer::start_async().await;
    let filed = filed(Some(&server), true).await?;
    let job_id = filed.job_id().await?;
    server
        .mock_async(|when, then| {
            when.method(POST).path("/api/query");
            then.status(400)
                .json_body(json!({"error": "Catalog Error: Table warehouse does not exist"}));
        })
        .await;

    let body = filed.get_json(&filed.results("")).await?;
    crate::api_schema::assert_conforms("NamedPlanJobs", &body)?;
    assert_eq!(body["results_read"], "failed");
    assert_eq!(body["jobs"][0]["job_id"], job_id.as_str());
    // Still queued, so it has no run to miss.
    assert_eq!(
        body["jobs"][0]["result"],
        json!({"status": "not_run"}),
        "{body}"
    );
    Ok(())
}

/// A hung pipette-duckdb holds the page for [`JOB_RESULTS_TIMEOUT`], not for
/// the testkit's [`DUCKDB_TIMEOUT`].
#[tokio::test]
async fn test_jobs_answer_without_results_when_duckdb_hangs() -> anyhow::Result<()> {
    let server = MockServer::start_async().await;
    let filed = filed(Some(&server), true).await?;
    let job_id = filed.job_id().await?;
    server
        .mock_async(|when, then| {
            when.method(POST).path("/api/query");
            then.delay(Duration::from_secs(30)).status(200);
        })
        .await;

    let started = Instant::now();
    let body = filed.get_json(&filed.results("")).await?;
    let waited = started.elapsed();
    assert!(
        waited >= JOB_RESULTS_TIMEOUT && waited < DUCKDB_TIMEOUT,
        "{waited:?}"
    );
    crate::api_schema::assert_conforms("NamedPlanJobs", &body)?;
    assert_eq!(body["results_read"], "timed_out");
    assert_eq!(body["jobs"].as_array().map(Vec::len), Some(1), "{body}");
    assert_eq!(body["jobs"][0]["job_id"], job_id.as_str());
    assert_eq!(
        body["jobs"][0]["result"],
        json!({"status": "not_run"}),
        "{body}"
    );
    Ok(())
}

/// A row the warehouse holds no parameter count for reads it from mgmt's
/// model-params rows, matched on either descriptor, on the page and in the
/// results file; a stored count is kept, and a failed read of the rows leaves
/// the page as the warehouse has it.
#[rstest]
#[case::its_descriptor(
    "model_descriptor",
    r#"{"org":"LiquidAI","repo_name":"LFM2.5-350M-MLX-4bit","source":"huggingface","type":"mlx"}"#,
    "lfm2.5-350m"
)]
#[case::its_vault_descriptor(
    "model_vault_descriptor",
    r#"{"type":"gguf_text","source":"vault_url","url":"/vault/resources/vault_0199aaaa-bbbb-7ccc-8ddd-000000000001/content","principal_id":"org_1"}"#,
    "vault_0199aaaa-bbbb-7ccc-8ddd-000000000001"
)]
fn test_an_empty_parameter_count_reads_from_model_params(
    #[case] column: &str,
    #[case] descriptor: &str,
    #[case] key: &str,
) -> anyhow::Result<()> {
    run_mocked(assert_an_empty_count_is_filled(column, descriptor, key))
}

async fn assert_an_empty_count_is_filled(
    column: &str,
    descriptor: &str,
    key: &str,
) -> anyhow::Result<()> {
    let server = MockServer::start_async().await;
    let mut filed = filed(Some(&server), true).await?;
    let job_id = filed.job_id().await?;
    let run = |total: Value| {
        json!({
            "columns": ["job_id", "benchmark_id", "benchmark_type", "metric", "value",
                        "value_stddev", "unit", "failed", column,
                        "model_params_total_millions", "model_params_active_millions"],
            "rowCount": 1,
            "rows": [[job_id, "prefill_throughput_256", "prefill_throughput",
                      "tokens_per_second", 42.5, null, "tok/s", false, descriptor,
                      total, total]],
        })
    };
    let count = |page: &Value, which: &str| {
        page.pointer(&format!(
            "/jobs/0/result/rows/0/model_params_{which}_millions"
        ))
        .cloned()
    };
    let total = |page: &Value| count(page, "total");

    let empty = mock_rows(&server, &job_id, run(Value::Null)).await;
    assert_eq!(
        total(&filed.get_json(&filed.results("")).await?),
        Some(Value::Null)
    );

    filed
        .state
        .model_params_store
        .put_model_params(&pipette_store_testkit::a_model_params(key, 354)?)
        .await?;
    for uri in [filed.results(""), filed.results("&all=true")] {
        let page = filed.get_json(&uri).await?;
        assert_eq!(total(&page), Some(json!(354)), "{uri}: {page}");
        assert_eq!(count(&page, "active"), Some(json!(354)), "{uri}");
    }
    assert_eq!(
        csv_counts(&filed, &server, run(Value::Null)).await?,
        ("354".to_owned(), "354".to_owned())
    );

    empty.delete_async().await;
    let stored = mock_rows(&server, &job_id, run(json!(350))).await;
    assert_eq!(
        total(&filed.get_json(&filed.results("")).await?),
        Some(json!(350)),
        "a stored count is kept"
    );
    assert_eq!(
        csv_counts(&filed, &server, run(json!(350))).await?,
        ("350".to_owned(), "350".to_owned()),
        "a stored count is kept in the file"
    );

    stored.delete_async().await;
    filed.state.model_params_store =
        std::sync::Arc::new(pipette_store_testkit::ModelParamsStoreUnimplemented);
    mock_rows(&server, &job_id, run(Value::Null)).await;
    assert_eq!(
        total(&filed.get_json(&filed.results("")).await?),
        Some(Value::Null),
        "the page answers without the rows"
    );
    Ok(())
}

/// The plan page names each runtime digest that it shows, as its reader sees
/// the catalog: the planned runtime's and the reported runtime's. The results
/// file carries the reported runtime's name, and leaves the cell of a runtime
/// with no name empty. Runs on the SQL store only, so it skips without one.
#[tokio::test]
async fn test_runtime_digests_carry_their_catalog_names() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(sql) = seeded_sql_state(dir.path()).await? else {
        return Ok(());
    };
    let duckdb = MockServer::start_async().await;
    let state = with_duckdb(sql.state.clone(), &duckdb.base_url())?;
    register_and_approve(&state).await?;
    let acme = seed_org(&state, "acme").await?;
    let beta = seed_org(&state, "beta").await?;
    let owner = registered_user_with_detail(&state, true).await?;
    set_user_org(&state, &owner.1, Some(acme.org_id.clone())).await?;
    let outsider = registered_user(&state, &[UserPermission::ReadAnyPlans]).await?;
    set_user_org(&state, &outsider.1, Some(beta.org_id.clone())).await?;
    let (_, plan_id) =
        created_campaign_from(&state, &owner, &[("name", "Nightly")], ONE_CELL_PLAN).await?;

    // The plan's runtime has a global name and an `acme` name; the runtime
    // that the row reports has a global name.
    let catalog = state
        .descriptor_catalog_store
        .clone()
        .context("the SQL state keeps the catalog")?;
    let planned = DescriptorMember::new(
        serde_json::from_value(json!({
            "type": "mlx_macos_pipette", "version": "0.31.3", "flavor": "macos-arm64",
            "source": {"type": "pip_requirements_text", "contents": "mlx-lm==0.31.3\n"},
        }))?,
        chrono::Utc::now(),
    )?;
    let planned_digest = planned.digest().to_owned();
    catalog
        .create_entry(&a_descriptor_entry(
            "Mlx-Stable-1",
            DescriptorEntryKind::Stable,
            100,
            planned.clone(),
        )?)
        .await?;
    let mut own = a_descriptor_entry("Acme-Mlx-1", DescriptorEntryKind::HandMade, 200, planned)?;
    own.scope = CatalogScope::Org(acme.org_id.clone());
    catalog.create_entry(&own).await?;
    let reported = a_member("b11669", "macos-arm64")?;
    let reported_digest = reported.digest().to_owned();
    let reported_text = serde_json::to_string(reported.runtime().context("runtime member")?)?;
    catalog
        .create_entry(&a_descriptor_entry(
            "Llama-Row-1",
            DescriptorEntryKind::Stable,
            300,
            reported,
        )?)
        .await?;

    let page_uri = format!("/api/v1/plans/{plan_id}/jobs?results=true");
    let (key, user_id, key_id) = &owner;
    let listed = body_json(authed_user_get(&state, key, user_id, key_id, &page_uri).await?).await?;
    let job_id = listed
        .pointer("/jobs/0/job_id")
        .and_then(Value::as_str)
        .context("the plan lists a job")?
        .to_owned();
    let row = |metric: &str, digest: &str| {
        json!([
            &job_id,
            "prefill_throughput_256",
            "perf",
            metric,
            42.5,
            "tok/s",
            false,
            &reported_text,
            digest,
            "2026-09-29T10:05:00.000000Z"
        ])
    };
    duckdb
        .mock_async(|when, then| {
            when.method(POST).path("/api/query");
            then.status(200).json_body(json!({
                "columns": ["job_id", "benchmark_id", "benchmark_type", "metric", "value", "unit",
                            "failed", "runtime_descriptor", "runtime_descriptor_sha256",
                            "scored_at"],
                "rowCount": 2,
                "rows": [row("tokens_per_second", &reported_digest), row("ttft", &"cd".repeat(32))],
            }));
        })
        .await;

    // Each reader sees the planned runtime under its own org's name first.
    let cases = [
        (
            &owner,
            "Acme-Mlx-1::macos-arm64",
            json!(["Mlx-Stable-1::macos-arm64"]),
        ),
        (&outsider, "Mlx-Stable-1::macos-arm64", json!([])),
    ];
    for (reader, display, synonyms) in cases {
        let (key, user_id, key_id) = reader;
        let resp = authed_user_get(&state, key, user_id, key_id, &page_uri).await?;
        anyhow::ensure!(
            resp.status() == StatusCode::OK,
            "{page_uri}: {}",
            resp.status()
        );
        let body = body_json(resp).await?;
        crate::api_schema::assert_conforms("NamedPlanJobs", &body)?;
        assert_eq!(
            body["jobs"][0]["planned_runtime_sha256"],
            json!(planned_digest)
        );
        assert_eq!(
            body["jobs"][0]["result"]["rows"][0]["runtime_descriptor_sha256"],
            json!(reported_digest)
        );
        let names = &body["descriptor_names"];
        assert_eq!(names[&planned_digest]["display"], json!(display), "{body}");
        assert_eq!(names[&planned_digest]["synonyms"], synonyms, "{body}");
        assert_eq!(
            names[&reported_digest]["display"],
            json!("Llama-Row-1::macos-arm64")
        );
        assert_eq!(names[&reported_digest]["name"], json!("Llama-Row-1"));
        assert!(names.get("cd".repeat(32)).is_none(), "{body}");
    }

    let csv_uri = format!("/api/v1/plans/{plan_id}/results.csv");
    let resp = authed_user_get(&state, key, user_id, key_id, &csv_uri).await?;
    anyhow::ensure!(
        resp.status() == StatusCode::OK,
        "{csv_uri}: {}",
        resp.status()
    );
    let csv = String::from_utf8(resp.into_body().collect().await?.to_bytes().to_vec())?;
    let mut reader = csv::Reader::from_reader(csv.as_bytes());
    let at = reader
        .headers()?
        .iter()
        .position(|column| column == "runtime_catalog_name")
        .with_context(|| format!("no runtime_catalog_name in {csv}"))?;
    let cells = reader
        .records()
        .map(|record| Ok(record?.get(at).unwrap_or_default().to_owned()))
        .collect::<anyhow::Result<Vec<_>>>()?;
    assert_eq!(cells, ["Llama-Row-1::macos-arm64", ""]);
    drop(sql);
    Ok(())
}

/// [`ONE_CELL_PLAN`] with a model that pins a commit, so a catalog model
/// member can name it.
const PINNED_MODEL_PLAN: &str = r#"
benchmarks = ["prefill_throughput_256"]

[[variants]]
requires = ["os:macos"]
models   = [{ type = "mlx", source = "huggingface", org = "acme", repo_name = "AcmeLM-3B-MLX-4bit", revision = "0123456789abcdef0123456789abcdef01234567" }]
runtimes = [{ type = "mlx_macos_pipette", version = "0.31.3", flavor = "macos-arm64", source = { type = "pip_requirements_text", contents = "mlx-lm==0.31.3\n" } }]
"#;

/// The plan page names each model digest that it shows: the planned model's
/// and the reported model's. Two members of one entry show different
/// selectors. The results file carries the reported model's name, and leaves
/// the cell of a model with no name empty. Runs on the SQL store only, so it
/// skips without one.
#[tokio::test]
async fn test_model_digests_carry_their_catalog_names() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(sql) = seeded_sql_state(dir.path()).await? else {
        return Ok(());
    };
    let duckdb = MockServer::start_async().await;
    let state = with_duckdb(sql.state.clone(), &duckdb.base_url())?;
    register_and_approve(&state).await?;
    let owner = registered_user_with_detail(&state, true).await?;
    let (_, plan_id) =
        created_campaign_from(&state, &owner, &[("name", "Nightly")], PINNED_MODEL_PLAN).await?;

    // One entry names the planned MLX model and the reported GGUF model.
    let catalog = state
        .descriptor_catalog_store
        .clone()
        .context("the SQL state keeps the catalog")?;
    let planned = a_model_member("mlx", "AcmeLM-3B-MLX-4bit", "4bit")?;
    let reported = a_model_member("gguf_text", "AcmeLM-3B-GGUF", "q4_0")?;
    let planned_digest = planned.digest().to_owned();
    let reported_digest = reported.digest().to_owned();
    let reported_text = serde_json::to_string(reported.model().context("a model member")?)?;
    let mut entry = a_descriptor_entry("AcmeLM-3B", DescriptorEntryKind::HandMade, 100, reported)?;
    entry.members = NonEmptyVec::try_new(vec![entry.members.head().clone(), planned])?;
    catalog.create_entry(&entry).await?;

    let page_uri = format!("/api/v1/plans/{plan_id}/jobs?results=true");
    let (key, user_id, key_id) = &owner;
    let listed = body_json(authed_user_get(&state, key, user_id, key_id, &page_uri).await?).await?;
    let job_id = listed
        .pointer("/jobs/0/job_id")
        .and_then(Value::as_str)
        .context("the plan lists a job")?
        .to_owned();
    let row = |metric: &str, digest: &str| {
        json!([
            &job_id,
            "prefill_throughput_256",
            "perf",
            metric,
            42.5,
            "tok/s",
            false,
            &reported_text,
            digest,
            "2026-09-29T10:05:00.000000Z"
        ])
    };
    duckdb
        .mock_async(|when, then| {
            when.method(POST).path("/api/query");
            then.status(200).json_body(json!({
                "columns": ["job_id", "benchmark_id", "benchmark_type", "metric", "value", "unit",
                            "failed", "model_descriptor", "model_descriptor_sha256", "scored_at"],
                "rowCount": 2,
                "rows": [row("tokens_per_second", &reported_digest), row("ttft", &"cd".repeat(32))],
            }));
        })
        .await;

    let resp = authed_user_get(&state, key, user_id, key_id, &page_uri).await?;
    anyhow::ensure!(
        resp.status() == StatusCode::OK,
        "{page_uri}: {}",
        resp.status()
    );
    let body = body_json(resp).await?;
    crate::api_schema::assert_conforms("NamedPlanJobs", &body)?;
    assert_eq!(
        body["jobs"][0]["planned_model_sha256"],
        json!(planned_digest)
    );
    assert_eq!(
        body["jobs"][0]["result"]["rows"][0]["model_descriptor_sha256"],
        json!(reported_digest)
    );
    let names = &body["descriptor_names"];
    assert_eq!(
        names[&planned_digest]["display"],
        json!("AcmeLM-3B::mlx::4bit"),
        "{body}"
    );
    assert_eq!(
        names[&reported_digest]["display"],
        json!("AcmeLM-3B::gguf::q4_0"),
        "{body}"
    );
    assert_eq!(names[&reported_digest]["name"], json!("AcmeLM-3B"));
    assert!(names.get("cd".repeat(32)).is_none(), "{body}");

    let csv_uri = format!("/api/v1/plans/{plan_id}/results.csv");
    let resp = authed_user_get(&state, key, user_id, key_id, &csv_uri).await?;
    anyhow::ensure!(
        resp.status() == StatusCode::OK,
        "{csv_uri}: {}",
        resp.status()
    );
    let csv = String::from_utf8(resp.into_body().collect().await?.to_bytes().to_vec())?;
    let mut reader = csv::Reader::from_reader(csv.as_bytes());
    let at = reader
        .headers()?
        .iter()
        .position(|column| column == "model_catalog_name")
        .with_context(|| format!("no model_catalog_name in {csv}"))?;
    let cells = reader
        .records()
        .map(|record| Ok(record?.get(at).unwrap_or_default().to_owned()))
        .collect::<anyhow::Result<Vec<_>>>()?;
    assert_eq!(cells, ["AcmeLM-3B::gguf::q4_0", ""]);
    drop(sql);
    Ok(())
}

/// [`filed`] at the full tier, its job answered and the plan set to `status`.
async fn answered(server: &MockServer, status: PlanStatus) -> anyhow::Result<(Filed, String)> {
    let filed = filed(Some(server), true).await?;
    let job_id = filed.job_id().await?;
    filed
        .answer(&JobId::try_new(job_id.as_str())?, Some("success"))
        .await?;
    edit_manifest(&filed, |manifest| manifest.status = status).await?;
    Ok((filed, job_id))
}

/// An `all` read of a complete plan fills the results cache, and a read after
/// it makes no query. A page read uses a kept plan, but never fills the cache.
/// The cache keeps only complete plans.
#[rstest]
#[case::all_then_all(PlanStatus::Complete, "&all=true", "&all=true", 1)]
#[case::all_then_page(PlanStatus::Complete, "&all=true", "", 1)]
#[case::page_then_all(PlanStatus::Complete, "", "&all=true", 2)]
#[case::page_then_page(PlanStatus::Complete, "", "", 2)]
#[case::active(PlanStatus::Active, "&all=true", "&all=true", 2)]
#[tokio::test]
async fn test_a_complete_plans_results_are_kept(
    #[case] status: PlanStatus,
    #[case] first: &str,
    #[case] then: &str,
    #[case] queries: usize,
) -> anyhow::Result<()> {
    let server = MockServer::start_async().await;
    let (filed, job_id) = answered(&server, status).await?;
    let query = mock_rows(&server, &job_id, rows_body(&job_id, None)).await;

    for query in [first, then] {
        let body = filed.get_json(&filed.results(query)).await?;
        assert_eq!(body["results_read"], "ok", "{body}");
        assert_eq!(
            statuses(&body)?,
            [(job_id.clone(), "finished".into(), "scored".into())]
        );
    }
    assert_eq!(query.calls_async().await, queries);
    Ok(())
}

/// A kept plan still answers its jobs after the independent cell cache is
/// cleared and the stored job source is gone.
#[tokio::test]
async fn test_a_kept_plan_does_not_read_job_sources_again() -> anyhow::Result<()> {
    let server = MockServer::start_async().await;
    let (mut filed, job_id) = answered(&server, PlanStatus::Complete).await?;
    let query = mock_rows(&server, &job_id, rows_body(&job_id, None)).await;
    let uri = filed.results("&all=true");
    let first = filed.get_json(&uri).await?;

    let source = filed
        ._dir
        .path()
        .join("job-sources")
        .join(format!("{job_id}.json"));
    std::fs::remove_file(&source)?;
    filed.state.cell_cache = Arc::default();

    let kept = filed.get_json(&uri).await?;
    assert_eq!(kept, first);
    assert_eq!(query.calls_async().await, 1);
    let cold = filed.get_json_cold(&uri).await?;
    assert_ne!(
        cold, first,
        "the missing job source must affect a cold read"
    );
    Ok(())
}

/// Concurrent `all` reads of a complete plan wait on one query.
#[tokio::test]
async fn test_concurrent_cold_reads_make_one_query() -> anyhow::Result<()> {
    let server = MockServer::start_async().await;
    let (filed, job_id) = answered(&server, PlanStatus::Complete).await?;
    let body = rows_body(&job_id, None);
    let query = server
        .mock_async(|when, then| {
            when.method(POST).path("/api/query");
            then.delay(Duration::from_millis(500))
                .status(200)
                .json_body(body);
        })
        .await;

    let uri = filed.results("&all=true");
    let bodies = futures::future::try_join_all((0..4).map(|_| filed.get_json(&uri))).await?;
    assert_eq!(bodies[0]["results_read"], "ok", "{}", bodies[0]);
    assert!(bodies.iter().all(|body| body == &bodies[0]));
    assert_eq!(query.calls_async().await, 1);
    Ok(())
}

/// A complete plan's queue read starts before a cold results fill finishes.
#[tokio::test]
async fn test_cold_complete_read_starts_queue_beside_results() -> anyhow::Result<()> {
    let server = MockServer::start_async().await;
    let (mut filed, job_id) = answered(&server, PlanStatus::Complete).await?;
    let job_id = JobId::try_new(job_id.as_str())?;
    edit_manifest(&filed, |manifest| {
        manifest.expired_jobs.insert(
            job_id.clone(),
            ExpiredJob {
                horizon: chrono::Utc::now() - chrono::Duration::hours(2),
                recorded_at: chrono::Utc::now(),
            },
        );
    })
    .await?;
    filed.state.todo_store = Arc::new(TodoStoreUnimplemented);
    let _query = server
        .mock_async(|when, then| {
            when.method(POST).path("/api/query");
            then.delay(Duration::from_secs(2))
                .status(200)
                .json_body(rows_body(job_id.as_str(), None));
        })
        .await;

    let response = tokio::time::timeout(
        Duration::from_secs(1),
        filed.get(&filed.results("&all=true")),
    )
    .await
    .context("the queue lookup waited for the results read")??;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    Ok(())
}

/// The cache discards failed reads and reads that time out. The next read
/// sends a new query.
#[rstest]
#[case::timed_out("timed_out")]
#[case::failed("failed")]
#[tokio::test]
async fn test_a_degraded_read_is_not_kept(#[case] read: &str) -> anyhow::Result<()> {
    let server = MockServer::start_async().await;
    let (filed, job_id) = answered(&server, PlanStatus::Complete).await?;
    let bad = server
        .mock_async(|when, then| {
            when.method(POST).path("/api/query");
            if read == "timed_out" {
                then.delay(DUCKDB_TIMEOUT + Duration::from_secs(1))
                    .status(200);
            } else {
                then.status(500).body("boom");
            }
        })
        .await;
    let uri = filed.results("&all=true");

    let body = filed.get_json(&uri).await?;
    assert_eq!(body["results_read"], read, "{body}");
    bad.delete_async().await;
    let good = mock_rows(&server, &job_id, rows_body(&job_id, None)).await;
    let body = filed.get_json(&uri).await?;
    assert_eq!(body["results_read"], "ok", "{body}");
    assert_eq!(good.calls_async().await, 1);
    Ok(())
}

/// A kept plan answers each tier as an empty cache does, whichever tier
/// filled it.
#[rstest]
#[case::full_first(true)]
#[case::withheld_first(false)]
#[tokio::test]
async fn test_a_kept_plan_answers_each_tier_as_a_cold_read(
    #[case] full_first: bool,
) -> anyhow::Result<()> {
    let server = MockServer::start_async().await;
    let (filed, job_id) = answered(&server, PlanStatus::Complete).await?;
    let query = mock_rows(&server, &job_id, rows_body(&job_id, Some("out of memory"))).await;
    let uri = filed.results("&all=true");

    for full in [full_first, !full_first] {
        let mut permissions = vec![UserPermission::SubmitPlans, UserPermission::ReadPlans];
        if full {
            permissions.push(UserPermission::ReadClients);
        }
        grant_user(&filed.state, &filed.owner.1, &permissions).await?;
        let kept = filed.get_json(&uri).await?;
        let tier = if full { "full" } else { "withheld" };
        assert_eq!(kept["fleet_detail"], tier, "{kept}");
        assert_eq!(kept, filed.get_json_cold(&uri).await?, "{tier}");
    }
    // One fill on the server, and one for each cold read.
    assert_eq!(query.calls_async().await, 3);
    Ok(())
}
