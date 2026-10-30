//! Held submissions: a **pending** client's results under
//! `[unverified_submissions] enabled = true`. A hold consults no permission: a
//! grant comes only after vetting, so requiring one would empty the archive.

use anyhow::Context;
use axum::http::StatusCode;
use serde_json::{Value, json};

use pipette_plan_types::{ClientId, ExpiresAt};
use pipette_web_testkit::{
    authed_post, body_json, denied_clients, job, json_str, make_state_with_unverified,
    register_and_approve, register_pending, seed_avail, seed_leased, setup_benchmarks,
};

/// A minimal valid `prefill_throughput` success body.
fn sample_body() -> Value {
    json!({
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
    })
}

fn unverified_file(
    dir: &std::path::Path,
    client_id: &ClientId,
    job_id: &str,
) -> std::path::PathBuf {
    dir.join("submissions")
        .join("unverified")
        .join(client_id.as_str())
        .join(format!("{job_id}.json"))
}

/// Where a submission should land given (client status, feature flag).
enum Dest {
    /// Held under `unverified/{client_id}/`, never staged into incoming.
    Held,
    Incoming,
    /// Rejected with `403`; nothing written.
    Rejected,
}

/// Pending is held when the feature is on and `403` when it is off; approved
/// always flows to incoming.
#[tokio::test]
async fn test_submission_disposition_matrix() -> anyhow::Result<()> {
    struct Case {
        name: &'static str,
        approved: bool,
        enabled: bool,
        want_status: StatusCode,
        dest: Dest,
    }
    let cases = [
        Case {
            name: "pending + enabled → held",
            approved: false,
            enabled: true,
            want_status: StatusCode::ACCEPTED,
            dest: Dest::Held,
        },
        Case {
            name: "pending + disabled → 403",
            approved: false,
            enabled: false,
            want_status: StatusCode::FORBIDDEN,
            dest: Dest::Rejected,
        },
        Case {
            name: "approved + enabled → incoming",
            approved: true,
            enabled: true,
            want_status: StatusCode::ACCEPTED,
            dest: Dest::Incoming,
        },
    ];

    for case in cases {
        let dir = tempfile::tempdir()?;
        setup_benchmarks(dir.path())?;
        let state = make_state_with_unverified(dir.path(), case.enabled).await?;
        let (sk, client_id) = if case.approved {
            register_and_approve(&state).await?
        } else {
            register_pending(&state).await?
        };

        let resp = authed_post(&state, &sk, &client_id, "/benchmarks", &sample_body()).await?;
        assert_eq!(resp.status(), case.want_status, "{}: status", case.name);

        let incoming = |job_id: &str| {
            dir.path()
                .join("submissions/incoming")
                .join(format!("{job_id}.json"))
        };
        let unverified_root = dir.path().join("submissions/unverified");

        match case.dest {
            Dest::Held => {
                let body = body_json(resp).await?;
                let job_id = json_str(&body, "job_id")?;
                let held = unverified_file(dir.path(), &client_id, job_id);
                assert!(held.exists(), "{}: expected held file", case.name);
                // Real client_id stamped, and not staged for the scorer.
                let stored: Value = serde_json::from_slice(&std::fs::read(&held)?)?;
                assert_eq!(stored["client_id"], client_id.as_str(), "{}", case.name);
                assert!(!incoming(job_id).exists(), "{}: must not stage", case.name);
            }
            Dest::Incoming => {
                let body = body_json(resp).await?;
                let job_id = json_str(&body, "job_id")?;
                assert!(
                    incoming(job_id).exists(),
                    "{}: expected incoming",
                    case.name
                );
                assert!(!unverified_root.exists(), "{}: must not hold", case.name);
            }
            Dest::Rejected => {
                assert!(!unverified_root.exists(), "{}: nothing written", case.name);
            }
        }
    }
    Ok(())
}

