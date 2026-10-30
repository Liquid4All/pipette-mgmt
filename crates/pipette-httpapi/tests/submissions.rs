//! `POST /benchmarks` and `GET /jobs/{job_id}`: recording a result and reading
//! it back. A body with a `job_id` reports a claimed job and is bound to a live
//! lease before anything is written, since `incoming/` keys are not partitioned
//! by client; a body without one is ad-hoc and the server mints its id.

#![expect(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    reason = "tests build the requests a client sends, so they keep `json!` (see clippy.toml)"
)]

use std::path::Path;

use anyhow::Context;
use axum::http::StatusCode;
use ed25519_dalek::SigningKey;
use rstest::rstest;
use serde_json::json;

use pipette_core::client_permission::ClientPermissionSet;
use pipette_httpapi::state::AppState;
use pipette_plan_types::{ClientId, ClientPermission, RuntimeOutput};
use pipette_store_testkit::TEST_LIST_LIMIT;
use pipette_web_testkit::{
    authed_get, authed_post, body_json, denied_clients, job, make_state, register_and_approve,
    seed_leased, setup_benchmarks, submit_benchmark, unauthed_post, valid_submission,
};

/// A claimed `job_id` in the server-minted shape (`job-{uuid}`).
const CLAIMED_JOB_ID: &str = "job-550e8400-e29b-41d4-a716-446655440000";

/// Plant a lease with an empty job body, for the tests that only need the
/// claim-binding check to pass.
fn seed_lease(dir: &Path, job_id: &str, client_id: &ClientId) -> anyhow::Result<()> {
    seed_leased(dir, job_id, client_id, &json!({}))?;
    Ok(())
}

#[tokio::test]
async fn test_submit_and_get_job() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let job_id = submit_benchmark(
        &state,
        &sk,
        &client_id,
        &json!({
            "benchmark_id": "prefill_throughput_256",
            "device_name": "test-device",
            "device_form_factor": "embedded",
            "device_os_name": "Linux",
            "device_os_version": "22.04",
            "device_chip_model": "test-chip",
            "device_ram_bytes": 17179869184i64,
            "model_name": "llama-3.2-1b",
            "model_quant": "q4_0",
            "model_params_total_millions": 1000,
            "runtime_name": "llama.cpp",
            "runtime_version": "b5000",
            "prefill_time_ms": 34.7
        }),
    )
    .await?;

    let resp = authed_get(&state, &sk, &client_id, &format!("/jobs/{job_id}")).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["job_id"], job_id);
    assert_eq!(body["status"], "incoming");
    // The wire named only the id; the type is resolved from the catalog.
    assert_eq!(body["benchmark_id"], "prefill_throughput_256");
    assert_eq!(body["benchmark_type"], "prefill_throughput");
    let submitted_at = body["submitted_at"]
        .as_str()
        .context("submitted_at is a string")?;
    chrono::DateTime::parse_from_rfc3339(submitted_at)
        .with_context(|| format!("submitted_at is not RFC 3339: {submitted_at}"))?;
    // An unscored job spells its score fields as explicit nulls; `get` tells a
    // missing key from a null one.
    for field in ["scored_at", "score_runtime_version", "metrics"] {
        assert_eq!(
            body.get(field),
            Some(&serde_json::Value::Null),
            "{field} must be present and null: {body}"
        );
    }
    Ok(())
}

/// A job another client submitted reads as the `404` an unknown id draws, so a
/// client can neither read nor probe for another's results.
#[tokio::test]
async fn test_another_clients_job_reads_as_not_found() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk_owner, owner) = register_and_approve(&state).await?;
    let (sk_other, other) = register_and_approve(&state).await?;

    let job_id = submit_benchmark(
        &state,
        &sk_owner,
        &owner,
        &valid_submission("prefill_throughput_256"),
    )
    .await?;
    let uri = format!("/jobs/{job_id}");

    let resp = authed_get(&state, &sk_owner, &owner, &uri).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await?["job_id"], job_id);

    let resp = authed_get(&state, &sk_other, &other, &uri).await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let foreign = body_json(resp).await?;
    assert_eq!(foreign, json!({"error": "job not found"}));

    let resp = authed_get(
        &state,
        &sk_other,
        &other,
        &format!("/jobs/{CLAIMED_JOB_ID}"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        body_json(resp).await?,
        foreign,
        "a foreign job must be indistinguishable from an unknown one"
    );
    Ok(())
}

/// Used verbatim as the storage key; any charset-safe id is accepted, not only
/// a UUID.
#[tokio::test]
async fn test_submit_with_client_supplied_job_id_is_echoed() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let claimed = CLAIMED_JOB_ID;
    seed_lease(dir.path(), claimed, &client_id)?;
    let mut body = valid_submission("prefill_throughput_256");
    body["job_id"] = json!(claimed);
    let job_id = submit_benchmark(&state, &sk, &client_id, &body).await?;
    assert_eq!(job_id, claimed);

    let resp = authed_get(&state, &sk, &client_id, &format!("/jobs/{claimed}")).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await?["job_id"], claimed);
    Ok(())
}

/// A `job_id` needs `perform_work`, an ad-hoc body `submit_results`. Each case
/// holds the other, so a check that ignored the body would let it through.
#[rstest]
#[case::claimed_job_needs_perform_work(
    true,
    ClientPermission::SubmitResults,
    "client lacks the perform_work permission"
)]
#[case::ad_hoc_needs_submit_results(
    false,
    ClientPermission::PerformWork,
    "client lacks the submit_results permission"
)]
#[tokio::test]
async fn test_submit_requires_the_permission_the_body_names(
    #[case] client_supplied_job_id: bool,
    #[case] held: ClientPermission,
    #[case] expected_error: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let mut body = valid_submission("prefill_throughput_256");
    if client_supplied_job_id {
        // Seeded so the refusal cannot be the claim-binding check instead.
        seed_lease(dir.path(), CLAIMED_JOB_ID, &client_id)?;
        body["job_id"] = json!(CLAIMED_JOB_ID);
    }

    state
        .client_store
        .put_client_permissions(&client_id, &ClientPermissionSet::from_iter([held]))
        .await?;

    let resp = authed_post(&state, &sk, &client_id, "/benchmarks", &body).await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(resp).await?["error"], expected_error);
    Ok(())
}

/// An explicit `"job_id": null` is ad-hoc, like an absent key: the server mints
/// the id with no lease to bind, and the permission it needs is
/// `submit_results`, not `perform_work`.
#[tokio::test]
async fn test_submit_with_null_job_id_is_ad_hoc() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    // No lease seeded, so a claimed-job reading would draw the claim check's 404.
    let mut body = valid_submission("prefill_throughput_256");
    body["job_id"] = serde_json::Value::Null;

    let resp = authed_post(&state, &sk, &client_id, "/benchmarks", &body).await?;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let job_id = body_json(resp).await?["job_id"]
        .as_str()
        .context("the 202 carries a job_id")?
        .to_owned();
    assert!(job_id.starts_with("job-"), "expected a minted id: {job_id}");
    assert!(
        state
            .submission_store
            .find_job(&job(&job_id)?)
            .await?
            .is_some(),
        "the minted job must be recorded"
    );

    // Holding only the claimed-job permission refuses it as ad-hoc.
    state
        .client_store
        .put_client_permissions(
            &client_id,
            &ClientPermissionSet::from_iter([ClientPermission::PerformWork]),
        )
        .await?;
    let resp = authed_post(&state, &sk, &client_id, "/benchmarks", &body).await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        body_json(resp).await?["error"],
        "client lacks the submit_results permission"
    );
    assert_eq!(
        state
            .submission_store
            .list_incoming(TEST_LIST_LIMIT)
            .await?
            .len(),
        1,
        "the refused submission must not be written"
    );
    Ok(())
}

/// The batch route reads the caller's set once and judges each item against the
/// operation that item names — so one request can hold a refused item beside an
/// accepted one, each reported at its own index.
#[tokio::test]
async fn test_batch_judges_each_item_against_its_own_operation() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    // Ad-hoc submissions only.
    state
        .client_store
        .put_client_permissions(
            &client_id,
            &ClientPermissionSet::from_iter([ClientPermission::SubmitResults]),
        )
        .await?;

    seed_lease(dir.path(), CLAIMED_JOB_ID, &client_id)?;
    let mut claimed = valid_submission("prefill_throughput_256");
    claimed["job_id"] = json!(CLAIMED_JOB_ID);

    let resp = authed_post(
        &state,
        &sk,
        &client_id,
        "/benchmarks/batch",
        &json!({ "submissions": [valid_submission("prefill_throughput_256"), claimed] }),
    )
    .await?;

    assert_eq!(resp.status(), StatusCode::OK);
    let results = body_json(resp).await?;
    assert!(
        results["results"][0]["job_id"].is_string(),
        "the ad-hoc item is accepted: {results}"
    );
    assert_eq!(
        results["results"][1]["error"], "client lacks the perform_work permission",
        "the claimed item is refused at its own index: {results}"
    );
    Ok(())
}

/// How a claimed `job_id` fails the claim-binding gate.
#[derive(Clone, Copy)]
enum UnboundClaim {
    /// No lease on the id at all.
    Unleased,
    /// The lease belongs to another client.
    LeasedElsewhere,
    /// The caller holds the lease, but its pending-reindex flag voids it.
    PendingReindex,
}

/// The batch route runs the single route's claim gate per item: an unbound
/// `job_id` is refused at its own index with the same message, nothing lands on
/// its shared key, and the ad-hoc sibling is still written.
#[rstest]
#[case::unleased(UnboundClaim::Unleased, "no active claim; reclaim before submitting")]
#[case::leased_elsewhere(UnboundClaim::LeasedElsewhere, "the job is leased to another client")]
#[case::pending_reindex(
    UnboundClaim::PendingReindex,
    "no active claim; client re-evaluation pending"
)]
#[tokio::test]
async fn test_batch_item_without_a_live_claim_is_a_per_item_error(
    #[case] claim: UnboundClaim,
    #[case] expected_error: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    match claim {
        UnboundClaim::Unleased => {}
        UnboundClaim::LeasedElsewhere => {
            seed_lease(dir.path(), CLAIMED_JOB_ID, &ClientId::try_new("ev1_other")?)?;
        }
        UnboundClaim::PendingReindex => {
            seed_lease(dir.path(), CLAIMED_JOB_ID, &client_id)?;
            state.todo_store.write_pending_reindex(&client_id).await?;
        }
    }
    let mut claimed = valid_submission("prefill_throughput_256");
    claimed["job_id"] = json!(CLAIMED_JOB_ID);

    let resp = authed_post(
        &state,
        &sk,
        &client_id,
        "/benchmarks/batch",
        &json!({ "submissions": [valid_submission("prefill_throughput_256"), claimed] }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let reported = body_json(resp).await?;
    let results = reported["results"]
        .as_array()
        .with_context(|| format!("no results array: {reported}"))?;
    let at = |index: usize| {
        results
            .iter()
            .find(|r| r["index"] == json!(index))
            .with_context(|| format!("no result for item {index}: {reported}"))
    };

    let ad_hoc = at(0)?["job_id"]
        .as_str()
        .with_context(|| format!("the ad-hoc item must be accepted: {reported}"))?;
    assert!(
        state
            .submission_store
            .find_job(&job(ad_hoc)?)
            .await?
            .is_some(),
        "the ad-hoc item must be written"
    );
    assert_eq!(*at(1)?, json!({"index": 1, "error": expected_error}));
    assert!(
        state
            .submission_store
            .find_job(&job(CLAIMED_JOB_ID)?)
            .await?
            .is_none(),
        "the refused item must not be written"
    );
    Ok(())
}

/// A present-but-malformed `job_id` is a `400` at the boundary, not a
/// silently-accepted non-UUID key.
#[tokio::test]
async fn test_submit_with_invalid_job_id_returns_400() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    // An unsafe charset, not "not a UUID": a `.` is rejected on format by
    // `JobId::try_new`, before the claim is checked.
    let mut body = valid_submission("prefill_throughput_256");
    body["job_id"] = json!("bad.id");
    let resp = authed_post(&state, &sk, &client_id, "/benchmarks", &body).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    Ok(())
}

