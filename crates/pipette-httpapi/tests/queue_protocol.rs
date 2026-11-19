//! The queue protocol a client runs: claim, heartbeat, reclaim and the
//! `POST /benchmarks` teardown, tested together as one conversation.

#![expect(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    reason = "tests build the requests a client sends, so they keep `json!` (see clippy.toml)"
)]

use anyhow::Context;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use futures::stream::{self, StreamExt, TryStreamExt};
use jiff::Timestamp;
use rstest::rstest;
use serde_json::{Value, json};
use tower::ServiceExt;

use pipette_core::denial::DenialRecord;
use pipette_core::todo_filename::Lease;
use pipette_httpapi::state::AppState;
use pipette_plan_types::{ClientId, DeviceProfile, ExpiresAt, JobId};
use pipette_store::{ClaimAffinity, ClaimOutcome, ClaimResult};
use pipette_web_testkit::{
    approved_client, approved_queue_client, auth_headers, authed_post, authed_put_empty, body_json,
    commit_job, job, make_state, raw_request, register_and_approve, register_pending,
    set_matching_input, setup_benchmarks, test_app,
};

async fn seed_avail(state: &AppState, job_id: &str, body: &Value) -> anyhow::Result<JobId> {
    pipette_web_testkit::seed_avail(state.todo_store.as_ref(), job_id, ExpiresAt::Never, body).await
}

/// The marker's expiry matches the `avail/` key's, as `queue-maintenance` writes
/// it: the claim handler addresses the rename target by it.
async fn seed_eligible(
    state: &AppState,
    client_id: &ClientId,
    job_id: &str,
    expires_at: ExpiresAt,
) -> anyhow::Result<JobId> {
    seed_eligible_body(state, client_id, job_id, expires_at, &job_body(job_id)).await
}

/// Like [`seed_eligible`], with a caller-supplied body. The job belongs to a
/// committed plan, as a filed job does.
async fn seed_eligible_body(
    state: &AppState,
    client_id: &ClientId,
    job_id: &str,
    expires_at: ExpiresAt,
    body: &Value,
) -> anyhow::Result<JobId> {
    let job = pipette_web_testkit::seed_avail(state.todo_store.as_ref(), job_id, expires_at, body)
        .await?;
    commit_job(state, &job).await?;
    state
        .todo_store
        .write_eligible(client_id, &job, expires_at)
        .await?;
    Ok(job)
}

async fn seed_leased(
    state: &AppState,
    job: &JobId,
    expires_at: ExpiresAt,
    client: &ClientId,
    expiry: jiff::Timestamp,
) -> anyhow::Result<()> {
    seed_leased_body(state, job, expires_at, client, expiry, &json!({})).await
}

