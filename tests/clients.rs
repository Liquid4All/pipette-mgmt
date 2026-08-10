mod helpers;

use std::collections::BTreeSet;

use axum::http::StatusCode;
use rstest::rstest;
use serde_json::json;

use pipette_mgmt::stores::{ClaimResult, purge_client_todo_state};
use pipette_mgmt::types::{ClientId, ExpiresAt};
use pipette_mgmt::validated::Tag;

use helpers::{
    authed_get, authed_patch, body_json, job, make_state, register_and_approve, seed_avail,
    setup_benchmarks,
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

#[tokio::test]
async fn test_get_me_returns_unset_device_fields_as_null() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    let resp = authed_get(&state, &sk, &client_id, "/clients/me").await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;

    // The response always carries the device_* keys; unset → present as null
    // (httpapi.md §2.3.1), distinct from the stored record which omits them.
    let obj = body
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("expected object"))?;
    ["device_name", "device_form_factor", "device_gpu_vram_bytes"]
        .into_iter()
        .for_each(|key| {
            assert!(obj.contains_key(key), "missing {key}");
            assert!(body[key].is_null(), "{key} should be null");
        });
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
    // The response tells the client its queue standing was voided — the
    // client can't compute the profile diff itself (httpapi.md §2.4).
    assert_eq!(body["reindex_pending"], true);

    // A device-profile change flags the client for an eligible-index reindex —
    // two distinct flag keys, one written before the lease relinquish (the
    // gate) and one after the record persist (so a reindex racing the PATCH
    // cannot consume the request against the old record).
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

#[tokio::test]
async fn test_patch_capabilities_replaces_and_queues_reindex() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    // First PATCH sets a capability set; it feeds the matcher, so it voids the
    // client's queue standing exactly like a device-profile change.
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

    // An absent `capabilities` (a `client_details`-only PATCH) leaves the set
    // unchanged and does not queue a reindex.
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

    // Resubmitting the identical set is a no-op change: the response echoes it,
    // but no reindex is queued (the "safe to PATCH unconditionally" guarantee).
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

    // An explicit empty array clears the set — a real change, so it voids the
    // client's standing and queues a reindex.
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

    // The same validation guards register and PATCH: a canonical reserved flag
    // is rejected as reserved, and a non-canonical spelling is rejected earlier
    // as non-canonical (so it can't smuggle a reserved namespace past the check).
    for (flag, want) in [("chip:a19", "reserved namespace"), ("OS:ios", "lowercase")] {
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

    // A second PATCH adds a new field; previously-set fields are left unchanged
    // (merge, not replace).
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

/// An explicit `null` is treated as absent (httpapi.md §2.4.1): a device field
/// cannot be individually cleared, so a seeded value survives a `null` PATCH.
/// Swept over every field — the null→no-op mechanism is uniform (serde
/// `flatten` + `Option`), but the per-field sweep guards the 10 hand-written
/// merge arms in `apply_to` against a miswired field. The `seed` carries any
/// companion the dependency rules require (e.g. `device_gpu_model` for
/// `device_gpu_vram_bytes`).
#[rstest]
#[case::name(json!({"device_name": "studio-01"}), "device_name")]
#[case::form_factor(json!({"device_form_factor": "laptop"}), "device_form_factor")]
#[case::os_name(json!({"device_os_name": "macOS"}), "device_os_name")]
#[case::os_version(json!({"device_os_name": "macOS", "device_os_version": "15.3"}), "device_os_version")]
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

    // Seed the field under test and capture what was stored.
    let resp = authed_patch(&state, &sk, &client_id, "/clients/me", &seed).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let expected = body_json(resp).await?[field].clone();
    assert!(!expected.is_null(), "seed failed to set {field}");

    // PATCH the field to `null`; the stored value must survive unchanged.
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
    // Nothing was voided, and the response says so.
    assert_eq!(body_json(resp).await?["reindex_pending"], false);

    // The device profile didn't change, so no (full avail/-scan) reindex is
    // queued — `match` rules read only the device profile.
    let flags = state.todo_store.list_pending_reindex().await?;
    assert!(flags.is_empty());
    Ok(())
}

/// A device-profile validation failure on PATCH is a `400` whose body names the
/// offending field (httpapi.md §2.4.3). One case per rule: bad form factor, and
/// each `*_vram`/`os_version` field present without its required companion.
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

/// A dependent field (`*_vram` / `os_version`) is validated against the *merged*
/// profile, so a PATCH that sets only the dependent succeeds once its companion
/// is already stored. Seed the companion, then PATCH the dependent and confirm
/// both survive in the response.
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

    // Store the companion (model / OS name) first.
    let resp = authed_patch(&state, &sk, &client_id, "/clients/me", &companion).await?;
    assert_eq!(resp.status(), StatusCode::OK);

    // The dependent PATCH is now valid against the merged profile.
    let resp = authed_patch(&state, &sk, &client_id, "/clients/me", &dependent).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;

    // Both the seeded companion and the dependent value are echoed back.
    [&companion, &dependent]
        .into_iter()
        .filter_map(|v| v.as_object())
        .flat_map(|obj| obj.iter())
        .for_each(|(k, v)| assert_eq!(&body[k], v, "{k} should be echoed"));
    Ok(())
}