/// A `job_id` that is present but not a string is a `400`, not read as absent
/// and minted as an ad-hoc result: nothing is written.
#[rstest]
#[case::number(json!(42))]
#[case::boolean(json!(true))]
#[case::object(json!({}))]
#[tokio::test]
async fn test_submit_non_string_job_id_returns_400(
    #[case] job_id: serde_json::Value,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let mut body = valid_submission("prefill_throughput_256");
    body["job_id"] = job_id;
    let resp = authed_post(&state, &sk, &client_id, "/benchmarks", &body).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(resp).await?["error"], "job_id must be a string");

    assert!(
        state
            .submission_store
            .list_incoming(TEST_LIST_LIMIT)
            .await?
            .is_empty(),
        "the refused submission must not be written"
    );
    Ok(())
}

/// On both job routes, before any store lookup. `bad.id` routes as one segment
/// but fails the charset.
#[tokio::test]
async fn test_get_job_invalid_job_id_in_path_is_400() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let job = authed_get(&state, &sk, &client_id, "/jobs/bad.id").await?;
    assert_eq!(job.status(), StatusCode::BAD_REQUEST);

    let samples = authed_get(&state, &sk, &client_id, "/jobs/bad.id/eval-sample-results").await?;
    assert_eq!(samples.status(), StatusCode::BAD_REQUEST);
    Ok(())
}

/// A well-formed id that names no job is a `404` with the `job not found`
/// envelope on both job routes.
#[rstest]
#[case::job("/jobs/{id}")]
#[case::eval_sample_results("/jobs/{id}/eval-sample-results")]
#[tokio::test]
async fn test_get_job_unknown_job_id_is_404(#[case] route: &str) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let uri = route.replace("{id}", CLAIMED_JOB_ID);
    let resp = authed_get(&state, &sk, &client_id, &uri).await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_json(resp).await?, json!({"error": "job not found"}));
    Ok(())
}

/// No live lease on the echoed id (recycled, completed, or never claimed): the
/// claim-binding gate stops a client clobbering another's unpartitioned key.
#[tokio::test]
async fn test_submit_with_unclaimed_job_id_returns_404() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    // No lease seeded for this client.
    let mut body = valid_submission("prefill_throughput_256");
    body["job_id"] = json!(CLAIMED_JOB_ID);
    let resp = authed_post(&state, &sk, &client_id, "/benchmarks", &body).await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    // The message is what tells this 404 from the pending-reindex one.
    assert_eq!(
        body_json(resp).await?["error"],
        "no active claim; reclaim before submitting"
    );
    assert!(
        state
            .submission_store
            .find_job(&job(CLAIMED_JOB_ID)?)
            .await?
            .is_none(),
        "the refused submission must not be written"
    );
    Ok(())
}

/// The id has only been checked for shape when the claim check refuses it, so
/// the log must not announce it accepted. The accepting case proves the event
/// still fires.
#[rstest]
#[case::rejected(false, StatusCode::NOT_FOUND, false)]
#[case::accepted(true, StatusCode::ACCEPTED, true)]
#[test]
fn accepted_submission_is_logged_only_once_the_claim_holds(
    #[case] seed_the_lease: bool,
    #[case] expected: StatusCode,
    #[case] expect_logged: bool,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;

    let (result, logs) = pipette_web_testkit::capture_logs(|rt| -> anyhow::Result<StatusCode> {
        rt.block_on(async {
            let state = make_state(dir.path()).await?;
            let (sk, client_id) = register_and_approve(&state).await?;
            if seed_the_lease {
                seed_lease(dir.path(), CLAIMED_JOB_ID, &client_id)?;
            }
            let mut body = valid_submission("prefill_throughput_256");
            body["job_id"] = json!(CLAIMED_JOB_ID);
            let resp = authed_post(&state, &sk, &client_id, "/benchmarks", &body).await?;
            Ok(resp.status())
        })
    })?;

    assert_eq!(result?, expected);
    // Registration logs this, so a missing line below is not an empty capture.
    assert!(logs.contains("client registered"), "{logs}");
    assert_eq!(
        logs.contains("accepted submission"),
        expect_logged,
        "log said the wrong thing for a {expected} response:\n{logs}"
    );
    Ok(())
}

/// The profile change that set the flag relinquished the client's leases, so
/// the claim is void, and the claim's renewal must not resurrect the lease.
#[tokio::test]
async fn test_submit_gated_while_pending_reindex() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    seed_lease(dir.path(), CLAIMED_JOB_ID, &client_id)?;
    state.todo_store.write_pending_reindex(&client_id).await?;
    let leases_before = state.todo_store.list_leased().await?;

    let mut body = valid_submission("prefill_throughput_256");
    body["job_id"] = json!(CLAIMED_JOB_ID);
    let resp = authed_post(&state, &sk, &client_id, "/benchmarks", &body).await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        body_json(resp).await?["error"],
        "no active claim; client re-evaluation pending"
    );
    assert!(
        state
            .submission_store
            .find_job(&job(CLAIMED_JOB_ID)?)
            .await?
            .is_none(),
        "the refused submission must not be written"
    );
    assert_eq!(
        state.todo_store.list_leased().await?,
        leases_before,
        "the voided lease must not be renamed"
    );

    // Clearing the flag (what the reindex pass does) lifts the gate.
    pipette_web_testkit::clear_pending_reindex(&*state.todo_store, &client_id).await?;
    let job_id = submit_benchmark(&state, &sk, &client_id, &body).await?;
    assert_eq!(job_id, CLAIMED_JOB_ID);
    Ok(())
}

/// The caller has been superseded: nothing lands on the shared key and the
/// holder's lease is untouched.
#[tokio::test]
async fn test_submit_job_id_leased_to_other_client_returns_409() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let other = ClientId::try_new("ev1_other")?;
    seed_lease(dir.path(), CLAIMED_JOB_ID, &other)?;
    let leases_before = state.todo_store.list_leased().await?;

    let mut body = valid_submission("prefill_throughput_256");
    body["job_id"] = json!(CLAIMED_JOB_ID);
    let resp = authed_post(&state, &sk, &client_id, "/benchmarks", &body).await?;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(
        body_json(resp).await?["error"],
        "the job is leased to another client"
    );
    assert!(
        state
            .submission_store
            .find_job(&job(CLAIMED_JOB_ID)?)
            .await?
            .is_none(),
        "the zombie's submission must not be written"
    );
    assert_eq!(state.todo_store.list_leased().await?, leases_before);
    Ok(())
}

/// No "already submitted" check: completing the job tears its lease down, so
/// the retry meets the same claim-binding gate an unclaimed id does.
#[tokio::test]
async fn test_resubmitting_a_completed_job_id_returns_404() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    seed_lease(dir.path(), CLAIMED_JOB_ID, &client_id)?;
    let mut body = valid_submission("prefill_throughput_256");
    body["job_id"] = json!(CLAIMED_JOB_ID);

    assert_eq!(
        submit_benchmark(&state, &sk, &client_id, &body).await?,
        CLAIMED_JOB_ID
    );

    let resp = authed_post(&state, &sk, &client_id, "/benchmarks", &body).await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        body_json(resp).await?["error"],
        "no active claim; reclaim before submitting"
    );

    let stored = state
        .submission_store
        .find_job(&job(CLAIMED_JOB_ID)?)
        .await?
        .context("the first submission's record must survive the refused retry")?;
    assert_eq!(stored.body["message_type"], "success");
    assert_eq!(stored.state.as_str(), "incoming");
    Ok(())
}

/// One lease admits one submitter, so the second write takes the shared key.
/// The completion teardown is best-effort, so a lease outliving its result is a
/// real state.
#[tokio::test]
async fn test_resubmitting_under_a_live_lease_supersedes() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    seed_lease(dir.path(), CLAIMED_JOB_ID, &client_id)?;
    let mut body = valid_submission("prefill_throughput_256");
    body["job_id"] = json!(CLAIMED_JOB_ID);
    submit_benchmark(&state, &sk, &client_id, &body).await?;

    seed_lease(dir.path(), CLAIMED_JOB_ID, &client_id)?;
    body["runtime_version"] = json!("b6000");
    submit_benchmark(&state, &sk, &client_id, &body).await?;

    let stored = state
        .submission_store
        .find_job(&job(CLAIMED_JOB_ID)?)
        .await?
        .context("expected the superseding record")?;
    assert_eq!(stored.body["runtime_version"], "b6000");
    assert_eq!(stored.state.as_str(), "incoming");
    Ok(())
}

/// The server mints a fresh id per request, so a client retrying an
/// unacknowledged ad-hoc `202` double-counts its run.
#[tokio::test]
async fn test_ad_hoc_resubmission_records_a_second_result() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let body = valid_submission("prefill_throughput_256");
    let first = submit_benchmark(&state, &sk, &client_id, &body).await?;
    let second = submit_benchmark(&state, &sk, &client_id, &body).await?;

    assert_ne!(
        first, second,
        "each ad-hoc submission must mint its own job id"
    );
    for id in [&first, &second] {
        assert!(
            state.submission_store.find_job(&job(id)?).await?.is_some(),
            "both ad-hoc submissions must be recorded ({id})"
        );
    }
    Ok(())
}

/// A failure body carrying device or measurement fields is refused with `400`,
/// naming them so the client author can find them.
#[tokio::test]
async fn test_failure_body_with_device_and_metric_fields_returns_400() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let claimed = CLAIMED_JOB_ID;
    let body = leased_job_body(claimed, &[client_id.as_str()]);
    seed_leased(dir.path(), claimed, &client_id, &body)?;

    let mut failure = valid_failure("prefill_throughput_256", claimed, false);
    failure["device_name"] = json!("test-device");
    failure["prefill_time_ms"] = json!(34.7);

    let resp = authed_post(&state, &sk, &client_id, "/benchmarks", &failure).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        body_json(resp).await?["error"],
        "failure submission must not carry device or measurement fields: \
         device_name, prefill_time_ms"
    );

    // Refused before anything is written — no half-recorded job.
    assert!(
        state
            .submission_store
            .find_job(&job(claimed)?)
            .await?
            .is_none()
    );
    Ok(())
}

/// An unknown key on the same body is still ignored, so a retired field an
/// older client still sends (here `plan_id`) does not turn into a `400`.
#[tokio::test]
async fn test_failure_body_still_ignores_unknown_fields() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let claimed = CLAIMED_JOB_ID;
    let body = leased_job_body(claimed, &[client_id.as_str()]);
    seed_leased(dir.path(), claimed, &client_id, &body)?;

    let mut failure = valid_failure("prefill_throughput_256", claimed, false);
    failure["plan_id"] = json!("release-v1-evals");

    let resp = authed_post(&state, &sk, &client_id, "/benchmarks", &failure).await?;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    let stored = state
        .submission_store
        .find_job(&job(claimed)?)
        .await?
        .context("expected the failure record")?;
    assert_eq!(stored.body["message_type"], "failure");
    assert!(
        stored.body.get("plan_id").is_none(),
        "the ignored key must not survive onto the stored record"
    );
    Ok(())
}

/// The mirror gate: a success body carrying the failure variant's own fields is
/// refused too, naming them.
#[tokio::test]
async fn test_success_body_with_failure_fields_returns_400() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let mut body = valid_submission("prefill_throughput_256");
    body["failure_reason"] = json!("runtime OOM");
    body["retriable"] = json!(true);

    let resp = authed_post(&state, &sk, &client_id, "/benchmarks", &body).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        body_json(resp).await?["error"],
        "success submission must not carry failure-only fields: failure_reason, retriable"
    );
    Ok(())
}

