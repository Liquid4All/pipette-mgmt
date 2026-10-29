//! The request span (`pipette_httpapi::router::app`) puts a request's ids on
//! every log line. The events read here name none of their own, so a case fails
//! if the span stops declaring a field: `Span::current().record` on an
//! undeclared one is a silent no-op. `get_benchmark` has no case (it writes at
//! `INFO` only on an induced `500`), nor `get_eval_sample_results` (only when
//! refusing another client's scored eval).
//!
//! The file also holds other log-capture tests: nothing here drives the events
//! they read outside a capture, which `tracing`'s global callsite cache
//! requires.

#![expect(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    reason = "tests build the requests a client sends, so they keep `json!` (see clippy.toml)"
)]

use axum::http::StatusCode;
use rstest::rstest;
use serde_json::json;

use pipette_core::user_permission::UserPermissionGroup;
use pipette_plan_types::UserPermission;
use pipette_web_testkit::{
    VAULT_GGUF, auth_headers, authed_get, authed_get_with_headers, authed_post, authed_put_empty,
    body_json, capture_logs_at_level, created_campaign, grant_user, json_str, legacy_auth_headers,
    make_state, make_state_with_legacy_signatures, register_and_approve,
    register_approve_and_grant, registered_user, seed_leased, seed_org, setup_benchmarks,
    sha256_hex, signed_empty, signed_json, submit_benchmark, upload_gguf_text, valid_submission,
};

/// A server-minted job id (`job-{uuid}`) the test plants a corrupt record for.
const CORRUPT_JOB_ID: &str = "job-550e8400-e29b-41d4-a716-446655440000";

const BENCHMARK_ID: &str = "prefill_throughput_256";

/// A versioned id, whose `.` the path extractor must still record.
const VERSIONED_BENCHMARK_ID: &str = "eval_ifbench_2026.06.1";

/// The job a [`Route::Reclaim`] or [`Route::HeartbeatRefused`] case leases.
const LEASED_JOB_ID: &str = "job-1";

/// A well-formed plan id no plan carries, for [`Route::PlanRefused`]: the
/// refusal comes before the store is asked about the plan.
const UNFILED_PLAN_ID: &str = "plan-0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e";

#[derive(Clone, Copy, Debug)]
enum Route {
    Register,
    Submit,
    Batch,
    FailedGet,
    /// A device refused a vault artifact it does not own. The refusal is the
    /// vault's audit line, so it names both the device and the resource.
    VaultRefused,
    /// A device renews the lease it still holds.
    Reclaim,
    /// A device heartbeats a job another device holds.
    HeartbeatRefused,
    /// An account granted nothing asks for a plan. The refusal names only the
    /// permission.
    PlanRefused,
    /// An account holding `manage_orgs` but not `read_plans` follows its own
    /// campaign's pointer: the plan is refused after the pointer resolved.
    CampaignPointerRefused,
    /// An account renames its own campaign.
    CampaignRenamed,
    /// An unaffiliated account asks for an organization's benchmark. The
    /// refusal names the organization; only the path names the benchmark.
    OrgBenchmarkOutOfScope,
}

/// The record that exists but does not parse, for the `500` of
/// [`Route::FailedGet`].
fn plant_corrupt_record(dir: &std::path::Path) -> anyhow::Result<()> {
    let incoming = dir.join("submissions/incoming");
    std::fs::create_dir_all(&incoming)?;
    std::fs::write(
        incoming.join(format!("{CORRUPT_JOB_ID}.json")),
        b"{ not json",
    )?;
    Ok(())
}

