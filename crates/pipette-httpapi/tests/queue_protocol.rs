//! The queue protocol a client runs: claim, heartbeat, reclaim and the
//! `POST /benchmarks` teardown, tested together as one conversation. The queue
//! is seeded by writing `avail/` and `leased/` keys directly.

#![expect(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    reason = "tests build the requests a client sends, so they keep `json!` (see clippy.toml)"
)]

use std::path::Path;

use anyhow::Context;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use chrono::{DateTime, SecondsFormat, SubsecRound, Utc};
use rstest::rstest;
use serde_json::{Value, json};
use tower::ServiceExt;

use pipette_core::denial::DenialRecord;
use pipette_core::todo_filename::{leased_key, parse_leased_key};
use pipette_httpapi::state::AppState;
use pipette_plan_types::{ClientId, DeviceProfile, ExpiresAt, JobId};
use pipette_web_testkit::{
    approved_client, authed_post, authed_put_empty, body_json, denied_clients, job, make_state,
    register_and_approve, register_pending, set_matching_input, setup_benchmarks, test_app,
};

fn seed_avail(dir: &Path, job_id: &str, body: &Value) -> anyhow::Result<JobId> {
    pipette_web_testkit::seed_avail(dir, job_id, ExpiresAt::Never, body)
}

/// The marker's expiry matches the `avail/` key's, as `queue-maintenance` writes
/// it: the claim handler addresses the rename target by it.
async fn seed_eligible(
    state: &AppState,
    dir: &Path,
    client_id: &ClientId,
    job_id: &str,
    expires_at: ExpiresAt,
) -> anyhow::Result<JobId> {
    seed_eligible_body(state, dir, client_id, job_id, expires_at, &job_body(job_id)).await
}

/// Like [`seed_eligible`], with a caller-supplied body.
async fn seed_eligible_body(
    state: &AppState,
    dir: &Path,
    client_id: &ClientId,
    job_id: &str,
    expires_at: ExpiresAt,
    body: &Value,
) -> anyhow::Result<JobId> {
    let job = pipette_web_testkit::seed_avail(dir, job_id, expires_at, body)?;
    state
        .todo_store
        .write_eligible(client_id, &job, expires_at)
        .await?;
    Ok(job)
}

fn seed_leased(
    dir: &Path,
    job: &JobId,
    expires_at: ExpiresAt,
    client: &ClientId,
    expiry: chrono::DateTime<chrono::Utc>,
) -> anyhow::Result<()> {
    seed_leased_body(dir, job, expires_at, client, expiry, &json!({}))
}

/// Like [`seed_leased`], with a caller-supplied body.
fn seed_leased_body(
    dir: &Path,
    job: &JobId,
    expires_at: ExpiresAt,
    client: &ClientId,
    expiry: chrono::DateTime<chrono::Utc>,
    body: &Value,
) -> anyhow::Result<()> {
    let path = dir
        .join("todo")
        .join("leased")
        .join(leased_key(job, expires_at, client, expiry));
    std::fs::create_dir_all(
        path.parent()
            .context("the fixture path has a parent directoryath")?,
    )?;
    std::fs::write(path, serde_json::to_vec(body)?)?;
    Ok(())
}

/// The `time_window` is deliberately wrong: the claim response must carry the
/// server's lease increment, never a stored value.
fn job_body(job_id: &str) -> Value {
    json!({
        "job_id": job_id,
        "time_window": "PT99M",
        "spec": pipette_web_testkit::job_spec(),
    })
}

/// The `PT{n}S` form a lease's remaining life takes when not whole minutes.
fn window_secs(body: &Value) -> anyhow::Result<u64> {
    let window = body
        .get("time_window")
        .and_then(Value::as_str)
        .context("time_window should be a string")?;
    Ok(window
        .strip_prefix("PT")
        .and_then(|s| s.strip_suffix('S'))
        .context("time_window should be an ISO 8601 second duration")?
        .parse()?)
}

// ── claim ─────────────────────────────────────────────────────

#[tokio::test]
async fn test_claim_returns_eligible_job() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;

    let job = seed_eligible(&state, dir.path(), &client_id, "job-1", ExpiresAt::Never).await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["job_id"], "job-1");
    // Lifted from `spec.benchmark`, so the envelope and the spec cannot disagree.
    assert_eq!(body["benchmark_id"], "eval_test");
    // The lease increment (default 300s), not the stored "PT99M".
    assert_eq!(body["time_window"], "PT5M");

    // The spec is forwarded verbatim, nested — not flattened into the envelope.
    assert_eq!(body["spec"]["benchmark"], "eval_test");
    assert_eq!(body["spec"]["model"]["type"], "gguf_text");
    assert_eq!(body["spec"]["runtime"]["repository_version"], "b5000");
    assert!(body.get("model").is_none(), "spec leaked into the envelope");

    // Always present: this body has no clauses, so it admits no build.
    assert_eq!(body["client_versions"], json!([]));

    // Scheduling fields stay server-side: they are spent by the time a job is
    // handed out, and a device has no use for the roster it was picked from.
    ["requires", "any_of", "clients"]
        .into_iter()
        .for_each(|scheduling| {
            assert!(
                body.get(scheduling).is_none(),
                "{scheduling} was forwarded to the client"
            );
        });

    let leased = state.todo_store.list_leased().await?;
    assert_eq!(leased.len(), 1);
    let lease = parse_leased_key(&leased[0])?;
    assert_eq!(lease.job_id, job);
    assert_eq!(lease.client_id, client_id);
    Ok(())
}

#[tokio::test]
async fn test_claim_response_carries_only_the_envelope_fields() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;

    let stored = json!({
        "job_id": "job-1",
        "benchmark_id": "stale",
        "time_window": "PT99M",
        "spec": pipette_web_testkit::job_spec(),
        "clients": [client_id.to_string()],
        "requires": ["os:macos"],
        "any_of": [["device:macbookpro16", "device:macbookpro14"]],
        "exclude_clients": ["ev1_other"],
        "excludes": ["os_version:15.0"],
        "not_all_of": [["runtime:llama_cpp", "device:macbookpro14"]],
        "extra": 1,
    });
    seed_eligible_body(
        &state,
        dir.path(),
        &client_id,
        "job-1",
        ExpiresAt::Never,
        &stored,
    )
    .await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    let mut fields: Vec<&str> = body
        .as_object()
        .context("the claim response is an object")?
        .keys()
        .map(String::as_str)
        .collect();
    fields.sort_unstable();
    assert_eq!(
        fields,
        [
            "benchmark_id",
            "client_versions",
            "job_id",
            "spec",
            "time_window"
        ]
    );
    assert_eq!(
        body["benchmark_id"], "eval_test",
        "benchmark_id is lifted from spec, not the stored copy"
    );
    assert_eq!(body["spec"], pipette_web_testkit::job_spec());
    Ok(())
}