/// In a batch the refusal is the item's own error, and the valid item beside it
/// is still written.
#[tokio::test]
async fn test_batch_item_with_cross_variant_fields_is_a_per_item_error() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let mut mixed = valid_submission("prefill_throughput_256");
    mixed["failure_reason"] = json!("runtime OOM");

    let resp = authed_post(
        &state,
        &sk,
        &client_id,
        "/benchmarks/batch",
        &json!({"submissions": [valid_submission("prefill_throughput_256"), mixed]}),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);

    let body = body_json(resp).await?;
    let results = body["results"]
        .as_array()
        .context("missing results array")?;
    assert_eq!(results.len(), 2);
    assert!(
        results[0]["job_id"].is_string(),
        "the valid item must still be written: {}",
        results[0]
    );
    assert_eq!(
        results[1]["error"],
        json!("success submission must not carry failure-only fields: failure_reason")
    );
    Ok(())
}

/// A client may declare the `benchmark_type` it ran with its
/// `benchmark_params`, always as a pair. The catalog stays authoritative: a
/// pair that agrees with it on every field it states, or neither, is accepted,
/// and anything else is a `400`.
#[rstest]
#[case::agrees(json!("prefill_throughput"), json!({"prefill_tokens": 256, "batch_size": 1}), None)]
#[case::neither(json!(null), json!(null), None)]
#[case::type_alone(
    json!("prefill_throughput"),
    json!(null),
    Some("benchmark_type requires benchmark_params")
)]
#[case::params_alone(
    json!(null),
    json!({"prefill_tokens": 256, "batch_size": 1}),
    Some("benchmark_params requires benchmark_type")
)]
#[case::other_type(
    json!("max_memory_usage"),
    json!({"prefill_tokens": 256}),
    Some("benchmark_type max_memory_usage does not match prefill_throughput, the type of the benchmark")
)]
#[case::other_params(
    json!("prefill_throughput"),
    json!({"prefill_tokens": 512, "batch_size": 1}),
    Some("benchmark_params do not match ")
)]
#[case::omitted_param(json!("prefill_throughput"), json!({"prefill_tokens": 256}), None)]
#[case::empty_params(json!("prefill_throughput"), json!({}), None)]
// The catalog fills only its own type's fields.
#[case::narrow_other_type(
    json!("decode_throughput"),
    json!({"decode_tokens": 8}),
    Some("invalid benchmark_params: missing field `prefill_tokens`")
)]
#[case::bad_param(
    json!("prefill_throughput"),
    json!({"prefill_tokens": 256, "batch_size": 0}),
    Some("invalid benchmark_params: ")
)]
#[case::inner_type_disagrees(
    json!("prefill_throughput"),
    json!({"benchmark_type": "max_memory_usage", "prefill_tokens": 256}),
    Some("invalid benchmark_params: its benchmark_type disagrees with the body's")
)]
#[case::unknown_type(
    json!("nope"),
    json!({}),
    Some("invalid benchmark_type: unknown variant `nope`")
)]
#[case::kebab_case(
    json!("prefill-throughput"),
    json!({"prefill_tokens": 256, "batch_size": 1}),
    Some("invalid benchmark_type: unknown variant `prefill-throughput`")
)]
#[tokio::test]
async fn test_submit_declared_benchmark_params(
    #[case] benchmark_type: serde_json::Value,
    #[case] benchmark_params: serde_json::Value,
    #[case] error: Option<&str>,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let mut body = valid_submission("prefill_throughput_256");
    body["benchmark_type"] = benchmark_type;
    body["benchmark_params"] = benchmark_params;

    let resp = authed_post(&state, &sk, &client_id, "/benchmarks", &body).await?;
    match error {
        None => assert_eq!(resp.status(), StatusCode::ACCEPTED),
        Some(prefix) => {
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
            let got = body_json(resp).await?["error"]
                .as_str()
                .context("error is a string")?
                .to_owned();
            assert!(got.starts_with(prefix), "{got}");
        }
    }
    Ok(())
}

/// Pinned sampling is checked when declared, and so is the eval-id rule's
/// temperature on an entry that pins none; the narrow pair older clients send
/// still lands, and a knob the entry leaves unpinned is the client's.
#[rstest]
#[case::narrow("eval_test_t1", json!({"eval_id": "test_eval", "dataset_name": "test_ds", "max_tokens": 100}), None)]
#[case::pinned(
    "eval_test_t1",
    json!({"eval_id": "test_eval", "dataset_name": "test_ds", "max_tokens": 100, "temperature": 1.0}),
    None
)]
#[case::other_temperature(
    "eval_test_t1",
    json!({"eval_id": "test_eval", "dataset_name": "test_ds", "max_tokens": 100, "temperature": 0.6}),
    Some(r#"benchmark_params do not match {"benchmark_type":"eval","eval_id":"test_eval","dataset_name":"test_ds","max_tokens":100,"temperature":1.0}"#)
)]
#[case::null_temperature(
    "eval_test_t1",
    json!({"eval_id": "test_eval", "dataset_name": "test_ds", "max_tokens": 100, "temperature": null}),
    Some("invalid benchmark_params: invalid type: null")
)]
#[case::unpinned_knob(
    "eval_test_t1",
    json!({"eval_id": "test_eval", "dataset_name": "test_ds", "max_tokens": 100, "temperature": 1.0, "top_p": 0.9}),
    None
)]
// An unknown eval id's rule is greedy.
#[case::rule_temperature(
    "eval_test",
    json!({"eval_id": "test_eval", "dataset_name": "test_ds", "max_tokens": 100, "temperature": 0.0}),
    None
)]
#[case::narrow_on_the_rule("eval_test", json!({"eval_id": "test_eval", "dataset_name": "test_ds", "max_tokens": 100}), None)]
#[case::other_than_the_rule(
    "eval_test",
    json!({"eval_id": "test_eval", "dataset_name": "test_ds", "max_tokens": 100, "temperature": 0.6}),
    Some(r#"benchmark_params do not match {"benchmark_type":"eval","eval_id":"test_eval","dataset_name":"test_ds","max_tokens":100,"temperature":0.0}"#)
)]
#[tokio::test]
async fn test_submit_declared_sampling(
    #[case] benchmark_id: &str,
    #[case] benchmark_params: serde_json::Value,
    #[case] error: Option<&str>,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let eval = r#"benchmark_type = "eval"
parameter_eval_id = "test_eval"
parameter_dataset_name = "test_ds"
parameter_max_tokens = 100"#;
    let benchmarks = dir.path().join("benchmarks");
    std::fs::write(benchmarks.join("eval_test.toml"), eval)?;
    std::fs::write(
        benchmarks.join("eval_test_t1.toml"),
        format!("{eval}\nparameter_temperature = 1.0"),
    )?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let body = json!({
        "benchmark_id": benchmark_id,
        "benchmark_type": "eval",
        "benchmark_params": benchmark_params,
        "device_name": "test-device",
        "device_form_factor": "embedded",
        "device_os_name": "Linux",
        "device_os_version": "22.04",
        "device_chip_model": "test-chip",
        "device_ram_bytes": 17179869184i64,
        "model_name": "llama-3.2-1b",
        "model_quant": "q4_0",
        "model_params_total_millions": 1000,
        "runtime_name": "llama.cpp",
        "runtime_version": "b5000",
        "completions": [{"id": "s1", "completion": "a"}],
    });
    let resp = authed_post(&state, &sk, &client_id, "/benchmarks", &body).await?;
    match error {
        None => assert_eq!(resp.status(), StatusCode::ACCEPTED),
        Some(prefix) => {
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
            let got = body_json(resp).await?["error"]
                .as_str()
                .context("error is a string")?
                .to_owned();
            assert!(got.starts_with(prefix), "{got}");
        }
    }
    Ok(())
}

/// The check covers the failure variant too, and refuses before anything is
/// written.
#[tokio::test]
async fn test_failure_with_mismatched_benchmark_params_returns_400() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let claimed = CLAIMED_JOB_ID;
    let body = leased_job_body(claimed, &[client_id.as_str()]);
    seed_leased(dir.path(), claimed, &client_id, &body)?;

    let mut failure = valid_failure("prefill_throughput_256", claimed, false);
    failure["benchmark_type"] = json!("prefill_throughput");
    failure["benchmark_params"] = json!({"prefill_tokens": 128, "batch_size": 1});

    let resp = authed_post(&state, &sk, &client_id, "/benchmarks", &failure).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(
        state
            .submission_store
            .find_job(&job(claimed)?)
            .await?
            .is_none()
    );
    Ok(())
}

/// A failure's narrow pair is accepted like a success's, and the record keeps
/// no pair.
#[tokio::test]
async fn test_failure_with_a_narrow_pair_is_recorded() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let claimed = CLAIMED_JOB_ID;
    let body = leased_job_body(claimed, &[client_id.as_str()]);
    seed_leased(dir.path(), claimed, &client_id, &body)?;

    let mut failure = valid_failure("prefill_throughput_256", claimed, false);
    failure["benchmark_type"] = json!("prefill_throughput");
    failure["benchmark_params"] = json!({"prefill_tokens": 256});

    let resp = authed_post(&state, &sk, &client_id, "/benchmarks", &failure).await?;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let record = state
        .submission_store
        .find_job(&job(claimed)?)
        .await?
        .context("the failure is recorded")?;
    assert!(
        record.body.get("benchmark_params").is_none(),
        "{}",
        record.body
    );
    Ok(())
}

/// A `todo/` job body to plant in a `leased/` entry: `recycle_lease` reads its
/// `expires_at` and escalation its `clients`.
fn leased_job_body(job_id: &str, clients: &[&str]) -> serde_json::Value {
    json!({
        "job_id": job_id,
        "expires_at": "never",
        "clients": clients,
        "spec": {
            "benchmark": "prefill_throughput_256",
            "model": {"type": "gguf_text", "source": "huggingface", "org": "meta-llama", "repo_name": "Llama-3.2-1B-GGUF", "path": "Llama-3.2-1B-Q4_0.gguf"},
            "runtime": {"type": "llamacpp_cli_stock_tools", "source": "github_release", "repository_url": "github.com/ggml-org/llama.cpp", "repository_version": "b5000", "flavor": "macos-arm64"},
        },
    })
}

/// The two endpoints that accept a submission for a claimed job.
#[derive(Clone, Copy)]
enum SubmitPath {
    Single,
    Batch,
}

/// Submit one body through the chosen accept path. `Batch` wraps the same body
/// as a one-item batch, so both cases put identical content on the wire.
async fn submit_via(
    path: SubmitPath,
    state: &AppState,
    signing_key: &SigningKey,
    client_id: &ClientId,
    body: &serde_json::Value,
) -> anyhow::Result<()> {
    match path {
        SubmitPath::Single => {
            submit_benchmark(state, signing_key, client_id, body).await?;
        }
        SubmitPath::Batch => {
            let resp = authed_post(
                state,
                signing_key,
                client_id,
                "/benchmarks/batch",
                &json!({"submissions": [body]}),
            )
            .await?;
            assert_eq!(resp.status(), StatusCode::OK);
            let reported = body_json(resp).await?;
            assert!(
                reported
                    .pointer("/results/0/job_id")
                    .is_some_and(serde_json::Value::is_string),
                "batch item was not accepted: {reported}"
            );
        }
    }
    Ok(())
}