/// The id is a receipt, not a lookup key, until promotion.
#[tokio::test]
async fn test_held_submission_not_visible_via_get_job() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state_with_unverified(dir.path(), true).await?;
    let (sk, client_id) = register_pending(&state).await?;

    let resp = authed_post(&state, &sk, &client_id, "/benchmarks", &sample_body()).await?;
    let body = body_json(resp).await?;
    let job_id = json_str(&body, "job_id")?.to_string();

    // Even the owning, still-pending client cannot resolve a held job.
    let resp = pipette_web_testkit::authed_get(&state, &sk, &client_id, &format!("/jobs/{job_id}"))
        .await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    Ok(())
}

/// A claimed id in the server-minted shape, echoed by a pending device.
const ECHOED_JOB_ID: &str = "job-550e8400-e29b-41d4-a716-446655440000";

/// A retriable `message_type: "failure"` body reporting `job_id`.
fn retriable_failure_body(job_id: &str) -> Value {
    json!({
        "message_type": "failure",
        "job_id": job_id,
        "benchmark_id": "prefill_throughput_256",
        "retriable": true,
        "failure_reason": "runtime OOM",
        "model_name": "llama-3.2-1b",
        "model_quant": "q4_0",
        "runtime_name": "llama.cpp",
        "runtime_version": "b5000",
    })
}

/// Where the echoed job sits in `todo/` when the held body arrives.
enum Queued {
    /// No `todo/` entry names the job.
    Nowhere,
    Avail,
    /// Leased to another, approved client.
    LeasedToOther,
}

/// A held body touches no queue state: no claim check (`404`/`409`), no
/// `avail/` teardown, no lease renewal; a retriable failure is not recycled.
#[tokio::test]
async fn test_held_echoed_job_id_binds_no_claim() -> anyhow::Result<()> {
    struct Case {
        name: &'static str,
        body: Value,
        message_type: &'static str,
        queued: Queued,
    }
    let mut success = sample_body();
    success["job_id"] = json!(ECHOED_JOB_ID);
    let cases = [
        Case {
            name: "success, job nowhere in todo",
            body: success.clone(),
            message_type: "success",
            queued: Queued::Nowhere,
        },
        Case {
            name: "success, job unclaimed in avail",
            body: success.clone(),
            message_type: "success",
            queued: Queued::Avail,
        },
        Case {
            name: "success, leased to another client",
            body: success,
            message_type: "success",
            queued: Queued::LeasedToOther,
        },
        Case {
            name: "retriable failure, leased to another client",
            body: retriable_failure_body(ECHOED_JOB_ID),
            message_type: "failure",
            queued: Queued::LeasedToOther,
        },
    ];

    for case in cases {
        let dir = tempfile::tempdir()?;
        setup_benchmarks(dir.path())?;
        let state = make_state_with_unverified(dir.path(), true).await?;
        let (sk, client_id) = register_pending(&state).await?;
        let (_, other) = register_and_approve(&state).await?;
        let job_id = job(ECHOED_JOB_ID)?;
        let job_body = json!({"job_id": ECHOED_JOB_ID});
        match case.queued {
            Queued::Nowhere => {}
            Queued::Avail => {
                seed_avail(dir.path(), ECHOED_JOB_ID, ExpiresAt::Never, &job_body)?;
            }
            Queued::LeasedToOther => {
                seed_leased(dir.path(), ECHOED_JOB_ID, &other, &job_body)?;
            }
        }
        let avail = state.todo_store.get_avail_by_job(&job_id).await?;
        let other_leases = state.todo_store.list_leased_for_client(&other).await?;
        assert_eq!(
            (avail.is_some(), other_leases.len()),
            match case.queued {
                Queued::Nowhere => (false, 0),
                Queued::Avail => (true, 0),
                Queued::LeasedToOther => (false, 1),
            },
            "{}: seeded queue state",
            case.name
        );

        let resp = authed_post(&state, &sk, &client_id, "/benchmarks", &case.body).await?;
        assert_eq!(resp.status(), StatusCode::ACCEPTED, "{}: status", case.name);
        let body = body_json(resp).await?;
        assert_eq!(body["job_id"], ECHOED_JOB_ID, "{}: receipt", case.name);

        let held = unverified_file(dir.path(), &client_id, ECHOED_JOB_ID);
        let stored: Value = serde_json::from_slice(
            &std::fs::read(&held).with_context(|| format!("{}: expected held file", case.name))?,
        )?;
        assert_eq!(stored["job_id"], ECHOED_JOB_ID, "{}", case.name);
        assert_eq!(stored["message_type"], case.message_type, "{}", case.name);
        assert_eq!(stored["client_id"], client_id.as_str(), "{}", case.name);

        assert!(
            state.submission_store.find_job(&job_id).await?.is_none(),
            "{}: must not stage",
            case.name
        );
        assert!(
            denied_clients(&state, &job_id).await?.is_empty(),
            "{}: no denial",
            case.name
        );
        assert_eq!(
            state.todo_store.get_avail_by_job(&job_id).await?,
            avail,
            "{}: avail/ is neither torn down nor recycled into",
            case.name
        );
        assert_eq!(
            state.todo_store.list_leased_for_client(&other).await?,
            other_leases,
            "{}: the other client's lease is untouched",
            case.name
        );
    }
    Ok(())
}

