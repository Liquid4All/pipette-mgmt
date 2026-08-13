mod helpers;

use std::path::Path;

use anyhow::Context;
use axum::http::StatusCode;
use chrono::Utc;
use pipette_mgmt::todo_filename::leased_key;
use pipette_mgmt::types::ClientId;
use rstest::rstest;
use serde_json::json;

use helpers::{
    authed_get, authed_post, body_json, job, make_state, register_and_approve, setup_benchmarks,
    submit_benchmark, unauthed_post,
};

/// Canonical claimed `job_id` for tests that echo a claim on the wire, in the
/// server-minted shape (`job-{uuid}`, see `JobId::from_uuid`) a real claim
/// would carry.
const CLAIMED_JOB_ID: &str = "job-550e8400-e29b-41d4-a716-446655440000";

/// Plant a `leased/{client_id}/{job_id}.{expiry}.json`so a submission echoing
/// `job_id` passes the claim-binding check (a real client gets this lease from
/// `POST /plans/claim`; tests seed it directly). The `TodoStore` trait has no
/// `write_leased`, and the local_fs layout is stable. Expiry is generated an
/// hour out so it reads as an active lease without a hardcoded date that rots.
fn seed_lease(dir: &Path, job_id: &str, client_id: &ClientId) -> anyhow::Result<()> {
    seed_lease_with_body(dir, job_id, client_id, &json!({}))
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
    assert!(body["metrics"].is_null());
    Ok(())
}

/// A plan-attached run echoes the `job_id` it claimed; the server uses it
/// as the storage key verbatim rather than minting a fresh one (see
/// planner.md §Results). Any charset-safe id is accepted — it need not be
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

/// A present-but-malformed `job_id` is a `400` at the boundary
/// (httpapi.md §2.7.3), not a silently-accepted non-UUID key.
#[tokio::test]
async fn test_submit_with_invalid_job_id_returns_400() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    // "Invalid" means an unsafe charset, not "not a UUID": every string-sourced
    // job id — server-minted `job-{uuid}` or client echo — goes through
    // `JobId::try_new`, so a path-significant `.` is rejected on format, before
    // the claim is even checked.
    let mut body = valid_submission("prefill_throughput_256");
    body["job_id"] = json!("bad.id");
    let resp = authed_post(&state, &sk, &client_id, "/benchmarks", &body).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    Ok(())
}

/// A malformed `job_id` in the path is a `400` at the boundary
/// (`JobId::try_new`), before any store lookup — for both `GET /jobs/{job_id}`
/// and `GET /jobs/{job_id}/eval-sample-results`. `bad.id` is a single path
/// segment (so it routes) but fails the `[A-Za-z0-9-]` charset.
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

/// A submission echoing a `job_id` the client doesn't hold a lease for is
/// rejected with `404` — no live claim (recycled, completed, or never
/// claimed). The client must reclaim before submitting (httpapi.md §2.7.3).
/// This is the claim-binding gate that stops a client clobbering the
/// (client-unpartitioned) `incoming/`/`processed/` key with a foreign id.
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
    Ok(())
}

/// The accepted-submission event records acceptances only. A submission naming a
/// `job_id` the client does not hold is refused by the claim check, and at that
/// point the id has only been checked for shape — so an operator reading the log
/// must not find the request announced as accepted. The accepting case is what
/// keeps this honest: it proves the event still fires when it should.
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

    let (result, logs) = helpers::capture_logs(|rt| -> anyhow::Result<StatusCode> {
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
    });

    assert_eq!(result?, expected);
    assert_eq!(
        logs.contains("accepted submission"),
        expect_logged,
        "log said the wrong thing for a {expected} response:\n{logs}"
    );
    Ok(())
}

