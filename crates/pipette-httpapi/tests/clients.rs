//! The client's own record over `GET /clients/me` and `PATCH /clients/me`, plus
//! the operator-side state that hangs off it: tags, the eligible-index reindex
//! flag, and the delete cascade.

use std::collections::BTreeSet;

use axum::http::StatusCode;
use rstest::rstest;
use serde_json::json;

use pipette_core::client::{Client, ClientStatus};
use pipette_plan_types::{ClientId, ExpiresAt, NonEmptyTrimmedString, Tag, UserPermission};
use pipette_store::{ClaimResult, drain_clients, purge_client_todo_state};
use pipette_web_testkit::{
    auth_headers, authed_get, authed_patch, authed_post, authed_user_get, body_json,
    denied_clients, job, json_str, make_state, raw_request, register_and_approve, register_pending,
    registered_user, seed_avail, setup_benchmarks, signed_empty, signed_json, unauthed_post,
};

#[tokio::test]
async fn test_get_me() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let resp = authed_get(&state, &sk, &client_id, "/clients/me").await?;

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(
        body["client_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing client_id"))?,
        client_id.as_str()
    );
    assert_eq!(body["organization"], "test-org");
    assert_eq!(body["status"], "approved");
    Ok(())
}

/// This is how a device watches for an operator's decision.
#[tokio::test]
async fn test_a_pending_client_reads_its_own_record_and_sees_its_approval() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_pending(&state).await?;

    let resp = authed_get(&state, &sk, &client_id, "/clients/me").await?;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "a pending client is not refused its own record"
    );
    let body = body_json(resp).await?;
    assert_eq!(body["client_id"], client_id.as_str());
    assert_eq!(body["status"], "pending");

    let admin = registered_user(&state, &[UserPermission::ManageAnyClients]).await?;
    let resp = signed_empty(
        &state,
        &admin,
        "POST",
        &format!("/api/v1/clients/{client_id}/approve"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);

    // The same key, nothing re-registered, now reads the decision.
    let resp = authed_get(&state, &sk, &client_id, "/clients/me").await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await?["status"], "approved");
    Ok(())
}

#[tokio::test]
async fn test_patch_client_me() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let resp = authed_patch(
        &state,
        &sk,
        &client_id,
        "/clients/me",
        &json!({"client_details": "updated-details"}),
    )
    .await?;

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["client_details"], "updated-details");
    Ok(())
}

/// It refreshes its profile at startup, before an operator has decided on it.
#[tokio::test]
async fn test_a_pending_client_updates_its_own_profile() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_pending(&state).await?;

    let resp = authed_patch(
        &state,
        &sk,
        &client_id,
        "/clients/me",
        &json!({"device_form_factor": "laptop"}),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["device_form_factor"], "laptop");
    assert_eq!(body["status"], "pending", "updating is not approval");
    assert_eq!(body["reindex_pending"], true);

    let stored = state
        .client_store
        .get_client(&client_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("client {client_id} not stored"))?;
    assert_eq!(
        serde_json::to_value(&stored)?["device_form_factor"],
        "laptop"
    );
    Ok(())
}

/// It reaches the device handler, which knows no such client, never the
/// operator route for a device named `me`.
#[tokio::test]
async fn test_a_user_patching_clients_me_is_refused_as_an_unknown_client() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    // A device whose id is literally `me`, which the dynamic route would write.
    let (_, client_id) = register_pending(&state).await?;
    let mut named_me = state
        .client_store
        .get_client(&client_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("client {client_id} not stored"))?;
    named_me.client_id = ClientId::try_new("me")?;
    state.client_store.put_client(&named_me).await?;
    let admin = registered_user(&state, &[UserPermission::ManageAnyClients]).await?;

    let resp = signed_json(
        &state,
        &admin,
        "PATCH",
        "/clients/me",
        &json!({"client_details": "renamed by an admin"}),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(body_json(resp).await?, json!({"error": "client not found"}));

    let stored = state
        .client_store
        .get_client(&named_me.client_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("the device named me vanished"))?;
    assert_eq!(stored.client_details.as_str(), "test");
    Ok(())
}

#[tokio::test]
async fn test_get_me_returns_unset_device_fields_as_null() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let resp = authed_get(&state, &sk, &client_id, "/clients/me").await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;

    // Present as null when unset, unlike the stored record, which omits them.
    let obj = body
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("expected object"))?;
    [
        "device_name",
        "device_form_factor",
        "device_os_name",
        "device_os_version",
        "device_cpu_arch",
        "device_os_family",
        "device_chip_model",
        "device_ram_bytes",
        "device_gpu_model",
        "device_gpu_vram_bytes",
        "device_npu_model",
        "device_npu_vram_bytes",
    ]
    .into_iter()
    .for_each(|key| {
        assert!(obj.contains_key(key), "missing {key}");
        assert!(body[key].is_null(), "{key} should be null");
    });
    Ok(())
}