/// The ceiling is `INFO`, the binary's default level, which holds the code to
/// what production records: a span opened below it records nothing there.
#[rstest]
#[case::register(Route::Register)]
#[case::submit(Route::Submit)]
#[case::batch(Route::Batch)]
#[case::failed_get(Route::FailedGet)]
#[case::vault_refused(Route::VaultRefused)]
#[case::reclaim(Route::Reclaim)]
#[case::heartbeat_refused(Route::HeartbeatRefused)]
#[case::plan_refused(Route::PlanRefused)]
#[case::campaign_pointer_refused(Route::CampaignPointerRefused)]
#[case::campaign_renamed(Route::CampaignRenamed)]
#[case::org_benchmark_out_of_scope(Route::OrgBenchmarkOutOfScope)]
#[test]
fn a_request_names_its_identifiers_as_span_fields(#[case] route: Route) -> anyhow::Result<()> {
    type Probe = (&'static str, Vec<(&'static str, String)>);

    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    if matches!(route, Route::FailedGet) {
        plant_corrupt_record(dir.path())?;
    }

    // The whole exchange stays on the capture thread: the subscriber is
    // thread-local, so a request driven off it writes nowhere this test reads.
    let (outcome, logs) =
        capture_logs_at_level(tracing::Level::INFO, |rt| -> anyhow::Result<Probe> {
            rt.block_on(async {
                let state = make_state(dir.path()).await?;
                let (sk, client_id) = register_and_approve(&state).await?;
                let client = ("client_id", client_id.to_string());

                match route {
                    Route::Register => Ok(("client registered", vec![client])),

                    Route::Submit => {
                        let job_id = submit_benchmark(
                            &state,
                            &sk,
                            &client_id,
                            &valid_submission(BENCHMARK_ID),
                        )
                        .await?;
                        Ok((
                            "accepted submission",
                            vec![
                                client,
                                ("job_id", job_id),
                                ("benchmark_id", BENCHMARK_ID.into()),
                            ],
                        ))
                    }

                    Route::Batch => {
                        let resp = authed_post(
                            &state,
                            &sk,
                            &client_id,
                            "/benchmarks/batch",
                            &json!({ "submissions": [valid_submission(BENCHMARK_ID)] }),
                        )
                        .await?;
                        anyhow::ensure!(
                            resp.status() == StatusCode::OK,
                            "batch was rejected whole"
                        );
                        let body = body_json(resp).await?;
                        let job_id = body["results"][0]["job_id"]
                            .as_str()
                            .ok_or_else(|| anyhow::anyhow!("batch item 0 was rejected: {body}"))?
                            .to_string();
                        Ok((
                            "accepted batch submission",
                            vec![client, ("job_id", job_id), ("batch_index", "0".into())],
                        ))
                    }

                    Route::FailedGet => {
                        let resp =
                            authed_get(&state, &sk, &client_id, &format!("/jobs/{CORRUPT_JOB_ID}"))
                                .await?;
                        anyhow::ensure!(
                            resp.status() == StatusCode::INTERNAL_SERVER_ERROR,
                            "the corrupt record did not produce a 500"
                        );
                        Ok((
                            "request failed",
                            vec![client, ("job_id", CORRUPT_JOB_ID.into())],
                        ))
                    }

                    Route::VaultRefused => {
                        // An operator's own artifact, and the device above,
                        // which nobody owns, asking for it.
                        let owner = registered_user(&state, &[UserPermission::SubmitPlans]).await?;
                        let resource_id =
                            upload_gguf_text(&state, &owner, None, VAULT_GGUF).await?;

                        let resp = authed_get(
                            &state,
                            &sk,
                            &client_id,
                            &format!("/vault/resources/{resource_id}/content"),
                        )
                        .await?;
                        anyhow::ensure!(
                            resp.status() == StatusCode::FORBIDDEN,
                            "an unowned device must be refused"
                        );
                        Ok((
                            "refused: the vault resource belongs to another principal",
                            vec![client, ("resource_id", resource_id)],
                        ))
                    }

                    Route::Reclaim => {
                        seed_leased(dir.path(), LEASED_JOB_ID, &client_id, &json!({}))?;
                        let resp = authed_post(
                            &state,
                            &sk,
                            &client_id,
                            &format!("/plans/{LEASED_JOB_ID}/reclaim"),
                            &json!({}),
                        )
                        .await?;
                        anyhow::ensure!(
                            resp.status() == StatusCode::OK,
                            "the held lease was not renewed"
                        );
                        Ok((
                            "lease renewed on reclaim",
                            vec![client, ("job_id", LEASED_JOB_ID.into())],
                        ))
                    }

                    Route::HeartbeatRefused => {
                        let (_, holder) = register_and_approve(&state).await?;
                        seed_leased(dir.path(), LEASED_JOB_ID, &holder, &json!({}))?;
                        let resp = authed_put_empty(
                            &state,
                            &sk,
                            &client_id,
                            &format!("/plans/{LEASED_JOB_ID}/heartbeat"),
                        )
                        .await?;
                        anyhow::ensure!(
                            resp.status() == StatusCode::CONFLICT,
                            "a heartbeat on another device's lease must be refused"
                        );
                        Ok((
                            "refused heartbeat: the job is leased to another client",
                            vec![client, ("job_id", LEASED_JOB_ID.into())],
                        ))
                    }

                    Route::PlanRefused => {
                        let signer = registered_user(&state, &[]).await?;
                        let resp = signed_empty(
                            &state,
                            &signer,
                            "GET",
                            &format!("/api/v1/plans/{UNFILED_PLAN_ID}"),
                        )
                        .await?;
                        anyhow::ensure!(
                            resp.status() == StatusCode::FORBIDDEN
                                && body_json(resp).await?
                                    == json!({"error": "user lacks the read_plans permission"}),
                            "an account without read_plans must be refused for it"
                        );
                        Ok((
                            "refused: user lacks permission",
                            vec![
                                ("client_id", signer.1.to_string()),
                                ("plan_id", UNFILED_PLAN_ID.into()),
                            ],
                        ))
                    }

                    Route::CampaignPointerRefused => {
                        // `submit_plans` implies `read_plans`, so the author
                        // keeps only `manage_orgs` once the campaign is filed:
                        // it reaches the campaign and not its plan.
                        let signer =
                            registered_user(&state, &[UserPermission::SubmitPlans]).await?;
                        let (campaign_id, plan_id) =
                            created_campaign(&state, &signer, &[("name", "Nightly")]).await?;
                        grant_user(&state, &signer.1, &[UserPermission::ManageOrgs]).await?;
                        let resp = signed_empty(
                            &state,
                            &signer,
                            "GET",
                            &format!("/api/v1/campaigns/{campaign_id}/plan"),
                        )
                        .await?;
                        // The plan gate, not the campaign reach, which
                        // refuses with its own message.
                        anyhow::ensure!(
                            resp.status() == StatusCode::FORBIDDEN
                                && body_json(resp).await?
                                    == json!({"error": "user lacks the read_plans permission"}),
                            "an account without read_plans must be refused the plan"
                        );
                        Ok((
                            "refused: user lacks permission",
                            vec![
                                ("client_id", signer.1.to_string()),
                                ("campaign_id", campaign_id),
                                ("plan_id", plan_id.to_string()),
                            ],
                        ))
                    }

                    Route::CampaignRenamed => {
                        let signer =
                            registered_user(&state, &[UserPermission::SubmitPlans]).await?;
                        let (campaign_id, _) =
                            created_campaign(&state, &signer, &[("name", "Nightly")]).await?;
                        let resp = signed_json(
                            &state,
                            &signer,
                            "PATCH",
                            &format!("/api/v1/campaigns/{campaign_id}"),
                            &json!({ "name": "Renamed" }),
                        )
                        .await?;
                        anyhow::ensure!(resp.status() == StatusCode::OK, "the rename was refused");
                        let body = body_json(resp).await?;
                        anyhow::ensure!(
                            body["changed"] == json!(true) && body["name"] == json!("Renamed"),
                            "the rename was not applied: {body}"
                        );
                        Ok((
                            "campaign renamed",
                            vec![
                                ("client_id", signer.1.to_string()),
                                ("campaign_id", campaign_id),
                            ],
                        ))
                    }

                    Route::OrgBenchmarkOutOfScope => {
                        seed_org(&state, "acme").await?;
                        let signer = registered_user(&state, &[]).await?;
                        let resp = signed_empty(
                            &state,
                            &signer,
                            "GET",
                            &format!("/api/v1/orgs/acme/benchmarks/{VERSIONED_BENCHMARK_ID}"),
                        )
                        .await?;
                        anyhow::ensure!(
                            resp.status() == StatusCode::NOT_FOUND,
                            "an unaffiliated account must find the organization absent"
                        );
                        Ok((
                            "refused: the organization is outside the caller's scope",
                            vec![
                                ("client_id", signer.1.to_string()),
                                ("benchmark_id", VERSIONED_BENCHMARK_ID.into()),
                            ],
                        ))
                    }
                }
            })
        })?;

    let (message, expected) = outcome?;
    let line = logs
        .lines()
        .find(|l| l.contains(message))
        .ok_or_else(|| anyhow::anyhow!("no {message:?} event in the log:\n{logs}"))?;
    for (field, value) in expected {
        assert!(
            line.contains(&format!("{field}={value}")),
            "{message:?} did not name {field}:\n{line}"
        );
    }
    Ok(())
}