/// A plan-attached submission is rejected with `404` while the client's
/// pending-reindex flag is up, even though it holds the lease: the profile
/// change that set the flag relinquished the client's leases, so the claim is
/// void — and the claim-verification renewal must not rename (resurrect) the
/// lease mid-relinquish (httpapi.md §2.7.3).
#[tokio::test]
async fn test_submit_gated_while_pending_reindex() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    seed_lease(dir.path(), CLAIMED_JOB_ID, &client_id)?;
    state.todo_store.write_pending_reindex(&client_id).await?;

    let mut body = valid_submission("prefill_throughput_256");
    body["job_id"] = json!(CLAIMED_JOB_ID);
    let resp = authed_post(&state, &sk, &client_id, "/benchmarks", &body).await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // Clearing the flag (what the reindex pass does) lifts the gate.
    helpers::clear_pending_reindex(&*state.todo_store, &client_id).await?;
    let job_id = submit_benchmark(&state, &sk, &client_id, &body).await?;
    assert_eq!(job_id, CLAIMED_JOB_ID);
    Ok(())
}

/// A submission echoing a `job_id` that is leased to a *different* client is
/// rejected with `409` — the caller has been superseded (httpapi.md §2.7.3).
#[tokio::test]
async fn test_submit_job_id_leased_to_other_client_returns_409() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    // The job is leased to someone else, not the caller.
    let other = ClientId::try_new("ev1_other")?;
    seed_lease(dir.path(), CLAIMED_JOB_ID, &other)?;

    let mut body = valid_submission("prefill_throughput_256");
    body["job_id"] = json!(CLAIMED_JOB_ID);
    let resp = authed_post(&state, &sk, &client_id, "/benchmarks", &body).await?;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    Ok(())
}

/// A complete `todo/` job body to plant inside a `leased/` entry, so a
/// retriable failure can recycle it (`recycle_lease` reads `expires_at` from the
/// body) and the escalation can read its `clients` array. `clients` lists the
/// eligible clients by id.
fn leased_job_body(job_id: &str, clients: &[&str]) -> serde_json::Value {
    json!({
        "job_id": job_id,
        "expires_at": "never",
        "clients": clients,
        "spec": {
            "benchmark": "prefill_throughput_256",
            "model": {"type": "gguf_text", "source": "huggingface", "org": "meta-llama", "repo_name": "Llama-3.2-1B-GGUF", "path": "Llama-3.2-1B-Q4_0.gguf"},
            "runtime": {"type": "llamacpp_cli_stock_tools", "source": "github_release", "repository_version": "b5000", "flavor": "macos-arm64"},
        },
    })
}

