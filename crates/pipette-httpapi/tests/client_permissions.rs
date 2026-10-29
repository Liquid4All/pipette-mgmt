// A client's permission set over the HTTP surface: what a read renders, and
// what each gated route requires of it.

use axum::http::StatusCode;
use rstest::rstest;
use serde_json::{Value, json};

use pipette_core::client_permission::ClientPermissionSet;
use pipette_httpapi::state::AppState;
use pipette_plan_types::{ClientId, ClientPermission, ExpiresAt, JobId, UserPermission};
use pipette_store_testkit::TEST_LIST_LIMIT;
use pipette_web_testkit::{
    authed_get, authed_patch, authed_post, body_json, job_spec, make_state, register_and_approve,
    register_approve_and_grant, setup_benchmarks,
};

/// An absent object (a record from before the permission split) grants nothing;
/// no default fills it in. A stored user right grants nothing but still
/// renders: a name an operator cannot see is one they cannot remove.
#[rstest]
#[case::absent_grants_nothing(None, &[])]
#[case::cleared(Some(&[][..]), &[])]
#[case::granted(Some(&["perform_work"][..]), &["perform_work"])]
#[case::user_only_is_rendered(Some(&["submit_plans"][..]), &["submit_plans"])]
#[case::user_name_beside_a_client_right(
    Some(&["submit_any_plans", "perform_work"][..]),
    &["perform_work", "submit_any_plans"]
)]
// A newer release's right: dropping it would tell an operator on the older
// replica that the grant is gone.
#[case::newer_release_name_survives(
    Some(&["rescore_plans", "perform_work"][..]),
    &["perform_work", "rescore_plans"]
)]
#[tokio::test]
async fn test_get_me_renders_the_resolved_permission_set(
    #[case] stored: Option<&[&str]>,
    #[case] expected: &[&str],
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    match stored {
        Some(names) => {
            let set: ClientPermissionSet = serde_json::from_value(json!(names))?;
            state
                .client_store
                .put_client_permissions(&client_id, &set)
                .await?
        }
        // Registration writes an object, so a record with none is reached by
        // removing it.
        None => {
            state
                .client_store
                .delete_client_permissions(&client_id)
                .await?
        }
    }

    let resp = authed_get(&state, &sk, &client_id, "/clients/me").await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["permissions"], json!(expected));
    Ok(())
}

/// Nothing the body carried lands. The route flattens a device profile, so the
/// field is read for presence, not by serde: `null` tells the two apart.
#[rstest]
#[case::names(json!(["perform_work"]))]
#[case::empty(json!([]))]
#[case::null(serde_json::Value::Null)]
#[tokio::test]
async fn test_patch_me_cannot_set_permissions(
    #[case] permissions: serde_json::Value,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    // Granted nothing, so "the client did not gain `perform_work`" below is a
    // statement about the refusal rather than about what it already held.
    let (sk, client_id) = register_approve_and_grant(&state, &[]).await?;

    let resp = authed_patch(
        &state,
        &sk,
        &client_id,
        "/clients/me",
        &json!({"permissions": permissions, "client_details": "sneaky"}),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await?;
    assert_eq!(body["error"], "permissions are assigned by an operator");

    let stored = state
        .client_store
        .get_client_permissions(&client_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("registration writes a permissions object"))?;
    assert!(!stored.holds(ClientPermission::PerformWork));
    let client = state
        .client_store
        .get_client(&client_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("client missing"))?;
    assert_ne!(client.client_details.as_str(), "sneaky");
    Ok(())
}

/// A `PATCH` that changes something still reports the set, so a client reads it
/// from either route.
#[tokio::test]
async fn test_patch_me_reports_the_permission_set() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;

    state
        .client_store
        .put_client_permissions(
            &client_id,
            &[ClientPermission::PerformWork].into_iter().collect(),
        )
        .await?;

    let resp = authed_patch(
        &state,
        &sk,
        &client_id,
        "/clients/me",
        &json!({"client_details": "updated"}),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["permissions"], json!(["perform_work"]));
    Ok(())
}

/// A delete of the identity takes its permission set with it, so a client id
/// re-derived from the same public key does not inherit a grant.
#[tokio::test]
async fn test_permissions_cleaned_on_delete() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (_sk, client_id) = register_and_approve(&state).await?;

    state
        .client_store
        .put_client_permissions(
            &client_id,
            &[ClientPermission::PerformWork].into_iter().collect(),
        )
        .await?;
    state.client_store.delete_client(&client_id).await?;

    assert!(
        state
            .client_store
            .get_client_permissions(&client_id)
            .await?
            .is_none()
    );
    Ok(())
}