#[tokio::test]
async fn test_pending_client_batch_is_held() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state_with_unverified(dir.path(), true).await?;
    let (sk, client_id) = register_pending(&state).await?;

    let resp = authed_post(
        &state,
        &sk,
        &client_id,
        "/benchmarks/batch",
        &json!({"submissions": [sample_body(), sample_body()]}),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    let results = body["results"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("missing results array"))?;
    assert_eq!(results.len(), 2);
    for r in results {
        let job_id = json_str(r, "job_id")?;
        assert!(unverified_file(dir.path(), &client_id, job_id).exists());
    }
    assert!(
        !dir.path().join("submissions/incoming").exists()
            || std::fs::read_dir(dir.path().join("submissions/incoming"))?
                .next()
                .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn test_promote_moves_held_success_to_incoming() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state_with_unverified(dir.path(), true).await?;
    let (sk, client_id) = register_pending(&state).await?;

    let resp = authed_post(&state, &sk, &client_id, "/benchmarks", &sample_body()).await?;
    let body = body_json(resp).await?;
    let job_id = json_str(&body, "job_id")?.to_string();

    // Emulate `unverified promote`: list, re-stage by message_type, delete.
    let held = state
        .submission_store
        .list_unverified_client(&client_id)
        .await?;
    assert_eq!(held.len(), 1);
    for (jid, b) in &held {
        state.submission_store.write_incoming(jid, b).await?;
        state
            .submission_store
            .delete_unverified(&client_id, jid)
            .await?;
    }

    assert!(
        dir.path()
            .join("submissions/incoming")
            .join(format!("{job_id}.json"))
            .exists()
    );
    assert!(!unverified_file(dir.path(), &client_id, &job_id).exists());
    // The job now resolves for the (now-promoted) client.
    assert!(
        state
            .submission_store
            .find_job(&job(&job_id)?)
            .await?
            .is_some()
    );
    Ok(())
}

#[tokio::test]
async fn test_delete_client_clears_held_submissions() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state_with_unverified(dir.path(), true).await?;
    let (sk, client_id) = register_pending(&state).await?;

    for _ in 0..3 {
        authed_post(&state, &sk, &client_id, "/benchmarks", &sample_body()).await?;
    }

    // Dry-run counts but keeps everything.
    let would = state
        .submission_store
        .delete_unverified_client(&client_id, true)
        .await?;
    assert_eq!(would, 3);
    assert_eq!(
        state
            .submission_store
            .list_unverified_client(&client_id)
            .await?
            .len(),
        3
    );

    let deleted = state
        .submission_store
        .delete_unverified_client(&client_id, false)
        .await?;
    assert_eq!(deleted, 3);
    assert!(
        state
            .submission_store
            .list_unverified_client(&client_id)
            .await?
            .is_empty()
    );
    Ok(())
}