/// A body that cannot make a claim envelope is still leased and answered, never
/// a `500` that strands the lease. The warning is the operator's only trace.
#[rstest]
#[case::no_spec(json!({"job_id": "job-1"}), Some(json!("job-1")), None)]
#[case::null_spec(json!({"job_id": "job-1", "spec": null}), Some(json!("job-1")), None)]
#[case::non_string_benchmark(
    json!({"job_id": "job-1", "spec": {"benchmark": 42}}),
    Some(json!("job-1")),
    Some(json!({"benchmark": 42})),
)]
#[case::empty_body(json!({}), None, None)]
#[test]
fn test_claim_answers_an_undecodable_job_with_what_it_has(
    #[case] stored: Value,
    #[case] expected_job_id: Option<Value>,
    #[case] expected_spec: Option<Value>,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;

    let (outcome, logs) = pipette_web_testkit::capture_logs(
        |rt| -> anyhow::Result<(StatusCode, Value, Vec<String>)> {
            rt.block_on(async {
                let state = make_state(dir.path()).await?;
                let (key, client_id) = register_and_approve(&state).await?;
                seed_eligible_body(
                    &state,
                    dir.path(),
                    &client_id,
                    "job-1",
                    ExpiresAt::Never,
                    &stored,
                )
                .await?;

                let resp =
                    authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
                let status = resp.status();
                let body = body_json(resp).await?;
                Ok((status, body, state.todo_store.list_leased().await?))
            })
        },
    )?;

    let (status, body, leased) = outcome?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.get("job_id"), expected_job_id.as_ref());
    assert_eq!(body.get("benchmark_id"), None, "only a string is lifted");
    // A null spec is omitted, never echoed as null.
    assert_eq!(body.get("spec"), expected_spec.as_ref());
    assert_eq!(body["time_window"], "PT5M");
    assert_eq!(leased.len(), 1, "the job should be leased: {leased:?}");
    assert_eq!(parse_leased_key(&leased[0])?.job_id.as_str(), "job-1");
    assert!(
        logs.contains("claim envelope is undecodable"),
        "the degraded envelope went unlogged:\n{logs}"
    );
    Ok(())
}

/// Maintenance may re-date a job after the handler admits an unlapsed marker;
/// the claim retries under the current key and answers with its deadline. The
/// retry is logged, since a marker and key can disagree for other reasons too.
#[test]
fn test_claim_retries_under_the_key_a_stale_marker_misses() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    // Relative to `now`: the scenario needs both deadlines in the future.
    let now = Utc::now().trunc_subsecs(0);
    let current = ExpiresAt::At(now + chrono::Duration::days(180));
    let stale = ExpiresAt::At(now + chrono::Duration::days(30));

    let (outcome, logs) =
        pipette_web_testkit::capture_logs(|rt| -> anyhow::Result<(Value, ExpiresAt)> {
            rt.block_on(async {
                let state = make_state(dir.path()).await?;
                let (key, client_id) = register_and_approve(&state).await?;

                // The key carries the re-dated deadline; the marker still names the old one.
                let job = job("job-1")?;
                pipette_web_testkit::seed_avail(dir.path(), "job-1", current, &job_body("job-1"))?;
                state
                    .todo_store
                    .write_eligible(&client_id, &job, stale)
                    .await?;

                let resp =
                    authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
                assert_eq!(resp.status(), StatusCode::OK);
                let body = body_json(resp).await?;
                let leased = state.todo_store.list_leased().await?;
                assert_eq!(leased.len(), 1);
                Ok((body, parse_leased_key(&leased[0])?.expires_at))
            })
        })?;

    let (body, lease_expires_at) = outcome?;
    assert_eq!(body["job_id"], "job-1");
    assert_eq!(
        body["expires_at"],
        json!(current.to_string()),
        "the client hears the deadline the key carries"
    );
    assert_eq!(lease_expires_at, current);
    assert!(
        logs.contains("the client's eligible marker names an older one"),
        "the retry went unlogged:\n{logs}"
    );
    Ok(())
}

#[tokio::test]
async fn test_claim_skips_a_stale_marker_whose_job_is_gone() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;

    let gone = job("job-gone")?;
    state
        .todo_store
        .write_eligible(&client_id, &gone, ExpiresAt::Never)
        .await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert!(state.todo_store.list_leased().await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn test_claim_falls_through_a_gone_job_to_the_next_candidate() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;

    // A dated marker ranks ahead of a `never` one, so the gone job is tried first.
    let gone = job("job-gone")?;
    state
        .todo_store
        .write_eligible(
            &client_id,
            &gone,
            ExpiresAt::At(Utc::now() + chrono::Duration::hours(1)),
        )
        .await?;
    let real = seed_eligible(&state, dir.path(), &client_id, "job-real", ExpiresAt::Never).await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await?["job_id"], "job-real");

    let leased = state.todo_store.list_leased().await?;
    assert_eq!(leased.len(), 1);
    assert_eq!(parse_leased_key(&leased[0])?.job_id, real);
    Ok(())
}

/// Failing means the retry under the job's current key leases an expired job.
#[tokio::test]
async fn test_claim_204_when_the_marker_is_live_but_the_key_has_lapsed() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;

    // Truncated to the second, the precision the stored spelling carries.
    let now = Utc::now().trunc_subsecs(0);
    let lapsed = ExpiresAt::At(now - chrono::Duration::hours(1));
    let job = pipette_web_testkit::seed_avail(dir.path(), "job-1", lapsed, &job_body("job-1"))?;
    state
        .todo_store
        .write_eligible(
            &client_id,
            &job,
            ExpiresAt::At(now + chrono::Duration::days(30)),
        )
        .await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert!(state.todo_store.list_leased().await?.is_empty());
    assert!(
        state.todo_store.get_avail(&job, lapsed).await?.is_some(),
        "the lapsed job should stay in avail/ for queue-maintenance"
    );
    Ok(())
}

/// Which deadline a job's `avail/` key carries.
#[derive(Debug, Clone, Copy)]
enum KeyExpiry {
    Never,
    /// A deadline in the future, so the claim is inside it.
    Dated,
}

/// A dated case names the key's instant: under test is which of the two
/// reaches the client, not which date they hold.
#[derive(Debug, Clone, Copy)]
enum BodyExpiry {
    Absent,
    Never,
    /// The stored basic spelling, which is the ingested shape.
    Basic,
    /// What `validate_job` accepts, so likely in a hand-written body; not the stored
    /// form, so it reads as a disagreement.
    Rfc3339,
    Unparseable,
    NonString,
}