/// The whole contract `pipette-clients` deserializes: no key dropped or added.
#[tokio::test]
async fn test_get_me_returns_every_field_registration_stored() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let sk = pipette_web_testkit::signing_key()?;

    let resp = unauthed_post(
        &state,
        "/clients/register",
        &json!({
            "public_key": hex::encode(sk.verifying_key().as_bytes()),
            "organization": "test-org",
            "client_details": "bench rig",
            "contact_email": "lab@example.test",
            "capabilities": ["runtime:mlx", "runtime:llama_cpp"],
            "device_name": "MacBook Pro 14-inch (2023)",
            "device_form_factor": "laptop",
            "device_os_name": "macOS",
            "device_os_version": "15.3",
            "device_cpu_arch": "amd64",
            "device_os_family": "Linux",
            "device_chip_model": "Apple M3 Pro",
            "device_ram_bytes": 36_000_000_000u64,
            "device_gpu_model": "M3 Pro GPU",
            "device_gpu_vram_bytes": 18_000_000_000u64,
            "device_npu_model": "Apple Neural Engine",
            "device_npu_vram_bytes": 8_000_000_000u64,
        }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let client_id = ClientId::try_new(json_str(&body_json(resp).await?, "client_id")?)?;

    let stored = state
        .client_store
        .get_client(&client_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("client {client_id} not stored"))?;
    let derived = pipette_core::client::normalized_flags(&stored.device_profile);
    assert!(derived.contains("arch:x86_64"));
    assert!(derived.contains("os_family:linux"));

    let (key, user_id, key_id) = registered_user(&state, &[UserPermission::ReadClients]).await?;
    let fleet_response = authed_user_get(
        &state,
        &key,
        &user_id,
        &key_id,
        &format!("/api/v1/clients/{client_id}"),
    )
    .await?;
    assert_eq!(fleet_response.status(), StatusCode::OK);
    let fleet_body = body_json(fleet_response).await?;
    let fleet_flags = fleet_body["derived_capabilities"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("derived_capabilities is not an array"))?;
    assert!(fleet_flags.contains(&json!("arch:x86_64")));
    assert!(fleet_flags.contains(&json!("os_family:linux")));

    let resp = authed_get(&state, &sk, &client_id, "/clients/me").await?;
    assert_eq!(resp.status(), StatusCode::OK);
    // Pending (no auto-approve), the registration-default grant, and a reindex
    // flag because the device registered a profile.
    assert_eq!(
        body_json(resp).await?,
        json!({
            "client_id": client_id.as_str(),
            "organization": "test-org",
            "client_details": "bench rig",
            "contact_email": "lab@example.test",
            "status": "pending",
            "tags": [],
            "permissions": ["submit_results"],
            "reindex_pending": true,
            "capabilities": ["runtime:llama_cpp", "runtime:mlx"],
            "device_name": "MacBook Pro 14-inch (2023)",
            "device_form_factor": "laptop",
            "device_os_name": "macOS",
            "device_os_version": "15.3",
            "device_cpu_arch": "amd64",
            "device_os_family": "Linux",
            "device_chip_model": "Apple M3 Pro",
            "device_ram_bytes": 36_000_000_000u64,
            "device_gpu_model": "M3 Pro GPU",
            "device_gpu_vram_bytes": 18_000_000_000u64,
            "device_npu_model": "Apple Neural Engine",
            "device_npu_vram_bytes": 8_000_000_000u64,
        })
    );
    Ok(())
}

#[tokio::test]
async fn test_patch_device_field_queues_reindex() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let resp = authed_patch(
        &state,
        &sk,
        &client_id,
        "/clients/me",
        &json!({"device_form_factor": "laptop", "device_ram_bytes": 36_000_000_000u64}),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["device_form_factor"], "laptop");
    assert_eq!(body["device_ram_bytes"], 36_000_000_000u64);
    // The client cannot compute the profile diff itself.
    assert_eq!(body["reindex_pending"], true);

    // One flag before the lease relinquish (the gate), one after the record
    // persist, so a racing reindex cannot consume it against the old record.
    let keys: Vec<_> = state
        .todo_store
        .list_pending_reindex()
        .await?
        .into_iter()
        .filter(|(flagged, _)| flagged == &client_id)
        .map(|(_, key)| key)
        .collect();
    assert_eq!(keys.len(), 2, "expected pre- and post-persist flag writes");
    assert_ne!(keys[0], keys[1]);

    // GET /clients/me surfaces the same signal while the flag is up.
    let resp = authed_get(&state, &sk, &client_id, "/clients/me").await?;
    assert_eq!(body_json(resp).await?["reindex_pending"], true);
    Ok(())
}