/// Like [`seed_leased`], with a caller-supplied body.
async fn seed_leased_body(
    state: &AppState,
    job: &JobId,
    expires_at: ExpiresAt,
    client: &ClientId,
    expiry: jiff::Timestamp,
    body: &Value,
) -> anyhow::Result<()> {
    pipette_web_testkit::seed_leased_until(
        state.todo_store.as_ref(),
        job.as_str(),
        expires_at,
        client,
        expiry,
        body,
    )
    .await?;
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
    let Some((mut queue, key, client_id)) = approved_queue_client(dir.path()).await? else {
        return Ok(());
    };
    let observed = pipette_store_testkit::ObservedTodoStore::wrap(&queue.state.todo_store);
    queue.state.todo_store = observed.clone();
    let state = &queue.state;

    let job = seed_eligible(state, &client_id, "job-1", ExpiresAt::Never).await?;

    let resp = authed_post(state, &key, &client_id, "/plans/claim", &json!({})).await?;
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
    let read_client_leases = observed
        .take_reads()
        .iter()
        .any(|(method, _)| *method == "leases_for_client");
    assert!(
        !read_client_leases,
        "the handler must leave lease discovery to the store's claim operation"
    );

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

    let leased = state.todo_store.leases_for_client(&client_id).await?;
    assert_eq!(leased.len(), 1);
    let lease = &leased[0];
    assert_eq!(lease.job_id, job);
    assert_eq!(lease.client_id, client_id);
    observed.unexpected.none()?;
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
    seed_eligible_body(&state, &client_id, "job-1", ExpiresAt::Never, &stored).await?;

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
        |rt| -> anyhow::Result<(StatusCode, Value, Vec<Lease>)> {
            rt.block_on(async {
                let state = make_state(dir.path()).await?;
                let (key, client_id) = register_and_approve(&state).await?;
                seed_eligible_body(&state, &client_id, "job-1", ExpiresAt::Never, &stored).await?;

                let resp =
                    authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
                let status = resp.status();
                let body = body_json(resp).await?;
                Ok((status, body, state.todo_store.list_leases().await?))
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
    assert_eq!(leased[0].job_id.as_str(), "job-1");
    assert!(
        logs.contains("claim envelope is undecodable"),
        "the degraded envelope went unlogged:\n{logs}"
    );
    Ok(())
}

#[tokio::test]
async fn test_claim_skips_a_stale_marker_whose_job_is_gone() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;

    // Committed, so the claim reaches the rename and misses there.
    let gone = job("job-gone")?;
    commit_job(&state, &gone).await?;
    state
        .todo_store
        .write_eligible(&client_id, &gone, ExpiresAt::Never)
        .await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert!(state.todo_store.list_leases().await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn test_claim_falls_through_a_gone_job_to_the_next_candidate() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;

    // A dated marker ranks ahead of a `never` one, so the gone job is tried
    // first. Committed, so the claim reaches the rename and misses there.
    let gone = job("job-gone")?;
    commit_job(&state, &gone).await?;
    state
        .todo_store
        .write_eligible(
            &client_id,
            &gone,
            ExpiresAt::At(Timestamp::now() + jiff::SignedDuration::from_hours(1)),
        )
        .await?;
    let real = seed_eligible(&state, &client_id, "job-real", ExpiresAt::Never).await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await?["job_id"], "job-real");

    let leased = state.todo_store.list_leases().await?;
    assert_eq!(leased.len(), 1);
    assert_eq!(leased[0].job_id, real);
    Ok(())
}

/// Failing means the retry under the job's current key leases an expired job.
#[tokio::test]
async fn test_claim_204_when_the_marker_is_live_but_the_key_has_lapsed() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;

    // Truncated to the second, the precision the stored spelling carries.
    let now = Timestamp::now().round(
        jiff::TimestampRound::new()
            .smallest(jiff::Unit::Second)
            .mode(jiff::RoundMode::Trunc),
    )?;
    let lapsed = ExpiresAt::At(now - jiff::SignedDuration::from_hours(1));
    let job = pipette_web_testkit::seed_avail(
        state.todo_store.as_ref(),
        "job-1",
        lapsed,
        &job_body("job-1"),
    )
    .await?;
    commit_job(&state, &job).await?;
    state
        .todo_store
        .write_eligible(
            &client_id,
            &job,
            ExpiresAt::At(now + jiff::SignedDuration::from_hours(30 * 24)),
        )
        .await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert!(state.todo_store.list_leases().await?.is_empty());
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
    /// form.
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
    let deadline = (Timestamp::now() + jiff::SignedDuration::from_hours(24)).round(
        jiff::TimestampRound::new()
            .smallest(jiff::Unit::Second)
            .mode(jiff::RoundMode::Trunc),
    )?;
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
            body["expires_at"] = json!(deadline.strftime("%Y-%m-%dT%H:%M:%SZ").to_string());
        }
        BodyExpiry::Unparseable => body["expires_at"] = json!("soon"),
        BodyExpiry::NonString => body["expires_at"] = json!(42),
    }
    seed_eligible_body(&state, &client_id, "job-1", key_expiry, &body).await?;

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
    seed_avail(&state, "job-1", &job_body("job-1")).await?;

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
async fn test_claim_idempotent_when_holding_single_lease() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some((queue, key, client_id)) = approved_queue_client(dir.path()).await? else {
        return Ok(());
    };
    let state = &queue.state;

    // The selected expiry gives the lease 90 seconds of life. The response gives the
    // window in seconds as `PTnS`.
    let job_a = job("job-a")?;
    let expiry = Timestamp::now() + jiff::SignedDuration::from_secs(90);
    seed_leased_body(
        state,
        &job_a,
        ExpiresAt::Never,
        &client_id,
        expiry,
        &job_body("job-a"),
    )
    .await?;

    // A second eligible job exists; a client holding a lease must not be handed it.
    seed_eligible(state, &client_id, "job-b", ExpiresAt::Never).await?;

    let leased_before = state.todo_store.list_leases().await?;

    // A lost claim response leaves the client no job_id to reclaim by, so a
    // re-poll hands the same job back.
    let resp = authed_post(state, &key, &client_id, "/plans/claim", &json!({})).await?;
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
    assert_eq!(state.todo_store.list_leases().await?, leased_before);
    Ok(())
}

/// Once per lease, so a client that crashes and re-polls inside its lease is
/// still bounded. At `plan_lost_lease_limit` (3) the claim withholds the job.
#[tokio::test]
async fn test_claim_hand_back_counts_the_held_lease_as_lost() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some((queue, key, client_id)) = approved_queue_client(dir.path()).await? else {
        return Ok(());
    };
    let state = &queue.state;
    let job_a = job("job-a")?;
    seed_leased_body(
        state,
        &job_a,
        ExpiresAt::Never,
        &client_id,
        Timestamp::now() + jiff::SignedDuration::from_secs(90),
        &job_body("job-a"),
    )
    .await?;
    // One loss on an earlier lease.
    state
        .todo_store
        .write_lost_lease(
            &job_a,
            &client_id,
            Timestamp::now() - jiff::SignedDuration::from_hours(1),
        )
        .await?;

    // The second loss; a repeat poll on the same lease adds nothing.
    for _ in 0..2 {
        let resp = authed_post(state, &key, &client_id, "/plans/claim", &json!({})).await?;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_json(resp).await?["job_id"], "job-a");
    }
    assert_eq!(state.todo_store.list_lost_leases(&job_a).await?.len(), 2);

    // A heartbeat renews the lease, so the next re-poll is a third loss.
    let resp = authed_put_empty(state, &key, &client_id, "/plans/job-a/heartbeat").await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = authed_post(state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert_eq!(state.todo_store.list_lost_leases(&job_a).await?.len(), 3);
    assert_eq!(state.todo_store.list_leases().await?.len(), 1);
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
        &state,
        &job_a,
        ExpiresAt::Never,
        &client_id,
        Timestamp::now() + jiff::SignedDuration::from_secs(90),
        &job_body("job-a"),
    )
    .await?;

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
    client: &ClientId,
    body: &Value,
) -> anyhow::Result<()> {
    match path {
        ClaimPath::Fresh => {
            seed_eligible_body(state, client, "job-v", ExpiresAt::Never, body).await?;
        }
        ClaimPath::HandBack => {
            seed_leased_body(
                state,
                &job("job-v")?,
                ExpiresAt::Never,
                client,
                Timestamp::now() + jiff::SignedDuration::from_secs(90),
                body,
            )
            .await?
        }
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
    seed_on_path(path, &state, &client_id, &versioned_job_body()).await?;

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
    seed_on_path(path, &state, &client_id, &versioned_job_body()).await?;
    let leased_before = state.todo_store.list_leases().await?;
    state.tag_store = std::sync::Arc::new(pipette_store_testkit::TagStoreUnimplemented);

    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR, "{path:?}");
    assert_eq!(state.todo_store.list_leases().await?, leased_before);
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
        KeyExpiry::Dated => ExpiresAt::At(
            (Timestamp::now() + jiff::SignedDuration::from_hours(24)).round(
                jiff::TimestampRound::new()
                    .smallest(jiff::Unit::Second)
                    .mode(jiff::RoundMode::Trunc),
            )?,
        ),
    };
    seed_leased_body(
        &state,
        &job("job-a")?,
        expires_at,
        &client_id,
        Timestamp::now() + jiff::SignedDuration::from_secs(90),
        &job_body("job-a"),
    )
    .await?;
    let leased_before = state.todo_store.list_leases().await?;

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
    assert_eq!(state.todo_store.list_leases().await?, leased_before);
    Ok(())
}