/// Every scheduling path reads the key; the body's copy drives nothing and need
/// not agree. The wire form is ISO 8601 basic, `ExpiresAt`'s stored spelling.
#[rstest]
#[case::never_key_absent_body(KeyExpiry::Never, BodyExpiry::Absent, false)]
#[case::never_key_never_body(KeyExpiry::Never, BodyExpiry::Never, false)]
// The regression this pins: a body deadline the key lacks never reaches the client.
#[case::never_key_dated_body(KeyExpiry::Never, BodyExpiry::Basic, false)]
// The agreeing body is the ingested shape, so the busy path.
#[case::dated_key_agreeing_body(KeyExpiry::Dated, BodyExpiry::Basic, true)]
#[case::dated_key_absent_body(KeyExpiry::Dated, BodyExpiry::Absent, true)]
#[case::dated_key_never_body(KeyExpiry::Dated, BodyExpiry::Never, true)]
#[case::dated_key_rfc3339_body(KeyExpiry::Dated, BodyExpiry::Rfc3339, true)]
#[case::dated_key_unparseable_body(KeyExpiry::Dated, BodyExpiry::Unparseable, true)]
#[case::dated_key_non_string_body(KeyExpiry::Dated, BodyExpiry::NonString, true)]
#[tokio::test]
async fn test_claim_expires_at_comes_from_the_avail_key(
    #[case] key_expiry: KeyExpiry,
    #[case] stored: BodyExpiry,
    #[case] on_the_wire: bool,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;

    // Relative to `now`: a passed key deadline makes the job ineligible. Truncated
    // to the second, the stored precision.
    let deadline = (Utc::now() + chrono::Duration::days(1)).trunc_subsecs(0);
    let key_expiry = match key_expiry {
        KeyExpiry::Never => ExpiresAt::Never,
        KeyExpiry::Dated => ExpiresAt::At(deadline),
    };
    let mut body = job_body("job-1");
    match stored {
        BodyExpiry::Absent => {
            body.as_object_mut()
                .context("job body is an object")?
                .remove("expires_at");
        }
        BodyExpiry::Never => body["expires_at"] = json!("never"),
        BodyExpiry::Basic => body["expires_at"] = json!(ExpiresAt::At(deadline).to_string()),
        BodyExpiry::Rfc3339 => {
            body["expires_at"] = json!(deadline.to_rfc3339_opts(SecondsFormat::Secs, true));
        }
        BodyExpiry::Unparseable => body["expires_at"] = json!("soon"),
        BodyExpiry::NonString => body["expires_at"] = json!(42),
    }
    seed_eligible_body(&state, dir.path(), &client_id, "job-1", key_expiry, &body).await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let got = body_json(resp).await?;

    if on_the_wire {
        assert_eq!(
            got["expires_at"],
            json!(key_expiry.to_string()),
            "key {key_expiry} with body {stored:?} should reach the wire as the key's deadline"
        );
    } else {
        assert!(
            got.get("expires_at").is_none(),
            "key {key_expiry} with body {stored:?} should be omitted, got {:?}",
            got.get("expires_at")
        );
    }
    Ok(())
}

#[tokio::test]
async fn test_claim_204_when_no_eligible_jobs() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;

    // A job exists in avail/ but this client has no eligible marker for it.
    seed_avail(dir.path(), "job-1", &job_body("job-1"))?;

    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    Ok(())
}

#[tokio::test]
async fn test_claim_204_when_eligible_marker_is_stale() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;

    // The marker's job left avail/: claim must not 500 or hand out a phantom job.
    let job = job("ghost")?;
    state
        .todo_store
        .write_eligible(&client_id, &job, ExpiresAt::Never)
        .await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    Ok(())
}

#[tokio::test]
async fn test_claim_403_when_pending() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (key, client_id) = register_pending(&state).await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    Ok(())
}

#[tokio::test]
async fn test_claim_401_when_unauthenticated() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let app = test_app(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/plans/claim")
                .header("Content-Type", "application/json")
                .body(Body::from("{}"))?,
        )
        .await?;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    Ok(())
}

#[tokio::test]
async fn test_claim_204_when_suspended() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;

    let job = seed_eligible(&state, dir.path(), &client_id, "job-1", ExpiresAt::Never).await?;
    state
        .todo_store
        .write_suspension(&client_id, chrono::Utc::now(), &job)
        .await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    Ok(())
}

#[tokio::test]
async fn test_claim_idempotent_when_holding_single_lease() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;

    // Seeded directly to fix a known remaining life; 90s is not whole minutes, so
    // the window renders as `PTnS`.
    let job_a = job("job-a")?;
    let expiry = Utc::now() + chrono::Duration::seconds(90);
    seed_leased_body(
        dir.path(),
        &job_a,
        ExpiresAt::Never,
        &client_id,
        expiry,
        &job_body("job-a"),
    )?;

    // A second eligible job exists; a client holding a lease must not be handed it.
    seed_eligible(&state, dir.path(), &client_id, "job-b", ExpiresAt::Never).await?;

    let leased_before = state.todo_store.list_leased().await?;

    // A lost claim response leaves the client no job_id to reclaim by, so a
    // re-poll hands the same job back.
    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["job_id"], "job-a");

    // The lease's remaining life, not the stored "PT99M" nor a fresh increment.
    let secs = window_secs(&body)?;
    assert!(
        (1..=90).contains(&secs),
        "time_window {} should reflect ~90s remaining",
        body["time_window"]
    );

    // Neither renewed nor a second lease granted.
    assert_eq!(state.todo_store.list_leased().await?, leased_before);

    assert!(
        state
            .todo_store
            .read_suspension(&client_id)
            .await?
            .is_none()
    );
    Ok(())
}

/// Once per lease, so a client that crashes and re-polls inside its lease is
/// still bounded. At `plan_lost_lease_limit` (3) the claim withholds the job.
#[tokio::test]
async fn test_claim_hand_back_counts_the_held_lease_as_lost() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;
    let job_a = job("job-a")?;
    seed_leased_body(
        dir.path(),
        &job_a,
        ExpiresAt::Never,
        &client_id,
        Utc::now() + chrono::Duration::seconds(90),
        &job_body("job-a"),
    )?;
    // One loss on an earlier lease.
    state
        .todo_store
        .write_lost_lease(&job_a, &client_id, Utc::now() - chrono::Duration::hours(1))
        .await?;

    // The second loss; a repeat poll on the same lease adds nothing.
    for _ in 0..2 {
        let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_json(resp).await?["job_id"], "job-a");
    }
    assert_eq!(state.todo_store.list_lost_leases(&job_a).await?.len(), 2);

    // A heartbeat renews the lease, so the next re-poll is a third loss.
    let resp = authed_put_empty(&state, &key, &client_id, "/plans/job-a/heartbeat").await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert_eq!(state.todo_store.list_lost_leases(&job_a).await?.len(), 3);
    assert_eq!(state.todo_store.list_leased().await?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn test_claim_hand_back_records_no_loss_when_the_limit_is_zero() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = pipette_web_testkit::make_state_from_config(pipette_core::config::Config {
        plan_lost_lease_limit: 0,
        ..pipette_web_testkit::test_config(dir.path())
    })
    .await?;
    let (key, client_id) = register_and_approve(&state).await?;
    let job_a = job("job-a")?;
    seed_leased_body(
        dir.path(),
        &job_a,
        ExpiresAt::Never,
        &client_id,
        Utc::now() + chrono::Duration::seconds(90),
        &job_body("job-a"),
    )?;

    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(state.todo_store.list_lost_leases(&job_a).await?.is_empty());
    Ok(())
}