/// The `5xx` body still says nothing, whatever the log carries. The span moved
/// the identifiers out of the payload; it must not move them onto the wire.
#[tokio::test]
async fn the_response_body_stays_redacted() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    plant_corrupt_record(dir.path())?;

    let state = make_state(dir.path()).await?;
    let (sk, client_id) = register_and_approve(&state).await?;
    let resp = authed_get(&state, &sk, &client_id, &format!("/jobs/{CORRUPT_JOB_ID}")).await?;
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        body_json(resp).await?,
        json!({"error": "internal server error"})
    );
    Ok(())
}

/// Fail unless the first log line naming `message` carries every one of `fields`.
fn assert_logged(logs: &str, message: &str, fields: &[String]) -> anyhow::Result<()> {
    let line = logs
        .lines()
        .find(|line| line.contains(message))
        .ok_or_else(|| anyhow::anyhow!("no {message:?} line in the log:\n{logs}"))?;
    let missing: Vec<&String> = fields
        .iter()
        .filter(|field| !line.contains(field.as_str()))
        .collect();
    anyhow::ensure!(missing.is_empty(), "{message:?} lacks {missing:?}: {line}");
    Ok(())
}

/// A permission refusal names the permission it required, so the clients a
/// grant is missing on are a log query rather than a guess.
#[test]
fn a_permission_refusal_names_the_permission_it_required() -> anyhow::Result<()> {
    let (result, logs) = capture_logs_at_level(tracing::Level::INFO, |rt| {
        rt.block_on(async {
            let dir = tempfile::tempdir()?;
            setup_benchmarks(dir.path())?;
            let state = make_state(dir.path()).await?;
            // Approved and granted nothing, so the refusal is the permission
            // rather than the status.
            let (key, client_id) = register_approve_and_grant(&state, &[]).await?;
            let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
            anyhow::ensure!(
                resp.status() == StatusCode::FORBIDDEN
                    && body_json(resp).await?
                        == json!({"error": "client lacks the perform_work permission"}),
                "the claim was not refused for the permission"
            );
            anyhow::Ok(client_id)
        })
    })?;
    let client_id = result?;

    assert_logged(
        &logs,
        "client lacks permission",
        &[
            "required_permission=perform_work".to_string(),
            format!("client_id={client_id}"),
            "method=POST".to_string(),
            "path=/plans/claim".to_string(),
        ],
    )
}