/// It fails before any lease is relinquished or the record written, so a retry
/// sees the same diff.
#[tokio::test]
async fn test_a_failed_reindex_flag_write_fails_the_patch_and_keeps_the_old_record()
-> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;
    let job = seed_avail(
        dir.path(),
        "job-1",
        ExpiresAt::Never,
        &json!({"job_id": "job-1"}),
    )?;
    let claimed = state
        .todo_store
        .claim_job(
            &job,
            ExpiresAt::Never,
            &client_id,
            chrono::Utc::now() + chrono::Duration::hours(1),
        )
        .await?;
    assert!(matches!(claimed, ClaimResult::Claimed(_)));

    // A regular file where the flag directory belongs fails the flag write.
    let flags = dir.path().join("todo").join("pending-reindex");
    std::fs::remove_dir_all(&flags)?;
    std::fs::write(&flags, b"not a directory")?;

    let patch = json!({"device_form_factor": "laptop"});
    let resp = authed_patch(&state, &sk, &client_id, "/clients/me", &patch).await?;
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        body_json(resp).await?,
        json!({"error": "internal server error"})
    );

    // Read the store, not `GET /clients/me`, whose flag read hits the same file.
    let stored = state
        .client_store
        .get_client(&client_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("client {client_id} not stored"))?;
    assert!(
        stored.device_profile.device_form_factor.is_none(),
        "the old record stays durable"
    );
    assert_eq!(
        state
            .todo_store
            .list_leased_for_client(&client_id)
            .await?
            .len(),
        1,
        "the flag goes first, so no lease was relinquished"
    );

    // Once the store recovers, the retry computes the same diff and repairs.
    std::fs::remove_file(&flags)?;
    std::fs::create_dir(&flags)?;
    let resp = authed_patch(&state, &sk, &client_id, "/clients/me", &patch).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["device_form_factor"], "laptop");
    assert_eq!(body["reindex_pending"], true);
    assert!(
        state
            .todo_store
            .list_leased_for_client(&client_id)
            .await?
            .is_empty(),
        "the retry relinquishes the lease the failed PATCH kept"
    );
    Ok(())
}