/// The two paths that answer a claim with a job.
#[derive(Debug, Clone, Copy)]
enum ClaimPath {
    Fresh,
    HandBack,
}

async fn seed_on_path(
    path: ClaimPath,
    state: &AppState,
    dir: &Path,
    client: &ClientId,
    body: &Value,
) -> anyhow::Result<()> {
    match path {
        ClaimPath::Fresh => {
            seed_eligible_body(state, dir, client, "job-v", ExpiresAt::Never, body).await?;
        }
        ClaimPath::HandBack => seed_leased_body(
            dir,
            &job("job-v")?,
            ExpiresAt::Never,
            client,
            Utc::now() + chrono::Duration::seconds(90),
            body,
        )?,
    }
    Ok(())
}

/// A job for one tagged phone at one of three builds.
fn versioned_job_body() -> Value {
    json!({
        "job_id": "job-v",
        "requires": ["tag:lab-phone"],
        "any_of": [[
            "client_version:2026.10.02-430a0bf-internal",
            "client_version:2026.09.30-8aed2cc-internal",
            "client_version:2026.08.01-0000000-internal",
        ]],
        "spec": pipette_web_testkit::job_spec(),
    })
}

/// Sorted and always present. The job requires a tag, so the tags must be read
/// for any version to be admitted.
#[rstest]
#[case::admitted(
    &["client_version:2026.10.02-430a0bf-internal", "client_version:2026.09.30-8aed2cc-internal", "client_version:2026.07.01-1111111-internal"],
    &["lab-phone"],
    json!(["2026.09.30-8aed2cc-internal", "2026.10.02-430a0bf-internal"])
)]
#[case::untagged(&["client_version:2026.10.02-430a0bf-internal"], &[], json!([]))]
#[case::no_version_reported(&["runtime:llama_cpp"], &["lab-phone"], json!([]))]
#[tokio::test]
async fn test_claim_names_the_admitted_client_versions(
    #[values(ClaimPath::Fresh, ClaimPath::HandBack)] path: ClaimPath,
    #[case] capabilities: &[&str],
    #[case] tags: &[&str],
    #[case] expected: Value,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;
    set_matching_input(
        &state,
        &client_id,
        DeviceProfile::default(),
        capabilities,
        tags,
    )
    .await?;
    seed_on_path(path, &state, dir.path(), &client_id, &versioned_job_body()).await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["job_id"], "job-v");
    assert_eq!(body["client_versions"], expected, "{path:?}: {body}");
    assert!(
        body.get("any_of").is_none(),
        "the version group was forwarded"
    );
    Ok(())
}

/// A failed tag read answers 500 before any rename: a fresh claim takes no
/// lease, and a held lease is left as it was.
#[rstest]
#[tokio::test]
async fn test_claim_500_when_the_tags_cannot_be_read(
    #[values(ClaimPath::Fresh, ClaimPath::HandBack)] path: ClaimPath,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (mut state, key, client_id) = approved_client(dir.path()).await?;
    seed_on_path(path, &state, dir.path(), &client_id, &versioned_job_body()).await?;
    let leased_before = state.todo_store.list_leased().await?;
    state.tag_store = std::sync::Arc::new(pipette_store_testkit::TagStoreUnimplemented);

    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR, "{path:?}");
    assert_eq!(state.todo_store.list_leased().await?, leased_before);
    Ok(())
}

/// Failing means a client that lost its claim response hears a different job
/// than a fresh claim.
#[rstest]
#[case::never_key(KeyExpiry::Never)]
#[case::dated_key(KeyExpiry::Dated)]
#[tokio::test]
async fn test_claim_hand_back_carries_the_envelope_and_the_lease_keys_deadline(
    #[case] key_expiry: KeyExpiry,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;

    // Truncated to the second, the precision the lease key carries.
    let expires_at = match key_expiry {
        KeyExpiry::Never => ExpiresAt::Never,
        KeyExpiry::Dated => {
            ExpiresAt::At((Utc::now() + chrono::Duration::days(1)).trunc_subsecs(0))
        }
    };
    seed_leased_body(
        dir.path(),
        &job("job-a")?,
        expires_at,
        &client_id,
        Utc::now() + chrono::Duration::seconds(90),
        &job_body("job-a"),
    )?;
    let leased_before = state.todo_store.list_leased().await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["job_id"], "job-a");
    assert_eq!(body["benchmark_id"], "eval_test");
    assert_eq!(body["spec"], pipette_web_testkit::job_spec());
    match key_expiry {
        KeyExpiry::Never => assert!(
            body.get("expires_at").is_none(),
            "a never key should omit expires_at, got {:?}",
            body.get("expires_at")
        ),
        KeyExpiry::Dated => assert_eq!(body["expires_at"], json!(expires_at.to_string())),
    }
    // Handed back, not renewed or claimed afresh.
    assert_eq!(state.todo_store.list_leased().await?, leased_before);
    Ok(())
}

#[tokio::test]
async fn test_claim_suspends_client_holding_multiple_leases() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;

    // Two live leases is a protocol anomaly (leases accumulated across crashes);
    // the suspension records one held job_id for triage.
    let future = Utc::now() + chrono::Duration::hours(1);
    let job_a = job("job-a")?;
    let job_b = job("job-b")?;
    seed_leased(dir.path(), &job_a, ExpiresAt::Never, &client_id, future)?;
    seed_leased(dir.path(), &job_b, ExpiresAt::Never, &client_id, future)?;

    seed_eligible(&state, dir.path(), &client_id, "job-c", ExpiresAt::Never).await?;
    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let suspension = state
        .todo_store
        .read_suspension(&client_id)
        .await?
        .context("client holding two live leases should be suspended")?;
    assert!([job_a, job_b].contains(&suspension.conflicting_job_id));
    Ok(())
}

#[tokio::test]
async fn test_claim_not_suspended_when_one_job_appears_under_two_lease_keys() -> anyhow::Result<()>
{
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;

    // A renewal renames the lease key, and a non-snapshot listing can show both:
    // one lease, not an accumulation.
    let job_a = job("job-a")?;
    let now = Utc::now();
    seed_leased_body(
        dir.path(),
        &job_a,
        ExpiresAt::Never,
        &client_id,
        now + chrono::Duration::seconds(60),
        &job_body("job-a"),
    )?;
    seed_leased_body(
        dir.path(),
        &job_a,
        ExpiresAt::Never,
        &client_id,
        now + chrono::Duration::seconds(120),
        &job_body("job-a"),
    )?;

    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await?["job_id"], "job-a");
    assert!(
        state
            .todo_store
            .read_suspension(&client_id)
            .await?
            .is_none()
    );
    Ok(())
}