/// Plant a `leased/{client_id}/{job_id}.{expiry}.json`carrying a full job
/// `body`, simulating a job this client currently holds (the failure paths read
/// and recycle this body, unlike the empty marker `seed_lease` plants).
fn seed_lease_with_body(
    dir: &Path,
    job_id: &str,
    client_id: &ClientId,
    body: &serde_json::Value,
) -> anyhow::Result<()> {
    let job = job(job_id);
    let expiry = Utc::now() + chrono::Duration::hours(1);
    let path = dir
        .join("todo")
        .join("leased")
        .join(leased_key(&job, client_id, expiry));
    std::fs::create_dir_all(path.parent().context("lease path has no parent")?)?;
    std::fs::write(path, serde_json::to_vec(body)?)?;
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
    seed_lease_with_body(dir.path(), claimed, &client_id, &body)?;

    let failure = valid_failure("prefill_throughput_256", claimed, true);
    let resp = authed_post(&state, &sk, &client_id, "/benchmarks", &failure).await?;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    let job = job(claimed);
    // No result recorded — the job is neither in incoming/ nor processed/.
    assert!(state.submission_store.find_job(&job).await?.is_none());
    // This client is denied; the job is back in avail/; its lease is gone.
    let denied = state.todo_store.list_denied_for_job(&job).await?;
    assert!(denied.iter().any(|c| c.as_str() == client_id.as_str()));
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
    seed_lease_with_body(dir.path(), claimed, &client_id, &body)?;

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
    let job = job(claimed);
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

/// When the reporting client is the *sole* eligible client of a `clients`-only
/// job, a retriable failure exhausts the eligible set: the job can never
/// succeed. The submit path records only the denial — escalation belongs to
/// the `queue-maintenance` all-denied reconciliation pass — so the job sits
/// unclaimable (every listed client is denied) until the next run converts it
/// to a synthetic `"system"` terminal failure and tears it down.
#[tokio::test]
async fn test_submit_retriable_failure_all_denied_escalates_via_queue_maintenance()
-> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let claimed = CLAIMED_JOB_ID;
    let body = leased_job_body(claimed, &[client_id.as_str()]);
    seed_lease_with_body(dir.path(), claimed, &client_id, &body)?;

    let failure = valid_failure("prefill_throughput_256", claimed, true);
    let resp = authed_post(&state, &sk, &client_id, "/benchmarks", &failure).await?;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    let job = job(claimed);
    // The request records the denial and recycles the job; no record yet.
    assert!(state.submission_store.find_job(&job).await?.is_none());
    assert!(
        state
            .todo_store
            .list_denied_for_job(&job)
            .await?
            .contains(&client_id)
    );
    assert!(state.todo_store.get_avail_by_job(&job).await?.is_some());

    // The next queue-maintenance run escalates.
    let catalog = state.catalog_cache.get().await?;
    pipette_mgmt::queue_maintenance::run(
        &*state.todo_store,
        &*state.auth_store,
        &*state.submission_store,
        &catalog,
        std::time::Duration::from_secs(86_400),
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
    assert_eq!(
        rec.body["failure_reason"],
        "All eligible clients reported failure"
    );
    // Recorded as terminal in processed/, not parked in incoming/: the scorer
    // ignores failure bodies, so an incoming/ write would linger forever.
    assert_eq!(rec.state.as_str(), "processed");
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
    let signing_key = ed25519_dalek::SigningKey::generate(&mut rand_core::OsRng);
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

/// Field-omission behavior at the submission boundary: a required device field
/// (`device_name`) is still a `400`, but the runtime grouping fields
/// (`runtime_name`, `runtime_version`) are now optional — the authoritative
/// identity lives in `runtime_descriptor` — so omitting them is accepted (`202`).
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

/// The opaque refs are stored verbatim, but a present-but-non-JSON value would
/// silently defeat canonicalization — so each one must at least parse as JSON,
/// and a non-JSON blob is a `400`. `benchmark_flags` is held to more than
/// that: it is always a map of settings, so valid JSON that is not an object
/// is rejected too, rather than canonicalizing cleanly into a useless
/// grouping key.
#[rstest]
#[case::model_descriptor("model_descriptor", "not valid json {")]
#[case::runtime_descriptor("runtime_descriptor", "llama_cpp b8683")]
#[case::benchmark_flags_not_json("benchmark_flags", "skip_thermal=true")]
#[case::benchmark_flags_not_an_object("benchmark_flags", "\"enforced\"")]
#[case::benchmark_flags_array("benchmark_flags", "[{\"skip_thermal\":true}]")]
#[case::benchmark_flags_empty("benchmark_flags", "")]
#[case::benchmark_flags_blank("benchmark_flags", "   ")]
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

/// Both `model_params_total_millions` and `model_params_active_millions` are
/// optional at the HTTP layer. A submission that omits both is
/// accepted; the scorer will fill from the catalog if the model is
/// known, or leave the warehouse columns null otherwise.
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

/// The per-run memory observations are byte counts, so a negative value is
/// impossible and is a `400`. Zero is a real reading — a run that touched no
/// swap reports `0` rather than omitting the field — so it is accepted.
#[rstest]
#[case::negative_swap("observation_max_swap_bytes", -1, StatusCode::BAD_REQUEST)]
#[case::negative_host("observation_max_host_bytes", -4096, StatusCode::BAD_REQUEST)]
#[case::zero_swap("observation_max_swap_bytes", 0, StatusCode::ACCEPTED)]
#[case::zero_host("observation_max_host_bytes", 0, StatusCode::ACCEPTED)]
#[tokio::test]
async fn test_submit_observed_memory_sign(
    #[case] field: &str,
    #[case] value: i64,
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
    body[field] = json!(value);

    let resp = authed_post(&state, &sk, &client_id, "/benchmarks", &body).await?;
    assert_eq!(resp.status(), expected);
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

#[tokio::test]
async fn test_submit_gpu_vram_without_gpu_model_returns_400() -> anyhow::Result<()> {
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
            "device_gpu_vram_bytes": 8589934592i64,
            // missing device_gpu_model
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

fn valid_submission(benchmark_id: &str) -> serde_json::Value {
    json!({
        "benchmark_id": benchmark_id,
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
    })
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
        assert_eq!(r["index"].as_u64().unwrap() as usize, i);
        let job_id = r["job_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("result {i} missing job_id: {r}"))?;

        // Each returned job should be fetchable.
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

    // Second submission is missing device_name (a required field; the
    // model/runtime grouping fields are now optional).
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
    let results = body["results"].as_array().unwrap();
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
            .unwrap()
            .contains("device_name")
    );

    // Item 2: unknown benchmark
    assert_eq!(results[2]["index"], 2);
    assert!(results[2]["job_id"].is_null());
    assert!(results[2]["error"].as_str().unwrap().contains("not found"));
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

#[tokio::test]
async fn test_submit_batch_rejected_for_pending_client() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    // Register but don't approve.
    let signing_key = ed25519_dalek::SigningKey::generate(&mut rand_core::OsRng);
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
    let client_id = ClientId::try_new(body["client_id"].as_str().unwrap())?;

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

/// Padded device / model / runtime fields are accepted but trimmed
/// at ingress, so the on-disk body has canonical values and every
/// downstream consumer reads them clean.
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
        .find_job(&job(job_id))
        .await?
        .context("submission should be persisted")?;

    // Padded fields are trimmed before write.
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

/// Failure submissions land in `processed/` at write time and
/// `GET /jobs/{job_id}` reports them as `status: "failed"` with
/// `failure_reason`. They never appear as `incoming`.
#[tokio::test]
async fn test_submit_failure_routes_to_processed_and_get_reports_failed() -> anyhow::Result<()> {
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

/// Padded model / runtime / failure_reason fields on a failure body
/// are trimmed at ingest, so `GET /jobs/{job_id}` returns canonical
/// values. A non-retriable (terminal) failure bypasses the scorer and is
/// recorded directly, so without ingest-time trimming the padding would
/// round-trip back out via GET. (A retriable failure records no result —
/// see `test_submit_retriable_failure_denies_and_recycles`.)
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

/// Blank strings are rejected at the wire by `NonEmptyTrimmedString`, whether
/// the field is required (`model_name`) or optional (`client_version`). An
/// optional field is the more interesting case: `""` must not become a second
/// spelling of "not reported", so it is a `400` rather than a silent `None`.
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

/// Failure body missing a required field (`retriable`) is rejected at
/// the boundary with a 400, not silently routed to `processed/`. A valid
/// `job_id` is supplied so the 400 is attributable to `retriable`, not the
/// separate missing-`job_id` check.
#[tokio::test]
async fn test_submit_failure_missing_retriable_returns_400() -> anyhow::Result<()> {
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
            "job_id": CLAIMED_JOB_ID,
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

/// Only plan-attached runs report failures, so `job_id` is required on a
/// failure body — an absent id is a 400, never a server mint
/// (httpapi.md §2.7.2). Contrast with success bodies, where an absent
/// `job_id` is minted (`test_submit_and_get_job`).
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

/// A processed job whose metrics are not within the recent read window
/// (here: no warehouse rows at all) must report as `processed` with
/// `metrics: null` — not `500`. This locks in the hard-cap behavior:
/// `GET /jobs` does not full-scan the archive for an aged-out job.
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
        .write_processed(&job(job_id), &body)
        .await?;

    let resp = authed_get(&state, &sk, &client_id, &format!("/jobs/{job_id}")).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let b = body_json(resp).await?;
    assert_eq!(b["status"], "processed");
    assert!(b["metrics"].is_null());
    Ok(())
}