/// The server records the flags the claimed job asked for from the lease it
/// issued, not the wire, discriminants kept and any `auth_token` dropped: a
/// gated model's token goes to the device in the claim, never into storage.
#[rstest]
#[case::single_submit(SubmitPath::Single)]
#[case::batch_submit(SubmitPath::Batch)]
#[tokio::test]
async fn test_submission_records_the_claimed_jobs_requested_flags(
    #[case] path: SubmitPath,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let claimed = CLAIMED_JOB_ID;
    seed_leased(
        dir.path(),
        claimed,
        &client_id,
        &json!({
            "job_id": claimed,
            "spec": {
                "benchmark": "prefill_throughput_256",
                "model": {"type": "gguf_text", "path": "m.gguf", "auth_token": "hf_SECRET"},
                "runtime": {"type": "llamacpp_cli_stock_tools"},
                // Deliberately not in sorted key order — the server canonicalizes.
                "model_flags": {"model_type": "gguf_text", "enable_thinking": true},
                "runtime_flags": {"threads": 8, "runtime_type": "llamacpp_cli_stock_tools"},
                // An axis the plan left unset spells the key null, which reads
                // as absent rather than as an empty request.
                "benchmark_flags": serde_json::Value::Null,
            },
        }),
    )?;

    let mut body = valid_submission("prefill_throughput_256");
    body["job_id"] = json!(claimed);
    submit_via(path, &state, &sk, &client_id, &body).await?;

    let stored = state
        .submission_store
        .find_job(&job(claimed)?)
        .await?
        .context("expected a stored submission")?;
    assert_eq!(
        stored.body["requested_model_flags"],
        json!(r#"{"enable_thinking":true,"model_type":"gguf_text"}"#)
    );
    assert_eq!(
        stored.body["requested_runtime_flags"],
        json!(r#"{"runtime_type":"llamacpp_cli_stock_tools","threads":8}"#)
    );
    assert!(stored.body["requested_benchmark_flags"].is_null());
    let record = serde_json::to_string(&stored.body)?;
    assert!(
        !record.contains("hf_SECRET"),
        "an auth_token reached the stored record: {record}"
    );
    Ok(())
}

/// The three fields are absent from the wire type, so a body spelling them is
/// ignored; the stored value comes from the lease.
#[tokio::test]
async fn test_client_supplied_requested_flags_are_ignored() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let claimed = CLAIMED_JOB_ID;
    seed_leased(
        dir.path(),
        claimed,
        &client_id,
        &json!({
            "job_id": claimed,
            "spec": {
                "benchmark": "prefill_throughput_256",
                "model": {"type": "gguf_text", "path": "m.gguf"},
                "runtime": {"type": "llamacpp_cli_stock_tools"},
                "model_flags": {"model_type": "gguf_text", "enable_thinking": true},
            },
        }),
    )?;

    let mut body = valid_submission("prefill_throughput_256");
    body["job_id"] = json!(claimed);
    body["requested_model_flags"] = json!(r#"{"forged":true}"#);
    body["requested_runtime_flags"] = json!(r#"{"forged":true}"#);
    body["requested_benchmark_flags"] = json!(r#"{"forged":true}"#);
    submit_benchmark(&state, &sk, &client_id, &body).await?;

    let stored = state
        .submission_store
        .find_job(&job(claimed)?)
        .await?
        .context("expected a stored submission")?;
    assert_eq!(
        stored.body["requested_model_flags"],
        json!(r#"{"enable_thinking":true,"model_type":"gguf_text"}"#)
    );
    assert!(stored.body["requested_runtime_flags"].is_null());
    assert!(stored.body["requested_benchmark_flags"].is_null());
    Ok(())
}

/// Null means no request was recorded, not an empty request.
#[tokio::test]
async fn test_ad_hoc_submission_records_no_requested_flags() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let job_id = submit_benchmark(
        &state,
        &sk,
        &client_id,
        &valid_submission("prefill_throughput_256"),
    )
    .await?;

    let stored = state
        .submission_store
        .find_job(&job(&job_id)?)
        .await?
        .context("expected a stored submission")?;
    for field in [
        "requested_model_flags",
        "requested_runtime_flags",
        "requested_benchmark_flags",
    ] {
        assert!(stored.body[field].is_null(), "{field} should be null");
    }
    Ok(())
}

/// A `message_type: "failure"` body echoing a claimed `job_id`. `retriable`
/// drives the routing under test.
fn valid_failure(benchmark_id: &str, job_id: &str, retriable: bool) -> serde_json::Value {
    json!({
        "message_type": "failure",
        "job_id": job_id,
        "benchmark_id": benchmark_id,
        "retriable": retriable,
        "failure_reason": "runtime OOM",
        "model_name": "llama-3.2-1b",
        "model_quant": "q4_0",
        "runtime_name": "llama.cpp",
        "runtime_version": "b5000",
    })
}

/// A retriable failure records no job result: the job is denied for the
/// reporting client and recycled back to `avail/` for the *other* eligible
/// client, so it is not escalated.
#[tokio::test]
async fn test_submit_retriable_failure_denies_and_recycles() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let claimed = CLAIMED_JOB_ID;
    let body = leased_job_body(claimed, &[client_id.as_str(), "ev1_other"]);
    seed_leased(dir.path(), claimed, &client_id, &body)?;

    let failure = valid_failure("prefill_throughput_256", claimed, true);
    let resp = authed_post(&state, &sk, &client_id, "/benchmarks", &failure).await?;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    let job = job(claimed)?;
    // No result recorded — the job is neither in incoming/ nor processed/.
    assert!(state.submission_store.find_job(&job).await?.is_none());
    // This client is denied; the job is back in avail/; its lease is gone.
    let denied = denied_clients(&state, &job).await?;
    assert!(denied.iter().any(|c| c.as_str() == client_id.as_str()));
    // The denial marker keeps the client's reason for the all-denied summary.
    let record = state
        .todo_store
        .get_denied(&job, &client_id)
        .await?
        .context("the denial marker is missing")?;
    assert_eq!(record.failure_reason.as_deref(), Some("runtime OOM"));
    assert_eq!(record.runtime_version.as_deref(), Some("b5000"));
    assert!(record.denied_at.is_some());
    assert!(state.todo_store.get_avail_by_job(&job).await?.is_some());
    assert!(
        state
            .todo_store
            .list_leased_for_client(&client_id)
            .await?
            .is_empty()
    );
    Ok(())
}

/// A retriable failure records no result, so the reporter's read of the job is
/// the `404` it drew while running it, never `failed`.
#[tokio::test]
async fn test_job_reads_as_not_found_after_a_retriable_failure() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let claimed = CLAIMED_JOB_ID;
    let body = leased_job_body(claimed, &[client_id.as_str(), "ev1_other"]);
    seed_leased(dir.path(), claimed, &client_id, &body)?;
    let uri = format!("/jobs/{claimed}");

    let resp = authed_get(&state, &sk, &client_id, &uri).await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let while_running = body_json(resp).await?;
    assert_eq!(while_running, json!({"error": "job not found"}));

    let failure = valid_failure("prefill_throughput_256", claimed, true);
    let resp = authed_post(&state, &sk, &client_id, "/benchmarks", &failure).await?;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    let resp = authed_get(&state, &sk, &client_id, &uri).await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_json(resp).await?, while_running);
    Ok(())
}

/// On both submit routes, captured at `INFO`, the binary's level; the span
/// supplies the `job_id`.
#[rstest]
#[case::single("/benchmarks")]
#[case::batch("/benchmarks/batch")]
#[test]
fn retriable_failure_logs_the_reason_at_warn(#[case] route: &str) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;

    let (status, logs) = pipette_web_testkit::capture_logs_at_level(
        tracing::Level::INFO,
        |rt| -> anyhow::Result<StatusCode> {
            rt.block_on(async {
                let state = make_state(dir.path()).await?;
                let (sk, client_id) = register_and_approve(&state).await?;
                let body = leased_job_body(CLAIMED_JOB_ID, &[client_id.as_str(), "ev1_other"]);
                seed_leased(dir.path(), CLAIMED_JOB_ID, &client_id, &body)?;
                let failure = valid_failure("prefill_throughput_256", CLAIMED_JOB_ID, true);
                let payload = if route == "/benchmarks/batch" {
                    json!({ "submissions": [failure] })
                } else {
                    failure
                };
                let resp = authed_post(&state, &sk, &client_id, route, &payload).await?;
                Ok(resp.status())
            })
        },
    )?;

    assert!(status?.is_success());
    let line = logs
        .lines()
        .find(|l| l.contains("job denied for client: retriable failure"))
        .with_context(|| format!("no denial event in the log:\n{logs}"))?;
    assert!(line.contains("WARN"), "{line}");
    assert!(line.contains("runtime OOM"), "{line}");
    assert!(line.contains(CLAIMED_JOB_ID), "{line}");
    Ok(())
}

/// A non-retriable failure is the job's terminal result: recorded as `failed`
/// and the job's `todo/` state torn down (no lease, no avail entry).
#[tokio::test]
async fn test_submit_non_retriable_failure_is_terminal() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let claimed = CLAIMED_JOB_ID;
    let body = leased_job_body(claimed, &[client_id.as_str()]);
    seed_leased(dir.path(), claimed, &client_id, &body)?;

    let failure = valid_failure("prefill_throughput_256", claimed, false);
    let resp = authed_post(&state, &sk, &client_id, "/benchmarks", &failure).await?;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    // Visible to the owner as a terminal failure.
    let resp = authed_get(&state, &sk, &client_id, &format!("/jobs/{claimed}")).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let b = body_json(resp).await?;
    assert_eq!(b["status"], "failed");
    assert_eq!(b["failure_reason"], "runtime OOM");

    // Torn down: no lease, no avail entry.
    let job = job(claimed)?;
    assert!(
        state
            .todo_store
            .list_leased_for_client(&client_id)
            .await?
            .is_empty()
    );
    assert!(state.todo_store.get_avail_by_job(&job).await?.is_none());
    Ok(())
}