/// The earlier key is one a renewal already renamed away.
#[tokio::test]
async fn test_claim_hand_back_reports_the_later_of_two_lease_keys() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;

    let job_a = job("job-a")?;
    let now = Utc::now();
    for offset in [60, 120] {
        seed_leased_body(
            dir.path(),
            &job_a,
            ExpiresAt::Never,
            &client_id,
            now + chrono::Duration::seconds(offset),
            &job_body("job-a"),
        )?;
    }
    let leased_before = state.todo_store.list_leased().await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["job_id"], "job-a");
    let secs = window_secs(&body)?;
    assert!(
        (61..=120).contains(&secs),
        "time_window {} should be the later key's remaining life, not the earlier's",
        body["time_window"]
    );
    assert_eq!(state.todo_store.list_leased().await?, leased_before);
    Ok(())
}

/// Else foreign cruft fails every claim from that client or reads as a lease.
#[tokio::test]
async fn test_claim_passes_over_unparseable_files_in_the_lease_partition() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;
    let job = seed_eligible(&state, dir.path(), &client_id, "job-1", ExpiresAt::Never).await?;

    let partition = dir
        .path()
        .join("todo")
        .join("leased")
        .join(client_id.as_str());
    std::fs::create_dir_all(&partition)?;
    let cruft = [
        partition.join("garbage.json"),
        partition.join("job-x.cruft.json"),
    ];
    for path in &cruft {
        std::fs::write(path, b"not a lease")?;
    }

    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await?["job_id"], "job-1");
    assert!(
        state
            .todo_store
            .read_suspension(&client_id)
            .await?
            .is_none()
    );
    let leases: Vec<JobId> = state
        .todo_store
        .list_leased()
        .await?
        .iter()
        .filter_map(|name| parse_leased_key(name).ok())
        .map(|lease| lease.job_id)
        .collect();
    assert_eq!(leases, [job]);
    for path in &cruft {
        assert!(
            path.try_exists()?,
            "{} should be left alone",
            path.display()
        );
    }
    Ok(())
}

#[tokio::test]
async fn test_claim_skips_job_denied_for_client() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;

    let job = seed_eligible(&state, dir.path(), &client_id, "job-1", ExpiresAt::Never).await?;
    state
        .todo_store
        .write_denied(&job, &client_id, &DenialRecord::default())
        .await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    Ok(())
}

#[tokio::test]
async fn test_claim_skips_job_leased_to_another_client() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;

    let job = seed_eligible(&state, dir.path(), &client_id, "job-1", ExpiresAt::Never).await?;
    let other = ClientId::try_new("someone-else")?;
    seed_leased(
        dir.path(),
        &job,
        ExpiresAt::Never,
        &other,
        chrono::Utc::now() + chrono::Duration::hours(1),
    )?;

    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    // Another client's lease must not suspend *us*.
    assert!(
        state
            .todo_store
            .read_suspension(&client_id)
            .await?
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn test_claim_succeeds_when_own_lease_is_expired() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;

    // An expired own lease is a timed-out device, not a double-claim.
    let old_job = job("old-job")?;
    seed_leased(
        dir.path(),
        &old_job,
        ExpiresAt::Never,
        &client_id,
        chrono::Utc::now() - chrono::Duration::hours(1),
    )?;

    seed_eligible(&state, dir.path(), &client_id, "job-1", ExpiresAt::Never).await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["job_id"], "job-1");
    assert!(
        state
            .todo_store
            .read_suspension(&client_id)
            .await?
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn test_claim_ignores_denial_by_another_client() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;

    let job = seed_eligible(&state, dir.path(), &client_id, "job-1", ExpiresAt::Never).await?;
    let other = ClientId::try_new("someone-else")?;
    state
        .todo_store
        .write_denied(&job, &other, &DenialRecord::default())
        .await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["job_id"], "job-1");
    Ok(())
}

#[tokio::test]
async fn test_claim_skips_denied_job_but_claims_another() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;

    let job_a = seed_eligible(&state, dir.path(), &client_id, "job-a", ExpiresAt::Never).await?;
    let job_b = seed_eligible(&state, dir.path(), &client_id, "job-b", ExpiresAt::Never).await?;
    state
        .todo_store
        .write_denied(&job_a, &client_id, &DenialRecord::default())
        .await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["job_id"], "job-b");

    let leased = state.todo_store.list_leased().await?;
    assert_eq!(leased.len(), 1);
    assert_eq!(parse_leased_key(&leased[0])?.job_id, job_b);
    Ok(())
}

#[tokio::test]
async fn test_claim_prefers_soonest_expiring_job() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;

    // The eligible listing is unordered. Both dates are in the future, so neither
    // is filtered as expired.
    let soon = ExpiresAt::At(Utc::now() + chrono::Duration::hours(1));
    let later = ExpiresAt::At(Utc::now() + chrono::Duration::days(30));

    seed_eligible(
        &state,
        dir.path(),
        &client_id,
        "job-never",
        ExpiresAt::Never,
    )
    .await?;
    seed_eligible(&state, dir.path(), &client_id, "job-later", later).await?;
    seed_eligible(&state, dir.path(), &client_id, "job-soon", soon).await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["job_id"], "job-soon");
    Ok(())
}

#[tokio::test]
async fn test_claim_picks_within_soonest_expiry_tier() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;

    // The pick within the soonest tier is random, so assert membership.
    let soon = ExpiresAt::At(Utc::now() + chrono::Duration::hours(1));
    let later = ExpiresAt::At(Utc::now() + chrono::Duration::days(30));

    let mut tier: Vec<JobId> = Vec::new();
    for id in ["soon-a", "soon-b", "soon-c"] {
        tier.push(seed_eligible(&state, dir.path(), &client_id, id, soon).await?);
    }
    seed_eligible(&state, dir.path(), &client_id, "job-later", later).await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    let claimed = body["job_id"]
        .as_str()
        .context("claim response missing job_id")?;
    assert!(
        tier.iter().any(|j| j.as_str() == claimed),
        "claimed {claimed:?} should be in the soonest-expiry tier, not the later job"
    );
    Ok(())
}

#[tokio::test]
async fn test_claim_204_when_only_eligible_job_is_expired() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;

    // Not yet swept from avail/, but an expired job is never handed out.
    let expired = ExpiresAt::At(Utc::now() - chrono::Duration::hours(1));
    let job = seed_eligible(&state, dir.path(), &client_id, "job-1", expired).await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    assert!(state.todo_store.list_leased().await?.is_empty());
    assert!(state.todo_store.get_avail(&job, expired).await?.is_some());
    Ok(())
}