/// A gate refuses before the handler parses the path, so it records the path's
/// ids itself, leaving off a value that is not an id's characters: it could
/// forge a field.
#[rstest]
#[case::a_device(
    "/api/v1/clients/ev1_0123456789abcdef",
    "target_client_id=ev1_0123456789abcdef",
    true
)]
#[case::not_an_id("/api/v1/plans/x%20forged%3D1", "plan_id=", false)]
#[test]
fn a_gate_refusal_names_only_a_well_formed_path_id(
    #[case] uri: &str,
    #[case] field: &str,
    #[case] logged: bool,
) -> anyhow::Result<()> {
    let (result, logs) = capture_logs_at_level(tracing::Level::INFO, |rt| {
        rt.block_on(async {
            let dir = tempfile::tempdir()?;
            setup_benchmarks(dir.path())?;
            let state = make_state(dir.path()).await?;
            let signer = registered_user(&state, &[]).await?;
            let resp = signed_empty(&state, &signer, "GET", uri).await?;
            anyhow::ensure!(
                resp.status() == StatusCode::FORBIDDEN,
                "an account granted nothing must be refused by the gate"
            );
            anyhow::Ok(())
        })
    })?;
    result?;

    let line = logs
        .lines()
        .find(|line| line.contains("user lacks permission"))
        .ok_or_else(|| anyhow::anyhow!("no refusal line in the log:\n{logs}"))?;
    anyhow::ensure!(line.contains(field) == logged, "{field:?} in: {line}");
    Ok(())
}