/// Two live leases on two jobs is reachable only on a backend without the claim
/// transaction. The claim hands back the later lease and grants nothing new.
#[tokio::test]
async fn test_claim_hands_back_the_later_of_two_held_jobs() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;
    let now = Timestamp::now();
    for (job_id, secs) in [("job-a", 60), ("job-b", 120)] {
        seed_leased_body(
            &state,
            &job(job_id)?,
            ExpiresAt::Never,
            &client_id,
            now + jiff::SignedDuration::from_secs(secs),
            &job_body(job_id),
        )
        .await?;
    }
    seed_eligible(&state, &client_id, "job-c", ExpiresAt::Never).await?;
    let leased_before = state.todo_store.list_leases().await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await?["job_id"], "job-b");
    assert_eq!(state.todo_store.list_leases().await?, leased_before);
    Ok(())
}

#[tokio::test]
async fn test_claim_skips_job_denied_for_client() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some((queue, key, client_id)) = approved_queue_client(dir.path()).await? else {
        return Ok(());
    };
    let state = &queue.state;

    let job = seed_eligible(state, &client_id, "job-1", ExpiresAt::Never).await?;
    state
        .todo_store
        .write_denied(&job, &client_id, &DenialRecord::default())
        .await?;

    let resp = authed_post(state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    Ok(())
}

#[tokio::test]
async fn test_claim_skips_job_leased_to_another_client() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some((queue, key, client_id)) = approved_queue_client(dir.path()).await? else {
        return Ok(());
    };
    let state = &queue.state;
    // Eligibility can outlive another client's claim. The atomic claim must
    // refuse that job without scanning every client's leases.
    let job = seed_eligible(state, &client_id, "job-1", ExpiresAt::Never).await?;
    let other = ClientId::try_new("someone-else")?;
    assert!(matches!(
        state
            .todo_store
            .claim_job(
                &job,
                ExpiresAt::Never,
                &other,
                Timestamp::now() + jiff::SignedDuration::from_hours(1)
            )
            .await?,
        pipette_store::ClaimResult::Claimed(_)
    ));

    let resp = authed_post(state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let leases = state.todo_store.leases_for_jobs(&[job]).await?;
    assert_eq!(leases.len(), 1);
    assert_eq!(leases[0].client_id, other);
    Ok(())
}

/// What the lapsed lease's job carries before the claim.
#[derive(Clone, Copy, Debug)]
enum LapsedJob {
    Fresh,
    Recorded,
    /// Two losses already: the lapse is the third, the default limit.
    OneLossShort,
}

/// A lapsed own lease is a timed-out device, not a double-claim. The claim
/// settles it as `queue-maintenance` does: a job with a record loses its
/// lease, a job at the lost-lease limit retires, and any other job goes back
/// to the queue with one more lost lease.
#[rstest]
#[tokio::test]
async fn test_claim_settles_a_lapsed_own_lease(
    #[values(LapsedJob::Fresh, LapsedJob::Recorded, LapsedJob::OneLossShort)] lapsed: LapsedJob,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some((queue, key, client_id)) = approved_queue_client(dir.path()).await? else {
        return Ok(());
    };
    let state = &queue.state;
    let old_job = job("old-job")?;
    let lapsed_at = Timestamp::now() - jiff::SignedDuration::from_hours(1);
    seed_leased_body(
        state,
        &old_job,
        ExpiresAt::Never,
        &client_id,
        lapsed_at,
        &job_body("old-job"),
    )
    .await?;
    match lapsed {
        LapsedJob::Fresh => {}
        LapsedJob::Recorded => {
            state
                .submission_store
                .write_incoming(&old_job, &json!({"job_id": "old-job"}))
                .await?;
        }
        LapsedJob::OneLossShort => {
            for hours in [2, 3] {
                state
                    .todo_store
                    .write_lost_lease(
                        &old_job,
                        &client_id,
                        lapsed_at - jiff::SignedDuration::from_hours(hours),
                    )
                    .await?;
            }
        }
    }
    seed_eligible(state, &client_id, "job-1", ExpiresAt::Never).await?;

    let resp = authed_post(state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await?["job_id"], "job-1");
    let held: Vec<JobId> = state
        .todo_store
        .leases_for_client(&client_id)
        .await?
        .into_iter()
        .map(|lease| lease.job_id)
        .collect();
    assert_eq!(held, [job("job-1")?]);
    let (requeued, losses) = match lapsed {
        LapsedJob::Fresh => (true, 1),
        LapsedJob::Recorded => (false, 0),
        // The database queue deletes a retired job's lost leases with its row.
        LapsedJob::OneLossShort => (false, 0),
    };
    assert_eq!(
        state.todo_store.get_avail_by_job(&old_job).await?.is_some(),
        requeued
    );
    assert_eq!(
        state.todo_store.list_lost_leases(&old_job).await?.len(),
        losses
    );
    assert_eq!(
        state.submission_store.has_record(&old_job).await?,
        !requeued
    );
    if matches!(lapsed, LapsedJob::OneLossShort) {
        let record = state
            .submission_store
            .get_submission(&old_job)
            .await?
            .context("the third loss retires the job")?;
        assert_eq!(record.body["failure_source"], "lease_lost");
    }
    Ok(())
}