#[tokio::test]
async fn test_claim_skips_expired_job_and_claims_valid_one() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;

    // An expired job ranks soonest, so without the expiry filter it would win.
    let expired = ExpiresAt::At(Utc::now() - chrono::Duration::hours(1));
    let valid = ExpiresAt::At(Utc::now() + chrono::Duration::days(7));
    seed_eligible(&state, dir.path(), &client_id, "job-expired", expired).await?;
    let good = seed_eligible(&state, dir.path(), &client_id, "job-valid", valid).await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["job_id"], "job-valid");

    let leased = state.todo_store.list_leased().await?;
    assert_eq!(leased.len(), 1);
    assert_eq!(parse_leased_key(&leased[0])?.job_id, good);
    Ok(())
}

// ── heartbeat ────────────────────────────────────────────────

fn lease_expiry(dir: &Path, job: &JobId, client: &ClientId) -> anyhow::Result<DateTime<Utc>> {
    let part = dir.join("todo").join("leased").join(client.as_str());
    let prefix = format!("{}.", job.as_str());
    std::fs::read_dir(&part)?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .find(|name| name.starts_with(&prefix))
        .map(|name| {
            let key = format!("{}/{name}", client.as_str());
            parse_leased_key(&key).map(|lease| lease.lease_expiry)
        })
        .transpose()?
        .ok_or_else(|| anyhow::anyhow!("no lease for {} held by {}", job.as_str(), client.as_str()))
}

#[tokio::test]
async fn test_heartbeat_renews_active_lease() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;

    let job = job("job-1")?;
    let old = Utc::now() + chrono::Duration::seconds(30);
    seed_leased(dir.path(), &job, ExpiresAt::Never, &client_id, old)?;

    let resp = authed_put_empty(&state, &key, &client_id, "/plans/job-1/heartbeat").await?;
    assert_eq!(resp.status(), StatusCode::OK);
    // 200 carries an empty body, not the renewal's locator.
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await?;
    assert!(body.is_empty(), "heartbeat 200 body should be empty");

    let renewed = lease_expiry(dir.path(), &job, &client_id)?;
    assert!(
        renewed > old,
        "lease expiry {renewed} should advance past the original {old}"
    );
    Ok(())
}

/// Failing means an expiry gate on heartbeat yanks a job in progress, or the
/// renewal rewrote the job's deadline.
#[tokio::test]
async fn test_heartbeat_renews_a_held_lease_past_the_jobs_deadline() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;

    // Truncated to the second, the precision the lease key carries.
    let past = ExpiresAt::At((Utc::now() - chrono::Duration::hours(1)).trunc_subsecs(0));
    let job = job("job-1")?;
    let old = Utc::now() + chrono::Duration::seconds(30);
    seed_leased(dir.path(), &job, past, &client_id, old)?;

    let resp = authed_put_empty(&state, &key, &client_id, "/plans/job-1/heartbeat").await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await?;
    assert!(body.is_empty(), "heartbeat 200 body should be empty");

    let leased = state.todo_store.list_leased().await?;
    assert_eq!(leased.len(), 1);
    let lease = parse_leased_key(&leased[0])?;
    assert_eq!(lease.job_id, job);
    assert_eq!(lease.client_id, client_id);
    assert!(
        lease.lease_expiry > old,
        "lease expiry {} should advance past the original {old}",
        lease.lease_expiry
    );
    assert_eq!(
        lease.expires_at, past,
        "the job's deadline rides across the renewal"
    );
    Ok(())
}

#[tokio::test]
async fn test_heartbeat_404_when_no_lease() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;

    let resp = authed_put_empty(&state, &key, &client_id, "/plans/ghost/heartbeat").await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    Ok(())
}

async fn renewal(
    state: &AppState,
    key: &ed25519_dalek::SigningKey,
    client_id: &ClientId,
    method: &Method,
    path: &str,
) -> anyhow::Result<axum::http::Response<Body>> {
    match *method {
        Method::PUT => authed_put_empty(state, key, client_id, path).await,
        Method::POST => authed_post(state, key, client_id, path, &json!({})).await,
        _ => anyhow::bail!("no renewal is sent as {method}"),
    }
}

/// The profile change that set the flag gave up the client's standing, so a
/// renewal is refused `404` without renaming the lease, which would resurrect
/// it mid-relinquish. Reclaim renews an expired held lease once the flag clears.
#[rstest]
#[case::heartbeat(chrono::Duration::seconds(30), Method::PUT, "/plans/job-1/heartbeat")]
#[case::reclaim_held_lease(-chrono::Duration::hours(1), Method::POST, "/plans/job-1/reclaim")]
#[tokio::test]
async fn test_renewal_gated_while_pending_reindex(
    #[case] lease_offset: chrono::Duration,
    #[case] method: Method,
    #[case] path: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;
    let job = job("job-1")?;
    seed_leased(
        dir.path(),
        &job,
        ExpiresAt::Never,
        &client_id,
        Utc::now() + lease_offset,
    )?;
    let before = lease_expiry(dir.path(), &job, &client_id)?;
    state.todo_store.write_pending_reindex(&client_id).await?;

    let resp = renewal(&state, &key, &client_id, &method, path).await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        lease_expiry(dir.path(), &job, &client_id)?,
        before,
        "a gated renewal must not rename the lease"
    );

    // Clearing the flag (what the reindex pass does) lifts the gate.
    pipette_web_testkit::clear_pending_reindex(&*state.todo_store, &client_id).await?;
    let resp = renewal(&state, &key, &client_id, &method, path).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    Ok(())
}

#[tokio::test]
async fn test_invalid_job_id_in_path_is_400() -> anyhow::Result<()> {
    // `bad.id` routes as one segment but fails the `JobId` charset, so it is a
    // `400` before it can reach a store as a key.
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;

    let hb = authed_put_empty(&state, &key, &client_id, "/plans/bad.id/heartbeat").await?;
    assert_eq!(hb.status(), StatusCode::BAD_REQUEST);

    let rc = authed_post(
        &state,
        &key,
        &client_id,
        "/plans/bad.id/reclaim",
        &json!({}),
    )
    .await?;
    assert_eq!(rc.status(), StatusCode::BAD_REQUEST);
    Ok(())
}

#[tokio::test]
async fn test_heartbeat_409_when_leased_to_another_client() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;

    let job = job("job-1")?;
    let other = ClientId::try_new("someone-else")?;
    seed_leased(
        dir.path(),
        &job,
        ExpiresAt::Never,
        &other,
        Utc::now() + chrono::Duration::hours(1),
    )?;

    let resp = authed_put_empty(&state, &key, &client_id, "/plans/job-1/heartbeat").await?;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    Ok(())
}