/// A terminal failure is the job's result, so it carries the request too: a run
/// that OOMs on the flags it was handed is the row an analyst needs it for.
#[rstest]
#[case::single_submit(SubmitPath::Single)]
#[case::batch_submit(SubmitPath::Batch)]
#[tokio::test]
async fn test_non_retriable_failure_records_the_claimed_jobs_requested_flags(
    #[case] path: SubmitPath,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let claimed = CLAIMED_JOB_ID;
    seed_leased(
        dir.path(),
        claimed,
        &client_id,
        &json!({
            "job_id": claimed,
            "expires_at": "never",
            "clients": [client_id.as_str()],
            "spec": {
                "benchmark": "prefill_throughput_256",
                "model": {"type": "gguf_text", "path": "m.gguf", "auth_token": "hf_SECRET"},
                "runtime": {"type": "llamacpp_cli_stock_tools"},
                // Deliberately not in sorted key order — the server canonicalizes.
                "model_flags": {"model_type": "gguf_text", "enable_thinking": true},
                "runtime_flags": {"threads": 8, "runtime_type": "llamacpp_cli_stock_tools"},
                "benchmark_flags": serde_json::Value::Null,
            },
        }),
    )?;

    let failure = valid_failure("prefill_throughput_256", claimed, false);
    submit_via(path, &state, &sk, &client_id, &failure).await?;

    let stored = state
        .submission_store
        .find_job(&job(claimed)?)
        .await?
        .context("expected a stored failure record")?;
    assert_eq!(stored.body["message_type"], "failure");
    assert_eq!(stored.state.as_str(), "incoming");
    assert_eq!(stored.body["failure_source"], "client");
    assert_eq!(
        stored.body["requested_model_flags"],
        json!(r#"{"enable_thinking":true,"model_type":"gguf_text"}"#)
    );
    assert_eq!(
        stored.body["requested_runtime_flags"],
        json!(r#"{"runtime_type":"llamacpp_cli_stock_tools","threads":8}"#)
    );
    assert!(stored.body["requested_benchmark_flags"].is_null());
    let record = serde_json::to_string(&stored.body)?;
    assert!(
        !record.contains("hf_SECRET"),
        "an auth_token reached the stored failure record: {record}"
    );
    Ok(())
}

/// The submit path records only the denial; the `queue-maintenance` all-denied
/// pass then converts the unclaimable job to a `"system"` terminal failure.
#[tokio::test]
async fn test_submit_retriable_failure_all_denied_escalates_via_queue_maintenance()
-> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let claimed = CLAIMED_JOB_ID;
    let body = leased_job_body(claimed, &[client_id.as_str()]);
    seed_leased(dir.path(), claimed, &client_id, &body)?;

    let failure = valid_failure("prefill_throughput_256", claimed, true);
    let resp = authed_post(&state, &sk, &client_id, "/benchmarks", &failure).await?;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    let job = job(claimed)?;
    // The request records the denial and recycles the job; no record yet.
    assert!(state.submission_store.find_job(&job).await?.is_none());
    assert!(denied_clients(&state, &job).await?.contains(&client_id));
    assert!(state.todo_store.get_avail_by_job(&job).await?.is_some());

    // The next queue-maintenance run escalates.
    let catalog = state.catalog_cache.get().await?;
    let plans = state.plan_store.clone();
    pipette_pipeline::queue_maintenance::run(
        &*state.todo_store,
        &*state.client_store,
        &*state.tag_store,
        &*state.submission_store,
        &*plans,
        &catalog,
        pipette_pipeline::queue_maintenance::MaintenanceSettings {
            tmp_max: std::time::Duration::from_secs(86_400),
            plan_creating_max: std::time::Duration::from_secs(21_600),
            lost_lease_limit: 3,
        },
    )
    .await?;

    // Escalated to a synthetic system failure in the pipeline.
    let rec = state
        .submission_store
        .find_job(&job)
        .await?
        .context("expected a synthetic system failure record")?;
    assert_eq!(rec.body["client_id"], "system");
    assert_eq!(rec.body["message_type"], "failure");
    // The summary carries the reason that the client reported.
    assert_eq!(
        rec.body["failure_reason"],
        format!(
            "All eligible clients reported failure (1 client, 1 distinct reason):\n\
             - 1 client ({client_id}): runtime OOM"
        )
    );
    assert_eq!(rec.body["failure_source"], "all_clients_denied");
    // Recorded in incoming/, where `process-submissions` turns it into
    // warehouse failure rows and then archives it.
    assert_eq!(rec.state.as_str(), "incoming");
    // The now-terminal job is removed from avail/.
    assert!(state.todo_store.get_avail_by_job(&job).await?.is_none());
    Ok(())
}

#[tokio::test]
async fn test_submit_rejected_for_pending_client() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    // Register but don't approve
    let signing_key = pipette_web_testkit::signing_key()?;
    let pk_hex = hex::encode(signing_key.verifying_key().as_bytes());

    let resp = unauthed_post(
        &state,
        "/clients/register",
        &json!({
            "public_key": pk_hex,
            "organization": "test-org",
            "client_details": "test",
            "contact_email": "t@t.com"
        }),
    )
    .await?;
    let body = body_json(resp).await?;
    let client_id = ClientId::try_new(
        body["client_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing client_id"))?,
    )?;

    let resp = authed_post(
        &state,
        &signing_key,
        &client_id,
        "/benchmarks",
        &json!({
            "benchmark_id": "prefill_throughput_256",
            "device_name": "test-device",
            "device_form_factor": "embedded",
            "device_os_name": "Linux",
            "device_os_version": "22.04",
            "device_chip_model": "test-chip",
            "device_ram_bytes": 17179869184i64,
            "model_name": "m",
            "model_quant": "q",
            "runtime_name": "rt",
            "runtime_version": "v1",
            "prefill_time_ms": 10.0
        }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    Ok(())
}

#[tokio::test]
async fn test_submit_unknown_benchmark_404() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let resp = authed_post(
        &state,
        &sk,
        &client_id,
        "/benchmarks",
        &json!({
            "benchmark_id": "nonexistent",
            "device_name": "test-device",
            "device_form_factor": "embedded",
            "device_os_name": "Linux",
            "device_os_version": "22.04",
            "device_chip_model": "test-chip",
            "device_ram_bytes": 17179869184i64,
            "model_name": "m",
            "model_quant": "q",
            "runtime_name": "rt",
            "runtime_version": "v1",
            "prefill_time_ms": 10.0
        }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    Ok(())
}

/// A `benchmark_id` outside the id grammar is a `400` naming the rule, not the
/// `404` an unknown id draws: a path-shaped id never reaches a catalog lookup.
#[rstest]
#[case::slash(
    "bad/id",
    "invalid benchmark_id: benchmark id contains '/'; allowed: [A-Za-z0-9][A-Za-z0-9_.-]*"
)]
#[case::leading_underscore(
    "_x",
    "invalid benchmark_id: benchmark id must start with [A-Za-z0-9], not '_'"
)]
#[tokio::test]
async fn test_submit_malformed_benchmark_id_returns_400(
    #[case] benchmark_id: &str,
    #[case] expected_error: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let body = valid_submission(benchmark_id);
    let resp = authed_post(&state, &sk, &client_id, "/benchmarks", &body).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(resp).await?["error"], expected_error);
    Ok(())
}

/// A missing `device_name` is a `400`, while `runtime_name` and
/// `runtime_version` are optional: the identity lives in `runtime_descriptor`.
#[rstest]
#[case::missing_device_name("device_name", StatusCode::BAD_REQUEST)]
#[case::optional_runtime_name("runtime_name", StatusCode::ACCEPTED)]
#[case::optional_runtime_version("runtime_version", StatusCode::ACCEPTED)]
#[tokio::test]
async fn test_submit_field_omission(
    #[case] omit: &str,
    #[case] expected: StatusCode,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let mut body = json!({
        "benchmark_id": "prefill_throughput_256",
        "device_name": "test-device",
        "device_form_factor": "embedded",
        "device_os_name": "Linux",
        "device_os_version": "22.04",
        "device_chip_model": "test-chip",
        "device_ram_bytes": 17179869184i64,
        "model_name": "m",
        "model_quant": "q",
        "runtime_name": "rt",
        "runtime_version": "v1",
        "prefill_time_ms": 10.0
    });
    body.as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("body is not a JSON object"))?
        .remove(omit);

    let resp = authed_post(&state, &sk, &client_id, "/benchmarks", &body).await?;
    assert_eq!(resp.status(), expected);
    Ok(())
}

/// The verbatim round trip, on both accept paths. The body carries
/// `runtime_flags` too, so a merge in either direction fails here.
#[rstest]
#[case::single_submit(SubmitPath::Single)]
#[case::batch_submit(SubmitPath::Batch)]
#[tokio::test]
async fn test_submission_persists_client_configuration(
    #[case] path: SubmitPath,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    // Unsorted keys and a `GGML_*` `NAME=VALUE`: canonicalization would show.
    let wire = r#"{"envs":["CUDA_VISIBLE_DEVICES","GGML_CUDA_ENABLE_UNIFIED_MEMORY=0","CUDA_DEVICE_ORDER"]}"#;
    let claimed = CLAIMED_JOB_ID;
    seed_leased(
        dir.path(),
        claimed,
        &client_id,
        &json!({"job_id": claimed, "spec": {"benchmark": "prefill_throughput_256"}}),
    )?;

    let runtime_flags = "--n-gpu-layers 999";

    let mut body = valid_submission("prefill_throughput_256");
    body["job_id"] = json!(claimed);
    body["client_configuration"] = json!(wire);
    body["runtime_flags"] = json!(runtime_flags);
    submit_via(path, &state, &sk, &client_id, &body).await?;

    let stored = state
        .submission_store
        .find_job(&job(claimed)?)
        .await?
        .context("expected a stored submission")?;
    assert_eq!(stored.body["client_configuration"], json!(wire));
    assert_eq!(stored.body["runtime_flags"], json!(runtime_flags));
    Ok(())
}

/// The runtime's streams leave the stored body for the job's runtime-output
/// object, on both accept paths. `client_extras` stays in the body verbatim,
/// and an empty stream is kept as a capture of nothing.
#[rstest]
#[case::single_submit(SubmitPath::Single)]
#[case::batch_submit(SubmitPath::Batch)]
#[tokio::test]
async fn test_submission_stores_runtime_output_apart_from_the_body(
    #[case] path: SubmitPath,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;
    let claimed = CLAIMED_JOB_ID;
    seed_lease(dir.path(), claimed, &client_id)?;

    let extras = r#"{ "build": "nightly" }"#;
    let mut body = valid_submission("prefill_throughput_256");
    body["job_id"] = json!(claimed);
    body["runtime_stdout"] = json!("model loaded\n");
    body["runtime_stderr"] = json!("");
    body["client_extras"] = json!(extras);
    submit_via(path, &state, &sk, &client_id, &body).await?;

    let job = job(claimed)?;
    let stored = state
        .submission_store
        .find_job(&job)
        .await?
        .context("expected a stored submission")?;
    assert_eq!(stored.body["client_extras"], json!(extras));
    assert!(
        stored.body.get("runtime_stdout").is_none(),
        "{}",
        stored.body
    );
    assert!(
        stored.body.get("runtime_stderr").is_none(),
        "{}",
        stored.body
    );
    assert_eq!(
        state.submission_store.list_runtime_output(&job).await?,
        vec![(
            client_id,
            RuntimeOutput {
                runtime_stdout: Some("model loaded\n".to_owned()),
                runtime_stderr: Some(String::new()),
            }
        )]
    );
    Ok(())
}

/// A retriable failure records no result, but keeps the output of the run
/// that failed: the denial marker holds only the tail of the reason.
#[tokio::test]
async fn test_retriable_failure_keeps_its_runtime_output() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;
    let claimed = CLAIMED_JOB_ID;
    let leased = leased_job_body(claimed, &[client_id.as_str(), "ev1_other"]);
    seed_leased(dir.path(), claimed, &client_id, &leased)?;

    let mut failure = valid_failure("prefill_throughput_256", claimed, true);
    failure["runtime_stderr"] = json!("CUDA error: out of memory\n");
    let resp = authed_post(&state, &sk, &client_id, "/benchmarks", &failure).await?;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    let job = job(claimed)?;
    assert!(state.submission_store.find_job(&job).await?.is_none());
    assert_eq!(
        state.submission_store.list_runtime_output(&job).await?,
        vec![(
            client_id,
            RuntimeOutput {
                runtime_stderr: Some("CUDA error: out of memory\n".to_owned()),
                ..RuntimeOutput::default()
            }
        )]
    );
    Ok(())
}

/// A field that fails validation is a `400` on both variants, and nothing is
/// stored. The rules themselves are tested in `pipette-pipeline`.
#[rstest]
#[case::client_extras(
    "client_extras",
    json!("[1]"),
    "client_extras is present but not valid JSON"
)]
#[case::negative_device_time_ms("device_time_ms", json!(-1), "ElapsedMillis is too small")]
#[tokio::test]
async fn test_invalid_field_is_refused(
    #[case] field: &str,
    #[case] value: serde_json::Value,
    #[case] message: &str,
    #[values(false, true)] is_failure: bool,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;
    seed_lease(dir.path(), CLAIMED_JOB_ID, &client_id)?;

    let mut body = if is_failure {
        valid_failure("prefill_throughput_256", CLAIMED_JOB_ID, false)
    } else {
        valid_submission("prefill_throughput_256")
    };
    body["job_id"] = json!(CLAIMED_JOB_ID);
    body[field] = value;
    let resp = authed_post(&state, &sk, &client_id, "/benchmarks", &body).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let error = body_json(resp).await?;
    assert!(
        error["error"].as_str().is_some_and(|e| e.contains(message)),
        "{error}"
    );
    assert!(
        state
            .submission_store
            .find_job(&job(CLAIMED_JOB_ID)?)
            .await?
            .is_none()
    );
    Ok(())
}

/// `device_time_ms` stays in the stored body on both variants and both accept
/// paths.
#[rstest]
#[tokio::test]
async fn test_device_time_ms_is_stored(
    #[values(false, true)] is_failure: bool,
    #[values(SubmitPath::Single, SubmitPath::Batch)] path: SubmitPath,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;
    seed_lease(dir.path(), CLAIMED_JOB_ID, &client_id)?;

    let mut body = if is_failure {
        valid_failure("prefill_throughput_256", CLAIMED_JOB_ID, false)
    } else {
        valid_submission("prefill_throughput_256")
    };
    body["job_id"] = json!(CLAIMED_JOB_ID);
    body["device_time_ms"] = json!(184_250);
    submit_via(path, &state, &sk, &client_id, &body).await?;

    let stored = state
        .submission_store
        .find_job(&job(CLAIMED_JOB_ID)?)
        .await?
        .context("expected a stored submission")?;
    assert_eq!(stored.body["device_time_ms"], json!(184_250));
    Ok(())
}