#[tokio::test]
async fn test_claim_ignores_denial_by_another_client() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;

    let job = seed_eligible(&state, &client_id, "job-1", ExpiresAt::Never).await?;
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

    let job_a = seed_eligible(&state, &client_id, "job-a", ExpiresAt::Never).await?;
    let job_b = seed_eligible(&state, &client_id, "job-b", ExpiresAt::Never).await?;
    state
        .todo_store
        .write_denied(&job_a, &client_id, &DenialRecord::default())
        .await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["job_id"], "job-b");

    let leased = state.todo_store.list_leases().await?;
    assert_eq!(leased.len(), 1);
    assert_eq!(leased[0].job_id, job_b);
    Ok(())
}

#[tokio::test]
async fn test_claim_prefers_soonest_expiring_job() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;

    // The eligible listing is unordered. Both dates are in the future, so neither
    // is filtered as expired.
    let soon = ExpiresAt::At(Timestamp::now() + jiff::SignedDuration::from_hours(1));
    let later = ExpiresAt::At(Timestamp::now() + jiff::SignedDuration::from_hours(30 * 24));

    seed_eligible(&state, &client_id, "job-never", ExpiresAt::Never).await?;
    seed_eligible(&state, &client_id, "job-later", later).await?;
    seed_eligible(&state, &client_id, "job-soon", soon).await?;

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
    let soon = ExpiresAt::At(Timestamp::now() + jiff::SignedDuration::from_hours(1));
    let later = ExpiresAt::At(Timestamp::now() + jiff::SignedDuration::from_hours(30 * 24));

    let mut tier: Vec<JobId> = Vec::new();
    for id in ["soon-a", "soon-b", "soon-c"] {
        tier.push(seed_eligible(&state, &client_id, id, soon).await?);
    }
    seed_eligible(&state, &client_id, "job-later", later).await?;

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
    let expired = ExpiresAt::At(Timestamp::now() - jiff::SignedDuration::from_hours(1));
    let job = seed_eligible(&state, &client_id, "job-1", expired).await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    assert!(state.todo_store.list_leases().await?.is_empty());
    assert!(state.todo_store.get_avail(&job, expired).await?.is_some());
    Ok(())
}

#[tokio::test]
async fn test_claim_skips_expired_job_and_claims_valid_one() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;

    // An expired job ranks soonest, so without the expiry filter it would win.
    let expired = ExpiresAt::At(Timestamp::now() - jiff::SignedDuration::from_hours(1));
    let valid = ExpiresAt::At(Timestamp::now() + jiff::SignedDuration::from_hours(7 * 24));
    seed_eligible(&state, &client_id, "job-expired", expired).await?;
    let good = seed_eligible(&state, &client_id, "job-valid", valid).await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["job_id"], "job-valid");

    let leased = state.todo_store.list_leases().await?;
    assert_eq!(leased.len(), 1);
    assert_eq!(leased[0].job_id, good);
    Ok(())
}

// ── heartbeat ────────────────────────────────────────────────

async fn lease_expiry(
    state: &AppState,
    job: &JobId,
    client: &ClientId,
) -> anyhow::Result<Timestamp> {
    state
        .todo_store
        .leases_for_client(client)
        .await?
        .into_iter()
        .find(|lease| lease.job_id == *job)
        .map(|lease| lease.lease_expiry)
        .ok_or_else(|| anyhow::anyhow!("no lease for {} held by {}", job.as_str(), client.as_str()))
}