/// The body is held back until an admin deletes or edits the record, so the write is stale.
#[rstest]
#[case::deleted(true)]
#[case::edited(false)]
#[tokio::test]
async fn test_a_patch_held_open_across_an_admin_change_is_refused(
    #[case] delete: bool,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;
    let read = state
        .client_store
        .get_client(&client_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("client {client_id} not stored"))?;
    let edited = Client {
        client_details: NonEmptyTrimmedString::try_new("set by an admin")?,
        ..read
    };
    let patch = json!({"device_form_factor": "laptop"});

    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (body_tx, body_rx) = tokio::sync::oneshot::channel::<axum::body::Bytes>();
    // Header extractors run before `ApiJson`, so the first poll comes after authentication.
    let body = axum::body::Body::from_stream(futures::stream::once(async move {
        started_tx
            .send(())
            .map_err(|()| std::io::Error::other("the test stopped waiting"))?;
        body_rx.await.map_err(std::io::Error::other)
    }));
    let headers = auth_headers(&sk, &client_id, "PATCH", "/clients/me")?;
    let admin = async {
        started_rx.await?;
        if delete {
            state.client_store.delete_client(&client_id).await?;
        } else {
            state.client_store.put_client(&edited).await?;
        }
        body_tx
            .send(serde_json::to_vec(&patch)?.into())
            .map_err(|_| anyhow::anyhow!("the request stopped reading its body"))?;
        anyhow::Ok(())
    };
    let request = raw_request(
        &state,
        "PATCH",
        "/clients/me",
        &headers,
        Some("application/json"),
        body,
    );
    let (resp, admin) = tokio::join!(request, admin);
    admin?;
    let resp = resp?;
    anyhow::ensure!(
        resp.status() == StatusCode::CONFLICT,
        "a stale PATCH answered {}",
        resp.status()
    );

    let stored = state.client_store.get_client(&client_id).await?;
    let (expected, resend) = if delete {
        (None, StatusCode::UNAUTHORIZED)
    } else {
        (Some(&edited), StatusCode::OK)
    };
    anyhow::ensure!(
        stored.as_ref() == expected,
        "the stale PATCH overwrote the admin's change: {stored:?}"
    );
    let resp = authed_patch(&state, &sk, &client_id, "/clients/me", &patch).await?;
    anyhow::ensure!(
        resp.status() == resend,
        "a resend answered {}",
        resp.status()
    );
    if !delete {
        let body = body_json(resp).await?;
        anyhow::ensure!(
            body["client_details"] == "set by an admin" && body["device_form_factor"] == "laptop",
            "the resend did not apply on top of the admin's edit: {body}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn test_patch_capabilities_replaces_and_queues_reindex() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    // Capabilities feed the matcher, so they void standing as a profile does.
    let resp = authed_patch(
        &state,
        &sk,
        &client_id,
        "/clients/me",
        &json!({"capabilities": ["runtime:llama_cpp"]}),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["capabilities"], json!(["runtime:llama_cpp"]));
    assert_eq!(body["reindex_pending"], true);

    // A present `capabilities` replaces the stored set wholesale.
    let resp = authed_patch(
        &state,
        &sk,
        &client_id,
        "/clients/me",
        &json!({"capabilities": ["runtime:mlx"]}),
    )
    .await?;
    let body = body_json(resp).await?;
    assert_eq!(body["capabilities"], json!(["runtime:mlx"]));

    let before = state.todo_store.list_pending_reindex().await?.len();
    let resp = authed_patch(
        &state,
        &sk,
        &client_id,
        "/clients/me",
        &json!({"client_details": "renamed"}),
    )
    .await?;
    let body = body_json(resp).await?;
    assert_eq!(body["capabilities"], json!(["runtime:mlx"]));
    assert_eq!(
        state.todo_store.list_pending_reindex().await?.len(),
        before,
        "a client_details-only PATCH must not queue a reindex"
    );

    // An identical set queues nothing, so a client may PATCH unconditionally.
    let baseline = state.todo_store.list_pending_reindex().await?.len();
    let resp = authed_patch(
        &state,
        &sk,
        &client_id,
        "/clients/me",
        &json!({"capabilities": ["runtime:mlx"]}),
    )
    .await?;
    assert_eq!(
        body_json(resp).await?["capabilities"],
        json!(["runtime:mlx"])
    );
    assert_eq!(
        state.todo_store.list_pending_reindex().await?.len(),
        baseline,
        "an identical capability set must not queue a reindex"
    );

    // `null` leaves the stored set unchanged and queues no reindex.
    let resp = authed_patch(
        &state,
        &sk,
        &client_id,
        "/clients/me",
        &json!({"capabilities": null}),
    )
    .await?;
    assert_eq!(
        body_json(resp).await?["capabilities"],
        json!(["runtime:mlx"])
    );
    assert_eq!(
        state.todo_store.list_pending_reindex().await?.len(),
        baseline,
        "a null capabilities PATCH must not queue a reindex"
    );

    // An explicit empty array clears the set, a real change.
    let resp = authed_patch(
        &state,
        &sk,
        &client_id,
        "/clients/me",
        &json!({"capabilities": []}),
    )
    .await?;
    let body = body_json(resp).await?;
    assert_eq!(body["capabilities"], json!([]));
    assert_eq!(body["reindex_pending"], true);
    assert!(
        state.todo_store.list_pending_reindex().await?.len() > baseline,
        "clearing the capability set must queue a reindex"
    );
    Ok(())
}

#[tokio::test]
async fn test_patch_invalid_capability_returns_400() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    // Register's validation; its cases are in client_registration.rs.
    for (flag, want) in [
        ("chip:a19", "reserved namespace"),
        ("os_family:linux", "reserved namespace"),
        ("tag:lab", "names a tag"),
        ("OS:ios", "lowercase"),
    ] {
        let resp = authed_patch(
            &state,
            &sk,
            &client_id,
            "/clients/me",
            &json!({ "capabilities": [flag] }),
        )
        .await?;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "flag {flag:?}");
        assert!(
            body_json(resp)
                .await?
                .get("error")
                .and_then(|e| e.as_str())
                .is_some_and(|e| e.contains(want)),
            "flag {flag:?} should report {want:?}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn test_patch_merges_device_fields_without_clobbering() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    authed_patch(
        &state,
        &sk,
        &client_id,
        "/clients/me",
        &json!({"device_form_factor": "laptop", "device_ram_bytes": 36_000_000_000u64}),
    )
    .await?;

    let resp = authed_patch(
        &state,
        &sk,
        &client_id,
        "/clients/me",
        &json!({"device_chip_model": "Apple M3 Pro"}),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["device_chip_model"], "Apple M3 Pro");
    assert_eq!(body["device_form_factor"], "laptop");
    assert_eq!(body["device_ram_bytes"], 36_000_000_000u64);
    Ok(())
}

/// A device field cannot be cleared one at a time. Swept per field to guard
/// the hand-written merge arms in `apply_to`; `seed` carries any companion.
#[rstest]
#[case::name(json!({"device_name": "studio-01"}), "device_name")]
#[case::form_factor(json!({"device_form_factor": "laptop"}), "device_form_factor")]
#[case::os_name(json!({"device_os_name": "macOS"}), "device_os_name")]
#[case::os_version(json!({"device_os_name": "macOS", "device_os_version": "15.3"}), "device_os_version")]
#[case::cpu_arch(json!({"device_cpu_arch": "amd64"}), "device_cpu_arch")]
#[case::os_family(json!({"device_os_family": "Linux"}), "device_os_family")]
#[case::chip_model(json!({"device_chip_model": "Apple M3 Pro"}), "device_chip_model")]
#[case::ram(json!({"device_ram_bytes": 36_000_000_000u64}), "device_ram_bytes")]
#[case::gpu_model(json!({"device_gpu_model": "M3 Pro GPU"}), "device_gpu_model")]
#[case::gpu_vram(json!({"device_gpu_model": "M3 Pro GPU", "device_gpu_vram_bytes": 18_000_000_000u64}), "device_gpu_vram_bytes")]
#[case::npu_model(json!({"device_npu_model": "Apple Neural Engine"}), "device_npu_model")]
#[case::npu_vram(json!({"device_npu_model": "Apple Neural Engine", "device_npu_vram_bytes": 8_000_000_000u64}), "device_npu_vram_bytes")]
#[tokio::test]
async fn test_patch_explicit_null_is_noop_per_field(
    #[case] seed: serde_json::Value,
    #[case] field: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let resp = authed_patch(&state, &sk, &client_id, "/clients/me", &seed).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let expected = body_json(resp).await?[field].clone();
    assert!(!expected.is_null(), "seed failed to set {field}");

    let clear = serde_json::Value::Object(
        [(field.to_string(), serde_json::Value::Null)]
            .into_iter()
            .collect(),
    );
    let resp = authed_patch(&state, &sk, &client_id, "/clients/me", &clear).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body[field], expected, "{field} should be unchanged by null");
    Ok(())
}