#[tokio::test]
async fn test_heartbeat_403_when_pending() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (key, client_id) = register_pending(&state).await?;

    let resp = authed_put_empty(&state, &key, &client_id, "/plans/job-1/heartbeat").await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    Ok(())
}

#[tokio::test]
async fn test_heartbeat_401_when_unauthenticated() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let app = test_app(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/plans/job-1/heartbeat")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    Ok(())
}

/// The client's markers reflect its old profile, or do not exist yet, so
/// nothing is handed out until `queue-maintenance` clears the flag. Claim hides
/// the gate behind its usual `204`.
#[rstest]
#[case::claim("/plans/claim", StatusCode::NO_CONTENT)]
#[case::reclaim_reacquire("/plans/job-1/reclaim", StatusCode::NOT_FOUND)]
#[tokio::test]
async fn test_gated_while_pending_reindex(
    #[case] path: &str,
    #[case] gated_status: StatusCode,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;
    let job = seed_eligible(&state, dir.path(), &client_id, "job-1", ExpiresAt::Never).await?;
    state.todo_store.write_pending_reindex(&client_id).await?;

    let resp = authed_post(&state, &key, &client_id, path, &json!({})).await?;
    assert_eq!(resp.status(), gated_status);
    assert!(
        state.todo_store.get_avail_by_job(&job).await?.is_some(),
        "job must not be handed out while the gate is up"
    );

    pipette_web_testkit::clear_pending_reindex(&*state.todo_store, &client_id).await?;
    let resp = authed_post(&state, &key, &client_id, path, &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    Ok(())
}

// ── reclaim ───────────────────────────────────────────────────

#[tokio::test]
async fn test_reclaim_renews_own_held_lease() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;

    // An expired but still-held lease, the canonical outage recovery: renewed
    // without an expiry check on the job.
    let job = job("job-1")?;
    let old = Utc::now() - chrono::Duration::hours(1);
    seed_leased(dir.path(), &job, ExpiresAt::Never, &client_id, old)?;

    let resp = authed_post(&state, &key, &client_id, "/plans/job-1/reclaim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    // 200 carries an empty body, not the renewal's locator.
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await?;
    assert!(body.is_empty(), "reclaim 200 body should be empty");

    let renewed = lease_expiry(dir.path(), &job, &client_id)?;
    assert!(
        renewed > old && renewed > Utc::now(),
        "lease expiry {renewed} should have advanced into the future"
    );
    Ok(())
}

/// Failing means the expiry check, meant for the re-acquire path only, yanks a
/// job in progress.
#[tokio::test]
async fn test_reclaim_renews_a_held_lease_past_the_jobs_deadline() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;

    // Truncated to the second, the precision the lease key carries. No avail/
    // entry and no marker, so only the in-progress path can answer 200.
    let past = ExpiresAt::At((Utc::now() - chrono::Duration::hours(1)).trunc_subsecs(0));
    let job = job("job-1")?;
    seed_leased(
        dir.path(),
        &job,
        past,
        &client_id,
        Utc::now() - chrono::Duration::hours(1),
    )?;

    let resp = authed_post(&state, &key, &client_id, "/plans/job-1/reclaim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await?;
    assert!(body.is_empty(), "reclaim 200 body should be empty");

    let leased = state.todo_store.list_leased().await?;
    assert_eq!(leased.len(), 1);
    let lease = parse_leased_key(&leased[0])?;
    assert_eq!(lease.job_id, job);
    assert_eq!(lease.client_id, client_id);
    assert!(
        lease.lease_expiry > Utc::now(),
        "lease expiry {} should have advanced into the future",
        lease.lease_expiry
    );
    assert_eq!(
        lease.expires_at, past,
        "the job's deadline rides across the renewal"
    );
    Ok(())
}

#[tokio::test]
async fn test_reclaim_reacquires_job_from_avail() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;

    // Recycled to avail/, but the client is still eligible.
    let job = seed_eligible(&state, dir.path(), &client_id, "job-1", ExpiresAt::Never).await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/job-1/reclaim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    // Empty: the client has the job from its original claim.
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await?;
    assert!(body.is_empty(), "reclaim 200 body should be empty");

    let leased = state.todo_store.list_leased().await?;
    assert_eq!(leased.len(), 1);
    let lease = parse_leased_key(&leased[0])?;
    assert_eq!(lease.job_id, job);
    assert_eq!(lease.client_id, client_id);
    Ok(())
}

#[tokio::test]
async fn test_reclaim_409_when_leased_to_another_client() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;

    let job = job("job-1")?;
    let other = ClientId::try_new("someone-else")?;
    seed_leased(
        dir.path(),
        &job,
        ExpiresAt::Never,
        &other,
        Utc::now() + chrono::Duration::hours(1),
    )?;
    // Even with an eligible marker, the live foreign lease wins.
    state
        .todo_store
        .write_eligible(&client_id, &job, ExpiresAt::Never)
        .await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/job-1/reclaim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    Ok(())
}

#[tokio::test]
async fn test_reclaim_404_for_an_unknown_job() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/ghost/reclaim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    Ok(())
}

/// If this failed, any client could take a job by id.
#[tokio::test]
async fn test_reclaim_404_when_not_eligible() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;
    let (other_key, other) = register_and_approve(&state).await?;
    let job = seed_eligible(&state, dir.path(), &other, "job-1", ExpiresAt::Never).await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/job-1/reclaim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert!(
        state.todo_store.get_avail_by_job(&job).await?.is_some(),
        "the job stays in avail/"
    );
    assert!(state.todo_store.list_leased().await?.is_empty());

    let resp = authed_post(
        &state,
        &other_key,
        &other,
        "/plans/job-1/reclaim",
        &json!({}),
    )
    .await?;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "the eligible client takes it"
    );
    Ok(())
}

#[tokio::test]
async fn test_reclaim_404_when_gone_from_avail() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;

    let job = job("ghost")?;
    state
        .todo_store
        .write_eligible(&client_id, &job, ExpiresAt::Never)
        .await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/ghost/reclaim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn test_reclaim_404_when_job_expired() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;

    let expired = ExpiresAt::At(Utc::now() - chrono::Duration::hours(1));
    let job = seed_eligible(&state, dir.path(), &client_id, "job-1", expired).await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/job-1/reclaim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    // The job is left untouched in avail/ for queue-maintenance to expire.
    assert!(state.todo_store.list_leased().await?.is_empty());
    assert!(state.todo_store.get_avail(&job, expired).await?.is_some());
    Ok(())
}

#[tokio::test]
async fn test_reclaim_404_when_denied_for_client() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;

    let job = seed_eligible(&state, dir.path(), &client_id, "job-1", ExpiresAt::Never).await?;
    state
        .todo_store
        .write_denied(&job, &client_id, &DenialRecord::default())
        .await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/job-1/reclaim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn test_reclaim_403_when_pending() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (key, client_id) = register_pending(&state).await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/job-1/reclaim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    Ok(())
}