#[tokio::test]
async fn test_heartbeat_renews_active_lease() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some((queue, key, client_id)) = approved_queue_client(dir.path()).await? else {
        return Ok(());
    };
    let state = &queue.state;

    let job = job("job-1")?;
    let old = Timestamp::now() + jiff::SignedDuration::from_secs(30);
    seed_leased(state, &job, ExpiresAt::Never, &client_id, old).await?;

    let resp = authed_put_empty(state, &key, &client_id, "/plans/job-1/heartbeat").await?;
    assert_eq!(resp.status(), StatusCode::OK);
    // 200 carries an empty body, not the renewal's locator.
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await?;
    assert!(body.is_empty(), "heartbeat 200 body should be empty");

    let renewed = lease_expiry(state, &job, &client_id).await?;
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
    let past = ExpiresAt::At(
        (Timestamp::now() - jiff::SignedDuration::from_hours(1)).round(
            jiff::TimestampRound::new()
                .smallest(jiff::Unit::Second)
                .mode(jiff::RoundMode::Trunc),
        )?,
    );
    let job = job("job-1")?;
    let old = Timestamp::now() + jiff::SignedDuration::from_secs(30);
    seed_leased(&state, &job, past, &client_id, old).await?;

    let resp = authed_put_empty(&state, &key, &client_id, "/plans/job-1/heartbeat").await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await?;
    assert!(body.is_empty(), "heartbeat 200 body should be empty");

    let leased = state.todo_store.list_leases().await?;
    assert_eq!(leased.len(), 1);
    let lease = &leased[0];
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
#[case::heartbeat(
    jiff::SignedDuration::from_secs(30),
    Method::PUT,
    "/plans/job-1/heartbeat"
)]
#[case::reclaim_held_lease(-jiff::SignedDuration::from_hours(1), Method::POST, "/plans/job-1/reclaim")]
#[tokio::test]
async fn test_renewal_gated_while_pending_reindex(
    #[case] lease_offset: jiff::SignedDuration,
    #[case] method: Method,
    #[case] path: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;
    let job = job("job-1")?;
    seed_leased(
        &state,
        &job,
        ExpiresAt::Never,
        &client_id,
        Timestamp::now() + lease_offset,
    )
    .await?;
    let before = lease_expiry(&state, &job, &client_id).await?;
    state.todo_store.write_pending_reindex(&client_id).await?;

    let resp = renewal(&state, &key, &client_id, &method, path).await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        lease_expiry(&state, &job, &client_id).await?,
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
        &state,
        &job,
        ExpiresAt::Never,
        &other,
        Timestamp::now() + jiff::SignedDuration::from_hours(1),
    )
    .await?;

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
    let job = seed_eligible(&state, &client_id, "job-1", ExpiresAt::Never).await?;
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
    let Some((queue, key, client_id)) = approved_queue_client(dir.path()).await? else {
        return Ok(());
    };
    let state = &queue.state;

    // An expired but still-held lease, the canonical outage recovery: renewed
    // without an expiry check on the job.
    let job = job("job-1")?;
    let old = Timestamp::now() - jiff::SignedDuration::from_hours(1);
    seed_leased(state, &job, ExpiresAt::Never, &client_id, old).await?;

    let resp = authed_post(state, &key, &client_id, "/plans/job-1/reclaim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    // 200 carries an empty body, not the renewal's locator.
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await?;
    assert!(body.is_empty(), "reclaim 200 body should be empty");

    let renewed = lease_expiry(state, &job, &client_id).await?;
    assert!(
        renewed > old && renewed > Timestamp::now(),
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
    let past = ExpiresAt::At(
        (Timestamp::now() - jiff::SignedDuration::from_hours(1)).round(
            jiff::TimestampRound::new()
                .smallest(jiff::Unit::Second)
                .mode(jiff::RoundMode::Trunc),
        )?,
    );
    let job = job("job-1")?;
    seed_leased(
        &state,
        &job,
        past,
        &client_id,
        Timestamp::now() - jiff::SignedDuration::from_hours(1),
    )
    .await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/job-1/reclaim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await?;
    assert!(body.is_empty(), "reclaim 200 body should be empty");

    let leased = state.todo_store.list_leases().await?;
    assert_eq!(leased.len(), 1);
    let lease = &leased[0];
    assert_eq!(lease.job_id, job);
    assert_eq!(lease.client_id, client_id);
    assert!(
        lease.lease_expiry > Timestamp::now(),
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
    let job = seed_eligible(&state, &client_id, "job-1", ExpiresAt::Never).await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/job-1/reclaim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    // Empty: the client has the job from its original claim.
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await?;
    assert!(body.is_empty(), "reclaim 200 body should be empty");

    let leased = state.todo_store.list_leases().await?;
    assert_eq!(leased.len(), 1);
    let lease = &leased[0];
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
        &state,
        &job,
        ExpiresAt::Never,
        &other,
        Timestamp::now() + jiff::SignedDuration::from_hours(1),
    )
    .await?;
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
    let job = seed_eligible(&state, &other, "job-1", ExpiresAt::Never).await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/job-1/reclaim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert!(
        state.todo_store.get_avail_by_job(&job).await?.is_some(),
        "the job stays in avail/"
    );
    assert!(state.todo_store.list_leases().await?.is_empty());

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

    let expired = ExpiresAt::At(Timestamp::now() - jiff::SignedDuration::from_hours(1));
    let job = seed_eligible(&state, &client_id, "job-1", expired).await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/job-1/reclaim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    // The job is left untouched in avail/ for queue-maintenance to expire.
    assert!(state.todo_store.list_leases().await?.is_empty());
    assert!(state.todo_store.get_avail(&job, expired).await?.is_some());
    Ok(())
}

#[tokio::test]
async fn test_reclaim_404_when_denied_for_client() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;

    let job = seed_eligible(&state, &client_id, "job-1", ExpiresAt::Never).await?;
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

/// The claim that grants `job-a` settles the lapsed lease on `job-b`, so a
/// renewal of `job-b` cannot give the client a second live lease.
#[rstest]
#[tokio::test]
async fn test_a_settled_lapsed_lease_is_not_renewed_beside_a_grant(
    #[values((Method::PUT, "/plans/job-b/heartbeat"), (Method::POST, "/plans/job-b/reclaim"))]
    renewal_route: (Method, &str),
) -> anyhow::Result<()> {
    let (method, path) = renewal_route;
    let dir = tempfile::tempdir()?;
    let Some((queue, key, client_id)) = approved_queue_client(dir.path()).await? else {
        return Ok(());
    };
    let state = &queue.state;
    seed_leased_body(
        state,
        &job("job-b")?,
        ExpiresAt::Never,
        &client_id,
        Timestamp::now() - jiff::SignedDuration::from_mins(1),
        &job_body("job-b"),
    )
    .await?;
    seed_eligible(state, &client_id, "job-a", ExpiresAt::Never).await?;

    let resp = authed_post(state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await?["job_id"], "job-a");

    let resp = renewal(state, &key, &client_id, &method, path).await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let held: Vec<JobId> = state
        .todo_store
        .leases_for_client(&client_id)
        .await?
        .into_iter()
        .map(|lease| lease.job_id)
        .collect();
    assert_eq!(held, [job("job-a")?]);
    Ok(())
}

/// The server refuses a reclaim of a free job while the client holds another live lease.
#[tokio::test]
async fn test_reclaim_404_while_holding_another_lease() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some((queue, key, client_id)) = approved_queue_client(dir.path()).await? else {
        return Ok(());
    };
    let state = &queue.state;
    seed_leased(
        state,
        &job("job-a")?,
        ExpiresAt::Never,
        &client_id,
        Timestamp::now() + jiff::SignedDuration::from_hours(1),
    )
    .await?;
    let second = seed_eligible(state, &client_id, "job-b", ExpiresAt::Never).await?;

    let resp = authed_post(state, &key, &client_id, "/plans/job-b/reclaim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert!(state.todo_store.get_avail_by_job(&second).await?.is_some());
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

// ─── claim affinity ──────────────────────────────────────────────────────────

/// [`job_body`], with the model file at `path`: a distinct model download.
fn job_body_with_model(job_id: &str, path: &str) -> anyhow::Result<Value> {
    let mut body = job_body(job_id);
    *body
        .pointer_mut("/spec/model/path")
        .context("the job spec has no model path")? = json!(path);
    Ok(body)
}

/// No job is seeded, so an accepted body answers `204`.
#[rstest]
#[case::no_body(None, "", StatusCode::NO_CONTENT)]
#[case::an_empty_json_body(Some("application/json"), "", StatusCode::NO_CONTENT)]
#[case::an_empty_object(Some("application/json"), "{}", StatusCode::NO_CONTENT)]
#[case::an_unknown_top_level_field(
    Some("application/json"),
    r#"{"other":1}"#,
    StatusCode::NO_CONTENT
)]
#[case::an_empty_cache(Some("application/json"), r#"{"cached":{}}"#, StatusCode::NO_CONTENT)]
#[case::a_descriptor_that_is_not_an_object(
    Some("application/json"),
    r#"{"cached":{"models":[1]}}"#,
    StatusCode::BAD_REQUEST
)]
#[case::an_unknown_cache_field(
    Some("application/json"),
    r#"{"cached":{"model":[]}}"#,
    StatusCode::BAD_REQUEST
)]
#[case::a_body_that_is_not_json(Some("text/plain"), "cached", StatusCode::BAD_REQUEST)]
#[tokio::test]
async fn test_claim_body_forms(
    #[case] content_type: Option<&str>,
    #[case] body: &str,
    #[case] expected: StatusCode,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;
    let headers = auth_headers(&key, &client_id, "POST", "/plans/claim")?;
    let resp = raw_request(
        &state,
        "POST",
        "/plans/claim",
        &headers,
        content_type,
        body.to_string(),
    )
    .await?;
    assert_eq!(resp.status(), expected);
    Ok(())
}