#[tokio::test]
async fn test_patch_client_details_only_skips_reindex() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let resp = authed_patch(
        &state,
        &sk,
        &client_id,
        "/clients/me",
        &json!({"client_details": "updated"}),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await?["reindex_pending"], false);

    // `match` rules read only the device profile.
    let flags = state.todo_store.list_pending_reindex().await?;
    assert!(flags.is_empty());
    Ok(())
}

#[rstest]
#[case::form_factor(json!({"device_form_factor": "spaceship"}), "must be one of")]
#[case::form_factor_empty(json!({"device_form_factor": ""}), "must be one of")]
#[case::gpu_vram(json!({"device_gpu_vram_bytes": 18_000_000_000u64}), "device_gpu_vram_bytes requires device_gpu_model")]
#[case::npu_vram(json!({"device_npu_vram_bytes": 8_000_000_000u64}), "device_npu_vram_bytes requires device_npu_model")]
#[case::os_version(json!({"device_os_version": "15.3"}), "device_os_version requires device_os_name")]
#[tokio::test]
async fn test_patch_invalid_device_profile_returns_400(
    #[case] payload: serde_json::Value,
    #[case] want: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let resp = authed_patch(&state, &sk, &client_id, "/clients/me", &payload).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await?;
    assert!(
        body["error"].as_str().is_some_and(|e| e.contains(want)),
        "error should contain {want:?}"
    );
    Ok(())
}