// ── enforcement ───────────────────────────────────────────────────────────
//
// Each route asserts the pair: a holder reaches the route's own answer, and a
// non-holder gets the `403`. A refusal alone passes on a route refusing all.

/// Distinct from `client is not approved`, so a client's error handler tells a
/// missing grant from an unvetted identity.
fn lacks(permission: ClientPermission) -> String {
    format!("client lacks the {permission} permission")
}

/// Every live lease, sorted — the value a test compares across a request to
/// see whether one was renamed.
async fn leased_keys(state: &AppState) -> anyhow::Result<Vec<String>> {
    let mut keys = state.todo_store.list_leased().await?;
    keys.sort();
    Ok(keys)
}

/// Seed a job into `avail/` and lease it to `client_id`, so a submission
/// echoing `job_id` has a claim to bind to.
async fn seed_claim(
    state: &AppState,
    dir: &std::path::Path,
    client_id: &ClientId,
    job_id: &str,
) -> anyhow::Result<JobId> {
    let job = pipette_web_testkit::seed_avail(
        dir,
        job_id,
        ExpiresAt::Never,
        &json!({"spec": job_spec()}),
    )?;
    state
        .todo_store
        .claim_job(
            &job,
            ExpiresAt::Never,
            client_id,
            chrono::Utc::now() + chrono::Duration::hours(1),
        )
        .await?;
    Ok(job)
}

/// A holder gets the route's own answer: `204` for a claim with nothing
/// eligible, `404` for a lease it does not hold.
#[rstest]
#[case::claim("POST", "/plans/claim", StatusCode::NO_CONTENT)]
#[case::heartbeat("PUT", "/plans/job-1/heartbeat", StatusCode::NOT_FOUND)]
#[case::reclaim("POST", "/plans/job-1/reclaim", StatusCode::NOT_FOUND)]
// A corpus is work input too; a benchmark with none is the holder's own answer.
#[case::dataset("GET", "/datasets/prefill_throughput_256", StatusCode::NOT_FOUND)]
#[tokio::test]
async fn test_the_queue_protocol_routes_require_perform_work(
    #[case] method: &str,
    #[case] path: &str,
    #[case] granted_status: StatusCode,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let (granted_key, granted) =
        register_approve_and_grant(&state, &[ClientPermission::PerformWork]).await?;
    let (refused_key, refused) = register_approve_and_grant(&state, &[]).await?;

    let resp = pipette_web_testkit::request_with_headers(
        &state,
        method,
        path,
        &pipette_web_testkit::auth_headers(&granted_key, &granted, method, path)?,
    )
    .await?;
    assert_eq!(resp.status(), granted_status, "{path} refused a holder");

    let resp = pipette_web_testkit::request_with_headers(
        &state,
        method,
        path,
        &pipette_web_testkit::auth_headers(&refused_key, &refused, method, path)?,
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        body_json(resp).await?["error"],
        json!(lacks(ClientPermission::PerformWork))
    );
    Ok(())
}

/// So an operator reading the log sees the cause actually blocking the client.
#[tokio::test]
async fn test_a_pending_client_hears_about_its_status_not_its_permissions() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (key, client_id) = pipette_web_testkit::register_pending(&state).await?;
    pipette_web_testkit::grant(&state, &client_id, &[ClientPermission::PerformWork]).await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(resp).await?["error"], "client is not approved");
    Ok(())
}