/// A client that sends no `client_configuration` at all: accepted unchanged,
/// and records none. That is every client without a `--config` file.
#[rstest]
#[case::single_submit(SubmitPath::Single)]
#[case::batch_submit(SubmitPath::Batch)]
#[tokio::test]
async fn test_submission_without_client_configuration_stores_null(
    #[case] path: SubmitPath,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let claimed = CLAIMED_JOB_ID;
    seed_leased(
        dir.path(),
        claimed,
        &client_id,
        &json!({"job_id": claimed, "spec": {"benchmark": "prefill_throughput_256"}}),
    )?;

    let mut body = valid_submission("prefill_throughput_256");
    body["job_id"] = json!(claimed);
    submit_via(path, &state, &sk, &client_id, &body).await?;

    let stored = state
        .submission_store
        .find_job(&job(claimed)?)
        .await?
        .context("expected a stored submission")?;
    assert!(
        stored.body["client_configuration"].is_null(),
        "a submission that sent no client_configuration recorded one: {}",
        stored.body
    );
    Ok(())
}

/// A fleet mixes configured and unconfigured boxes, so one batch may carry
/// both. Every item is accepted, and each records its own answer.
#[tokio::test]
async fn test_batch_mixes_submissions_with_and_without_client_configuration() -> anyhow::Result<()>
{
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let wire = r#"{"envs":["CUDA_VISIBLE_DEVICES"]}"#;
    let mut configured = valid_submission("prefill_throughput_256");
    configured["client_configuration"] = json!(wire);
    let plain = valid_submission("prefill_throughput_256");

    let resp = authed_post(
        &state,
        &sk,
        &client_id,
        "/benchmarks/batch",
        &json!({"submissions": [configured, plain]}),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let reported = body_json(resp).await?;

    // Both items are ad-hoc, so the server minted an id for each. Found by the
    // `index` each result carries, not by position.
    let accepted_job_id = |index: usize| -> anyhow::Result<String> {
        let results = reported["results"]
            .as_array()
            .with_context(|| format!("batch response has no results array: {reported}"))?;
        results
            .iter()
            .find(|r| r["index"] == json!(index))
            .and_then(|r| r["job_id"].as_str())
            .map(str::to_string)
            .with_context(|| format!("batch item {index} was not accepted: {reported}"))
    };

    let stored_configured = state
        .submission_store
        .find_job(&job(&accepted_job_id(0)?)?)
        .await?
        .context("expected a stored submission for the configured item")?;
    assert_eq!(stored_configured.body["client_configuration"], json!(wire));

    let stored_plain = state
        .submission_store
        .find_job(&job(&accepted_job_id(1)?)?)
        .await?
        .context("expected a stored submission for the unconfigured item")?;
    assert!(stored_plain.body["client_configuration"].is_null());
    Ok(())
}

/// A non-JSON ref would defeat canonicalization, so each must parse as JSON;
/// `benchmark_flags` must also be an object, or it makes a useless grouping
/// key.
#[rstest]
#[case::model_descriptor("model_descriptor", "not valid json {")]
#[case::runtime_descriptor("runtime_descriptor", "llama_cpp b8683")]
#[case::benchmark_flags_not_json("benchmark_flags", "skip_thermal=true")]
#[case::benchmark_flags_not_an_object("benchmark_flags", "\"enforced\"")]
#[case::benchmark_flags_array("benchmark_flags", "[{\"skip_thermal\":true}]")]
#[case::benchmark_flags_empty("benchmark_flags", "")]
#[case::benchmark_flags_blank("benchmark_flags", "   ")]
#[case::client_configuration_not_json("client_configuration", "envs=CUDA_VISIBLE_DEVICES")]
#[case::client_configuration_not_an_object("client_configuration", "\"CUDA_VISIBLE_DEVICES\"")]
#[case::client_configuration_array("client_configuration", "[\"CUDA_VISIBLE_DEVICES\"]")]
#[tokio::test]
async fn test_submit_invalid_ref_json_returns_400(
    #[case] field: &str,
    #[case] bad_value: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let mut body = json!({
        "benchmark_id": "prefill_throughput_256",
        "device_name": "test-device",
        "device_form_factor": "embedded",
        "device_os_name": "Linux",
        "device_os_version": "22.04",
        "device_chip_model": "test-chip",
        "device_ram_bytes": 17179869184i64,
        "model_name": "m",
        "model_quant": "q",
        "runtime_name": "rt",
        "runtime_version": "v1",
        "prefill_time_ms": 10.0
    });
    body[field] = json!(bad_value);

    let resp = authed_post(&state, &sk, &client_id, "/benchmarks", &body).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    Ok(())
}

/// The scorer fills them from the catalog for a known model, or leaves the
/// columns null.
#[tokio::test]
async fn test_submit_without_mill_params_is_accepted() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let resp = authed_post(
        &state,
        &sk,
        &client_id,
        "/benchmarks",
        &json!({
            "benchmark_id": "prefill_throughput_256",
            "device_name": "test-device",
            "device_form_factor": "embedded",
            "device_os_name": "Linux",
            "device_os_version": "22.04",
            "device_chip_model": "test-chip",
            "device_ram_bytes": 17179869184i64,
            "model_name": "m",
            "model_quant": "q",
            // no model_params_total_millions / _active
            "runtime_name": "rt",
            "runtime_version": "v1",
            "prefill_time_ms": 10.0
        }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    Ok(())
}

#[tokio::test]
async fn test_submit_negative_mill_params_total_returns_400() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let resp = authed_post(
        &state,
        &sk,
        &client_id,
        "/benchmarks",
        &json!({
            "benchmark_id": "prefill_throughput_256",
            "device_name": "test-device",
            "device_form_factor": "embedded",
            "device_os_name": "Linux",
            "device_os_version": "22.04",
            "device_chip_model": "test-chip",
            "device_ram_bytes": 17179869184i64,
            "model_name": "m",
            "model_quant": "q",
            "model_params_total_millions": -5,
            "runtime_name": "rt",
            "runtime_version": "v1",
            "prefill_time_ms": 10.0
        }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    Ok(())
}

#[tokio::test]
async fn test_submit_active_exceeds_total_returns_400() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let resp = authed_post(
        &state,
        &sk,
        &client_id,
        "/benchmarks",
        &json!({
            "benchmark_id": "prefill_throughput_256",
            "device_name": "test-device",
            "device_form_factor": "embedded",
            "device_os_name": "Linux",
            "device_os_version": "22.04",
            "device_chip_model": "test-chip",
            "device_ram_bytes": 17179869184i64,
            "model_name": "m",
            "model_quant": "q",
            "model_params_total_millions": 1000,
            "model_params_active_millions": 2000, // > total
            "runtime_name": "rt",
            "runtime_version": "v1",
            "prefill_time_ms": 10.0
        }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = http_body_util::BodyExt::collect(resp.into_body())
        .await?
        .to_bytes();
    let text = std::str::from_utf8(&body)?;
    assert!(
        text.contains("must not exceed"),
        "expected 'must not exceed' in error, got: {text}"
    );
    Ok(())
}

/// Every reported number is held to its range: a `400` outside it. A byte
/// count or a spread may be zero (a run that touched no swap reports `0`
/// rather than omitting the field), a time the scorer divides by may not.
#[rstest]
#[case::negative_swap("observation_max_swap_bytes", json!(-1), Err("ByteCount is too small"))]
#[case::negative_host("observation_max_host_bytes", json!(-4096), Err("ByteCount is too small"))]
#[case::zero_swap("observation_max_swap_bytes", json!(0), Ok(()))]
#[case::zero_host("observation_max_host_bytes", json!(0), Ok(()))]
#[case::negative_ram("device_ram_bytes", json!(-1), Err("ByteCount is too small"))]
#[case::zero_time("prefill_time_ms", json!(0.0), Err("PositiveMillis is too small"))]
#[case::negative_time("prefill_time_ms", json!(-10.0), Err("PositiveMillis is too small"))]
#[case::overflowing_time("prefill_time_ms", json!(1e300), Err("PositiveMillis is not finite"))]
#[case::negative_stddev(
    "prefill_time_ms_stddev",
    json!(-0.5),
    Err("NonNegativeMillis is too small")
)]
#[case::zero_stddev("prefill_time_ms_stddev", json!(0.0), Ok(()))]
// The body's only zero `PositiveMillis`.
#[case::zero_total_time("total_time_ms", json!(0.0), Err("PositiveMillis is too small"))]
#[case::zero_mill_params("model_params_total_millions", json!(0), Err("PositiveCount is too small"))]
#[tokio::test]
async fn test_submit_number_range(
    #[case] field: &str,
    #[case] value: serde_json::Value,
    #[case] expected: Result<(), &str>,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let mut body = json!({
        "benchmark_id": "prefill_throughput_256",
        "device_name": "test-device",
        "device_form_factor": "embedded",
        "device_os_name": "Linux",
        "device_os_version": "22.04",
        "device_chip_model": "test-chip",
        "device_ram_bytes": 17179869184i64,
        "model_name": "m",
        "model_quant": "q",
        "runtime_name": "rt",
        "runtime_version": "v1",
        "prefill_time_ms": 10.0
    });
    body[field] = value;

    let resp = authed_post(&state, &sk, &client_id, "/benchmarks", &body).await?;
    match expected {
        Ok(()) => assert_eq!(resp.status(), StatusCode::ACCEPTED),
        Err(message) => {
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
            let error = body_json(resp).await?;
            assert!(
                error["error"].as_str().is_some_and(|e| e.contains(message)),
                "{field}: {error}"
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn test_submit_invalid_form_factor_returns_400() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let resp = authed_post(
        &state,
        &sk,
        &client_id,
        "/benchmarks",
        &json!({
            "benchmark_id": "prefill_throughput_256",
            "device_name": "test-device",
            "device_form_factor": "spaceship",
            "device_os_name": "Linux",
            "device_os_version": "22.04",
            "device_chip_model": "test-chip",
            "device_ram_bytes": 17179869184i64,
            "model_name": "m",
            "model_quant": "q",
            "runtime_name": "rt",
            "runtime_version": "v1",
            "prefill_time_ms": 10.0
        }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    Ok(())
}

#[tokio::test]
async fn test_submit_missing_device_ram_bytes_returns_400() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let resp = authed_post(
        &state,
        &sk,
        &client_id,
        "/benchmarks",
        &json!({
            "benchmark_id": "prefill_throughput_256",
            "device_name": "test-device",
            "device_form_factor": "embedded",
            "device_os_name": "Linux",
            "device_os_version": "22.04",
            "device_chip_model": "test-chip",
            // missing device_ram_bytes
            "model_name": "m",
            "model_quant": "q",
            "runtime_name": "rt",
            "runtime_version": "v1",
            "prefill_time_ms": 10.0
        }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    Ok(())
}

#[tokio::test]
async fn test_submit_max_memory_missing_host_returns_400() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let resp = authed_post(
        &state,
        &sk,
        &client_id,
        "/benchmarks",
        &json!({
            "benchmark_id": "max_memory_usage_256",
            "device_name": "test-device",
            "device_form_factor": "embedded",
            "device_os_name": "Linux",
            "device_os_version": "22.04",
            "device_chip_model": "test-chip",
            "device_ram_bytes": 17179869184i64,
            "model_name": "m",
            "model_quant": "q",
            "runtime_name": "rt",
            "runtime_version": "v1",
            "max_gpu_bytes": 1024i64
        }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    Ok(())
}

/// Guards against the silent last-wins behavior of serde `alias` on collision.
#[tokio::test]
async fn test_submit_max_memory_both_spellings_returns_400() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let resp = authed_post(
        &state,
        &sk,
        &client_id,
        "/benchmarks",
        &json!({
            "benchmark_id": "max_memory_usage_256",
            "device_name": "test-device",
            "device_form_factor": "embedded",
            "device_os_name": "Linux",
            "device_os_version": "22.04",
            "device_chip_model": "test-chip",
            "device_ram_bytes": 17179869184i64,
            "model_name": "m",
            "model_quant": "q",
            "runtime_name": "rt",
            "runtime_version": "v1",
            "max_host_bytes": 1073741824i64,
            "max_ram_bytes": 1073741824i64
        }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    Ok(())
}

#[tokio::test]
async fn test_submit_max_memory_non_integer_bucket_returns_400() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let resp = authed_post(
        &state,
        &sk,
        &client_id,
        "/benchmarks",
        &json!({
            "benchmark_id": "max_memory_usage_256",
            "device_name": "test-device",
            "device_form_factor": "embedded",
            "device_os_name": "Linux",
            "device_os_version": "22.04",
            "device_chip_model": "test-chip",
            "device_ram_bytes": 17179869184i64,
            "model_name": "m",
            "model_quant": "q",
            "runtime_name": "rt",
            "runtime_version": "v1",
            "max_host_bytes": 1073741824i64,
            "max_gpu_bytes": "not-an-int"
        }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    Ok(())
}

/// An accelerator's VRAM without the accelerator's model is a `400` naming the
/// pair, for the GPU and the NPU alike.
#[rstest]
#[case::gpu(
    "device_gpu_vram_bytes",
    "device_gpu_vram_bytes requires device_gpu_model"
)]
#[case::npu(
    "device_npu_vram_bytes",
    "device_npu_vram_bytes requires device_npu_model"
)]
#[tokio::test]
async fn test_submit_vram_without_its_model_returns_400(
    #[case] vram_field: &str,
    #[case] expected_error: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let mut body = valid_submission("prefill_throughput_256");
    body[vram_field] = json!(8589934592i64);
    let resp = authed_post(&state, &sk, &client_id, "/benchmarks", &body).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(resp).await?["error"], expected_error);
    Ok(())
}

/// An eval with no completions can never score, so it is refused rather than
/// left in the scoring queue for good.
#[rstest]
#[case::absent(json!(null))]
#[case::empty(json!([]))]
#[tokio::test]
async fn test_submit_eval_without_completions_returns_400(
    #[case] completions: serde_json::Value,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let body = json!({
        "benchmark_id": "eval_test",
        "device_name": "test-device",
        "device_form_factor": "embedded",
        "device_os_name": "Linux",
        "device_os_version": "22.04",
        "device_chip_model": "test-chip",
        "device_ram_bytes": 17179869184i64,
        "model_name": "llama-3.2-1b",
        "model_quant": "q4_0",
        "model_params_total_millions": 1000,
        "runtime_name": "llama.cpp",
        "runtime_version": "b5000",
        "completions": completions,
    });
    let resp = authed_post(&state, &sk, &client_id, "/benchmarks", &body).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(resp).await?["error"], "missing completions");
    Ok(())
}

