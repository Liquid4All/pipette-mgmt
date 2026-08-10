mod helpers;

use axum::http::StatusCode;
use pipette_mgmt::types::ClientId;
use serde_json::{Value, json};

use helpers::{
    authed_post, body_json, job, json_str, make_state_with_unverified, register_and_approve,
    register_pending, setup_benchmarks,
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

// ---------------------------------------------------------------------------
// Auth-branch matrix
// ---------------------------------------------------------------------------

/// Where a submission should land given (client status, feature flag).
enum Dest {
    /// Held under `unverified/{client_id}/`, never staged into incoming.
    Held,
    /// Flows to `incoming/` like any approved submission.
    Incoming,
    /// Rejected with `403`; nothing written.
    Rejected,
}

/// Disposition matrix: a pending client is held only when the feature is
/// on, an approved client always flows to incoming, and a pending client
/// is `403` when the feature is off. The three branches share setup and
/// differ only in (approved, enabled) → (status, destination), so one
/// table covers them.
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

/// Held submissions must not surface through `GET /jobs/{job_id}` — the
/// id is a receipt, not a lookup key, until promotion.
#[tokio::test]
async fn test_held_submission_not_visible_via_get_job() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state_with_unverified(dir.path(), true).await?;
    let (sk, client_id) = register_pending(&state).await?;

    let resp = authed_post(&state, &sk, &client_id, "/benchmarks", &sample_body()).await?;
    let body = body_json(resp).await?;
    let job_id = json_str(&body, "job_id")?.to_string();

    // GET /jobs requires auth; even the owning (still-pending) client
    // cannot resolve a held job.
    let resp = helpers::authed_get(&state, &sk, &client_id, &format!("/jobs/{job_id}")).await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    Ok(())
}

/// A batch from a pending client holds every item under the client's
/// unverified prefix.
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

// ---------------------------------------------------------------------------
// Operator actions: promote / delete via the store
// ---------------------------------------------------------------------------

/// Promote moves a client's held success submission into incoming/ and
/// removes it from the unverified tree.
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
            .find_job(&job(&job_id))
            .await?
            .is_some()
    );
    Ok(())
}

/// Delete-by-client removes all of one client's held submissions and
/// reports the count; dry-run reports without deleting.
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

    // Live delete removes them.
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