/// Including a type error in the flattened `device_*` fields: a `400` in the
/// envelope, not axum's bare `422`.
#[rstest]
#[case::empty_details(
    json!({"client_details": ""}),
    "client_details: must be a non-empty string"
)]
// Flattening drops the field path, so the value and the wanted type identify it.
#[case::ram_as_string(
    json!({"device_ram_bytes": "lots"}),
    r#"invalid type: string "lots", expected u64"#
)]
#[case::capabilities_not_an_array(
    json!({"capabilities": "runtime:mlx"}),
    r#"capabilities: invalid type: string "runtime:mlx", expected a sequence"#
)]
#[tokio::test]
async fn test_patch_undeserializable_body_returns_400_and_writes_nothing(
    #[case] payload: serde_json::Value,
    #[case] want: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;
    let before = serde_json::to_value(state.client_store.get_client(&client_id).await?)?;

    let resp = authed_patch(&state, &sk, &client_id, "/clients/me", &payload).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await?;
    let error = json_str(&body, "error")?;
    assert!(
        error.contains(want),
        "error should contain {want:?}: {error}"
    );

    assert_eq!(
        serde_json::to_value(state.client_store.get_client(&client_id).await?)?,
        before,
        "a refused PATCH must not touch the record"
    );
    assert!(state.todo_store.list_pending_reindex().await?.is_empty());
    Ok(())
}

/// Validated against the merged profile, so a dependent alone succeeds once its
/// companion is stored.
#[rstest]
#[case::gpu(json!({"device_gpu_model": "NVIDIA RTX 4090"}), json!({"device_gpu_vram_bytes": 24_000_000_000u64}))]
#[case::npu(json!({"device_npu_model": "Apple Neural Engine"}), json!({"device_npu_vram_bytes": 8_000_000_000u64}))]
#[case::os(json!({"device_os_name": "macOS"}), json!({"device_os_version": "15.3"}))]
#[tokio::test]
async fn test_patch_dependent_field_validates_against_stored_companion(
    #[case] companion: serde_json::Value,
    #[case] dependent: serde_json::Value,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let resp = authed_patch(&state, &sk, &client_id, "/clients/me", &companion).await?;
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = authed_patch(&state, &sk, &client_id, "/clients/me", &dependent).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;

    [&companion, &dependent]
        .into_iter()
        .filter_map(|v| v.as_object())
        .flat_map(|obj| obj.iter())
        .for_each(|(k, v)| assert_eq!(&body[k], v, "{k} should be echoed"));
    Ok(())
}

// ─── Tags ────────────────────────────────────────────────────────────────────
//
// Tags are operator-assigned leaf markers, no record field, read-only on
// `GET /clients/me`.

#[tokio::test]
async fn test_get_me_returns_empty_tags_by_default() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let resp = authed_get(&state, &sk, &client_id, "/clients/me").await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    // Always present, empty array when untagged (no null-check needed by clients).
    assert_eq!(body["tags"], json!([]));
    Ok(())
}

#[tokio::test]
async fn test_get_me_returns_assigned_tags_sorted() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    // Assign out of order; the response is sorted (BTreeSet), read from markers.
    state
        .tag_store
        .add_client_tag(&client_id, &Tag::try_new("us-east")?)
        .await?;
    state
        .tag_store
        .add_client_tag(&client_id, &Tag::try_new("team-mobile")?)
        .await?;

    let resp = authed_get(&state, &sk, &client_id, "/clients/me").await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["tags"], json!(["team-mobile", "us-east"]));
    Ok(())
}

#[tokio::test]
async fn test_patch_me_cannot_set_tags() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    // Ignored: tags are operator-assigned.
    let resp = authed_patch(
        &state,
        &sk,
        &client_id,
        "/clients/me",
        &json!({"tags": ["sneaky"], "client_details": "d"}),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["tags"], json!([]), "client cannot set its own tags");
    assert!(
        state
            .tag_store
            .get_client_tags(&client_id)
            .await?
            .is_empty()
    );
    Ok(())
}

/// So a pending device cannot approve itself or rewrite the identity it
/// registered with; a field it owns still lands.
#[tokio::test]
async fn test_patch_me_cannot_change_status_organization_or_contact_email() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_pending(&state).await?;

    let resp = authed_patch(
        &state,
        &sk,
        &client_id,
        "/clients/me",
        &json!({
            "status": "approved",
            "organization": "evil-org",
            "contact_email": "attacker@example.test",
            "client_details": "renamed",
        }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["client_details"], "renamed", "the owned field lands");
    assert_eq!(body["status"], "pending");
    assert_eq!(body["organization"], "test-org");
    assert_eq!(body["contact_email"], "t@t.com");

    let stored = state
        .client_store
        .get_client(&client_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("client {client_id} not stored"))?;
    assert_eq!(stored.status, ClientStatus::Pending);
    assert_eq!(stored.organization.as_str(), "test-org");
    assert_eq!(stored.contact_email.as_str(), "t@t.com");

    // Still unapproved where it counts.
    let resp = authed_post(&state, &sk, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(resp).await?["error"], "client is not approved");
    Ok(())
}