#[tokio::test]
async fn test_submit_eval_duplicate_completion_ids_returns_400() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let resp = authed_post(
        &state,
        &sk,
        &client_id,
        "/benchmarks",
        &json!({
            "benchmark_id": "eval_test",
            "device_name": "test-device",
            "device_form_factor": "embedded",
            "device_os_name": "Linux",
            "device_os_version": "22.04",
            "device_chip_model": "test-chip",
            "device_ram_bytes": 17179869184i64,
            "model_name": "llama-3.2-1b",
            "model_quant": "q4_0",
            "model_params_total_millions": 1000,
            "runtime_name": "llama.cpp",
            "runtime_version": "b5000",
            "completions": [
                {"id": "s1", "completion": "a"},
                {"id": "s2", "completion": "b"},
                {"id": "s1", "completion": "c"},
            ]
        }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = http_body_util::BodyExt::collect(resp.into_body())
        .await?
        .to_bytes();
    let text = std::str::from_utf8(&body)?;
    assert!(
        text.contains("duplicate completion id") && text.contains("s1"),
        "expected duplicate-id error mentioning 's1', got: {text}"
    );
    Ok(())
}

#[tokio::test]
async fn test_submit_eval_missing_completion_id_returns_400() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let resp = authed_post(
        &state,
        &sk,
        &client_id,
        "/benchmarks",
        &json!({
            "benchmark_id": "eval_test",
            "device_name": "test-device",
            "device_form_factor": "embedded",
            "device_os_name": "Linux",
            "device_os_version": "22.04",
            "device_chip_model": "test-chip",
            "device_ram_bytes": 17179869184i64,
            "model_name": "llama-3.2-1b",
            "model_quant": "q4_0",
            "model_params_total_millions": 1000,
            "runtime_name": "llama.cpp",
            "runtime_version": "b5000",
            "completions": [
                {"completion": "a"},
            ]
        }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    Ok(())
}

#[tokio::test]
async fn test_submit_batch_all_valid() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let resp = authed_post(
        &state,
        &sk,
        &client_id,
        "/benchmarks/batch",
        &json!({
            "submissions": [
                valid_submission("prefill_throughput_256"),
                valid_submission("prefill_throughput_256"),
            ]
        }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);

    let body = body_json(resp).await?;
    let results = body["results"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("missing results array"))?;
    assert_eq!(results.len(), 2);
    for (i, r) in results.iter().enumerate() {
        assert_eq!(
            r["index"]
                .as_u64()
                .context("each result row carries a numeric index")?,
            u64::try_from(i)?
        );
        let job_id = r["job_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("result {i} missing job_id: {r}"))?;

        let get_resp = authed_get(&state, &sk, &client_id, &format!("/jobs/{job_id}")).await?;
        assert_eq!(get_resp.status(), StatusCode::OK);
    }
    Ok(())
}

#[tokio::test]
async fn test_submit_batch_mixed_valid_and_invalid() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    // Missing the required `device_name`.
    let mut invalid = valid_submission("prefill_throughput_256");
    invalid
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("submission body is a JSON object"))?
        .remove("device_name");

    let resp = authed_post(
        &state,
        &sk,
        &client_id,
        "/benchmarks/batch",
        &json!({
            "submissions": [
                valid_submission("prefill_throughput_256"),
                invalid,
                json!({"benchmark_id": "nonexistent"}),
            ]
        }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);

    let body = body_json(resp).await?;
    let results = body["results"]
        .as_array()
        .context("the response body carries a results array")?;
    assert_eq!(results.len(), 3);

    // Item 0: success
    assert_eq!(results[0]["index"], 0);
    assert!(results[0]["job_id"].is_string());
    assert!(results[0]["error"].is_null());

    // Item 1: missing device_name
    assert_eq!(results[1]["index"], 1);
    assert!(results[1]["job_id"].is_null());
    assert!(
        results[1]["error"]
            .as_str()
            .context("the failed row carries an error string")?
            .contains("device_name")
    );

    // Item 2: unknown benchmark
    assert_eq!(results[2]["index"], 2);
    assert!(results[2]["job_id"].is_null());
    assert!(
        results[2]["error"]
            .as_str()
            .context("the failed row carries an error string")?
            .contains("not found")
    );
    Ok(())
}

/// A body that is valid JSON but not an object is a `400`, alone or as a batch
/// item, rather than a handler that defaults `message_type` into it.
#[rstest]
#[case::array(json!([]))]
#[case::string(json!("prefill_throughput_256"))]
#[case::number(json!(42))]
#[tokio::test]
async fn test_submit_non_object_body_returns_400(
    #[case] body: serde_json::Value,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let resp = authed_post(&state, &sk, &client_id, "/benchmarks", &body).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let resp = authed_post(
        &state,
        &sk,
        &client_id,
        "/benchmarks/batch",
        &json!({ "submissions": [body] }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let reported = body_json(resp).await?;
    assert!(
        reported["results"][0]["error"]
            .as_str()
            .is_some_and(|e| e.contains("benchmark_id")),
        "the item is refused for its missing benchmark_id: {reported}"
    );
    Ok(())
}

/// A `message_type` naming neither variant is a `400` that names the value,
/// alone or as a batch item, rather than a body read as some default variant.
#[tokio::test]
async fn test_submit_unknown_message_type_returns_400() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let expected = "invalid submission: unknown variant `bogus`, expected `success` or `failure`";
    let mut body = valid_submission("prefill_throughput_256");
    body["message_type"] = json!("bogus");

    let resp = authed_post(&state, &sk, &client_id, "/benchmarks", &body).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(resp).await?["error"], expected);

    let resp = authed_post(
        &state,
        &sk,
        &client_id,
        "/benchmarks/batch",
        &json!({ "submissions": [body] }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let reported = body_json(resp).await?;
    assert_eq!(
        reported["results"],
        json!([{"index": 0, "error": expected}]),
        "{reported}"
    );

    assert!(
        state
            .submission_store
            .list_incoming(TEST_LIST_LIMIT)
            .await?
            .is_empty()
    );
    Ok(())
}

/// A body axum cannot read as JSON (bad syntax, or no JSON `Content-Type`) is a
/// `400` in the error envelope on both submit routes, not axum's own `415` or
/// plain-text reply, and nothing is written.
#[rstest]
#[case::malformed_syntax(
    Some("application/json"),
    true,
    "Failed to parse the request body as JSON"
)]
#[case::missing_type(None, false, "Expected request with `Content-Type: application/json`")]
#[case::non_json_type(
    Some("text/plain"),
    false,
    "Expected request with `Content-Type: application/json`"
)]
#[tokio::test]
async fn test_submit_unreadable_json_body_returns_400(
    #[values("/benchmarks", "/benchmarks/batch")] route: &str,
    #[case] content_type: Option<&str>,
    #[case] malformed: bool,
    #[case] expected_prefix: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let valid = valid_submission("prefill_throughput_256");
    let body = if malformed {
        "{ not json".to_owned()
    } else if route == "/benchmarks/batch" {
        json!({ "submissions": [valid] }).to_string()
    } else {
        valid.to_string()
    };
    let headers = pipette_web_testkit::auth_headers(&sk, &client_id, "POST", route)?;
    let resp =
        pipette_web_testkit::raw_request(&state, "POST", route, &headers, content_type, body)
            .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let reported = body_json(resp).await?;
    assert!(
        reported["error"]
            .as_str()
            .is_some_and(|e| e.starts_with(expected_prefix)),
        "{reported}"
    );

    assert!(
        state
            .submission_store
            .list_incoming(TEST_LIST_LIMIT)
            .await?
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn test_submit_batch_missing_array_returns_400() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let resp = authed_post(
        &state,
        &sk,
        &client_id,
        "/benchmarks/batch",
        &json!({"not_submissions": []}),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    Ok(())
}

#[tokio::test]
async fn test_submit_batch_empty_array_returns_400() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let resp = authed_post(
        &state,
        &sk,
        &client_id,
        "/benchmarks/batch",
        &json!({"submissions": []}),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    Ok(())
}

/// A batch over 1000 items is refused whole before any item is processed, so
/// none of its valid items is written; 1000 is still read item by item.
#[tokio::test]
async fn test_submit_batch_over_the_cap_returns_400() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let over = vec![valid_submission("prefill_throughput_256"); 1001];
    let resp = authed_post(
        &state,
        &sk,
        &client_id,
        "/benchmarks/batch",
        &json!({ "submissions": over }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        body_json(resp).await?,
        json!({"error": "too many submissions (max 1000)"})
    );
    assert!(
        state
            .submission_store
            .list_incoming(TEST_LIST_LIMIT)
            .await?
            .is_empty(),
        "no item of a refused batch may be written"
    );

    // At the cap the envelope passes and each empty item fails on its own.
    let resp = authed_post(
        &state,
        &sk,
        &client_id,
        "/benchmarks/batch",
        &json!({ "submissions": vec![json!({}); 1000] }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let reported = body_json(resp).await?;
    let results = reported["results"]
        .as_array()
        .context("the response body carries a results array")?;
    assert_eq!(results.len(), 1000);
    let stray = results
        .iter()
        .find(|r| r["error"] != "missing benchmark_id");
    assert!(
        stray.is_none(),
        "every empty item is refused for its missing benchmark_id: {stray:?}"
    );
    Ok(())
}

#[tokio::test]
async fn test_submit_batch_rejected_for_pending_client() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    // Register but don't approve.
    let signing_key = pipette_web_testkit::signing_key()?;
    let pk_hex = hex::encode(signing_key.verifying_key().as_bytes());
    let resp = unauthed_post(
        &state,
        "/clients/register",
        &json!({
            "public_key": pk_hex,
            "organization": "test-org",
            "client_details": "test",
            "contact_email": "t@t.com"
        }),
    )
    .await?;
    let body = body_json(resp).await?;
    let client_id = ClientId::try_new(
        body["client_id"]
            .as_str()
            .context("the response body carries a string client_id")?,
    )?;

    let resp = authed_post(
        &state,
        &signing_key,
        &client_id,
        "/benchmarks/batch",
        &json!({
            "submissions": [valid_submission("prefill_throughput_256")]
        }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    Ok(())
}

#[tokio::test]
async fn test_submit_trims_padded_fields_at_ingest() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let job_id = submit_benchmark(
        &state,
        &sk,
        &client_id,
        &json!({
            "benchmark_id": "prefill_throughput_256",
            "device_name": "  test-device  ",
            "device_form_factor": "embedded\n",
            "device_os_name": "Linux",
            "device_os_version": "22.04",
            "device_chip_model": "test-chip",
            "device_ram_bytes": 17179869184i64,
            "model_name": "  llama-3.2-1b\t",
            "model_quant": "q4_0",
            "runtime_name": "llama.cpp",
            "runtime_version": "b5000",
            "prefill_time_ms": 34.7
        }),
    )
    .await?;

    let record = state
        .submission_store
        .find_job(&job(job_id)?)
        .await?
        .context("submission should be persisted")?;

    assert_eq!(record.body["device_name"].as_str(), Some("test-device"));
    assert_eq!(record.body["device_form_factor"].as_str(), Some("embedded"));
    assert_eq!(record.body["model_name"].as_str(), Some("llama-3.2-1b"));
    // benchmark_id is payload-only and unchanged.
    assert_eq!(
        record.body["benchmark_id"].as_str(),
        Some("prefill_throughput_256")
    );
    assert_eq!(record.body["client_id"].as_str(), Some(client_id.as_str()));
    Ok(())
}

#[tokio::test]
async fn test_submit_padded_form_factor_validates_after_trim() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    // Without trimming, "  embedded  " would fail the DeviceFormFactor parse
    // and return 400. With trimming applied before validation, it's accepted.
    let resp = authed_post(
        &state,
        &sk,
        &client_id,
        "/benchmarks",
        &json!({
            "benchmark_id": "prefill_throughput_256",
            "device_name": "test-device",
            "device_form_factor": "  embedded  ",
            "device_os_name": "Linux",
            "device_os_version": "22.04",
            "device_chip_model": "test-chip",
            "device_ram_bytes": 17179869184i64,
            "model_name": "m",
            "model_quant": "q",
            "runtime_name": "rt",
            "runtime_version": "v1",
            "prefill_time_ms": 10.0
        }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    Ok(())
}

/// The status comes from the body's variant, not its location, so a failure
/// awaiting the scorer never reads as pending.
#[tokio::test]
async fn test_submit_failure_routes_to_incoming_and_get_reports_failed() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;
    seed_lease(dir.path(), CLAIMED_JOB_ID, &client_id)?;

    let job_id = submit_benchmark(
        &state,
        &sk,
        &client_id,
        &json!({
            "message_type": "failure",
            "benchmark_id": "prefill_throughput_256",
            "job_id": CLAIMED_JOB_ID,
            "retriable": false,
            "failure_reason": "runtime crashed: OOM at decode step",
            "model_name": "mlx-community/Qwen3.5-4B-4bit",
            "model_quant": "4bit",
            "runtime_name": "mlx-lm",
            "runtime_version": "0.26.0",
        }),
    )
    .await?;

    let resp = authed_get(&state, &sk, &client_id, &format!("/jobs/{job_id}")).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["job_id"], job_id);
    assert_eq!(body["status"], "failed");
    assert_eq!(body["benchmark_id"], "prefill_throughput_256");
    assert_eq!(body["benchmark_type"], "prefill_throughput");
    assert_eq!(
        body["failure_reason"],
        "runtime crashed: OOM at decode step"
    );
    assert_eq!(body["model_name"], "mlx-community/Qwen3.5-4B-4bit");
    assert_eq!(body["model_quant"], "4bit");
    assert_eq!(body["runtime_name"], "mlx-lm");
    assert_eq!(body["runtime_version"], "0.26.0");
    // The success-only fields must not surface on a failed response.
    assert!(body.get("scored_at").is_none() || body["scored_at"].is_null());
    assert!(body.get("metrics").is_none() || body["metrics"].is_null());
    Ok(())
}

/// A terminal failure is recorded verbatim, so untrimmed padding would
/// round-trip out through `GET /jobs/{job_id}`.
#[tokio::test]
async fn test_submit_failure_trims_padded_fields_at_ingest() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;
    seed_lease(dir.path(), CLAIMED_JOB_ID, &client_id)?;

    let job_id = submit_benchmark(
        &state,
        &sk,
        &client_id,
        &json!({
            "message_type": "failure",
            "benchmark_id": "prefill_throughput_256",
            "job_id": CLAIMED_JOB_ID,
            "retriable": false,
            "failure_reason": "  OOM at decode step\n",
            "model_name": "  llama-3.2-1b\t",
            "model_quant": " q4_0 ",
            "runtime_name": " llama.cpp ",
            "runtime_version": "b5000\n",
        }),
    )
    .await?;

    let resp = authed_get(&state, &sk, &client_id, &format!("/jobs/{job_id}")).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["status"], "failed");
    assert_eq!(body["failure_reason"], "OOM at decode step");
    assert_eq!(body["model_name"], "llama-3.2-1b");
    assert_eq!(body["model_quant"], "q4_0");
    assert_eq!(body["runtime_name"], "llama.cpp");
    assert_eq!(body["runtime_version"], "b5000");
    Ok(())
}

/// A failure is never scored, so it never reaches the warehouse — `GET
/// /jobs/{job_id}` is the only place its `client_version` is readable. Echoed
/// verbatim (trimmed) when reported, null when the failure record omitted it.
#[rstest]
#[case::reported(Some("  0.14.2 "), json!("0.14.2"))]
#[case::omitted(None, serde_json::Value::Null)]
#[tokio::test]
async fn test_failure_response_echoes_client_version(
    #[case] wire: Option<&str>,
    #[case] expected: serde_json::Value,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;
    seed_lease(dir.path(), CLAIMED_JOB_ID, &client_id)?;

    let mut body = json!({
        "message_type": "failure",
        "benchmark_id": "prefill_throughput_256",
        "job_id": CLAIMED_JOB_ID,
        "retriable": false,
        "failure_reason": "OOM at decode step",
        "model_name": "llama-3.2-1b",
        "model_quant": "q4_0",
        "runtime_name": "llama.cpp",
        "runtime_version": "b5000",
    });
    if let Some(v) = wire {
        body.as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("body is not a JSON object"))?
            .insert("client_version".to_string(), json!(v));
    }

    let job_id = submit_benchmark(&state, &sk, &client_id, &body).await?;

    let resp = authed_get(&state, &sk, &client_id, &format!("/jobs/{job_id}")).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["status"], "failed");
    assert_eq!(body["client_version"], expected);
    Ok(())
}