#[tokio::test]
async fn test_claim_refuses_too_many_cached_entries() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;
    let runtimes: Vec<Value> = (0..=256).map(|n| json!({"n": n})).collect();
    let resp = authed_post(
        &state,
        &key,
        &client_id,
        "/plans/claim",
        &json!({"cached": {"runtimes": runtimes}}),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await?;
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|e| e.contains("cached.runtimes")),
        "{body}"
    );
    Ok(())
}

#[tokio::test]
async fn test_claim_prefers_an_advertised_model() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some((queue, key, client_id)) = approved_queue_client(dir.path()).await? else {
        return Ok(());
    };
    let state = &queue.state;
    for (job, path) in [
        ("job-model-a", "a.gguf"),
        ("job-model-b", "b.gguf"),
        ("job-model-c", "c.gguf"),
    ] {
        let body = job_body_with_model(job, path)?;
        seed_eligible_body(state, &client_id, job, ExpiresAt::Never, &body).await?;
    }
    let held = job_body_with_model("job-model-b", "b.gguf")?;

    let resp = authed_post(
        state,
        &key,
        &client_id,
        "/plans/claim",
        &json!({"cached": {"models": [held["spec"]["model"]]}}),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await?["job_id"], "job-model-b");
    Ok(())
}

/// The client claims a job and ends it as `report` says. Its next claim
/// prefers the same artifacts only if the job did not fail. Every fixture job
/// has the same runtime, so a kept grant matches both artifacts.
#[rstest]
#[case::a_finished_job(None, ClaimAffinity::Both)]
#[case::a_retriable_failure(Some(true), ClaimAffinity::None)]
#[case::a_terminal_failure(Some(false), ClaimAffinity::None)]
#[tokio::test]
async fn test_a_failure_report_forgets_the_last_grant(
    #[case] report: Option<bool>,
    #[case] expected: ClaimAffinity,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some((queue, key, client_id)) = approved_queue_client(dir.path()).await? else {
        return Ok(());
    };
    let state = &queue.state;
    let first = job_body_with_model("job-first", "a.gguf")?;
    seed_eligible_body(state, &client_id, "job-first", ExpiresAt::Never, &first).await?;
    let resp = authed_post(state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await?["job_id"], "job-first");

    match report {
        Some(retriable) => {
            let failure = json!({
                "message_type": "failure",
                "benchmark_id": "eval_test",
                "job_id": "job-first",
                "retriable": retriable,
                "failure_reason": "the run stopped",
                "model_name": "lfm2-700m",
                "model_quant": "q4_0",
                "runtime_name": "llama.cpp",
                "runtime_version": "b5000",
            });
            let resp = authed_post(state, &key, &client_id, "/benchmarks", &failure).await?;
            assert_eq!(resp.status(), StatusCode::ACCEPTED);
        }
        None => {
            // A finished job's teardown deletes the lease.
            let leases = state.todo_store.leases_for_client(&client_id).await?;
            let lease = leases.first().context("the claim left no lease")?;
            state
                .todo_store
                .delete_lease(
                    &lease.job_id,
                    lease.expires_at,
                    &client_id,
                    lease.lease_expiry,
                )
                .await?;
        }
    }

    for (job, path) in [("job-same", "a.gguf"), ("job-other", "c.gguf")] {
        let body = job_body_with_model(job, path)?;
        seed_eligible_body(state, &client_id, job, ExpiresAt::Never, &body).await?;
    }
    let now = Timestamp::now();
    let outcome = state
        .todo_store
        .claim_for_client(
            &client_id,
            now + jiff::SignedDuration::from_hours(1),
            0,
            &[],
            None,
            None,
            now,
        )
        .await?;
    let ClaimOutcome::Granted { affinity, .. } = outcome else {
        anyhow::bail!("expected a grant, got {outcome:?}");
    };
    assert_eq!(affinity, expected);
    Ok(())
}