/// Each timestamp-only and replay outcome writes a `WARN` naming the client and
/// the request. The acceptance line is how an operator knows
/// `accept_legacy_signatures` can be cleared.
#[test]
fn a_signature_outcome_names_the_client_and_the_request() -> anyhow::Result<()> {
    const ME: &str = "/clients/me";

    let (result, logs) = capture_logs_at_level(tracing::Level::INFO, |rt| {
        rt.block_on(async {
            let dir = tempfile::tempdir()?;
            setup_benchmarks(dir.path())?;
            let state = make_state_with_legacy_signatures(dir.path(), true).await?;
            let (key, client_id) = register_and_approve(&state).await?;

            let legacy = legacy_auth_headers(&key, &client_id);
            let resp = authed_get_with_headers(&state, ME, &legacy).await?;
            anyhow::ensure!(
                resp.status() == StatusCode::OK,
                "a client yet to migrate may sign timestamp-only"
            );

            // The first `v1` request migrates the client, and the same headers
            // sent again are a replay.
            let v1 = auth_headers(&key, &client_id, "GET", ME)?;
            let resp = authed_get_with_headers(&state, ME, &v1).await?;
            anyhow::ensure!(resp.status() == StatusCode::OK, "the v1 request failed");
            let resp = authed_get_with_headers(&state, ME, &v1).await?;
            anyhow::ensure!(
                resp.status() == StatusCode::UNAUTHORIZED
                    && body_json(resp).await? == json!({"error": "signature already used"}),
                "the replay was not refused"
            );

            let legacy = legacy_auth_headers(&key, &client_id);
            let resp = authed_get_with_headers(&state, ME, &legacy).await?;
            anyhow::ensure!(
                resp.status() == StatusCode::UNAUTHORIZED
                    && body_json(resp).await? == json!({"error": "invalid signature"}),
                "a migrated client's timestamp-only signature was not refused"
            );
            anyhow::Ok(client_id)
        })
    })?;
    let client_id = result?;

    let fields = [
        "WARN".to_string(),
        format!("client_id={client_id}"),
        "method=GET".to_string(),
        format!("path={ME}"),
    ];
    [
        "accepted timestamp-only signature",
        "rejected replayed signature",
        "refused timestamp-only signature from a migrated client",
    ]
    .into_iter()
    .try_for_each(|message| assert_logged(&logs, message, &fields))
}