/// Blank strings are a `400`, required or optional: `""` must not become a
/// second spelling of "not reported".
#[rstest]
#[case::model_name_blank("model_name", "   \t  ")]
#[case::client_version_empty("client_version", "")]
#[case::client_version_blank("client_version", "  \t ")]
#[tokio::test]
async fn test_submit_blank_string_returns_400(
    #[case] field: &str,
    #[case] value: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let mut body = json!({
        "benchmark_id": "prefill_throughput_256",
        "device_name": "test-device",
        "device_form_factor": "embedded",
        "device_os_name": "Linux",
        "device_os_version": "22.04",
        "device_chip_model": "test-chip",
        "device_ram_bytes": 17179869184i64,
        "model_name": "m",
        "model_quant": "q",
        "runtime_name": "rt",
        "runtime_version": "v1",
        "prefill_time_ms": 10.0
    });
    body.as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("body is not a JSON object"))?
        .insert(field.to_string(), json!(value));

    let resp = authed_post(&state, &sk, &client_id, "/benchmarks", &body).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    Ok(())
}

/// A failure body missing `retriable` or `failure_reason`, or with a
/// non-boolean `retriable`, is a `400` before the claim check: nothing is
/// written and the lease is left as it was.
#[rstest]
#[case::missing_retriable("retriable", None, "invalid submission: missing field `retriable`")]
#[case::missing_failure_reason(
    "failure_reason",
    None,
    "invalid submission: missing field `failure_reason`"
)]
#[case::non_boolean_retriable(
    "retriable",
    Some(json!("yes")),
    "invalid submission: invalid type: string \"yes\", expected a boolean"
)]
#[tokio::test]
async fn test_submit_malformed_failure_body_returns_400(
    #[case] field: &str,
    #[case] value: Option<serde_json::Value>,
    #[case] expected_error: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;
    // A live lease, so the refusal cannot be the claim check's `404`.
    seed_lease(dir.path(), CLAIMED_JOB_ID, &client_id)?;
    let leases = state.todo_store.list_leased_for_client(&client_id).await?;

    let mut body = valid_failure("prefill_throughput_256", CLAIMED_JOB_ID, false);
    let fields = body
        .as_object_mut()
        .context("the failure body is a JSON object")?;
    match value {
        Some(value) => fields.insert(field.to_owned(), value),
        None => fields.remove(field),
    };

    let resp = authed_post(&state, &sk, &client_id, "/benchmarks", &body).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(resp).await?["error"], expected_error);

    assert!(
        state
            .submission_store
            .find_job(&job(CLAIMED_JOB_ID)?)
            .await?
            .is_none()
    );
    assert_eq!(
        state.todo_store.list_leased_for_client(&client_id).await?,
        leases,
        "the refused failure must not touch the lease"
    );
    Ok(())
}

/// Only plan-attached runs report failures, so an absent id is a `400`, never a
/// server mint.
#[tokio::test]
async fn test_submit_failure_missing_job_id_returns_400() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let resp = authed_post(
        &state,
        &sk,
        &client_id,
        "/benchmarks",
        &json!({
            "message_type": "failure",
            "benchmark_id": "prefill_throughput_256",
            "retriable": false,
            "failure_reason": "OOM",
            "model_name": "m",
            "model_quant": "q",
            "runtime_name": "rt",
            "runtime_version": "v1",
        }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    Ok(())
}

/// `GET /jobs` does not full-scan the archive for an aged-out job, so one with
/// no in-window rows reports `metrics: null`.
#[tokio::test]
async fn test_get_job_processed_without_in_window_metrics_is_null_not_500() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let job_id = "aged-out-job";
    let body = json!({
        "message_type": "success",
        "benchmark_id": "prefill_throughput_256",
        "benchmark_type": "prefill_throughput",
        "client_id": client_id.as_str(),
        "job_id": job_id,
        "submitted_at": "2020-01-01T00:00:00Z",
        "device_name": "d",
        "device_form_factor": "embedded",
        "device_os_name": "Linux",
        "device_os_version": "1",
        "device_chip_model": "c",
        "device_ram_bytes": 16_000_000_000i64,
        "model_name": "m",
        "model_quant": "q",
        "runtime_name": "rt",
        "runtime_version": "v1",
        "prefill_time_ms": 10.0
    });
    // Land it directly in processed/ with no warehouse rows.
    state
        .submission_store
        .write_processed(&job(job_id)?, &body)
        .await?;

    let resp = authed_get(&state, &sk, &client_id, &format!("/jobs/{job_id}")).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let b = body_json(resp).await?;
    assert_eq!(b["status"], "processed");
    assert!(b["metrics"].is_null());
    Ok(())
}