/// Failing means the refusal reads like the pending or permission `403`, or the
/// gate fired after the renewal, so a suspended client's leases never lapse.
#[rstest]
#[case::heartbeat(Method::PUT, "/plans/job-1/heartbeat")]
#[case::reclaim(Method::POST, "/plans/job-1/reclaim")]
#[tokio::test]
async fn test_renewal_403_when_suspended(
    #[case] method: Method,
    #[case] path: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;

    // `403`, not the `204` a claim gets, though the client holds a lease.
    let job = job("job-1")?;
    seed_leased(
        dir.path(),
        &job,
        ExpiresAt::Never,
        &client_id,
        Utc::now() + chrono::Duration::hours(1),
    )?;
    let before = lease_expiry(dir.path(), &job, &client_id)?;
    state
        .todo_store
        .write_suspension(&client_id, Utc::now(), &job)
        .await?;

    let resp = renewal(&state, &key, &client_id, &method, path).await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        body_json(resp).await?,
        json!({"error": "client is suspended"})
    );
    assert_eq!(
        lease_expiry(dir.path(), &job, &client_id)?,
        before,
        "a refused renewal must not rename the lease"
    );
    Ok(())
}

/// A client holding a live lease on `job-a` gains `job-b` by renewing a lapsed
/// lease or re-acquiring it. Failing means one client keeps several jobs.
#[rstest]
#[case::heartbeat_lapsed_lease(Method::PUT, "/plans/job-b/heartbeat", true, StatusCode::FORBIDDEN)]
#[case::reclaim_lapsed_lease(Method::POST, "/plans/job-b/reclaim", true, StatusCode::FORBIDDEN)]
#[case::reclaim_from_avail(Method::POST, "/plans/job-b/reclaim", false, StatusCode::NOT_FOUND)]
#[tokio::test]
async fn test_a_second_live_lease_suspends_the_client(
    #[case] method: Method,
    #[case] path: &str,
    #[case] lapsed_lease: bool,
    #[case] status: StatusCode,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;
    let held = job("job-a")?;
    seed_leased(
        dir.path(),
        &held,
        ExpiresAt::Never,
        &client_id,
        Utc::now() + chrono::Duration::hours(1),
    )?;
    let second = if lapsed_lease {
        let second = job("job-b")?;
        seed_leased(
            dir.path(),
            &second,
            ExpiresAt::Never,
            &client_id,
            Utc::now() - chrono::Duration::minutes(1),
        )?;
        second
    } else {
        seed_eligible(&state, dir.path(), &client_id, "job-b", ExpiresAt::Never).await?
    };

    let resp = renewal(&state, &key, &client_id, &method, path).await?;
    assert_eq!(resp.status(), status);
    let suspension = state
        .todo_store
        .read_suspension(&client_id)
        .await?
        .context("a client holding two live leases is suspended")?;
    assert_eq!(suspension.conflicting_job_id, held);
    if !lapsed_lease {
        assert!(
            state.todo_store.get_avail_by_job(&second).await?.is_some(),
            "the second grant goes back to avail/"
        );
    }

    // Suspended, the client can no longer keep the first lease alive.
    let resp = authed_put_empty(&state, &key, &client_id, "/plans/job-a/heartbeat").await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    Ok(())
}

#[tokio::test]
async fn test_reclaim_401_when_unauthenticated() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let app = test_app(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/plans/job-1/reclaim")
                .header("Content-Type", "application/json")
                .body(Body::from("{}"))?,
        )
        .await?;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    Ok(())
}

// ─── job-completion teardown (POST /benchmarks) ──────────────────────────────

/// In the server-minted shape (`job-{uuid}`) a real claim carries.
const TEARDOWN_JOB_ID: &str = "job-550e8400-e29b-41d4-a716-446655440000";

/// Generated, not a hardcoded date that would rot into the past.
fn future_expiry() -> DateTime<Utc> {
    Utc::now() + chrono::Duration::hours(1)
}

fn prefill_success_body(job_id: &str) -> Value {
    json!({
        "benchmark_id": "prefill_throughput_256",
        "job_id": job_id,
        "device_name": "d",
        "device_form_factor": "embedded",
        "device_os_name": "Linux",
        "device_os_version": "1",
        "device_chip_model": "c",
        "device_ram_bytes": 16_000_000_000_i64,
        "model_name": "m",
        "model_quant": "q",
        "runtime_name": "rt",
        "runtime_version": "v1",
        "prefill_time_ms": 10.0,
    })
}

/// Else `queue-maintenance` would recycle the lease into `avail/` and hand the
/// finished job out again.
#[tokio::test]
async fn test_submit_success_tears_down_own_lease() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;
    let job = job(TEARDOWN_JOB_ID)?;
    seed_leased(
        dir.path(),
        &job,
        ExpiresAt::Never,
        &client_id,
        future_expiry(),
    )?;

    let resp = authed_post(
        &state,
        &key,
        &client_id,
        "/benchmarks",
        &prefill_success_body(TEARDOWN_JOB_ID),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    assert!(
        state.todo_store.list_leased().await?.is_empty(),
        "lease should be deleted on completion"
    );
    Ok(())
}

/// `verify_claim` authorizes against `leased/`, so a late submit after a recycle
/// is a `404` and the client must reclaim; the `avail/` entry stays.
#[tokio::test]
async fn test_submit_recycled_job_without_lease_returns_404() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;
    seed_avail(dir.path(), TEARDOWN_JOB_ID, &job_body(TEARDOWN_JOB_ID))?;

    let resp = authed_post(
        &state,
        &key,
        &client_id,
        "/benchmarks",
        &prefill_success_body(TEARDOWN_JOB_ID),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let avail = state.todo_store.list_avail().await?;
    assert_eq!(
        avail.len(),
        1,
        "recycled avail/ entry must be left for reclaim"
    );
    Ok(())
}

/// `denied/` markers affect only claim eligibility and are `queue-maintenance`'s
/// to reconcile. The held lease lets the submission reach teardown.
#[tokio::test]
async fn test_submit_success_leaves_denied_markers() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;
    let job = job(TEARDOWN_JOB_ID)?;
    seed_leased(
        dir.path(),
        &job,
        ExpiresAt::Never,
        &client_id,
        future_expiry(),
    )?;
    let other = ClientId::try_new("ev1_other")?;
    state
        .todo_store
        .write_denied(&job, &other, &DenialRecord::default())
        .await?;

    let resp = authed_post(
        &state,
        &key,
        &client_id,
        "/benchmarks",
        &prefill_success_body(TEARDOWN_JOB_ID),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    let denied = denied_clients(&state, &job).await?;
    assert_eq!(
        denied,
        vec![other],
        "denied/ markers must survive completion"
    );
    Ok(())
}