/// No `job_id` is an ad-hoc result needing `submit_results`; an echoed claim
/// needs `perform_work`. An approved client is refused, not held, with the
/// unverified archive on too.
#[rstest]
#[case::ad_hoc(false, ClientPermission::SubmitResults)]
#[case::claimed_job(true, ClientPermission::PerformWork)]
#[tokio::test]
async fn test_a_submission_requires_the_permission_its_body_names(
    #[case] echoes_a_claim: bool,
    #[case] required: ClientPermission,
    #[values(false, true)] archive: bool,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = pipette_web_testkit::make_state_with_unverified(dir.path(), archive).await?;

    // The other name of the vocabulary, so the refusal is about *which*
    // permission the body needs rather than about holding none at all.
    let other = match required {
        ClientPermission::PerformWork => ClientPermission::SubmitResults,
        ClientPermission::SubmitResults => ClientPermission::PerformWork,
    };

    let body = async |state: &AppState, client_id: &ClientId, job: &str| -> anyhow::Result<Value> {
        let mut body = pipette_web_testkit::valid_submission("prefill_throughput_256");
        if echoes_a_claim {
            body["job_id"] = json!(
                seed_claim(state, dir.path(), client_id, job)
                    .await?
                    .as_str()
            );
        }
        Ok(body)
    };

    let (refused_key, refused) = register_approve_and_grant(&state, &[other]).await?;
    let refused_body = body(&state, &refused, "job-refused").await?;
    // By key, not by count: a lease carries its expiry in its key, so the
    // renewal that binds a `job_id` renames the entry rather than adding one —
    // a count would not move.
    let leases_before = leased_keys(&state).await?;
    let resp = authed_post(&state, &refused_key, &refused, "/benchmarks", &refused_body).await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(resp).await?["error"], json!(lacks(required)));
    // The check runs ahead of the lease renewal that binds a claimed `job_id`.
    assert_eq!(
        leased_keys(&state).await?,
        leases_before,
        "a refusal must not renew the claim it echoes"
    );
    assert!(
        state
            .submission_store
            .list_incoming(TEST_LIST_LIMIT)
            .await?
            .is_empty(),
        "a refusal must write no result"
    );
    assert!(
        state
            .submission_store
            .list_unverified_client(&refused)
            .await?
            .is_empty(),
        "a refused approved client is not held"
    );

    let (granted_key, granted) = register_approve_and_grant(&state, &[required]).await?;
    let resp = authed_post(
        &state,
        &granted_key,
        &granted,
        "/benchmarks",
        &body(&state, &granted, "job-granted").await?,
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    Ok(())
}

/// A refusal is that item's error, not the request's status.
#[rstest]
#[tokio::test]
async fn test_a_batch_refuses_per_item(#[values(false, true)] archive: bool) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = pipette_web_testkit::make_state_with_unverified(dir.path(), archive).await?;
    let (key, client_id) =
        register_approve_and_grant(&state, &[ClientPermission::SubmitResults]).await?;

    let job = seed_claim(&state, dir.path(), &client_id, "job-1").await?;
    let leases_before = leased_keys(&state).await?;
    let mut claimed = pipette_web_testkit::valid_submission("prefill_throughput_256");
    claimed["job_id"] = json!(job.as_str());

    let resp = authed_post(
        &state,
        &key,
        &client_id,
        "/benchmarks/batch",
        &json!({"submissions": [pipette_web_testkit::valid_submission("prefill_throughput_256"), claimed]}),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);

    let body = body_json(resp).await?;
    assert!(
        body["results"][0]["job_id"].is_string(),
        "the ad-hoc item is within the caller's grant: {body}"
    );
    assert_eq!(
        body["results"][1]["error"],
        json!(lacks(ClientPermission::PerformWork))
    );
    assert_eq!(
        leased_keys(&state).await?,
        leases_before,
        "a refused batch item must not renew the claim it echoes"
    );
    assert_eq!(
        state
            .submission_store
            .list_incoming(TEST_LIST_LIMIT)
            .await?
            .len(),
        1,
        "only the ad-hoc item is written"
    );
    assert!(
        state
            .submission_store
            .list_unverified_client(&client_id)
            .await?
            .is_empty(),
        "an approved client is not held"
    );
    Ok(())
}

/// The hold is for an identity nobody has vetted, and a grant follows vetting,
/// so requiring one would empty the archive of its purpose.
#[tokio::test]
async fn test_a_held_submission_needs_no_grant() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = pipette_web_testkit::make_state_with_unverified(dir.path(), true).await?;
    let (key, client_id) = pipette_web_testkit::register_pending(&state).await?;
    pipette_web_testkit::grant(&state, &client_id, &[]).await?;

    let resp = authed_post(
        &state,
        &key,
        &client_id,
        "/benchmarks",
        &pipette_web_testkit::valid_submission("prefill_throughput_256"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    Ok(())
}

/// Filing a plan is a user right no client can hold.
#[tokio::test]
async fn test_no_client_permission_lets_a_device_file_a_plan() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (key, client_id) = register_approve_and_grant(
        &state,
        &[
            ClientPermission::SubmitResults,
            ClientPermission::PerformWork,
        ],
    )
    .await?;
    let owner =
        pipette_web_testkit::registered_user(&state, &[UserPermission::SubmitPlans]).await?;
    let uri = pipette_web_testkit::own_campaign_plans_uri(&state, &owner.1).await?;

    let headers = pipette_web_testkit::auth_headers(&key, &client_id, "POST", &uri)?;
    let resp = pipette_web_testkit::raw_request(
        &state,
        "POST",
        &uri,
        &headers,
        Some("application/toml"),
        pipette_web_testkit::ONE_CELL_PLAN.to_string(),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    pipette_web_testkit::assert_nothing_staged(&state).await?;
    Ok(())
}