// ─── job-completion teardown (POST /benchmarks) ──────────────────────────────

/// In the server-minted shape (`job-{uuid}`) a real claim carries.
const TEARDOWN_JOB_ID: &str = "job-550e8400-e29b-41d4-a716-446655440000";

/// Generated, not a hardcoded date that would rot into the past.
fn future_expiry() -> Timestamp {
    Timestamp::now() + jiff::SignedDuration::from_hours(1)
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
    let Some((queue, key, client_id)) = approved_queue_client(dir.path()).await? else {
        return Ok(());
    };
    let state = &queue.state;
    let job = job(TEARDOWN_JOB_ID)?;
    seed_leased(state, &job, ExpiresAt::Never, &client_id, future_expiry()).await?;

    let resp = authed_post(
        state,
        &key,
        &client_id,
        "/benchmarks",
        &prefill_success_body(TEARDOWN_JOB_ID),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    assert!(
        state.todo_store.list_leases().await?.is_empty(),
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
    seed_avail(&state, TEARDOWN_JOB_ID, &job_body(TEARDOWN_JOB_ID)).await?;

    let resp = authed_post(
        &state,
        &key,
        &client_id,
        "/benchmarks",
        &prefill_success_body(TEARDOWN_JOB_ID),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let avail = state.todo_store.list_avail_entries().await?;
    assert_eq!(
        avail.len(),
        1,
        "recycled avail/ entry must be left for reclaim"
    );
    Ok(())
}

// ─── a final failure and the larger runs (POST /benchmarks) ──────────────────

/// One model's prompt sizes, queued as a family on the database queue.
const FAMILY: [(&str, i64); 4] = [
    ("job-p256", 256),
    ("job-p512", 512),
    ("job-p1024", 1024),
    ("job-p2048", 2048),
];

/// Queue [`FAMILY`] with the catalog entries it runs. The client leases the
/// shortest and another client the longest, so the two between stay queued.
async fn queue_family(
    dir: &std::path::Path,
    state: &AppState,
    client_id: &ClientId,
) -> anyhow::Result<()> {
    let todo = state.todo_store.as_ref();
    stream::iter(FAMILY)
        .map(anyhow::Ok)
        .try_for_each(|(job_id, tokens)| async move {
            let benchmark = format!("prefill_throughput_{tokens}");
            std::fs::write(
                dir.join("benchmarks").join(format!("{benchmark}.toml")),
                format!(
                    "benchmark_type = \"prefill_throughput\"\nparameter_prefill_tokens = {tokens}"
                ),
            )?;
            let job = job(job_id)?;
            todo.write_tmp(
                &job,
                &json!({"job_id": job_id, "spec": {"benchmark": benchmark}}),
            )
            .await?;
            // Prompt tokens and batch size.
            todo.promote_avail(
                &job,
                ExpiresAt::Never,
                Some(("family", [tokens, 1].as_slice())),
            )
            .await
        })
        .await?;
    stream::iter([
        ("job-p256", client_id.clone()),
        ("job-p2048", ClientId::try_new("ev1_other")?),
    ])
    .map(anyhow::Ok)
    .try_for_each(|(job_id, holder)| async move {
        let claimed = todo
            .claim_job(&job(job_id)?, ExpiresAt::Never, &holder, future_expiry())
            .await?;
        anyhow::ensure!(
            matches!(claimed, ClaimResult::Claimed(_)),
            "{job_id} is gone"
        );
        Ok(())
    })
    .await
}

/// A failure report of the smallest run.
fn smallest_failed(code: &str, retriable: bool) -> Value {
    json!({
        "message_type": "failure",
        "job_id": "job-p256",
        "benchmark_id": "prefill_throughput_256",
        "retriable": retriable,
        "failure_code": code,
        "failure_reason": "the run stopped",
    })
}

/// Post `failure` to `route`, the batch route as its one item, and check that
/// the server accepted it.
async fn report(
    state: &AppState,
    key: &ed25519_dalek::SigningKey,
    client_id: &ClientId,
    route: &str,
    failure: &Value,
) -> anyhow::Result<()> {
    if route == "/benchmarks" {
        let resp = authed_post(state, key, client_id, route, failure).await?;
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        return Ok(());
    }
    let batch = json!({"submissions": [failure]});
    let resp = authed_post(state, key, client_id, route, &batch).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let answered = body_json(resp).await?;
    assert_eq!(
        answered.pointer("/results/0/job_id"),
        Some(&json!("job-p256")),
        "{answered}"
    );
    Ok(())
}

/// A final timeout or out-of-memory report on the smallest run fails, in the
/// same request, each larger run of its family still queued, and leaves the
/// running one alone. A retriable failure, and a final one with another code,
/// fail none.
#[rstest]
#[case::a_timeout("attempt_timeout", false, true)]
#[case::out_of_memory("out_of_memory", false, true)]
#[case::a_retriable_timeout("attempt_timeout", true, false)]
#[case::a_runtime_error("runtime_error", false, false)]
#[tokio::test]
async fn test_a_final_timeout_fails_the_larger_runs_still_queued(
    #[case] code: &str,
    #[case] retriable: bool,
    #[case] fails_larger: bool,
    #[values("/benchmarks", "/benchmarks/batch")] route: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some((queue, key, client_id)) = approved_queue_client(dir.path()).await? else {
        return Ok(());
    };
    let state = &queue.state;
    queue_family(dir.path(), state, &client_id).await?;

    report(
        state,
        &key,
        &client_id,
        route,
        &smallest_failed(code, retriable),
    )
    .await?;

    let reason =
        &format!("not run: prefill_throughput_256 of this model failed with {code} in job-p256");
    stream::iter(["job-p512", "job-p1024"])
        .map(anyhow::Ok)
        .try_for_each(|queued| async move {
            let job = job(queued)?;
            let record = state.submission_store.find_job(&job).await?;
            if fails_larger {
                let record = record.with_context(|| format!("no failure recorded for {queued}"))?;
                assert_eq!(
                    (
                        &record.body["client_id"],
                        &record.body["failure_source"],
                        &record.body["failure_code"],
                        &record.body["failure_reason"]
                    ),
                    (
                        &json!("system"),
                        &json!("smaller_run_failed"),
                        &json!(code),
                        &json!(reason)
                    )
                );
            } else {
                assert!(record.is_none(), "{queued} was failed");
            }
            let in_queue = state.todo_store.get_avail_by_job(&job).await?.is_some();
            assert_eq!(in_queue, !fails_larger, "{queued}");
            Ok(())
        })
        .await?;
    let running = job("job-p2048")?;
    assert!(state.submission_store.find_job(&running).await?.is_none());
    let leases = state
        .todo_store
        .leases_for_jobs(std::slice::from_ref(&running))
        .await?;
    assert_eq!(leases.len(), 1, "{leases:?}");
    Ok(())
}

/// A store fault while the larger runs are failed is logged at `WARN`,
/// naming the job, and the report is still accepted and its job torn down.
#[rstest]
#[case::single("/benchmarks")]
#[case::batch("/benchmarks/batch")]
fn test_a_fault_failing_the_larger_runs_keeps_the_report(
    #[case] route: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (ran, logs) = pipette_web_testkit::capture_logs_at_level(tracing::Level::INFO, |rt| {
        rt.block_on(async {
            let Some((queue, key, client_id)) = approved_queue_client(dir.path()).await? else {
                return Ok(false);
            };
            let state = &queue.state;
            queue_family(dir.path(), state, &client_id).await?;
            queue
                .execute_sql(
                    "CREATE FUNCTION refuse_delete() RETURNS trigger LANGUAGE plpgsql AS \
                         $$ BEGIN RAISE EXCEPTION 'delete refused'; END $$; \
                         CREATE TRIGGER refuse_delete BEFORE DELETE ON queue_jobs \
                         FOR EACH ROW WHEN (OLD.job_id = 'job-p1024') \
                         EXECUTE FUNCTION refuse_delete();",
                )
                .await?;

            let failure = smallest_failed("attempt_timeout", false);
            report(state, &key, &client_id, route, &failure).await?;

            let reported = job("job-p256")?;
            assert!(state.submission_store.find_job(&reported).await?.is_some());
            assert!(
                state
                    .todo_store
                    .get_avail_by_job(&reported)
                    .await?
                    .is_none()
            );
            let leases = state.todo_store.leases_for_client(&client_id).await?;
            assert!(leases.is_empty(), "{leases:?}");
            let refused = job("job-p1024")?;
            assert!(state.todo_store.get_avail_by_job(&refused).await?.is_some());
            anyhow::Ok(true)
        })
    })?;
    if !ran? {
        return Ok(());
    }
    let line = logs
        .lines()
        .find(|l| l.contains("failed to fail the larger runs"))
        .with_context(|| format!("no warning in the log:\n{logs}"))?;
    assert!(line.contains("WARN"), "{line}");
    assert!(line.contains("job_id=job-p256"), "{line}");
    Ok(())
}