#[tokio::test]
async fn test_tags_bidirectional_and_cleaned_on_delete() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (_sk_a, a) = register_and_approve(&state).await?;
    let (_sk_b, b) = register_and_approve(&state).await?;

    let team = Tag::try_new("team-mobile")?;
    let east = Tag::try_new("us-east")?;
    state.tag_store.add_client_tag(&a, &team).await?;
    state.tag_store.add_client_tag(&a, &east).await?;
    state.tag_store.add_client_tag(&b, &team).await?;

    // Reverse: tag → clients (sorted by the store).
    let mut expected = vec![a.clone(), b.clone()];
    expected.sort();
    assert_eq!(
        state.tag_store.list_client_ids_by_tag(&team).await?,
        expected
    );
    assert_eq!(
        state.tag_store.list_client_ids_by_tag(&east).await?,
        vec![a.clone()]
    );
    // Forward: client → tags.
    assert_eq!(
        state.tag_store.get_client_tags(&a).await?,
        BTreeSet::from([team.clone(), east.clone()])
    );

    // Deleting a client clears it from both directions.
    let admin = registered_user(&state, &[UserPermission::ManageAnyClients]).await?;
    let resp = signed_empty(&state, &admin, "DELETE", &format!("/api/v1/clients/{b}")).await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        state.tag_store.list_client_ids_by_tag(&team).await?,
        vec![a.clone()]
    );
    assert!(state.tag_store.get_client_tags(&b).await?.is_empty());
    Ok(())
}

/// Pending-reindex is the sharp edge: flat `{client_id}_*` keys, one per
/// profile change, filtered by client.
#[tokio::test]
async fn test_purge_client_todo_state_removes_only_target() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let todo = &*state.todo_store;

    let a = ClientId::try_new("client-a")?;
    let b = ClientId::try_new("client-b")?;
    let job1 = job("job-1")?;
    let job2 = job("job-2")?;

    todo.write_suspension(&a, chrono::Utc::now(), &job1).await?;
    todo.stamp_liveness(&a).await?;
    todo.write_eligible(&a, &job1, ExpiresAt::Never).await?;
    todo.write_eligible(&a, &job2, ExpiresAt::Never).await?;
    todo.write_pending_reindex(&a).await?;
    todo.write_pending_reindex(&a).await?; // two flags, as a real client may hold

    todo.write_suspension(&b, chrono::Utc::now(), &job1).await?;
    todo.stamp_liveness(&b).await?;
    todo.write_eligible(&b, &job1, ExpiresAt::Never).await?;
    todo.write_pending_reindex(&b).await?;

    purge_client_todo_state(todo, &a).await;

    assert!(todo.read_suspension(&a).await?.is_none());
    assert!(todo.read_liveness(&a).await?.is_none());
    assert!(
        !todo
            .list_all_eligible()
            .await?
            .iter()
            .any(|(c, _, _)| c == &a)
    );
    assert!(
        !todo
            .list_pending_reindex()
            .await?
            .iter()
            .any(|(c, _)| c == &a)
    );

    assert!(todo.read_suspension(&b).await?.is_some());
    assert!(todo.read_liveness(&b).await?.is_some());
    assert!(
        todo.list_all_eligible()
            .await?
            .iter()
            .any(|(c, _, _)| c == &b)
    );
    assert!(
        todo.list_pending_reindex()
            .await?
            .iter()
            .any(|(c, _)| c == &b)
    );

    // A clean no-op, so `clients delete` re-runs to converge after a failure.
    purge_client_todo_state(todo, &a).await;
    assert!(todo.read_suspension(&a).await?.is_none());
    Ok(())
}

#[tokio::test]
async fn test_list_clients_ignores_tag_markers() -> anyhow::Result<()> {
    // Tag markers live under tags-index/, not clients/.
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (_sk, client_id) = register_and_approve(&state).await?;

    let before = drain_clients(state.client_store.as_ref())
        .await?
        .items
        .len();
    let store = &state.tag_store;
    store
        .add_client_tag(&client_id, &Tag::try_new("team-mobile")?)
        .await?;
    store
        .add_client_tag(&client_id, &Tag::try_new("us-east")?)
        .await?;
    store
        .add_client_tag(&client_id, &Tag::try_new("batch-2026q3")?)
        .await?;
    let after = drain_clients(state.client_store.as_ref()).await?.items;
    assert_eq!(
        after.len(),
        before,
        "tag markers must not appear as clients"
    );
    assert!(after.iter().any(|c| c.client_id == client_id));
    Ok(())
}