/// A record whose file is gone answers `404` and logs the documented
/// `vault_object_missing` event at `ERROR`, naming the record and the key an
/// operator has to restore.
#[test]
fn a_missing_vault_object_logs_the_record_and_its_key() -> anyhow::Result<()> {
    let (result, logs) = capture_logs_at_level(tracing::Level::INFO, |rt| {
        rt.block_on(async {
            let dir = tempfile::tempdir()?;
            setup_benchmarks(dir.path())?;
            let state = make_state(dir.path()).await?;
            let owner = registered_user(&state, &[]).await?;
            let resource_id = upload_gguf_text(&state, &owner, None, VAULT_GGUF).await?;

            // Removed behind mgmt's back, as only something outside it can.
            let sha256 = sha256_hex(VAULT_GGUF);
            std::fs::remove_file(
                dir.path()
                    .join("vault-resources")
                    .join(owner.1.as_str())
                    .join("blobs")
                    .join(&sha256),
            )?;

            let resp = signed_empty(
                &state,
                &owner,
                "GET",
                &format!("/vault/resources/{resource_id}/content"),
            )
            .await?;
            let status = resp.status();
            let object_key = format!("{}/blobs/{sha256}", owner.1);
            anyhow::Ok((status, body_json(resp).await?, resource_id, object_key))
        })
    })?;
    let (status, body, resource_id, object_key) = result?;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, json!({"error": "vault object not found"}));
    assert_logged(
        &logs,
        "vault_object_missing",
        &[
            "ERROR".to_string(),
            format!("resource_id={resource_id}"),
            format!("object_key={object_key}"),
        ],
    )
}

/// Creating an account with a grant logs the names it stored, in the `granted`
/// field `PUT /api/v1/users/{user_id}/permissions` logs, so the log answers who
/// was given what.
#[test]
fn a_user_grant_logs_the_names_it_granted() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let (outcome, logs) = capture_logs_at_level(tracing::Level::INFO, |rt| {
        rt.block_on(async {
            let state = make_state(dir.path()).await?;
            let admin = registered_user(
                &state,
                &[UserPermission::ManageAnyOrgs, UserPermission::ReadPlans],
            )
            .await?;
            let resp = signed_json(
                &state,
                &admin,
                "POST",
                "/api/v1/users",
                &json!({
                    "contact_email": "new@example.test",
                    "permissions": ["read_plans"],
                }),
            )
            .await?;
            anyhow::ensure!(
                resp.status() == StatusCode::CREATED,
                "the account was not created"
            );
            anyhow::Ok(json_str(&body_json(resp).await?, "user_id")?.to_string())
        })
    })?;
    let user_id = outcome?;

    let line = logs
        .lines()
        .find(|line| line.contains("user created") && line.contains(&format!("user_id={user_id}")))
        .ok_or_else(|| anyhow::anyhow!("no \"user created\" line for {user_id} in:\n{logs}"))?;
    assert!(
        line.contains(r#"granted=["read_plans"]"#),
        "the creation did not name the grant:\n{line}"
    );
    Ok(())
}

/// An administrator's revocation of another account's key logs the account,
/// the key and `revoked_by`.
#[test]
fn an_administrators_key_revocation_names_who_revoked_it() -> anyhow::Result<()> {
    let (result, logs) = capture_logs_at_level(tracing::Level::INFO, |rt| {
        rt.block_on(async {
            let dir = tempfile::tempdir()?;
            setup_benchmarks(dir.path())?;
            let state = make_state(dir.path()).await?;
            let admin =
                registered_user(&state, UserPermissionGroup::ServerAdmin.permissions()).await?;
            let (_, user_id, key_id) = registered_user(&state, &[]).await?;
            let resp = signed_empty(
                &state,
                &admin,
                "DELETE",
                &format!("/api/v1/users/{user_id}/keys/{key_id}"),
            )
            .await?;
            anyhow::ensure!(
                resp.status() == StatusCode::NO_CONTENT,
                "the revocation answered {}",
                resp.status()
            );
            anyhow::Ok((admin.1, user_id, key_id))
        })
    })?;
    let (admin, user_id, key_id) = result?;

    assert_logged(
        &logs,
        "user key revoked",
        &[
            "INFO".to_string(),
            format!("user_id={user_id}"),
            format!("key_id={key_id}"),
            format!("revoked_by={admin}"),
        ],
    )
}