// ─── Tags ────────────────────────────────────────────────────────────────────
//
// Tags are mgmt-assigned via the `AuthStore` (leaf markers, no record field) and
// surfaced read-only on `GET /clients/me`. These exercise the HTTP surface plus
// the store's bidirectional API through a real config-built (local_fs) store.

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
        .auth_store
        .add_client_tag(&client_id, &Tag::try_new("us-east")?)
        .await?;
    state
        .auth_store
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

    // A client putting `tags` in a PATCH body is ignored — tags are mgmt-only.
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
    // Nothing landed in the store either.
    assert!(
        state
            .auth_store
            .get_client_tags(&client_id)
            .await?
            .is_empty()
    );
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
    state.auth_store.add_client_tag(&a, &team).await?;
    state.auth_store.add_client_tag(&a, &east).await?;
    state.auth_store.add_client_tag(&b, &team).await?;

    // Reverse: tag → clients (sorted by the store).
    let mut expected = vec![a.clone(), b.clone()];
    expected.sort();
    assert_eq!(
        state.auth_store.list_client_ids_by_tag(&team).await?,
        expected
    );
    assert_eq!(
        state.auth_store.list_client_ids_by_tag(&east).await?,
        vec![a.clone()]
    );
    // Forward: client → tags.
    assert_eq!(
        state.auth_store.get_client_tags(&a).await?,
        BTreeSet::from([team.clone(), east.clone()])
    );

    // Deleting a client clears it from both directions.
    state.auth_store.delete_client(&b).await?;
    assert_eq!(
        state.auth_store.list_client_ids_by_tag(&team).await?,
        vec![a.clone()]
    );
    assert!(state.auth_store.get_client_tags(&b).await?.is_empty());
    Ok(())
}

/// `clients delete` tears down the deleted client's `todo/` queue state
/// (suspension, eligible markers, pending-reindex flags) via
/// `purge_client_todo_state`, touching only the target — a co-resident client's
/// state must survive. Pending-reindex is the sharp edge: flat `{client_id}_*`
/// keys, one per profile change, filtered by client.
#[tokio::test]
async fn test_purge_client_todo_state_removes_only_target() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let todo = &*state.todo_store;

    let a = ClientId::try_new("client-a")?;
    let b = ClientId::try_new("client-b")?;
    let job1 = job("job-1");
    let job2 = job("job-2");

    // Full set of queue state for the client being deleted, plus a control set
    // for a co-resident client that must be left intact.
    todo.write_suspension(&a, chrono::Utc::now(), &job1).await?;
    todo.write_eligible(&a, &job1, ExpiresAt::Never).await?;
    todo.write_eligible(&a, &job2, ExpiresAt::Never).await?;
    todo.write_pending_reindex(&a).await?;
    todo.write_pending_reindex(&a).await?; // two flags, as a real client may hold

    todo.write_suspension(&b, chrono::Utc::now(), &job1).await?;
    todo.write_eligible(&b, &job1, ExpiresAt::Never).await?;
    todo.write_pending_reindex(&b).await?;

    purge_client_todo_state(todo, &a).await;

    // Every scrap of A's state is gone.
    assert!(todo.read_suspension(&a).await?.is_none());
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

    // B's state is untouched.
    assert!(todo.read_suspension(&b).await?.is_some());
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

    // Re-running against an already-purged client is a clean no-op — this is
    // what lets `clients delete` be re-run to converge after a partial failure.
    purge_client_todo_state(todo, &a).await;
    assert!(todo.read_suspension(&a).await?.is_none());
    Ok(())
}

#[tokio::test]
async fn test_list_clients_ignores_tag_markers() -> anyhow::Result<()> {
    // Regression guard for the optimized tree: tag markers live under
    // tags-index/, not clients/, so tagging a client must not perturb
    // list_clients.
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (_sk, client_id) = register_and_approve(&state).await?;

    let before = state.auth_store.list_clients().await?.len();
    let store = &state.auth_store;
    store
        .add_client_tag(&client_id, &Tag::try_new("team-mobile")?)
        .await?;
    store
        .add_client_tag(&client_id, &Tag::try_new("us-east")?)
        .await?;
    store
        .add_client_tag(&client_id, &Tag::try_new("batch-2026q3")?)
        .await?;
    let after = state.auth_store.list_clients().await?;
    assert_eq!(
        after.len(),
        before,
        "tag markers must not appear as clients"
    );
    assert!(after.iter().any(|c| c.client_id == client_id));
    Ok(())
}

/// A device-profile PATCH relinquishes every lease the client holds — a lease
/// is granted against the profile at claim time, and a client must not
/// continue a job it may no longer be eligible for. The job returns to
/// `avail/` (no `denied/` marker), unless it already has a submission record —
/// then the stale lease is deleted and the finished job is not resurrected. A
/// `client_details`-only PATCH changes no match attributes and leaves the
/// lease alone.
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
    assert!(state.todo_store.list_denied_for_job(&job).await?.is_empty());
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