/// A lease was granted against the old profile. The job returns to `avail/`
/// unless a submission record exists: a finished job is not resurrected.
#[rstest]
#[case::profile_change_recycles_lease(
    json!({"device_form_factor": "laptop"}),
    false,
    false,
    true
)]
#[case::profile_change_deletes_stale_lease(
    json!({"device_form_factor": "laptop"}),
    true,
    false,
    false
)]
#[case::details_only_keeps_lease(
    json!({"client_details": "updated-details"}),
    false,
    true,
    false
)]
#[tokio::test]
async fn test_patch_profile_relinquishes_leases(
    #[case] patch_body: serde_json::Value,
    #[case] record_exists: bool,
    #[case] expect_leased: bool,
    #[case] expect_avail: bool,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let job = seed_avail(
        dir.path(),
        "job-1",
        ExpiresAt::Never,
        &json!({"job_id": "job-1"}),
    )?;
    let claimed = state
        .todo_store
        .claim_job(
            &job,
            ExpiresAt::Never,
            &client_id,
            chrono::Utc::now() + chrono::Duration::hours(1),
        )
        .await?;
    assert!(matches!(claimed, ClaimResult::Claimed(_)));

    let real_record = json!({
        "job_id": "job-1",
        "client_id": client_id.as_str(),
        "message_type": "success",
    });
    if record_exists {
        state
            .submission_store
            .write_processed(&job, &real_record)
            .await?;
    }

    let resp = authed_patch(&state, &sk, &client_id, "/clients/me", &patch_body).await?;
    assert_eq!(resp.status(), StatusCode::OK);

    assert_eq!(
        !state
            .todo_store
            .list_leased_for_client(&client_id)
            .await?
            .is_empty(),
        expect_leased
    );
    assert_eq!(
        state.todo_store.get_avail_by_job(&job).await?.is_some(),
        expect_avail
    );
    // Relinquishing writes no denied/ marker: under its new profile the
    // client may legitimately claim the job fresh.
    assert!(denied_clients(&state, &job).await?.is_empty());
    if record_exists {
        let record = state
            .submission_store
            .find_job(&job)
            .await?
            .ok_or_else(|| anyhow::anyhow!("existing record vanished"))?;
        assert_eq!(record.body, real_record);
    }
    Ok(())
}

/// Whole or in part, as every restarting client does.
#[rstest]
#[case::whole(json!({
    "device_form_factor": "laptop",
    "device_ram_bytes": 36_000_000_000u64,
    "device_gpu_model": "M3 Pro GPU",
    "device_gpu_vram_bytes": 18_000_000_000u64,
}))]
#[case::subset(json!({"device_gpu_model": "M3 Pro GPU", "device_ram_bytes": 36_000_000_000u64}))]
#[tokio::test]
async fn test_patch_resending_the_stored_profile_keeps_leases_and_queues_nothing(
    #[case] resend: serde_json::Value,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let profile = json!({
        "device_form_factor": "laptop",
        "device_ram_bytes": 36_000_000_000u64,
        "device_gpu_model": "M3 Pro GPU",
        "device_gpu_vram_bytes": 18_000_000_000u64,
    });
    let resp = authed_patch(&state, &sk, &client_id, "/clients/me", &profile).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    // Clear the flags the first PATCH raised, as a reindex pass would.
    for (_, key) in state.todo_store.list_pending_reindex().await? {
        state.todo_store.delete_pending_reindex(&key).await?;
    }

    let job = seed_avail(
        dir.path(),
        "job-1",
        ExpiresAt::Never,
        &json!({"job_id": "job-1"}),
    )?;
    let claimed = state
        .todo_store
        .claim_job(
            &job,
            ExpiresAt::Never,
            &client_id,
            chrono::Utc::now() + chrono::Duration::hours(1),
        )
        .await?;
    assert!(matches!(claimed, ClaimResult::Claimed(_)));

    let resp = authed_patch(&state, &sk, &client_id, "/clients/me", &resend).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await?["reindex_pending"], false);

    assert_eq!(
        state
            .todo_store
            .list_leased_for_client(&client_id)
            .await?
            .len(),
        1,
        "the lease survives"
    );
    assert!(state.todo_store.get_avail_by_job(&job).await?.is_none());
    assert!(state.todo_store.list_pending_reindex().await?.is_empty());
    Ok(())
}
