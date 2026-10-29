//! The vault surface end to end: files under `/api/v1/vault/blobs`, the index
//! under `/api/v1/vault/resources`, and the content route that serves a private
//! artifact to a device whose owner owns it.

use std::path::{Path, PathBuf};

use anyhow::Context;
use axum::body::to_bytes;
use axum::http::{StatusCode, header};
use futures::{StreamExt, TryStreamExt};
use rstest::rstest;
use serde_json::{Value, json};

use pipette_core::types::Mint;
use pipette_core::vault::{VaultObjectKey, VaultResource};
use pipette_httpapi::state::AppState;
use pipette_plan_types::{
    CampaignId, ClientId, ClientPermission, ExpiresAt, PlanId, Sha256, UploadVaultResourcesRequest,
    UserId, UserPermission, UserStatus, VaultArtifact, VaultResourceId, VaultStoredFile,
};
use pipette_store::{ArtifactBody, BlobBody, BlobWrite, VaultResourcePage, VaultStore};
use pipette_store_testkit::a_huggingface_origin;
use pipette_web_testkit::*;

/// The artifact every test uploads: a gguf by its first bytes. Small: what is
/// under test is the gate, not the streaming.
const BLOB: &[u8] = VAULT_GGUF;

/// The second file of a `gguf_vision` upload: a gguf by its first bytes.
const PROJECTOR: &[u8] = b"GGUF\x03\x00\x00\x00a projector";

// ── fixtures ────────────────────────────────────────────────────────────────

/// A state with the benchmark catalog seeded and an empty vault.
async fn vault_state() -> anyhow::Result<(tempfile::TempDir, AppState)> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    Ok((dir, state))
}

/// Where the filesystem store keeps `principal`'s file with `sha256`, for the
/// tests that tamper with it behind mgmt's back.
fn blob_path(dir: &Path, principal: &str, sha256: &str) -> PathBuf {
    dir.join("vault-resources")
        .join(principal)
        .join("blobs")
        .join(sha256)
}

/// A server admin: creates organizations, owns devices, and reaches every
/// resource.
async fn server_admin(state: &AppState) -> anyhow::Result<UserSigner> {
    registered_user(
        state,
        &[
            UserPermission::ManageAnyOrgs,
            UserPermission::ManageAnyClients,
        ],
    )
    .await
}

/// An account holding **nothing**. It still owns its own vault, which is the
/// point: the surface needs no grant of its own.
async fn operator(state: &AppState) -> anyhow::Result<UserSigner> {
    registered_user(state, &[]).await
}

/// An `org_admin`, placed on `org_id`: administering a team's artifacts is the
/// roster authority and nothing more.
async fn org_admin(state: &AppState, org_id: &str) -> anyhow::Result<UserSigner> {
    let member = registered_user(state, &[UserPermission::ManageOrgs]).await?;
    set_user_org(state, &member.1, Some(org_id.parse()?)).await?;
    Ok(member)
}

/// An approved, work-capable device belonging to `owner`. One owner shape
/// everywhere: the same object registers a resource and owns a device.
async fn owned_device(
    state: &AppState,
    admin: &UserSigner,
    owner: Option<Value>,
) -> anyhow::Result<(ed25519_dalek::SigningKey, ClientId)> {
    let (key, client_id) = register_and_approve(state).await?;
    if let Some(owner) = owner {
        let resp = signed_json(
            state,
            admin,
            "PUT",
            &format!("/api/v1/clients/{client_id}/owner"),
            &json!({ "owner": owner }),
        )
        .await?;
        anyhow::ensure!(
            resp.status() == StatusCode::OK,
            "setting the device owner failed with {}",
            resp.status()
        );
    }
    Ok((key, client_id))
}

fn org_owner(org_id: &str) -> Value {
    json!({ "type": "org", "org_id": org_id })
}

fn user_owner(user_id: &UserId) -> Value {
    json!({ "type": "user", "user_id": user_id })
}

/// `owner` defaults to the signer. The raw response, since several tests are
/// about which refusal it is.
async fn describe(
    state: &AppState,
    signer: &UserSigner,
    owner: Option<&str>,
    sha256: &str,
) -> anyhow::Result<axum::http::Response<axum::body::Body>> {
    describe_vault_upload(state, signer, &gguf_text_upload(owner, sha256)).await
}

/// Upload [`BLOB`] and [`PROJECTOR`] for `signer` and describe them as a
/// `gguf_vision` model. Returns the answer and the two minted ids.
async fn vision_upload(
    state: &AppState,
    signer: &UserSigner,
) -> anyhow::Result<(Value, String, String)> {
    let principal = signer.1.as_str();
    let model = upload_vault_blob(state, signer, principal, BLOB).await?;
    let mmproj = upload_vault_blob(state, signer, principal, PROJECTOR).await?;
    let upload = json!({
        "name": "acme LFM2-VL-3B q4",
        "type": "gguf_vision",
        "model": {"filename": "LFM2-VL-3B-Q4_K_M.gguf", "sha256": model},
        "mmproj": {"filename": "mmproj-LFM2-VL-3B-F16.gguf", "sha256": mmproj},
    });
    crate::api_schema::assert_conforms("UploadVaultResourcesRequest", &upload)?;
    let resp = describe_vault_upload(state, signer, &upload).await?;
    anyhow::ensure!(
        resp.status() == StatusCode::CREATED,
        "describing failed with {}",
        resp.status()
    );
    let answer = body_json(resp).await?;
    let ids: Vec<String> = answer
        .pointer("/resources")
        .and_then(Value::as_array)
        .context("the answer lists its resources")?
        .iter()
        .filter_map(|r| r.get("resource_id").and_then(Value::as_str))
        .map(str::to_string)
        .collect();
    let [model_id, mmproj_id] = <[String; 2]>::try_from(ids)
        .map_err(|ids| anyhow::anyhow!("one resource per file: {ids:?}"))?;
    Ok((answer, model_id, mmproj_id))
}

/// The whole arrangement one test in three needs: an account, its uploaded
/// file, and the record over it. Returns the record's id and the file's digest.
async fn own_resource(state: &AppState, signer: &UserSigner) -> anyhow::Result<(String, String)> {
    let sha256 = upload_vault_blob(state, signer, signer.1.as_str(), BLOB).await?;
    let resource_id = describe_gguf_text_ok(state, signer, None, &sha256).await?;
    Ok((resource_id, sha256))
}

/// A response's body as text, for asserting on what it does not say.
async fn text(resp: axum::http::Response<axum::body::Body>) -> anyhow::Result<String> {
    Ok(String::from_utf8(
        to_bytes(resp.into_body(), usize::MAX).await?.to_vec(),
    )?)
}

fn content_uri(resource_id: &str) -> String {
    format!("/vault/resources/{resource_id}/content")
}

/// A JSON object's keys, sorted, for pinning a wire shape.
fn fields(value: &Value) -> anyhow::Result<Vec<&str>> {
    let mut fields: Vec<&str> = value
        .as_object()
        .context("an answer is an object")?
        .keys()
        .map(String::as_str)
        .collect();
    fields.sort_unstable();
    Ok(fields)
}

// ── the gate on the bytes ───────────────────────────────────────────────────

/// A device fetches an organization's artifact when it belongs to that
/// organization, and nothing else does. This is the whole feature in one test.
#[rstest]
// The device the organization owns outright.
#[case::org_owned_device(true, false, None, StatusCode::OK)]
// A device belonging to an account on the organization: ownership follows the
// account's membership.
#[case::member_owned_device(false, true, None, StatusCode::OK)]
// A device nobody claimed reaches nothing, which is what keeps a
// self-registered machine away from customer bytes.
#[case::unowned_device(false, false, None, StatusCode::FORBIDDEN)]
// A range changes which bytes come back, not who gets them.
#[case::unowned_device_with_a_range(false, false, Some("bytes=4-"), StatusCode::FORBIDDEN)]
#[tokio::test]
async fn an_org_artifact_reaches_the_organization_s_devices(
    #[case] org_owned: bool,
    #[case] member_owned: bool,
    #[case] range: Option<&str>,
    #[case] expected: StatusCode,
) -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let admin = server_admin(&state).await?;
    let org_id = create_org(&state, &admin, "acme").await?;
    let owner = org_admin(&state, &org_id).await?;
    let resource_id = upload_gguf_text(&state, &owner, Some(&org_id), BLOB).await?;

    let device_owner = if org_owned {
        Some(org_owner(&org_id))
    } else if member_owned {
        let member = registered_user(&state, &[]).await?;
        set_user_org(&state, &member.1, Some(org_id.parse()?)).await?;
        Some(user_owner(&member.1))
    } else {
        None
    };
    let (device_key, device_id) = owned_device(&state, &admin, device_owner).await?;

    let uri = content_uri(&resource_id);
    let mut headers = auth_headers(&device_key, &device_id, "GET", &uri)?;
    headers.extend(range.map(|range| ("range", range.to_string())));
    let resp = authed_get_with_headers(&state, &uri, &headers).await?;
    assert_eq!(resp.status(), expected);
    Ok(())
}

/// A colleague on the same organization does not reach it: a personal artifact
/// stays personal.
#[tokio::test]
async fn a_personal_artifact_does_not_follow_its_owner_s_organization() -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let admin = server_admin(&state).await?;
    let org_id = create_org(&state, &admin, "acme").await?;
    let me = operator(&state).await?;
    set_user_org(&state, &me.1, Some(org_id.parse()?)).await?;
    let (resource_id, _) = own_resource(&state, &me).await?;

    let (mine_key, mine_id) = owned_device(&state, &admin, Some(user_owner(&me.1))).await?;
    assert_eq!(
        authed_get(&state, &mine_key, &mine_id, &content_uri(&resource_id))
            .await?
            .status(),
        StatusCode::OK
    );

    let colleague = registered_user(&state, &[]).await?;
    set_user_org(&state, &colleague.1, Some(org_id.parse()?)).await?;
    let (theirs_key, theirs_id) =
        owned_device(&state, &admin, Some(user_owner(&colleague.1))).await?;
    assert_eq!(
        authed_get(&state, &theirs_key, &theirs_id, &content_uri(&resource_id))
            .await?
            .status(),
        StatusCode::FORBIDDEN,
        "a colleague's device must not reach a personal artifact"
    );
    Ok(())
}

const VAULT_JOB: &str = "job-550e8400-e29b-41d4-a716-446655440000";

/// A [`VAULT_JOB`] body on `org_id`'s vault model `resource_id`.
fn vault_job(resource_id: &str, org_id: &str) -> Value {
    json!({
        "job_id": VAULT_JOB,
        "expires_at": "never",
        "spec": {
            "benchmark": "prefill_throughput_256",
            "model": {"type": "gguf_text", "source": "vault_url", "url": content_uri(resource_id), "principal_id": org_id},
            "runtime": {"type": "llamacpp_cli_stock_tools", "source": "github_release", "repository_url": "github.com/ggml-org/llama.cpp", "repository_version": "b5000", "flavor": "macos-arm64"},
        },
    })
}

const VAULT_REFUSAL: &str =
    "the job names a vault resource this client may not fetch; only a failure may be reported";

/// A device outside the organization may claim a job on its vault model, but
/// cannot fetch the weights, so only its failure is taken, on either route.
#[rstest]
#[case::member_success(true, "success", "/benchmarks", StatusCode::ACCEPTED, None, true)]
#[case::outsider_success(
    false,
    "success",
    "/benchmarks",
    StatusCode::FORBIDDEN,
    Some(VAULT_REFUSAL),
    false
)]
#[case::outsider_success_in_a_batch(
    false,
    "success",
    "/benchmarks/batch",
    StatusCode::OK,
    Some(VAULT_REFUSAL),
    false
)]
#[case::outsider_failure(false, "failure", "/benchmarks", StatusCode::ACCEPTED, None, true)]
#[case::outsider_retriable_failure(
    false,
    "retriable",
    "/benchmarks",
    StatusCode::ACCEPTED,
    None,
    false
)]
#[tokio::test]
async fn only_a_device_that_may_fetch_the_model_reports_its_result(
    #[case] member: bool,
    #[case] kind: &str,
    #[case] route: &str,
    #[case] status: StatusCode,
    #[case] refusal: Option<&str>,
    #[case] recorded: bool,
) -> anyhow::Result<()> {
    let (dir, state) = vault_state().await?;
    let admin = server_admin(&state).await?;
    let org_id = create_org(&state, &admin, "acme").await?;
    let other_org = create_org(&state, &admin, "rival").await?;
    let owner = org_admin(&state, &org_id).await?;
    let resource_id = upload_gguf_text(&state, &owner, Some(&org_id), BLOB).await?;
    let device_org = if member { &org_id } else { &other_org };
    let (key, device_id) = owned_device(&state, &admin, Some(org_owner(device_org))).await?;

    let job_id = VAULT_JOB;
    let job = seed_leased(
        dir.path(),
        job_id,
        &device_id,
        &vault_job(&resource_id, &org_id),
    )?;
    let leases = state.todo_store.list_leased_for_client(&device_id).await?;

    let mut result = if kind == "success" {
        valid_submission("prefill_throughput_256")
    } else {
        json!({
            "message_type": "failure",
            "benchmark_id": "prefill_throughput_256",
            "retriable": kind == "retriable",
            "failure_reason": "the vault refused the weights",
        })
    };
    result["job_id"] = json!(job_id);
    let (body, error_at) = if route == "/benchmarks" {
        (result, "/error")
    } else {
        (json!({ "submissions": [result] }), "/results/0/error")
    };
    let resp = authed_post(&state, &key, &device_id, route, &body).await?;

    assert_eq!(resp.status(), status);
    let answer = body_json(resp).await?;
    assert_eq!(
        answer.pointer(error_at).and_then(Value::as_str),
        refusal,
        "{answer}"
    );
    assert_eq!(
        state.submission_store.find_job(&job).await?.is_some(),
        recorded,
        "a result is recorded only from a device that may fetch the model"
    );
    assert_eq!(
        state.todo_store.get_avail_by_job(&job).await?.is_some(),
        kind == "retriable",
        "only a retriable failure puts the job back in the queue"
    );
    if refusal.is_some() {
        assert_eq!(
            state.todo_store.list_leased_for_client(&device_id).await?,
            leases,
            "a refused success must not renew the lease"
        );
    }
    Ok(())
}

/// A device that may not fetch the job's vault model gets no lease on it and
/// gives back any it holds, so a device that can run the job takes it.
#[rstest]
#[case::member_claims(true, "claim", false, StatusCode::OK)]
#[case::outsider_claims(false, "claim", false, StatusCode::NO_CONTENT)]
#[case::outsider_re_polls_its_lease(false, "claim", true, StatusCode::NO_CONTENT)]
#[case::member_heartbeats(true, "heartbeat", true, StatusCode::OK)]
#[case::outsider_heartbeats(false, "heartbeat", true, StatusCode::FORBIDDEN)]
#[case::member_renews_on_reclaim(true, "reclaim", true, StatusCode::OK)]
#[case::outsider_renews_on_reclaim(false, "reclaim", true, StatusCode::FORBIDDEN)]
#[case::member_reacquires_on_reclaim(true, "reclaim", false, StatusCode::OK)]
#[case::outsider_reacquires_on_reclaim(false, "reclaim", false, StatusCode::FORBIDDEN)]
#[tokio::test]
async fn only_a_device_that_may_fetch_the_model_holds_its_lease(
    #[case] member: bool,
    #[case] route: &str,
    #[case] held: bool,
    #[case] status: StatusCode,
) -> anyhow::Result<()> {
    let (dir, state) = vault_state().await?;
    let admin = server_admin(&state).await?;
    let org_id = create_org(&state, &admin, "acme").await?;
    let other_org = create_org(&state, &admin, "rival").await?;
    let owner = org_admin(&state, &org_id).await?;
    let resource_id = upload_gguf_text(&state, &owner, Some(&org_id), BLOB).await?;
    let device_org = if member { &org_id } else { &other_org };
    let (key, device_id) = owned_device(&state, &admin, Some(org_owner(device_org))).await?;

    let body = vault_job(&resource_id, &org_id);
    let job = if held {
        seed_leased(dir.path(), VAULT_JOB, &device_id, &body)?
    } else {
        let job = seed_avail(dir.path(), VAULT_JOB, ExpiresAt::Never, &body)?;
        state
            .todo_store
            .write_eligible(&device_id, &job, ExpiresAt::Never)
            .await?;
        job
    };
    let leases = state.todo_store.list_leased_for_client(&device_id).await?;

    let resp = match route {
        "claim" => authed_post(&state, &key, &device_id, "/plans/claim", &json!({})).await?,
        "heartbeat" => {
            authed_put_empty(&state, &key, &device_id, &format!("/plans/{job}/heartbeat")).await?
        }
        _ => {
            authed_post(
                &state,
                &key,
                &device_id,
                &format!("/plans/{job}/reclaim"),
                &json!({}),
            )
            .await?
        }
    };

    assert_eq!(resp.status(), status);
    if status == StatusCode::FORBIDDEN {
        let answer = body_json(resp).await?;
        assert_eq!(
            answer.pointer("/error").and_then(Value::as_str),
            Some("the job names a vault resource this client may not fetch"),
            "{answer}"
        );
    }
    // Every outsider lease, granted or held, goes straight back to the queue,
    // denied to the outsider.
    let after = state.todo_store.list_leased_for_client(&device_id).await?;
    if member {
        assert_ne!(after, leases, "a member is granted or renewed a lease");
    } else {
        assert!(after.is_empty(), "an outsider keeps no lease: {after:?}");
    }
    assert_eq!(
        state.todo_store.get_avail_by_job(&job).await?.is_some(),
        !member,
        "an outsider's lease goes back to the queue"
    );
    assert_eq!(
        state.todo_store.is_denied(&job, &device_id).await?,
        !member,
        "a put-back job is denied to the outsider"
    );
    Ok(())
}

/// A claim that puts a vault job back moves on to the next eligible job. Only
/// a refusal denies the job: a failed check may be the store's fault.
#[rstest]
#[case::outsider_skips_a_vault_job(false, true)]
#[case::member_skips_a_malformed_vault_job(true, false)]
#[tokio::test]
async fn a_claim_moves_past_a_vault_job_it_puts_back(
    #[case] member: bool,
    #[case] well_formed: bool,
) -> anyhow::Result<()> {
    let (dir, state) = vault_state().await?;
    let admin = server_admin(&state).await?;
    let org_id = create_org(&state, &admin, "acme").await?;
    let other_org = create_org(&state, &admin, "rival").await?;
    let owner = org_admin(&state, &org_id).await?;
    let resource_id = upload_gguf_text(&state, &owner, Some(&org_id), BLOB).await?;
    let device_org = if member { &org_id } else { &other_org };
    let (key, device_id) = owned_device(&state, &admin, Some(org_owner(device_org))).await?;

    let principal = if well_formed { org_id.as_str() } else { "" };
    // Sooner than `never`, so the vault job ranks first.
    let soon = ExpiresAt::At(chrono::Utc::now() + chrono::Duration::days(1));
    let vault = seed_avail(
        dir.path(),
        VAULT_JOB,
        soon,
        &vault_job(&resource_id, principal),
    )?;
    let other_id = "job-6ba7b810-9dad-11d1-80b4-00c04fd430c8";
    let other = seed_avail(
        dir.path(),
        other_id,
        ExpiresAt::Never,
        &json!({"job_id": other_id, "expires_at": "never", "spec": {"benchmark": "prefill_throughput_256"}}),
    )?;
    state
        .todo_store
        .write_eligible(&device_id, &vault, soon)
        .await?;
    state
        .todo_store
        .write_eligible(&device_id, &other, ExpiresAt::Never)
        .await?;

    let resp = authed_post(&state, &key, &device_id, "/plans/claim", &json!({})).await?;

    assert_eq!(resp.status(), StatusCode::OK);
    let answer = body_json(resp).await?;
    assert_eq!(
        answer.pointer("/job_id").and_then(Value::as_str),
        Some(other_id),
        "{answer}"
    );
    assert!(state.todo_store.get_avail_by_job(&vault).await?.is_some());
    assert_eq!(
        state.todo_store.is_denied(&vault, &device_id).await?,
        !member
    );
    Ok(())
}

/// An account cannot launder another principal's upload into its devices'
/// reach: knowing a digest is not holding the file. No refusal is an existence
/// oracle: the stranger is answered the same whether or not the file is held.
#[tokio::test]
async fn an_account_cannot_claim_another_principal_s_file() -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let admin = server_admin(&state).await?;
    let org_id = create_org(&state, &admin, "acme").await?;
    let team_admin = org_admin(&state, &org_id).await?;
    let held = upload_vault_blob(&state, &team_admin, &org_id, BLOB).await?;
    let absent = "b".repeat(64);

    let outsider = operator(&state).await?;
    let (state, outsider, org_id) = (&state, &outsider, org_id.as_str());
    let statuses: Vec<_> = futures::stream::iter([&held, &absent])
        .then(|sha256| async move {
            let put = put_vault_blob(state, outsider, org_id, sha256, BLOB).await?;
            let head = head_vault_blob(state, outsider, org_id, sha256).await?;
            let described = describe(state, outsider, Some(org_id), sha256).await?;
            anyhow::Ok((put.status(), head.status(), described.status()))
        })
        .try_collect()
        .await?;
    assert_eq!(
        statuses,
        [(
            StatusCode::FORBIDDEN,
            StatusCode::FORBIDDEN,
            StatusCode::FORBIDDEN
        ); 2],
        "another principal's files answer 403, held or not"
    );

    // Describing Acme's digest as the outsider's own finds nothing under the
    // outsider, and says so exactly as for a digest nobody holds.
    let refusals: Vec<(StatusCode, String)> = futures::stream::iter([&held, &absent])
        .then(|sha256| async move {
            let resp = describe(state, outsider, None, sha256).await?;
            let status = resp.status();
            let error = json_str(&body_json(resp).await?, "error")?.replace(sha256.as_str(), "…");
            anyhow::Ok((status, error))
        })
        .try_collect()
        .await?;
    let [for_held, for_absent] = refusals.as_slice() else {
        anyhow::bail!("one refusal per digest: {refusals:?}");
    };
    assert_eq!(for_held.0, StatusCode::BAD_REQUEST);
    assert_eq!(for_held, for_absent, "held and absent read alike");
    Ok(())
}

/// A device reads the record it is entitled to fetch, so it can check the
/// digest and the size against what it already holds before pulling a
/// multi-gigabyte checkpoint. A device that is not entitled reads neither.
#[tokio::test]
async fn a_device_reads_the_record_it_may_fetch() -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let admin = server_admin(&state).await?;
    let me = operator(&state).await?;
    let (resource_id, _) = own_resource(&state, &me).await?;
    let uri = format!("/vault/resources/{resource_id}");

    let (mine_key, mine_id) = owned_device(&state, &admin, Some(user_owner(&me.1))).await?;
    let resp = authed_get(&state, &mine_key, &mine_id, &uri).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["sha256"], sha256_hex(BLOB));
    assert_eq!(body["bytes"], BLOB.len());
    assert_eq!(body["entry"]["model"]["url"], content_uri(&resource_id));
    assert!(body.get("object_key").is_none(), "{body}");
    assert!(body.get("may_administer").is_none(), "{body}");

    // The record answers the same question the bytes do, so the two cannot
    // disagree about who may have it.
    let (theirs_key, theirs_id) = owned_device(&state, &admin, None).await?;
    assert_eq!(
        authed_get(&state, &theirs_key, &theirs_id, &uri)
            .await?
            .status(),
        StatusCode::FORBIDDEN
    );
    Ok(())
}

/// The served response: the object's length, the record's digest as the ETag,
/// and a cache directive that keeps private bytes out of a shared cache. A
/// fetch with no `Range` also learns that a broken fetch can resume.
#[tokio::test]
async fn the_bytes_arrive_with_the_pin_that_verifies_them() -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let me = operator(&state).await?;
    let (resource_id, _) = own_resource(&state, &me).await?;

    let resp = signed_empty(&state, &me, "GET", &content_uri(&resource_id)).await?;
    assert_eq!(resp.status(), StatusCode::OK);

    let headers = resp.headers().clone();
    assert_eq!(
        headers
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok()),
        Some(BLOB.len().to_string().as_str())
    );
    assert_eq!(
        headers.get(header::ETAG).and_then(|v| v.to_str().ok()),
        Some(format!("\"{}\"", sha256_hex(BLOB)).as_str())
    );
    assert_eq!(
        headers
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok()),
        Some("private, no-store")
    );
    assert_eq!(
        headers
            .get(header::ACCEPT_RANGES)
            .and_then(|v| v.to_str().ok()),
        Some("bytes")
    );

    let body = to_bytes(resp.into_body(), usize::MAX).await?;
    assert_eq!(body.as_ref(), BLOB, "the bytes must hash to the pin");
    Ok(())
}

/// A device that may not do work is refused at the header, before the record is
/// read — the gate `GET /datasets/{id}` takes, for its reason.
#[tokio::test]
async fn a_device_without_perform_work_fetches_nothing() -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let admin = server_admin(&state).await?;
    let me = operator(&state).await?;
    let (resource_id, _) = own_resource(&state, &me).await?;

    let (device_key, device_id) =
        register_approve_and_grant(&state, &[ClientPermission::SubmitResults]).await?;
    let resp = signed_json(
        &state,
        &admin,
        "PUT",
        &format!("/api/v1/clients/{device_id}/owner"),
        &json!({ "owner": user_owner(&me.1) }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);

    assert_eq!(
        authed_get(&state, &device_key, &device_id, &content_uri(&resource_id))
            .await?
            .status(),
        StatusCode::FORBIDDEN
    );
    Ok(())
}

/// An ordinary member of the organization reads its artifacts and does not
/// administer them, the asymmetry `read_orgs` and `manage_orgs` draw over the
/// roster. Its *devices* fetch them too, which is a relation rather than a
/// grant.
#[tokio::test]
async fn a_member_reads_its_organization_s_artifacts_and_deletes_none() -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let admin = server_admin(&state).await?;
    let org_id = create_org(&state, &admin, "acme").await?;
    let team_admin = org_admin(&state, &org_id).await?;
    let resource_id = upload_gguf_text(&state, &team_admin, Some(&org_id), BLOB).await?;

    let member = operator(&state).await?;
    set_user_org(&state, &member.1, Some(org_id.parse()?)).await?;
    assert_eq!(
        signed_empty(&state, &member, "GET", &content_uri(&resource_id))
            .await?
            .status(),
        StatusCode::OK
    );
    let record = format!("/api/v1/vault/resources/{resource_id}");
    let read = body_json(signed_empty(&state, &member, "GET", &record).await?).await?;
    assert_eq!(read["may_administer"], json!(false), "{read}");
    let resp = signed_empty(&state, &member, "DELETE", &record).await?;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "reading the team's record is not administering it"
    );
    let error = body_json(resp).await?;
    let error = json_str(&error, "error")?;
    assert!(
        error.contains("needs manage_orgs"),
        "the refusal names the grant, not another owner: {error}"
    );

    let (device_key, device_id) = owned_device(&state, &admin, Some(user_owner(&member.1))).await?;
    assert_eq!(
        authed_get(&state, &device_key, &device_id, &content_uri(&resource_id))
            .await?
            .status(),
        StatusCode::OK,
        "the member's device runs the work as well"
    );
    Ok(())
}

/// An organization's device administrator cannot make a device its colleague's,
/// which would let a device it holds the key of fetch the colleague's weights.
#[tokio::test]
async fn an_org_device_admin_cannot_hand_a_device_to_a_colleague() -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let admin = server_admin(&state).await?;
    let org_id = create_org(&state, &admin, "acme").await?;
    let device_admin = registered_user(&state, &[UserPermission::ManageClients]).await?;
    set_user_org(&state, &device_admin.1, Some(org_id.parse()?)).await?;
    let colleague = operator(&state).await?;
    set_user_org(&state, &colleague.1, Some(org_id.parse()?)).await?;
    let (resource_id, _) = own_resource(&state, &colleague).await?;
    let (device_key, device_id) = owned_device(&state, &admin, Some(org_owner(&org_id))).await?;

    let owner_uri = format!("/api/v1/clients/{device_id}/owner");
    let resp = signed_json(
        &state,
        &device_admin,
        "PUT",
        &owner_uri,
        &json!({ "owner": user_owner(&colleague.1) }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        authed_get(&state, &device_key, &device_id, &content_uri(&resource_id))
            .await?
            .status(),
        StatusCode::FORBIDDEN,
        "the device never became the colleague's"
    );

    let resp = signed_json(
        &state,
        &device_admin,
        "PUT",
        &owner_uri,
        &json!({ "owner": user_owner(&device_admin.1) }),
    )
    .await?;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "its own device is still its to keep"
    );
    Ok(())
}

// ── the index ───────────────────────────────────────────────────────────────

/// A file is kept only if it hashes to the digest it is sent under: a mismatch
/// keeps nothing. Sending it again is harmless, and `HEAD` says whether it is
/// there.
#[tokio::test]
async fn a_file_is_kept_only_as_its_digest_says() -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let me = operator(&state).await?;
    let principal = me.1.to_string();
    let sha256 = sha256_hex(BLOB);

    let wrong = "b".repeat(64);
    let resp = put_vault_blob(&state, &me, &principal, &wrong, BLOB).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let error = body_json(resp).await?;
    let error = json_str(&error, "error")?;
    assert!(
        error.contains(&sha256) && error.contains(&wrong),
        "the refusal names both digests: {error}"
    );
    assert_eq!(
        head_vault_blob(&state, &me, &principal, &wrong)
            .await?
            .status(),
        StatusCode::NOT_FOUND,
        "a mismatch keeps nothing"
    );

    assert_eq!(
        head_vault_blob(&state, &me, &principal, &sha256)
            .await?
            .status(),
        StatusCode::NOT_FOUND
    );
    let resp = put_vault_blob(&state, &me, &principal, &sha256, BLOB).await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(
        body_json(resp).await?,
        json!({"sha256": sha256, "bytes": BLOB.len()})
    );
    assert_eq!(
        head_vault_blob(&state, &me, &principal, &sha256)
            .await?
            .status(),
        StatusCode::OK
    );
    let again = put_vault_blob(&state, &me, &principal, &sha256, BLOB).await?;
    assert_eq!(again.status(), StatusCode::OK, "already uploaded");
    Ok(())
}

/// A file already uploaded is kept as it is and the new body goes unread: other
/// bytes under its digest answer `200` with the kept file's size, and the record
/// still serves the first bytes. A `400` mismatch or a replacement fails here.
#[tokio::test]
async fn a_file_sent_again_is_kept_as_first_uploaded() -> anyhow::Result<()> {
    const OTHER: &[u8] = b"GGUF other bytes, another length";
    let (_dir, state) = vault_state().await?;
    let me = operator(&state).await?;
    let sha256 = upload_vault_blob(&state, &me, me.1.as_str(), BLOB).await?;

    let resp = put_vault_blob(&state, &me, me.1.as_str(), &sha256, OTHER).await?;
    assert_eq!(resp.status(), StatusCode::OK, "already uploaded");
    assert_eq!(
        body_json(resp).await?,
        json!({"sha256": sha256, "bytes": BLOB.len()}),
        "the kept file's size, not the body's"
    );

    let resource_id = describe_gguf_text_ok(&state, &me, None, &sha256).await?;
    let resp = signed_empty(&state, &me, "GET", &content_uri(&resource_id)).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await?;
    assert_eq!(body.as_ref(), BLOB, "the first bytes are the ones kept");
    Ok(())
}

/// Uploads stream a few at a time: one past the limit is told to retry rather
/// than held, and goes through once a slot frees.
#[tokio::test]
async fn an_upload_past_the_limit_is_told_to_retry() -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let me = operator(&state).await?;
    let sha256 = sha256_hex(BLOB);
    let all = u32::try_from(state.vault_uploads.available_permits())?;
    let busy = state.vault_uploads.try_acquire_many(all)?;
    assert_eq!(
        put_vault_blob(&state, &me, me.1.as_str(), &sha256, BLOB)
            .await?
            .status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    drop(busy);
    assert_eq!(
        put_vault_blob(&state, &me, me.1.as_str(), &sha256, BLOB)
            .await?
            .status(),
        StatusCode::CREATED
    );
    Ok(())
}

/// Describing names files already uploaded for the owner, and each must be
/// what its field says: a `.gguf` under a gguf model's `model`, a packed
/// directory under `dir_archive`. Nothing is written for a refused description.
#[rstest]
#[case::gguf_as_gguf_text(BLOB, "gguf_text", "model", None)]
#[case::zip_as_gguf_text(
    b"PK\x03\x04 a zip, not a gguf",
    "gguf_text",
    "model",
    Some("the `model` file is not a gguf")
)]
#[case::zip_as_mlx(b"PK\x03\x04 an mlx directory, zipped", "mlx", "dir_archive", None)]
#[case::gguf_as_mlx(
    BLOB,
    "mlx",
    "dir_archive",
    Some("the `dir_archive` file is not a tar, tar.gz or zip archive")
)]
#[tokio::test]
async fn describing_names_uploaded_files_of_the_right_format(
    #[case] bytes: &[u8],
    #[case] variant: &str,
    #[case] field: &str,
    #[case] refused: Option<&str>,
) -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let me = operator(&state).await?;
    let sha256 = sha256_hex(bytes);
    let body = json!({
        "name": "acme private checkpoint",
        "type": variant,
        field: {"filename": "acme-checkpoint", "sha256": sha256},
    });

    let resp = describe_vault_upload(&state, &me, &body).await?;
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "nothing uploaded yet"
    );
    let error = body_json(resp).await?;
    assert!(
        json_str(&error, "error")?.contains("upload it first"),
        "{error}"
    );

    upload_vault_blob(&state, &me, me.1.as_str(), bytes).await?;
    let resp = describe_vault_upload(&state, &me, &body).await?;
    match refused {
        None => assert_eq!(resp.status(), StatusCode::CREATED),
        Some(reason) => {
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
            let error = body_json(resp).await?;
            assert!(json_str(&error, "error")?.contains(reason), "{error}");
            let listing =
                body_json(signed_empty(&state, &me, "GET", "/api/v1/vault/resources").await?)
                    .await?;
            assert_eq!(listing["items"], json!([]), "a refusal writes no record");
        }
    }
    Ok(())
}

/// A multi-file description is refused whole: the model is uploaded and a gguf,
/// but a projector not uploaded or not a gguf refuses it, and no record is
/// written for the model either. The refusal names the projector, never the
/// model; `{sha256}` stands for the projector's digest.
#[rstest]
#[case::mmproj_not_uploaded(PROJECTOR, false, &["{sha256}", "upload it first"])]
#[case::mmproj_not_a_gguf(
    b"PK\x03\x04 a zip, not a projector",
    true,
    &["the `mmproj` file is not a gguf"]
)]
#[tokio::test]
async fn a_multi_file_description_is_refused_whole(
    #[case] mmproj: &[u8],
    #[case] uploaded: bool,
    #[case] says: &[&str],
) -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let me = operator(&state).await?;
    let model = upload_vault_blob(&state, &me, me.1.as_str(), BLOB).await?;
    let mmproj_sha256 = sha256_hex(mmproj);
    if uploaded {
        upload_vault_blob(&state, &me, me.1.as_str(), mmproj).await?;
    }
    let body = json!({
        "name": "acme LFM2-VL-3B q4",
        "type": "gguf_vision",
        "model": {"filename": "LFM2-VL-3B-Q4_K_M.gguf", "sha256": model},
        "mmproj": {"filename": "mmproj-LFM2-VL-3B-F16.gguf", "sha256": mmproj_sha256},
    });

    let resp = describe_vault_upload(&state, &me, &body).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let error = body_json(resp).await?;
    let error = json_str(&error, "error")?;
    for needle in says {
        assert!(
            error.contains(&needle.replace("{sha256}", &mmproj_sha256)),
            "{error}"
        );
    }
    assert!(!error.contains(&model), "{error}");
    let listing =
        body_json(signed_empty(&state, &me, "GET", "/api/v1/vault/resources").await?).await?;
    assert_eq!(
        listing["items"],
        json!([]),
        "no record for the model either"
    );
    Ok(())
}

/// The answer to an upload: the plan entry, its TOML, and one record per file,
/// each record rebuilding the same entry when read back.
#[tokio::test]
async fn an_upload_answers_with_the_plan_entry() -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let me = operator(&state).await?;
    let (answer, model_id, mmproj_id) = vision_upload(&state, &me).await?;
    let mut fields: Vec<&str> = answer
        .as_object()
        .context("an answer is an object")?
        .keys()
        .map(String::as_str)
        .collect();
    fields.sort_unstable();
    assert_eq!(fields, ["entry", "resources", "toml"]);
    crate::api_schema::assert_conforms("VaultResourcesUploaded", &answer)?;

    assert_eq!(
        answer["entry"],
        json!({"model": {
            "type": "gguf_vision",
            "source": "vault_url",
            "model": content_uri(&model_id),
            "model_sha256": sha256_hex(BLOB),
            "mmproj": content_uri(&mmproj_id),
            "mmproj_sha256": sha256_hex(PROJECTOR),
            "principal_id": me.1,
            "custom_label": "acme LFM2-VL-3B q4",
        }})
    );
    let toml = json_str(&answer, "toml")?;
    assert!(
        toml.starts_with('{') && toml.contains("vault_url"),
        "{toml}"
    );

    let (state, me) = (&state, &me);
    let records: Vec<Value> = futures::stream::iter([&model_id, &mmproj_id])
        .then(|id| async move {
            body_json(signed_empty(state, me, "GET", &format!("/vault/resources/{id}")).await?)
                .await
        })
        .try_collect()
        .await?;
    records.iter().for_each(|record| {
        assert_eq!(record["entry"], answer["entry"], "{record}");
        assert_eq!(record["toml"], answer["toml"], "{record}");
    });
    Ok(())
}

/// The upload form's table: any account reads it, and a body filling every file
/// it lists parses as plan-types' upload of the family it is listed under and
/// conforms to the app's schemas, sent and stored alike.
#[tokio::test]
async fn the_types_are_every_upload_the_route_takes() -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let me = operator(&state).await?;
    let resp = signed_empty(&state, &me, "GET", "/api/v1/vault/types").await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let answer = body_json(resp).await?;
    crate::api_schema::assert_conforms("VaultTypes", &answer)?;

    let types = ["models", "runtimes"]
        .into_iter()
        .map(|family| {
            let listed = answer[family]
                .as_array()
                .with_context(|| format!("the answer lists {family}"))?;
            anyhow::Ok(
                listed
                    .iter()
                    .map(move |vault_type| (family == "runtimes", vault_type)),
            )
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    let stored_as: VaultResourceId = "vault_0195c3e2a4f37b8c".parse()?;
    types
        .into_iter()
        .flatten()
        .try_for_each(|(runtime, vault_type)| -> anyhow::Result<()> {
            let mut body = json!({"name": "acme upload", "type": vault_type["type"]});
            if runtime {
                body["flavor"] = vault_type["flavors"][0].clone();
            } else {
                assert_eq!(vault_type["flavors"], json!([]), "{vault_type}");
            }
            let body = vault_type["files"]
                .as_array()
                .context("each type lists files")?
                .iter()
                .try_fold(body, |mut body, field| {
                    let role = field["role"].as_str().context("a file names its role")?;
                    body[role] = json!({"filename": "acme.bin", "sha256": "a".repeat(64)});
                    Ok::<_, anyhow::Error>(body)
                })?;
            crate::api_schema::assert_conforms("UploadVaultResourcesRequest", &body)?;
            let upload: UploadVaultResourcesRequest =
                serde_json::from_value(body.clone()).with_context(|| format!("{body}"))?;
            assert_eq!(
                matches!(upload.artifact, VaultArtifact::Runtime(_)),
                runtime,
                "{body}"
            );
            let stored = upload.artifact.map(|_, file| VaultStoredFile {
                resource_id: stored_as.clone(),
                filename: file.filename,
                sha256: file.sha256,
            });
            crate::api_schema::assert_conforms("VaultArtifact", &serde_json::to_value(stored)?)
        })
}

/// Each record of a multi-file upload describes and serves its own file. If the
/// sizes or keys were crossed, the projector's id would answer the model's
/// digest, length or bytes.
#[tokio::test]
async fn each_file_of_an_upload_is_its_own_record() -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let me = operator(&state).await?;
    let (_, model_id, mmproj_id) = vision_upload(&state, &me).await?;

    for (resource_id, file) in [(&model_id, BLOB), (&mmproj_id, PROJECTOR)] {
        let resp = signed_empty(
            &state,
            &me,
            "GET",
            &format!("/vault/resources/{resource_id}"),
        )
        .await?;
        assert_eq!(resp.status(), StatusCode::OK, "{resource_id}");
        let record = body_json(resp).await?;
        assert_eq!(record["sha256"], sha256_hex(file), "{record}");
        assert_eq!(record["bytes"], file.len(), "{record}");

        let resp = signed_empty(&state, &me, "GET", &content_uri(resource_id)).await?;
        assert_eq!(resp.status(), StatusCode::OK, "{resource_id}");
        let headers = resp.headers().clone();
        assert_eq!(
            headers.get(header::ETAG).and_then(|v| v.to_str().ok()),
            Some(format!("\"{}\"", sha256_hex(file)).as_str())
        );
        assert_eq!(
            headers
                .get(header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok()),
            Some(file.len().to_string().as_str())
        );
        let body = to_bytes(resp.into_body(), usize::MAX).await?;
        assert_eq!(body.as_ref(), file, "{resource_id} serves its own file");
    }
    Ok(())
}

/// **No answer names where a file is stored**, on success or refusal: a
/// client names a file by its principal and digest and a resource by its id.
#[tokio::test]
async fn no_answer_names_where_a_file_is_stored() -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let me = operator(&state).await?;
    let principal = me.1.to_string();
    let sha256 = sha256_hex(BLOB);

    let mut answers = vec![
        text(put_vault_blob(&state, &me, &principal, &"b".repeat(64), BLOB).await?).await?,
        text(put_vault_blob(&state, &me, &principal, &sha256, BLOB).await?).await?,
        text(describe(&state, &me, None, &"c".repeat(64)).await?).await?,
    ];
    let answer = text(describe(&state, &me, None, &sha256).await?).await?;
    let resource_id = json_str(
        &serde_json::from_str::<Value>(&answer)?["resources"][0],
        "resource_id",
    )?
    .to_string();
    answers.push(answer);
    let record = format!("/vault/resources/{resource_id}");
    answers.push(text(signed_empty(&state, &me, "GET", &record).await?).await?);
    answers.push(text(signed_empty(&state, &me, "GET", "/api/v1/vault/resources").await?).await?);

    // The storage key is `{principal}/blobs/{sha256}`: neither it nor the
    // field that carried it appears, on success or refusal.
    let stored_at = [
        format!("{principal}/blobs"),
        "object_key".to_string(),
        "vault-resources".to_string(),
    ];
    let leaked: Vec<&String> = stored_at
        .iter()
        .filter(|internal| answers.iter().any(|a| a.contains(internal.as_str())))
        .collect();
    assert!(leaked.is_empty(), "{leaked:?} in {answers:?}");
    Ok(())
}

/// A file route's path is checked at the boundary, on the upload and the probe
/// alike: an owner that is not a principal id and a digest that is not 64
/// lowercase hex characters are a `400` before anything is read.
#[rstest]
#[case::not_a_principal("acme", &"a".repeat(64))]
#[case::a_device("ev1_0195c3e2", &"a".repeat(64))]
#[case::short_digest("{me}", "abc123")]
#[case::uppercase_digest("{me}", &"A".repeat(64))]
#[tokio::test]
async fn a_file_route_is_checked_at_the_boundary(
    #[case] principal: &str,
    #[case] sha256: &str,
    #[values("PUT", "HEAD")] method: &str,
) -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let me = operator(&state).await?;
    let principal = principal.replace("{me}", me.1.as_str());
    let resp = match method {
        "PUT" => put_vault_blob(&state, &me, &principal, sha256, BLOB).await?,
        _ => head_vault_blob(&state, &me, &principal, sha256).await?,
    };
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    Ok(())
}

/// Who may upload for an organization: its members and the server, and nobody
/// else, as for a campaign. The files and the description take one authority.
#[rstest]
#[case::outsider(false, false, StatusCode::FORBIDDEN, StatusCode::FORBIDDEN)]
#[case::member(true, false, StatusCode::OK, StatusCode::CREATED)]
#[case::org_admin(true, true, StatusCode::OK, StatusCode::CREATED)]
#[tokio::test]
async fn uploading_for_an_organization_takes_membership(
    #[case] on_roster: bool,
    #[case] manage_orgs: bool,
    #[case] put: StatusCode,
    #[case] described: StatusCode,
) -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let admin = server_admin(&state).await?;
    let org_id = create_org(&state, &admin, "acme").await?;
    let sha256 = upload_vault_blob(&state, &admin, &org_id, BLOB).await?;

    let grants: &[UserPermission] = if manage_orgs {
        &[UserPermission::ManageOrgs]
    } else {
        &[]
    };
    let caller = registered_user(&state, grants).await?;
    if on_roster {
        set_user_org(&state, &caller.1, Some(org_id.parse()?)).await?;
    }

    assert_eq!(
        put_vault_blob(&state, &caller, &org_id, &sha256, BLOB)
            .await?
            .status(),
        put
    );
    assert_eq!(
        describe(&state, &caller, Some(&org_id), &sha256)
            .await?
            .status(),
        described
    );
    assert_eq!(
        describe(&state, &admin, Some(&org_id), &sha256)
            .await?
            .status(),
        StatusCode::CREATED,
        "the server reaches every organization"
    );
    Ok(())
}

/// A member's upload for the team is the team's: the member cannot delete it,
/// and membership names no colleague as an owner.
#[tokio::test]
async fn a_member_uploads_for_its_organization_and_for_no_colleague() -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let admin = server_admin(&state).await?;
    let org_id = create_org(&state, &admin, "acme").await?;
    let member = operator(&state).await?;
    set_user_org(&state, &member.1, Some(org_id.parse()?)).await?;
    let colleague = operator(&state).await?;
    set_user_org(&state, &colleague.1, Some(org_id.parse()?)).await?;

    let resource_id = upload_gguf_text(&state, &member, Some(&org_id), BLOB).await?;
    let resp = signed_empty(
        &state,
        &member,
        "DELETE",
        &format!("/api/v1/vault/resources/{resource_id}"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let error = body_json(resp).await?;
    assert!(json_str(&error, "error")?.contains("needs manage_orgs"));

    let sha256 = sha256_hex(BLOB);
    assert_eq!(
        put_vault_blob(&state, &member, colleague.1.as_str(), &sha256, BLOB)
            .await?
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        describe(&state, &member, Some(colleague.1.as_str()), &sha256)
            .await?
            .status(),
        StatusCode::FORBIDDEN
    );
    Ok(())
}

/// `manage_orgs` administers only the holder's own organization: held on no
/// roster or on a rival's, it neither uploads, probes nor describes for acme,
/// and the answer is the same whether or not acme holds the file.
#[rstest]
#[case::on_no_org(false)]
#[case::on_a_rival(true)]
#[tokio::test]
async fn manage_orgs_off_the_roster_uploads_nothing_for_the_organization(
    #[case] on_a_rival: bool,
) -> anyhow::Result<()> {
    const OUT_OF_SCOPE: &str = "you may only upload to the vault for yourself, \
        your organization, or a member of an organization you administer";
    let (_dir, state) = vault_state().await?;
    let admin = server_admin(&state).await?;
    let org_id = create_org(&state, &admin, "acme").await?;
    let held = upload_vault_blob(&state, &admin, &org_id, BLOB).await?;
    // Its bytes hash to it, so a PUT let through would keep it.
    let absent = sha256_hex(PROJECTOR);
    let caller = if on_a_rival {
        let rival = create_org(&state, &admin, "rival").await?;
        org_admin(&state, &rival).await?
    } else {
        registered_user(&state, &[UserPermission::ManageOrgs]).await?
    };

    for (sha256, bytes) in [(&held, BLOB), (&absent, PROJECTOR)] {
        let put = put_vault_blob(&state, &caller, &org_id, sha256, bytes).await?;
        assert_eq!(put.status(), StatusCode::FORBIDDEN, "PUT {sha256}");
        assert_eq!(json_str(&body_json(put).await?, "error")?, OUT_OF_SCOPE);
        assert_eq!(
            head_vault_blob(&state, &caller, &org_id, sha256)
                .await?
                .status(),
            StatusCode::FORBIDDEN,
            "HEAD {sha256}"
        );
        let described = describe(&state, &caller, Some(&org_id), sha256).await?;
        assert_eq!(described.status(), StatusCode::FORBIDDEN, "POST {sha256}");
        assert_eq!(
            json_str(&body_json(described).await?, "error")?,
            OUT_OF_SCOPE
        );
    }

    assert_eq!(
        head_vault_blob(&state, &admin, &org_id, &absent)
            .await?
            .status(),
        StatusCode::NOT_FOUND,
        "no file kept"
    );
    let listing =
        body_json(signed_empty(&state, &admin, "GET", "/api/v1/vault/resources?owner=org").await?)
            .await?;
    assert_eq!(listing["items"], json!([]), "no record written");
    Ok(())
}

/// The listing is scoped to the caller's reach, and a stranger sees nothing.
#[tokio::test]
async fn a_listing_shows_only_what_the_caller_reaches() -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let me = operator(&state).await?;
    let (mine, _) = own_resource(&state, &me).await?;
    let stranger = operator(&state).await?;
    own_resource(&state, &stranger).await?;

    let ids = |body: &Value| -> anyhow::Result<Vec<String>> {
        Ok(body["items"]
            .as_array()
            .context("the listing must carry items")?
            .iter()
            .filter_map(|item| item["resource_id"].as_str())
            .map(str::to_string)
            .collect())
    };

    let all = body_json(signed_empty(&state, &me, "GET", "/api/v1/vault/resources").await?).await?;
    assert_eq!(ids(&all)?, vec![mine.clone()], "only the caller's own");

    // The filter narrows inside that reach: `mine` keeps it, `org` empties it.
    let filtered =
        body_json(signed_empty(&state, &me, "GET", "/api/v1/vault/resources?owner=mine").await?)
            .await?;
    assert_eq!(ids(&filtered)?, vec![mine]);
    let org_only =
        body_json(signed_empty(&state, &me, "GET", "/api/v1/vault/resources?owner=org").await?)
            .await?;
    assert!(
        ids(&org_only)?.is_empty(),
        "the caller owns nothing as an org"
    );
    Ok(())
}

/// The listing pages, and the reach is applied to every page rather than to the
/// first: a keyspace whose earlier records belong to somebody else still yields
/// a full page of the caller's own.
#[tokio::test]
async fn a_listing_pages_through_a_mixed_keyspace() -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let stranger = operator(&state).await?;
    own_resource(&state, &stranger).await?;

    let me = operator(&state).await?;
    let sha256 = upload_vault_blob(&state, &me, me.1.as_str(), BLOB).await?;
    let first = describe_gguf_text_ok(&state, &me, None, &sha256).await?;
    let second = describe_gguf_text_ok(&state, &me, None, &sha256).await?;

    let page =
        body_json(signed_empty(&state, &me, "GET", "/api/v1/vault/resources?limit=1").await?)
            .await?;
    assert_eq!(page["items"].as_array().map(Vec::len), Some(1));
    assert_eq!(page["items"][0]["resource_id"], first);

    let cursor = json_str(&page, "next_cursor")?;
    let rest = body_json(
        signed_empty(
            &state,
            &me,
            "GET",
            &format!("/api/v1/vault/resources?limit=10&cursor={cursor}"),
        )
        .await?,
    )
    .await?;
    let ids: Vec<&str> = rest["items"]
        .as_array()
        .context("the listing must carry items")?
        .iter()
        .filter_map(|item| item["resource_id"].as_str())
        .collect();
    assert_eq!(ids, vec![second.as_str()]);
    Ok(())
}

/// A resource outside the caller's scope answers the same `403`, in the same
/// words, on the read, the fetch and the delete, so the surfaces cannot be
/// played against each other. A server admin reaches all three.
#[rstest]
#[case::read("GET", "/vault/resources/{id}")]
#[case::fetch("GET", "/vault/resources/{id}/content")]
#[case::delete("DELETE", "/api/v1/vault/resources/{id}")]
#[tokio::test]
async fn a_stranger_reaches_no_resource_and_the_server_reaches_every_one(
    #[case] method: &str,
    #[case] path: &str,
) -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let me = operator(&state).await?;
    let (resource_id, _) = own_resource(&state, &me).await?;
    let uri = path.replace("{id}", &resource_id);

    let stranger = operator(&state).await?;
    let resp = signed_empty(&state, &stranger, method, &uri).await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        json_str(&body_json(resp).await?, "error")?,
        "this vault resource belongs to another owner"
    );

    let admin = server_admin(&state).await?;
    assert_ne!(
        signed_empty(&state, &admin, method, &uri).await?.status(),
        StatusCode::FORBIDDEN,
        "manage_any_orgs reaches another account's resource"
    );
    Ok(())
}

/// Who deletes a resource it does not own, in the matrix's [`World`].
#[derive(Debug, Clone, Copy)]
enum Deleter {
    AcmeAdmin,
    RivalAdmin,
    /// `manage_orgs` on no roster: a grant that administers nothing.
    ManageOrgsOnNoOrg,
    Server,
}

/// A delete by somebody other than the owner: acme's administrator takes acme's
/// record and the server takes anybody's, so the owner and its devices find it
/// gone. A `manage_orgs` holder off acme's roster is refused and the record stays,
/// and so is acme's administrator on a member's own record: deleting is not the
/// upload hand-off.
#[rstest]
#[case::acme_admin_on_acme(Deleter::AcmeAdmin, false, None)]
#[case::acme_admin_on_a_member_s(
    Deleter::AcmeAdmin,
    true,
    Some("this vault resource belongs to another owner")
)]
#[case::server_on_acme(Deleter::Server, false, None)]
#[case::server_on_a_member_s(Deleter::Server, true, None)]
#[case::rival_admin_on_acme(
    Deleter::RivalAdmin,
    false,
    Some("this vault resource belongs to another owner")
)]
#[case::manage_orgs_on_no_org_on_acme(
    Deleter::ManageOrgsOnNoOrg,
    false,
    Some("this vault resource belongs to another owner")
)]
#[tokio::test]
async fn an_administrator_deletes_only_within_its_reach(
    #[case] deleter: Deleter,
    #[case] personal: bool,
    #[case] refused: Option<&str>,
) -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let w = world(&state).await?;
    let caller = match deleter {
        Deleter::AcmeAdmin => w.acme_admin.clone(),
        Deleter::RivalAdmin => w.rival_admin.clone(),
        Deleter::ManageOrgsOnNoOrg => {
            registered_user(&state, &[UserPermission::ManageOrgs]).await?
        }
        Deleter::Server => w.server.clone(),
    };
    // The resource, an account that reads it, and the owner of a device that
    // fetches it.
    let (resource_id, reader, device_owner) = if personal {
        (&w.member_res, &w.member, user_owner(&w.member.1))
    } else {
        (&w.acme_res, &w.acme_admin, org_owner(&w.acme))
    };
    let (device_key, device_id) = owned_device(&state, &w.server, Some(device_owner)).await?;
    let uri = format!("/api/v1/vault/resources/{resource_id}");

    // The record says what the delete will, to a caller that reads it.
    let read = signed_empty(&state, &caller, "GET", &uri).await?;
    if read.status() == StatusCode::OK {
        let read = body_json(read).await?;
        assert_eq!(read["may_administer"], json!(refused.is_none()), "{read}");
    }
    let resp = signed_empty(&state, &caller, "DELETE", &uri).await?;
    let still_there = match refused {
        None => {
            assert_eq!(resp.status(), StatusCode::NO_CONTENT);
            let body = to_bytes(resp.into_body(), usize::MAX).await?;
            assert!(body.is_empty(), "a 204 carries no body: {body:?}");
            StatusCode::NOT_FOUND
        }
        Some(reason) => {
            assert_eq!(resp.status(), StatusCode::FORBIDDEN);
            assert_eq!(json_str(&body_json(resp).await?, "error")?, reason);
            StatusCode::OK
        }
    };
    assert_eq!(
        signed_empty(&state, reader, "GET", &uri).await?.status(),
        still_there,
        "the owner's side reads the record"
    );
    assert_eq!(
        authed_get(&state, &device_key, &device_id, &content_uri(resource_id))
            .await?
            .status(),
        still_there,
        "the owner's device fetches the bytes"
    );
    Ok(())
}

/// Who sets a vault model's parameter counts, in the matrix's [`World`].
#[derive(Debug, Clone, Copy)]
enum Setter {
    AcmeAdmin,
    RivalAdmin,
    AcmeMember,
    Server,
}

/// Which row a [`Setter`] writes.
#[derive(Debug, Clone, Copy)]
enum Row {
    AcmeFile,
    /// A personal file of an acme member's.
    MemberFile,
    /// A model name, which is every organization's.
    Name,
}

const NOT_YOURS: &str = "writing this row needs manage_any_orgs: its key is not a vault resource \
                         your organization owns";

async fn resource_read(
    state: &AppState,
    signer: &UserSigner,
    resource_id: &str,
) -> anyhow::Result<(StatusCode, Value)> {
    let uri = format!("/api/v1/vault/resources/{resource_id}");
    let read = signed_empty(state, signer, "GET", &uri).await?;
    Ok((read.status(), body_json(read).await?))
}

/// An organization's administrator sets the counts of a model its organization
/// owns and of nothing else; the server sets any. The record says so first.
#[rstest]
#[case::acme_admin_on_acme(Setter::AcmeAdmin, Row::AcmeFile, None)]
#[case::acme_admin_on_a_member_s(Setter::AcmeAdmin, Row::MemberFile, Some(NOT_YOURS))]
#[case::acme_admin_on_a_name(Setter::AcmeAdmin, Row::Name, Some(NOT_YOURS))]
#[case::rival_admin_on_acme(Setter::RivalAdmin, Row::AcmeFile, Some(NOT_YOURS))]
#[case::acme_member_on_acme(
    Setter::AcmeMember,
    Row::AcmeFile,
    Some("user lacks the manage_orgs permission")
)]
#[case::server_on_acme(Setter::Server, Row::AcmeFile, None)]
#[case::server_on_a_member_s(Setter::Server, Row::MemberFile, None)]
#[case::server_on_a_name(Setter::Server, Row::Name, None)]
#[tokio::test]
async fn an_administrator_sets_model_params_only_within_its_reach(
    #[case] setter: Setter,
    #[case] row: Row,
    #[case] refused: Option<&str>,
) -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let w = world(&state).await?;
    let caller = match setter {
        Setter::AcmeAdmin => &w.acme_admin,
        Setter::RivalAdmin => &w.rival_admin,
        Setter::AcmeMember => &w.member,
        Setter::Server => &w.server,
    };
    let resource_id = match row {
        Row::AcmeFile => Some(&w.acme_res),
        Row::MemberFile => Some(&w.member_res),
        Row::Name => None,
    };
    let key = resource_id.map_or("acmelm-4.5b-text", String::as_str);

    if let Some(resource_id) = resource_id {
        let (status, read) = resource_read(&state, caller, resource_id).await?;
        if status == StatusCode::OK {
            assert_eq!(read["model_params"]["model_key"], json!(key), "{read}");
            assert_eq!(read["model_params"]["row"], Value::Null);
            assert_eq!(read["model_params"]["may_set"], json!(refused.is_none()));
        }
    }
    let counts = json!({"total_millions": 4512, "note": "safetensors header at 93f895af"});
    let mut create = counts.clone();
    create["model_key"] = json!(key);
    let created = signed_json(&state, caller, "POST", "/api/v1/model-params", &create).await?;
    let row_uri = format!("/api/v1/model-params/{key}");
    match refused {
        None => {
            assert_eq!(created.status(), StatusCode::CREATED);
            if let Some(resource_id) = resource_id {
                let (_, read) = resource_read(&state, &w.server, resource_id).await?;
                assert_eq!(
                    read["model_params"]["row"]["total_millions"], 4512,
                    "{read}"
                );
            }
            let edited = signed_json(&state, caller, "PUT", &row_uri, &counts).await?;
            assert_eq!(edited.status(), StatusCode::OK);
            let edited = body_json(edited).await?;
            assert_eq!(
                edited["changed"], false,
                "the values it was created with: {edited}"
            );
            let deleted = signed_empty(&state, caller, "DELETE", &row_uri).await?;
            assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
        }
        Some(reason) => {
            assert_eq!(created.status(), StatusCode::FORBIDDEN);
            assert_eq!(json_str(&body_json(created).await?, "error")?, reason);
            // An edit and a delete of a row that is there are refused alike.
            let held = signed_json(&state, &w.server, "POST", "/api/v1/model-params", &create);
            assert_eq!(held.await?.status(), StatusCode::CREATED);
            let edited = signed_json(&state, caller, "PUT", &row_uri, &counts).await?;
            assert_eq!(edited.status(), StatusCode::FORBIDDEN);
            let deleted = signed_empty(&state, caller, "DELETE", &row_uri).await?;
            assert_eq!(deleted.status(), StatusCode::FORBIDDEN);
        }
    }
    Ok(())
}

/// The delete takes the record and, with no other record naming it, the file:
/// describing the bytes again takes uploading them again. What it revokes is
/// every device's access.
#[tokio::test]
async fn a_delete_revokes_access_and_takes_an_unshared_file() -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let admin = server_admin(&state).await?;
    let me = operator(&state).await?;
    let (resource_id, sha256) = own_resource(&state, &me).await?;
    let (device_key, device_id) = owned_device(&state, &admin, Some(user_owner(&me.1))).await?;

    assert_eq!(
        signed_empty(
            &state,
            &me,
            "DELETE",
            &format!("/api/v1/vault/resources/{resource_id}")
        )
        .await?
        .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        authed_get(&state, &device_key, &device_id, &content_uri(&resource_id))
            .await?
            .status(),
        StatusCode::NOT_FOUND,
        "the record is gone, so the id names nothing"
    );

    let resp = describe(&state, &me, None, &sha256).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "the file is gone");
    assert!(json_str(&body_json(resp).await?, "error")?.contains("upload it first"));
    upload_vault_blob(&state, &me, me.1.as_str(), BLOB).await?;
    assert_eq!(
        describe(&state, &me, None, &sha256).await?.status(),
        StatusCode::CREATED
    );
    Ok(())
}

/// An upload's files make one plan entry, so deleting one deletes the rest:
/// no surviving record names a file its entry can no longer fetch.
#[tokio::test]
async fn deleting_one_file_deletes_its_upload() -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let me = operator(&state).await?;
    let (_, model_id, mmproj_id) = vision_upload(&state, &me).await?;

    let uri = format!("/api/v1/vault/resources/{model_id}");
    assert_eq!(
        signed_empty(&state, &me, "DELETE", &uri).await?.status(),
        StatusCode::NO_CONTENT
    );
    let uri = format!("/vault/resources/{mmproj_id}");
    assert_eq!(
        signed_empty(&state, &me, "GET", &uri).await?.status(),
        StatusCode::NOT_FOUND,
        "the projector goes with its model"
    );
    let listing =
        body_json(signed_empty(&state, &me, "GET", "/api/v1/vault/resources").await?).await?;
    assert_eq!(listing["items"], json!([]));
    Ok(())
}

// ── delivery to a device ────────────────────────────────────────────────────

/// A device's own uploaded artifact and the device; the path is for the tests
/// that tamper with the file.
async fn device_and_artifact(
    state: &AppState,
    dir: &Path,
    bytes: &[u8],
) -> anyhow::Result<(ed25519_dalek::SigningKey, ClientId, String, PathBuf)> {
    let admin = server_admin(state).await?;
    let me = operator(state).await?;
    let sha256 = upload_vault_blob(state, &me, me.1.as_str(), bytes).await?;
    let resource_id = describe_gguf_text_ok(state, &me, None, &sha256).await?;
    let (device_key, device) = owned_device(state, &admin, Some(user_owner(&me.1))).await?;
    Ok((
        device_key,
        device,
        resource_id,
        blob_path(dir, me.1.as_str(), &sha256),
    ))
}

/// The bytes, `Content-Length` and body hash are the object's and the `ETag`
/// the record's pin. The object spans several store read chunks, so they must
/// reassemble in order.
#[tokio::test]
async fn a_device_receives_the_bytes_intact_with_the_pin() -> anyhow::Result<()> {
    // 1 MiB + 1: past four 256 KiB chunks with a partial fifth, and not a
    // repeating pattern, so a chunk served out of order changes the hash. A
    // gguf by its first bytes, so a `gguf_text` upload takes it.
    let big: Vec<u8> = b"GGUF"
        .iter()
        .copied()
        .chain((0..(1024 * 1024 + 1) as u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8))
        .collect();
    let (dir, state) = vault_state().await?;
    let (device_key, device, resource_id, _) =
        device_and_artifact(&state, dir.path(), &big).await?;

    let resp = authed_get(&state, &device_key, &device, &content_uri(&resource_id)).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let headers = resp.headers().clone();
    let content_length: usize = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .context("Content-Length must be set")?
        .parse()?;
    let etag = headers
        .get(header::ETAG)
        .and_then(|v| v.to_str().ok())
        .context("ETag must be set")?
        .to_string();
    assert_eq!(
        headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/octet-stream")
    );
    assert_eq!(
        headers
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok()),
        Some("private, no-store"),
        "private bytes must not land in a shared cache"
    );

    let body = to_bytes(resp.into_body(), usize::MAX).await?;
    assert_eq!(body.len(), content_length, "the body is as long as it said");
    assert_eq!(body.len(), big.len());
    assert_eq!(body.as_ref(), big.as_slice(), "byte for byte, in order");
    assert_eq!(
        etag,
        format!("\"{}\"", sha256_hex(&big)),
        "the ETag is the pinned digest, which the body hashes to"
    );
    Ok(())
}

/// Something replaces the file behind the record: the length and body are the
/// new object's but the `ETag` is still the pin, so a verifying client can
/// tell.
#[tokio::test]
async fn a_replaced_object_no_longer_hashes_to_the_pin() -> anyhow::Result<()> {
    const REPLACED: &[u8] = b"different weights, quietly swapped in";

    let (dir, state) = vault_state().await?;
    let (device_key, device, resource_id, blob) =
        device_and_artifact(&state, dir.path(), BLOB).await?;

    std::fs::write(blob, REPLACED)?;

    let resp = authed_get(&state, &device_key, &device, &content_uri(&resource_id)).await?;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "the record still names an object"
    );
    let headers = resp.headers().clone();
    assert_eq!(
        headers
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok()),
        Some(REPLACED.len().to_string().as_str()),
        "the length is the object's as it is now"
    );
    let etag = headers
        .get(header::ETAG)
        .and_then(|v| v.to_str().ok())
        .context("ETag must be set")?
        .to_string();
    assert_eq!(
        etag,
        format!("\"{}\"", sha256_hex(BLOB)),
        "the pin is the record's"
    );

    let body = to_bytes(resp.into_body(), usize::MAX).await?;
    assert_eq!(body.as_ref(), REPLACED);
    assert_ne!(
        format!("\"{}\"", sha256_hex(&body)),
        etag,
        "the body must not hash to the pin, which is what the client checks"
    );

    // The record read says the same thing a step earlier: its `bytes` is the
    // size at upload, so a client comparing the two sees the drift
    // before it downloads anything.
    let record = body_json(
        authed_get(
            &state,
            &device_key,
            &device,
            &format!("/vault/resources/{resource_id}"),
        )
        .await?,
    )
    .await?;
    assert_eq!(record["bytes"], BLOB.len());
    assert_eq!(record["sha256"], sha256_hex(BLOB));
    Ok(())
}

/// A device fetching a record whose file went missing gets a `404`,
/// not a `500` and not a truncated body: the refusal is decided before the
/// first byte. A request for a range gets the same `404`.
#[rstest]
#[tokio::test]
async fn a_device_fetching_a_removed_object_is_told_404(
    #[values(None, Some("bytes=4-"))] range: Option<&str>,
) -> anyhow::Result<()> {
    let (dir, state) = vault_state().await?;
    let (device_key, device, resource_id, blob) =
        device_and_artifact(&state, dir.path(), BLOB).await?;
    std::fs::remove_file(blob)?;

    let uri = content_uri(&resource_id);
    let mut headers = auth_headers(&device_key, &device, "GET", &uri)?;
    headers.extend(range.map(|range| ("range", range.to_string())));
    let resp = authed_get_with_headers(&state, &uri, &headers).await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert!(
        resp.headers().get(header::ETAG).is_none(),
        "no pin is offered for bytes that are not served"
    );
    assert!(resp.headers().get(header::CONTENT_RANGE).is_none());
    Ok(())
}

/// The signature covers the path, so a signature minted for one resource does
/// not fetch another: a device that owns both cannot be replayed from one to
/// the other by whoever captured the headers.
#[tokio::test]
async fn a_signature_for_one_resource_does_not_fetch_another() -> anyhow::Result<()> {
    let (dir, state) = vault_state().await?;
    let (device_key, device, first, _) = device_and_artifact(&state, dir.path(), BLOB).await?;
    // A second artifact under the same device's owner.
    let me = registered_user(&state, &[]).await?;
    let second = {
        let sha256 = upload_vault_blob(&state, &me, me.1.as_str(), BLOB).await?;
        describe_gguf_text_ok(&state, &me, None, &sha256).await?
    };

    let headers = auth_headers(&device_key, &device, "GET", &content_uri(&first))?;
    assert_eq!(
        authed_get_with_headers(&state, &content_uri(&second), &headers)
            .await?
            .status(),
        StatusCode::UNAUTHORIZED
    );
    Ok(())
}

/// A malformed id on the content path is a `400` at the boundary, for a device
/// as for an account.
#[tokio::test]
async fn a_device_naming_a_foreign_id_is_told_400() -> anyhow::Result<()> {
    let (dir, state) = vault_state().await?;
    let (device_key, device, _, _) = device_and_artifact(&state, dir.path(), BLOB).await?;
    assert_eq!(
        authed_get(&state, &device_key, &device, &content_uri("plan-0195c3e2"))
            .await?
            .status(),
        StatusCode::BAD_REQUEST
    );
    Ok(())
}

/// What the content route sends back for a range request.
#[derive(Debug, Clone, Copy)]
enum Served {
    /// `200` and the whole object.
    Whole,
    /// `206` and the bytes from this offset to the end.
    From(usize),
    /// `416`, which names the object's size.
    PastEnd,
}

/// The content route resumes a fetch from `Range: bytes=N-`, under an
/// `If-Range` that is absent or the pin. It sends the whole object for a stale
/// or weak validator and for every other range form. It refuses a range at or
/// past the end with the size, so the client can start again from zero.
#[rstest]
#[case::open_ended("bytes=4-".into(), None, Served::From(4))]
#[case::under_the_pin("bytes=4-".into(), Some(format!("\"{}\"", sha256_hex(BLOB))), Served::From(4))]
#[case::from_zero("bytes=0-".into(), None, Served::From(0))]
#[case::the_last_byte(format!("bytes={}-", BLOB.len() - 1), None, Served::From(BLOB.len() - 1))]
#[case::another_validator("bytes=4-".into(), Some("\"another\"".into()), Served::Whole)]
#[case::weak_pin("bytes=4-".into(), Some(format!("W/\"{}\"", sha256_hex(BLOB))), Served::Whole)]
#[case::at_the_end(format!("bytes={}-", BLOB.len()), None, Served::PastEnd)]
#[case::bounded("bytes=0-3".into(), None, Served::Whole)]
#[case::suffix("bytes=-4".into(), None, Served::Whole)]
#[case::several("bytes=0-1,4-".into(), None, Served::Whole)]
#[case::other_unit("items=4-".into(), None, Served::Whole)]
#[tokio::test]
async fn a_range_request_is_answered_by_its_validator(
    #[case] range: String,
    #[case] if_range: Option<String>,
    #[case] served: Served,
) -> anyhow::Result<()> {
    let (dir, state) = vault_state().await?;
    let (device_key, device, resource_id, _) =
        device_and_artifact(&state, dir.path(), BLOB).await?;

    let mut headers = auth_headers(&device_key, &device, "GET", &content_uri(&resource_id))?;
    headers.push(("range", range));
    headers.extend(if_range.map(|tag| ("if-range", tag)));
    let resp = authed_get_with_headers(&state, &content_uri(&resource_id), &headers).await?;
    let answered = resp.headers().clone();
    let header = |name| {
        answered
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    let len = BLOB.len();
    let (status, content_range, from) = match served {
        Served::Whole => (StatusCode::OK, None, Some(0)),
        Served::From(from) => (
            StatusCode::PARTIAL_CONTENT,
            Some(format!("bytes {from}-{}/{len}", len - 1)),
            Some(from),
        ),
        Served::PastEnd => (
            StatusCode::RANGE_NOT_SATISFIABLE,
            Some(format!("bytes */{len}")),
            None,
        ),
    };
    assert_eq!(resp.status(), status);
    assert_eq!(header(header::CONTENT_RANGE), content_range);
    assert_eq!(header(header::ACCEPT_RANGES).as_deref(), Some("bytes"));
    if let Some(from) = from {
        assert_eq!(
            header(header::CONTENT_LENGTH),
            Some((len - from).to_string())
        );
        assert_eq!(
            header(header::ETAG),
            Some(format!("\"{}\"", sha256_hex(BLOB))),
            "a part carries the pin of the whole"
        );
        assert_eq!(
            header(header::CACHE_CONTROL).as_deref(),
            Some("private, no-store")
        );
        let body = to_bytes(resp.into_body(), usize::MAX).await?;
        assert_eq!(body.as_ref(), &BLOB[from..]);
    }
    Ok(())
}

// ── the rest of the HTTP contract ───────────────────────────────────────────

/// The index is an operator's surface: a device signature reaches none of the
/// writes and not the listing, refused at the header before any body is read.
#[rstest]
#[case::describe("POST", "/api/v1/vault/resources")]
#[case::list("GET", "/api/v1/vault/resources")]
#[case::types("GET", "/api/v1/vault/types")]
#[case::delete("DELETE", "/api/v1/vault/resources/{id}")]
#[case::upload_file("PUT", "/api/v1/vault/blobs/{me}/{sha256}")]
#[case::probe_file("HEAD", "/api/v1/vault/blobs/{me}/{sha256}")]
#[tokio::test]
async fn a_device_reaches_no_index_route(
    #[case] method: &str,
    #[case] path: &str,
) -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let admin = server_admin(&state).await?;
    let me = operator(&state).await?;
    let (resource_id, sha256) = own_resource(&state, &me).await?;
    let (device_key, device) = owned_device(&state, &admin, Some(user_owner(&me.1))).await?;

    let uri = path
        .replace("{id}", &resource_id)
        .replace("{me}", me.1.as_str())
        .replace("{sha256}", &sha256);
    let headers = auth_headers(&device_key, &device, method, &uri)?;
    let resp = match method {
        "POST" => {
            let body = gguf_text_upload(None, &sha256);
            json_request(&state, method, &uri, &headers, &body).await?
        }
        "PUT" => raw_request(&state, method, &uri, &headers, None, BLOB.to_vec()).await?,
        _ => request_with_headers(&state, method, &uri, &headers).await?,
    };
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    Ok(())
}

/// A well-formed id that names nothing is a `404` on every route, for either
/// principal. Distinct from the `400` a malformed id gets and the `403` a
/// stranger's resource gets: three answers, each true.
#[tokio::test]
async fn an_unknown_id_is_a_404_everywhere() -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let admin = server_admin(&state).await?;
    let me = operator(&state).await?;
    let (_, _) = own_resource(&state, &me).await?;
    let (device_key, device) = owned_device(&state, &admin, Some(user_owner(&me.1))).await?;
    let absent = "vault_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e";

    for uri in [format!("/vault/resources/{absent}"), content_uri(absent)] {
        assert_eq!(
            signed_empty(&state, &me, "GET", &uri).await?.status(),
            StatusCode::NOT_FOUND,
            "account on {uri}"
        );
        assert_eq!(
            authed_get(&state, &device_key, &device, &uri)
                .await?
                .status(),
            StatusCode::NOT_FOUND,
            "device on {uri}"
        );
    }
    assert_eq!(
        signed_empty(
            &state,
            &me,
            "DELETE",
            &format!("/api/v1/vault/resources/{absent}")
        )
        .await?
        .status(),
        StatusCode::NOT_FOUND
    );
    Ok(())
}

/// A second delete is a `404`, not a silent success: the route reads before it
/// deletes, so a retry after a lost response is told the record is already gone
/// rather than told nothing.
#[tokio::test]
async fn a_second_delete_is_a_404() -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let me = operator(&state).await?;
    let (resource_id, _) = own_resource(&state, &me).await?;
    let uri = format!("/api/v1/vault/resources/{resource_id}");
    assert_eq!(
        signed_empty(&state, &me, "DELETE", &uri).await?.status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        signed_empty(&state, &me, "DELETE", &uri).await?.status(),
        StatusCode::NOT_FOUND
    );
    Ok(())
}

/// The listing's query is validated the way every cursor-paginated listing's
/// is, and an empty filter value reads as no filter.
#[rstest]
#[case::zero_limit("limit=0", StatusCode::BAD_REQUEST)]
#[case::over_max("limit=201", StatusCode::BAD_REQUEST)]
#[case::bad_cursor("cursor=not-a-resource-id", StatusCode::BAD_REQUEST)]
#[case::bad_owner("owner=everyone", StatusCode::BAD_REQUEST)]
#[case::bad_org("org_id=acme", StatusCode::BAD_REQUEST)]
#[case::empty_owner("owner=", StatusCode::OK)]
#[case::empty_limit("limit=", StatusCode::OK)]
#[case::newest("order=newest", StatusCode::OK)]
#[case::bad_order("order=bogus", StatusCode::BAD_REQUEST)]
#[case::bad_cursor_newest("order=newest&cursor=not-a-resource-id", StatusCode::BAD_REQUEST)]
#[case::bad_names("names=maybe", StatusCode::BAD_REQUEST)]
#[case::bad_object("object=1", StatusCode::BAD_REQUEST)]
#[case::empty_names("names=", StatusCode::OK)]
#[case::empty_object("object=", StatusCode::OK)]
#[tokio::test]
async fn a_listing_query_is_validated(
    #[case] query: &str,
    #[case] expected: StatusCode,
) -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let me = operator(&state).await?;
    assert_eq!(
        signed_empty(
            &state,
            &me,
            "GET",
            &format!("/api/v1/vault/resources?{query}")
        )
        .await?
        .status(),
        expected
    );
    Ok(())
}

/// The listing carries the readable rows and counts the other, so an operator
/// learns of a bad object without the surface going dark.
#[tokio::test]
async fn an_unreadable_record_is_counted_not_fatal() -> anyhow::Result<()> {
    let (dir, state) = vault_state().await?;
    let me = operator(&state).await?;
    let (mine, _) = own_resource(&state, &me).await?;
    std::fs::write(
        dir.path()
            .join("vault")
            .join("vault_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e.json"),
        b"{ this is not a record",
    )?;

    let body =
        body_json(signed_empty(&state, &me, "GET", "/api/v1/vault/resources").await?).await?;
    let ids: Vec<&str> = body["items"]
        .as_array()
        .context("the listing must carry items")?
        .iter()
        .filter_map(|item| item["resource_id"].as_str())
        .collect();
    assert_eq!(ids, vec![mine.as_str()]);
    assert_eq!(body["unreadable"], 1);
    Ok(())
}

/// An administrator's listing narrows by organization: its own, another it
/// does not administer (empty rather than refused), and `owner=org` alone.
#[tokio::test]
async fn an_administrator_narrows_the_listing_by_organization() -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let w = world(&state).await?;
    let ids = async |query: &str| -> anyhow::Result<Vec<String>> {
        let body = body_json(
            signed_empty(
                &state,
                &w.acme_admin,
                "GET",
                &format!("/api/v1/vault/resources?{query}"),
            )
            .await?,
        )
        .await?;
        Ok(body["items"]
            .as_array()
            .context("the listing must carry items")?
            .iter()
            .filter_map(|item| item["resource_id"].as_str())
            .map(str::to_string)
            .collect())
    };

    assert_eq!(ids("owner=org").await?, vec![w.acme_res.clone()]);
    assert_eq!(
        ids(&format!("org_id={}", w.acme)).await?,
        vec![w.acme_res.clone()]
    );
    assert!(
        ids(&format!("org_id={}", w.rival)).await?.is_empty(),
        "an organization outside the reach narrows to nothing rather than refusing"
    );
    Ok(())
}

/// A record's origin reaches both the record route and the listing, in its
/// stored shape, and both name the HuggingFace model its results are recorded
/// under.
#[tokio::test]
async fn an_origin_reaches_every_answer_for_its_record() -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let me = operator(&state).await?;
    let (resource_id, _) = own_resource(&state, &me).await?;
    let origin = a_huggingface_origin()?;
    let id: VaultResourceId = resource_id.parse()?;
    let mut resource = state
        .vault_store
        .get_resource(&id)
        .await?
        .context("the upload wrote a record")?;
    resource.origin = Some(origin.clone());
    state.vault_store.put_resource(&resource).await?;
    let origin = serde_json::to_value(origin)?;

    let record = body_json(
        signed_empty(
            &state,
            &me,
            "GET",
            &format!("/vault/resources/{resource_id}"),
        )
        .await?,
    )
    .await?;
    assert_eq!(record["origin"], origin);
    let alias = json!({
        "type": "gguf_text",
        "source": "huggingface",
        "org": "LiquidAI",
        "repo_name": "LFM2-700M-GGUF",
        "revision": "a".repeat(40),
        "path": "LFM2-700M-Q4_0.gguf",
    });
    assert_eq!(record["alias"], alias, "{record}");

    let listing =
        body_json(signed_empty(&state, &me, "GET", "/api/v1/vault/resources").await?).await?;
    let item = listing["items"]
        .as_array()
        .context("the listing must carry items")?
        .iter()
        .find(|item| item["resource_id"] == resource_id.as_str())
        .context("the listing names the record")?;
    assert_eq!(item["origin"], origin);
    assert_eq!(item["alias"], alias);
    crate::api_schema::assert_conforms("VaultResourceDetail", item)?;
    Ok(())
}

/// No `object_key`: where a file is stored is not the caller's business. No
/// `updated_at` on a record that is never updated.
#[tokio::test]
async fn the_record_wire_shape_is_pinned() -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let me = operator(&state).await?;
    let (resource_id, sha256) = own_resource(&state, &me).await?;

    let body = body_json(
        signed_empty(
            &state,
            &me,
            "GET",
            &format!("/vault/resources/{resource_id}"),
        )
        .await?,
    )
    .await?;
    let mut fields: Vec<&str> = body
        .as_object()
        .context("a record is an object")?
        .keys()
        .map(String::as_str)
        .collect();
    fields.sort_unstable();
    assert_eq!(
        fields,
        [
            "artifact",
            "bytes",
            "created_at",
            "entry",
            "may_administer",
            "model_params",
            "name",
            "owner",
            "resource_id",
            "sha256",
            "toml"
        ]
    );
    assert_eq!(body["resource_id"], resource_id);
    assert_eq!(body["owner"], json!({ "type": "user", "user_id": me.1 }));
    assert_eq!(body["sha256"], sha256);
    assert_eq!(body["bytes"], BLOB.len());
    assert_eq!(
        body["artifact"],
        json!({
            "type": "gguf_text",
            "model": {
                "resource_id": resource_id,
                "filename": "acme-checkpoint-Q4_K_M.gguf",
                "sha256": sha256,
            },
        })
    );
    Ok(())
}

/// The listing's wire shape, pinned: the standard envelope with a null cursor on
/// the last page, and each item the record with its `entry` and `toml`.
#[tokio::test]
async fn the_listing_wire_shape_is_pinned() -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let admin = server_admin(&state).await?;
    let org_id = create_org(&state, &admin, "acme").await?;
    let team_admin = org_admin(&state, &org_id).await?;
    let resource_id = upload_gguf_text(&state, &team_admin, Some(&org_id), BLOB).await?;

    let resp = signed_empty(&state, &team_admin, "GET", "/api/v1/vault/resources").await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let listing = body_json(resp).await?;
    assert_eq!(fields(&listing)?, ["items", "next_cursor", "unreadable"]);
    assert_eq!(listing["next_cursor"], Value::Null, "the last page");
    assert_eq!(listing["unreadable"], 0);
    let [item] = listing["items"]
        .as_array()
        .context("the listing must carry items")?
        .as_slice()
    else {
        anyhow::bail!("one resource in reach: {listing}");
    };
    assert_eq!(
        fields(item)?,
        [
            "artifact",
            "bytes",
            "created_at",
            "entry",
            "may_administer",
            "model_params",
            "name",
            "owner",
            "resource_id",
            "sha256",
            "toml"
        ]
    );
    assert_eq!(item["owner"], org_owner(&org_id));

    let record = body_json(
        signed_empty(
            &state,
            &team_admin,
            "GET",
            &format!("/vault/resources/{resource_id}"),
        )
        .await?,
    )
    .await?;
    assert_eq!(*item, record, "an item is the record");
    Ok(())
}

/// A record written before uploads went through mgmt has no artifact. It is
/// still served, without `artifact`, `entry` and `toml`, its bytes still
/// arrive, and its delete takes that record alone.
#[tokio::test]
async fn a_record_from_before_uploads_is_served_without_an_entry() -> anyhow::Result<()> {
    let (dir, state) = vault_state().await?;
    let me = operator(&state).await?;
    let (legacy, _) = own_resource(&state, &me).await?;
    let (other, _) = own_resource(&state, &me).await?;
    let stored_at = dir.path().join("vault").join(format!("{legacy}.json"));
    let mut stored: Value = serde_json::from_slice(&std::fs::read(&stored_at)?)?;
    stored
        .as_object_mut()
        .context("a record is an object")?
        .remove("artifact")
        .context("an uploaded record names its artifact")?;
    std::fs::write(&stored_at, serde_json::to_vec(&stored)?)?;

    let uri = format!("/api/v1/vault/resources/{legacy}");
    let resp = signed_empty(&state, &me, "GET", &uri).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let record = body_json(resp).await?;
    assert_eq!(
        fields(&record)?,
        [
            "bytes",
            "created_at",
            "may_administer",
            "model_params",
            "name",
            "owner",
            "resource_id",
            "sha256"
        ]
    );

    let listing =
        body_json(signed_empty(&state, &me, "GET", "/api/v1/vault/resources").await?).await?;
    let item = listing["items"]
        .as_array()
        .context("the listing must carry items")?
        .iter()
        .find(|item| item["resource_id"] == legacy.as_str())
        .context("the legacy record is listed")?;
    assert_eq!(*item, record, "listed as read");

    let resp = signed_empty(&state, &me, "GET", &content_uri(&legacy)).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(to_bytes(resp.into_body(), usize::MAX).await?.as_ref(), BLOB);

    assert_eq!(
        signed_empty(&state, &me, "DELETE", &uri).await?.status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        signed_empty(&state, &me, "GET", &uri).await?.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        signed_empty(&state, &me, "GET", &format!("/vault/resources/{other}"))
            .await?
            .status(),
        StatusCode::OK,
        "the delete takes that record alone"
    );
    Ok(())
}

/// The description is under the router-wide ceiling, so a name padded past it
/// is a `413` before the body is parsed, not a `400` about the name.
#[tokio::test]
async fn an_oversized_description_is_a_413() -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let me = operator(&state).await?;
    let sha256 = upload_vault_blob(&state, &me, me.1.as_str(), BLOB).await?;
    let mut body = gguf_text_upload(None, &sha256);
    body["name"] = json!("n".repeat(65 * 1024));
    assert_eq!(
        describe_vault_upload(&state, &me, &body).await?.status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    Ok(())
}

/// The file is uploaded and the rest valid, so only the name can refuse it. A
/// refused name writes no record; an accepted one is stored trimmed. `Err`
/// carries what the refusal says.
#[rstest]
#[case::blank("   ".to_string(), Err("name: must be a non-empty string"))]
#[case::past_the_ceiling("n".repeat(4097), Err("name must not exceed 4096 characters"))]
#[case::at_the_ceiling("n".repeat(4096), Ok("n".repeat(4096)))]
#[case::counted_in_characters("é".repeat(4096), Ok("é".repeat(4096)))]
#[case::padded("  padded  ".to_string(), Ok("padded".to_string()))]
// 4098 characters as sent, 4096 once trimmed: counted after the trim.
#[case::padded_at_the_ceiling(format!(" {} ", "n".repeat(4096)), Ok("n".repeat(4096)))]
#[tokio::test]
async fn a_name_is_trimmed_and_held_to_its_ceiling(
    #[case] name: String,
    #[case] outcome: Result<String, &str>,
) -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let me = operator(&state).await?;
    let sha256 = upload_vault_blob(&state, &me, me.1.as_str(), BLOB).await?;
    let mut body = gguf_text_upload(None, &sha256);
    body["name"] = json!(name);

    let resp = describe_vault_upload(&state, &me, &body).await?;
    let stored = match outcome {
        Ok(stored) => stored,
        Err(reason) => {
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
            let error = body_json(resp).await?;
            let error = json_str(&error, "error")?;
            assert!(error.contains(reason), "{error}");
            let listing =
                body_json(signed_empty(&state, &me, "GET", "/api/v1/vault/resources").await?)
                    .await?;
            assert_eq!(listing["items"], json!([]), "a refusal writes no record");
            return Ok(());
        }
    };
    assert_eq!(resp.status(), StatusCode::CREATED);
    let answer = body_json(resp).await?;
    assert_eq!(answer["resources"][0]["name"], json!(stored));
    let resource_id = json_str(&answer["resources"][0], "resource_id")?;
    let record = body_json(
        signed_empty(
            &state,
            &me,
            "GET",
            &format!("/vault/resources/{resource_id}"),
        )
        .await?,
    )
    .await?;
    assert_eq!(record["name"], json!(stored), "stored as trimmed");
    Ok(())
}

/// A file is not held to that ceiling: its body streams into the store rather
/// than being collected, so it may be far larger than any JSON body.
#[tokio::test]
async fn a_file_is_not_held_to_the_body_ceiling() -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let me = operator(&state).await?;
    let big: Vec<u8> = b"GGUF"
        .iter()
        .copied()
        .chain(std::iter::repeat_n(7, 256 * 1024))
        .collect();
    let sha256 = sha256_hex(&big);
    let resp = put_vault_blob(&state, &me, me.1.as_str(), &sha256, &big).await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(body_json(resp).await?["bytes"], big.len());
    Ok(())
}

/// It is held to `vault_blob_limit_bytes` instead: a file declaring more is a
/// `413` and nothing is kept, and one at the limit is stored.
#[tokio::test]
async fn a_file_past_the_vault_limit_is_a_413() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let limit = std::num::NonZeroU64::new(BLOB.len() as u64).context("BLOB is not empty")?;
    setup_benchmarks(dir.path())?;
    let state = make_state_with_vault_blob_limit(dir.path(), limit).await?;
    let me = operator(&state).await?;
    let principal = me.1.to_string();

    let over: Vec<u8> = BLOB.iter().copied().chain([0]).collect();
    let resp = put_vault_blob(&state, &me, &principal, &sha256_hex(&over), &over).await?;
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        head_vault_blob(&state, &me, &principal, &sha256_hex(&over))
            .await?
            .status(),
        StatusCode::NOT_FOUND,
        "nothing is kept"
    );

    assert_eq!(
        put_vault_blob(&state, &me, &principal, &sha256_hex(BLOB), BLOB)
            .await?
            .status(),
        StatusCode::CREATED
    );
    Ok(())
}

/// `PUT` `chunks` as one file for `principal`, streamed with no declared length,
/// so only the running count can hold it to the limit.
async fn put_streamed(
    state: &AppState,
    signer: &UserSigner,
    principal: &str,
    sha256: &str,
    chunks: &[&[u8]],
) -> anyhow::Result<axum::http::Response<axum::body::Body>> {
    let uri = blob_uri(principal, sha256);
    let headers = user_auth_headers(&signer.0, &signer.1, &signer.2, "PUT", &uri)?;
    let chunks: Vec<Result<axum::body::Bytes, std::io::Error>> = chunks
        .iter()
        .map(|chunk| Ok(axum::body::Bytes::copy_from_slice(chunk)))
        .collect();
    let body = axum::body::Body::from_stream(futures::stream::iter(chunks));
    anyhow::ensure!(
        axum::body::HttpBody::size_hint(&body).lower() == 0,
        "a streamed body declares no length"
    );
    raw_request(
        state,
        "PUT",
        &uri,
        &headers,
        Some("application/octet-stream"),
        body,
    )
    .await
}

/// A file that declares no length is held to `vault_blob_limit_bytes` as it
/// streams: one that runs past it is a `413` and nothing is kept, one that stops
/// at it is stored. Handing the store no limit, or the wrong one, fails here.
#[tokio::test]
async fn a_streamed_file_past_the_vault_limit_is_a_413() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let limit = std::num::NonZeroU64::new(BLOB.len() as u64).context("BLOB is not empty")?;
    setup_benchmarks(dir.path())?;
    let state = make_state_with_vault_blob_limit(dir.path(), limit).await?;
    let me = operator(&state).await?;
    let principal = me.1.to_string();

    // The whole limit in the first chunk, the byte past it in the second.
    let over: Vec<u8> = BLOB.iter().copied().chain([0]).collect();
    let resp = put_streamed(&state, &me, &principal, &sha256_hex(&over), &[BLOB, &[0]]).await?;
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        json_str(&body_json(resp).await?, "error")?,
        format!("the file is over the {limit}-byte limit")
    );
    assert_eq!(
        head_vault_blob(&state, &me, &principal, &sha256_hex(&over))
            .await?
            .status(),
        StatusCode::NOT_FOUND,
        "nothing is kept"
    );

    let (head, tail) = BLOB.split_at(4);
    let resp = put_streamed(&state, &me, &principal, &sha256_hex(BLOB), &[head, tail]).await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(
        body_json(resp).await?,
        json!({"sha256": sha256_hex(BLOB), "bytes": BLOB.len()})
    );
    Ok(())
}

/// `PUT` half of [`BLOB`] as `principal`'s file over a real connection,
/// declaring all of it, then close the sending side or, with `hang`, leave it
/// open past the server's one-second body timeout. Answers the status line.
async fn put_cut_short(
    state: &AppState,
    signer: &UserSigner,
    principal: &str,
    hang: bool,
) -> anyhow::Result<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let limits = pipette_httpapi::Limits {
        read: std::time::Duration::from_secs(1),
        drain: std::time::Duration::from_secs(1),
        connections: std::num::NonZeroUsize::MIN,
    };
    let app = test_app(state.clone());
    tokio::spawn(pipette_httpapi::serve(
        listener,
        app,
        std::future::pending(),
        limits,
    ));

    let uri = blob_uri(principal, &sha256_hex(BLOB));
    let headers: String = user_auth_headers(&signer.0, &signer.1, &signer.2, "PUT", &uri)?
        .iter()
        .map(|(name, value)| format!("{name}: {value}\r\n"))
        .collect();
    let head = format!(
        "PUT {uri} HTTP/1.1\r\nhost: x\r\ncontent-type: application/octet-stream\r\n\
         content-length: {}\r\n{headers}\r\n",
        BLOB.len()
    );
    let mut stream = tokio::net::TcpStream::connect(addr).await?;
    stream.write_all(head.as_bytes()).await?;
    let (half, _) = BLOB.split_at(BLOB.len() / 2);
    stream.write_all(half).await?;
    if !hang {
        stream.shutdown().await?;
    }
    let mut answer = vec![0; 1024];
    let read = tokio::time::timeout(std::time::Duration::from_secs(10), stream.read(&mut answer))
        .await??;
    answer.truncate(read);
    Ok(String::from_utf8_lossy(&answer)
        .lines()
        .next()
        .unwrap_or_default()
        .to_string())
}

/// A body that stops short of its declared length is the caller's: `400` when
/// the client closes its connection, `408` when it goes idle, as on the
/// buffered routes. Either keeps nothing. A store that fails after reading the
/// whole body is still a `500`.
#[rstest]
#[case::closed(false, "HTTP/1.1 400 Bad Request")]
#[case::idle(true, "HTTP/1.1 408 Request Timeout")]
#[tokio::test]
async fn a_file_cut_short_is_the_caller_s_and_a_store_fault_a_500(
    #[case] hang: bool,
    #[case] expected: &str,
) -> anyhow::Result<()> {
    let (dir, state) = vault_state().await?;
    let me = operator(&state).await?;
    let principal = me.1.to_string();
    let sha256 = sha256_hex(BLOB);

    let got = put_cut_short(&state, &me, &principal, hang).await?;
    assert_eq!(got, expected);
    assert_eq!(
        head_vault_blob(&state, &me, &principal, &sha256)
            .await?
            .status(),
        StatusCode::NOT_FOUND,
        "nothing is kept"
    );

    // A directory at the file's path reads as absent but refuses the write.
    std::fs::create_dir_all(blob_path(dir.path(), &principal, &sha256))?;
    let resp = put_vault_blob(&state, &me, &principal, &sha256, BLOB).await?;
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    Ok(())
}

/// Filing a revision into a campaign is the same ingestion as creating the
/// campaign, so it runs the same vault check: a bad reference is refused, and
/// the campaign stays on what it was running.
#[tokio::test]
async fn a_campaign_revision_naming_the_vault_is_checked_too() -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let admin = server_admin(&state).await?;
    let submitter = registered_user(&state, &[UserPermission::SubmitPlans]).await?;
    let (_, device) = owned_device(&state, &admin, None).await?;

    let (campaign_id, first) =
        created_campaign(&state, &submitter, &[("name", "vault campaign")]).await?;

    let model = format!(
        "{{ type = \"gguf_text\", source = \"vault_url\", url = \"{}\", \
         principal_id = \"org_0195c3e2\" }}",
        content_uri("vault_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e"),
    );
    let resp = post_plan_document(
        &state,
        &submitter,
        &format!("/api/v1/campaigns/{campaign_id}/plans"),
        &model_document(&device, &model),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let campaign = body_json(
        signed_empty(
            &state,
            &submitter,
            "GET",
            &format!("/api/v1/campaigns/{campaign_id}"),
        )
        .await?,
    )
    .await?;
    assert_eq!(
        campaign["current_plan_id"],
        json!(first.as_str()),
        "a refused revision must not move the campaign: {campaign}"
    );
    Ok(())
}

// ── the permission model, as two matrices ───────────────────────────────────
//
// These pin the whole model in one place each, so a change to the reach or to
// the device relation has to rewrite a table.

/// Two organizations, a personal and an organizational artifact on each, and
/// every kind of principal that could ask for them.
struct World {
    acme: String,
    rival: String,
    /// A member of acme, holding nothing, and its artifact.
    member: UserSigner,
    member_res: String,
    /// Another member of acme, and its artifact: the one a colleague and an
    /// administrator must **not** reach.
    colleague_res: String,
    acme_admin: UserSigner,
    acme_res: String,
    rival_admin: UserSigner,
    rival_res: String,
    /// A member of rival, and its artifact.
    rival_member: UserSigner,
    rival_member_res: String,
    server: UserSigner,
}

async fn world(state: &AppState) -> anyhow::Result<World> {
    let server = server_admin(state).await?;
    let acme = create_org(state, &server, "acme").await?;
    let rival = create_org(state, &server, "rival").await?;

    let member = operator(state).await?;
    set_user_org(state, &member.1, Some(acme.parse()?)).await?;
    let (member_res, _) = own_resource(state, &member).await?;

    let colleague = operator(state).await?;
    set_user_org(state, &colleague.1, Some(acme.parse()?)).await?;
    let (colleague_res, _) = own_resource(state, &colleague).await?;

    let acme_admin = org_admin(state, &acme).await?;
    let acme_res = upload_gguf_text(state, &acme_admin, Some(&acme), BLOB).await?;

    let rival_admin = org_admin(state, &rival).await?;
    let rival_res = upload_gguf_text(state, &rival_admin, Some(&rival), BLOB).await?;

    let rival_member = operator(state).await?;
    set_user_org(state, &rival_member.1, Some(rival.parse()?)).await?;
    let (rival_member_res, _) = own_resource(state, &rival_member).await?;

    Ok(World {
        acme,
        rival,
        member,
        member_res,
        colleague_res,
        acme_admin,
        acme_res,
        rival_admin,
        rival_res,
        rival_member,
        rival_member_res,
        server,
    })
}

/// What one account sees of one resource: whether the listing carries it, and
/// whether the record and the bytes answer. The three must agree, which is
/// asserted here rather than left to three tables.
async fn account_sees(
    state: &AppState,
    signer: &UserSigner,
    resource_id: &str,
) -> anyhow::Result<bool> {
    let listing =
        body_json(signed_empty(state, signer, "GET", "/api/v1/vault/resources").await?).await?;
    let listed = listing
        .get("items")
        .and_then(Value::as_array)
        .context("the listing must carry items")?
        .iter()
        .any(|item| item["resource_id"] == resource_id);
    let record = signed_empty(
        state,
        signer,
        "GET",
        &format!("/vault/resources/{resource_id}"),
    )
    .await?
    .status();
    let bytes = signed_empty(state, signer, "GET", &content_uri(resource_id))
        .await?
        .status();
    anyhow::ensure!(
        [record, bytes].iter().all(|s| *s == StatusCode::OK)
            || [record, bytes].iter().all(|s| *s == StatusCode::FORBIDDEN),
        "the record and the bytes must agree: record {record}, bytes {bytes}"
    );
    anyhow::ensure!(
        listed == (record == StatusCode::OK),
        "the listing must agree with the record: listed {listed}, record {record}"
    );
    Ok(listed)
}

/// **The operator model.** An account sees its own and its organization's,
/// grant or no grant; nobody sees a colleague's personal artifact or another
/// organization's. A server admin sees everything.
#[tokio::test]
async fn the_operator_matrix() -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let w = world(&state).await?;

    // Rows are callers; columns are the artifacts, in this order.
    let columns = [
        &w.member_res,
        &w.colleague_res,
        &w.acme_res,
        &w.rival_res,
        &w.rival_member_res,
    ];
    let expected: [(&str, &UserSigner, [bool; 5]); 5] = [
        //                                    own     colleague acme   rival  rival-member
        (
            "member of acme",
            &w.member,
            [true, false, true, false, false],
        ),
        (
            "acme admin",
            &w.acme_admin,
            [false, false, true, false, false],
        ),
        (
            "rival admin",
            &w.rival_admin,
            [false, false, false, true, false],
        ),
        (
            "member of rival",
            &w.rival_member,
            [false, false, false, true, true],
        ),
        ("server admin", &w.server, [true, true, true, true, true]),
    ];

    for (who, signer, row) in expected {
        for (resource_id, want) in columns.iter().zip(row) {
            let got = account_sees(&state, signer, resource_id).await?;
            assert_eq!(got, want, "{who} on {resource_id}");
        }
    }
    Ok(())
}

/// `read_any_orgs` is the roster read and opens no vault resource. If the reach
/// took it for `manage_any_orgs`, which implies it, private weights would open to
/// every roster reader.
#[tokio::test]
async fn read_any_orgs_reaches_no_vault_resource() -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let w = world(&state).await?;
    let reader = registered_user(&state, &[UserPermission::ReadAnyOrgs]).await?;
    assert_eq!(
        signed_empty(&state, &reader, "GET", &format!("/api/v1/orgs/{}", w.acme))
            .await?
            .status(),
        StatusCode::OK,
        "the grant is held: the reader reads a roster it is not on"
    );

    for resource_id in [
        &w.member_res,
        &w.colleague_res,
        &w.acme_res,
        &w.rival_res,
        &w.rival_member_res,
    ] {
        assert!(
            !account_sees(&state, &reader, resource_id).await?,
            "read_any_orgs on {resource_id}"
        );
    }
    for uri in [
        format!("/vault/resources/{}", w.acme_res),
        content_uri(&w.acme_res),
    ] {
        let resp = signed_empty(&state, &reader, "GET", &uri).await?;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{uri}");
        assert_eq!(
            json_str(&body_json(resp).await?, "error")?,
            "this vault resource belongs to another owner"
        );
    }
    Ok(())
}

/// **The device model.** A device reaches what its owner owns. An
/// organization's device reaches its artifacts and no member's personal ones;
/// an account's device also reaches its account's organization's, as
/// `owning_org` rolls devices up everywhere. An unowned device reaches nothing.
#[tokio::test]
async fn the_device_matrix() -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let w = world(&state).await?;

    let columns = [
        &w.member_res,
        &w.colleague_res,
        &w.acme_res,
        &w.rival_res,
        &w.rival_member_res,
    ];
    let devices: [(&str, Option<Value>, [bool; 5]); 5] = [
        //                                                  member  colleague acme   rival  rival-member
        ("unowned", None, [false, false, false, false, false]),
        (
            "owned by the member",
            Some(user_owner(&w.member.1)),
            [true, false, true, false, false],
        ),
        (
            "owned by acme",
            Some(org_owner(&w.acme)),
            [false, false, true, false, false],
        ),
        (
            "owned by rival",
            Some(org_owner(&w.rival)),
            [false, false, false, true, false],
        ),
        (
            "owned by rival's member",
            Some(user_owner(&w.rival_member.1)),
            [false, false, false, true, true],
        ),
    ];

    for (who, owner, row) in devices {
        let (key, device) = owned_device(&state, &w.server, owner).await?;
        for (resource_id, want) in columns.iter().zip(row) {
            let record = authed_get(
                &state,
                &key,
                &device,
                &format!("/vault/resources/{resource_id}"),
            )
            .await?
            .status();
            let bytes = authed_get(&state, &key, &device, &content_uri(resource_id))
                .await?
                .status();
            assert_eq!(
                record, bytes,
                "{who} on {resource_id}: record and bytes must agree"
            );
            assert_eq!(
                bytes == StatusCode::OK,
                want,
                "device {who} on {resource_id}: got {bytes}"
            );
        }
    }
    Ok(())
}

// ── the edges of the gate ───────────────────────────────────────────────────

/// A device belongs to whichever organization its **account** is on now, so
/// moving the account moves what the device may fetch: the old team's artifact
/// closes and the new team's opens, with nothing written to the device.
#[tokio::test]
async fn a_device_follows_its_account_between_organizations() -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let admin = server_admin(&state).await?;
    let acme = create_org(&state, &admin, "acme").await?;
    let rival = create_org(&state, &admin, "rival").await?;

    let acme_admin = org_admin(&state, &acme).await?;
    let acme_res = upload_gguf_text(&state, &acme_admin, Some(&acme), BLOB).await?;
    let rival_admin = org_admin(&state, &rival).await?;
    let rival_res = upload_gguf_text(&state, &rival_admin, Some(&rival), BLOB).await?;

    let member = registered_user(&state, &[]).await?;
    set_user_org(&state, &member.1, Some(acme.parse()?)).await?;
    let (key, device) = owned_device(&state, &admin, Some(user_owner(&member.1))).await?;
    let fetch = async |res: &str| -> anyhow::Result<StatusCode> {
        Ok(authed_get(&state, &key, &device, &content_uri(res))
            .await?
            .status())
    };

    assert_eq!(fetch(&acme_res).await?, StatusCode::OK);
    assert_eq!(fetch(&rival_res).await?, StatusCode::FORBIDDEN);

    set_user_org(&state, &member.1, Some(rival.parse()?)).await?;
    assert_eq!(
        fetch(&acme_res).await?,
        StatusCode::FORBIDDEN,
        "the old team's artifact closes the moment the account leaves"
    );
    assert_eq!(fetch(&rival_res).await?, StatusCode::OK);
    Ok(())
}

/// An organization's administrator and a server admin fetch the bytes as
/// accounts, under the same reach that opens the record to them.
#[tokio::test]
async fn an_administrator_fetches_the_bytes_as_an_account() -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let admin = server_admin(&state).await?;
    let org_id = create_org(&state, &admin, "acme").await?;
    let team_admin = org_admin(&state, &org_id).await?;
    let resource_id = upload_gguf_text(&state, &team_admin, Some(&org_id), BLOB).await?;

    for signer in [&team_admin, &admin] {
        let resp = signed_empty(&state, signer, "GET", &content_uri(&resource_id)).await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await?;
        assert_eq!(body.as_ref(), BLOB);
    }
    Ok(())
}

/// A pending device and an unsigned request are refused at the header, before
/// the record is read: the vault adds no gate in front of authentication, it
/// sits behind it.
#[tokio::test]
async fn an_unvetted_caller_reaches_nothing() -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let me = operator(&state).await?;
    let (resource_id, _) = own_resource(&state, &me).await?;

    assert_eq!(
        unauthed_get(&state, &content_uri(&resource_id))
            .await?
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let (pending_key, pending_id) = register_pending(&state).await?;
    assert_eq!(
        authed_get(
            &state,
            &pending_key,
            &pending_id,
            &content_uri(&resource_id)
        )
        .await?
        .status(),
        StatusCode::FORBIDDEN
    );
    Ok(())
}

/// An account that is pending or disabled reaches no vault route, its own
/// resource included, and nothing is stored or deleted. Swapping the active-user
/// gate for a bare signature check on any route would fail here.
#[rstest]
#[case::read("GET", "/vault/resources/{id}")]
#[case::fetch("GET", "/vault/resources/{id}/content")]
#[case::list("GET", "/api/v1/vault/resources")]
#[case::types("GET", "/api/v1/vault/types")]
#[case::describe("POST", "/api/v1/vault/resources")]
#[case::delete("DELETE", "/api/v1/vault/resources/{id}")]
#[case::upload_file("PUT", "/api/v1/vault/blobs/{me}/{sha256}")]
#[case::probe_file("HEAD", "/api/v1/vault/blobs/{me}/{sha256}")]
#[tokio::test]
async fn an_account_out_of_standing_reaches_none_of_its_vault(
    #[case] method: &str,
    #[case] path: &str,
    #[values(UserStatus::Pending, UserStatus::Disabled)] status: UserStatus,
) -> anyhow::Result<()> {
    let reason = match status {
        UserStatus::Pending => "user is pending an operator's approval",
        UserStatus::Disabled => "user is disabled",
        UserStatus::Active => anyhow::bail!("only the statuses that refuse belong here"),
    };
    let (_dir, state) = vault_state().await?;
    let me = operator(&state).await?;
    let (resource_id, sha256) = own_resource(&state, &me).await?;
    // The file routes name a file not yet uploaded, so a write would show.
    let projector = sha256_hex(PROJECTOR);
    let uri = path
        .replace("{id}", &resource_id)
        .replace("{me}", me.1.as_str())
        .replace("{sha256}", &projector);
    set_user_status(&state, &me.1, status).await?;

    let headers = user_auth_headers(&me.0, &me.1, &me.2, method, &uri)?;
    let resp = match method {
        "POST" => {
            let body = gguf_text_upload(None, &sha256);
            json_request(&state, method, &uri, &headers, &body).await?
        }
        "PUT" => raw_request(&state, method, &uri, &headers, None, PROJECTOR.to_vec()).await?,
        _ => request_with_headers(&state, method, &uri, &headers).await?,
    };
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert!(
        resp.headers().get(header::ETAG).is_none(),
        "no pin for bytes that are not served"
    );
    if method != "HEAD" {
        assert_eq!(json_str(&body_json(resp).await?, "error")?, reason);
    }

    set_user_status(&state, &me.1, UserStatus::Active).await?;
    let listing =
        body_json(signed_empty(&state, &me, "GET", "/api/v1/vault/resources").await?).await?;
    let ids: Vec<&str> = listing["items"]
        .as_array()
        .context("the listing must carry items")?
        .iter()
        .filter_map(|item| item["resource_id"].as_str())
        .collect();
    assert_eq!(ids, [resource_id.as_str()], "nothing written or deleted");
    assert_eq!(
        head_vault_blob(&state, &me, me.1.as_str(), &projector)
            .await?
            .status(),
        StatusCode::NOT_FOUND,
        "no file kept"
    );
    Ok(())
}

/// A record outlives its file only if the file went missing from the store. The
/// fetch is then a `404`, not a `500`: the record is intact, the bytes are gone.
#[tokio::test]
async fn a_record_whose_object_was_removed_is_a_404() -> anyhow::Result<()> {
    let (dir, state) = vault_state().await?;
    let me = operator(&state).await?;
    let (resource_id, sha256) = own_resource(&state, &me).await?;
    std::fs::remove_file(blob_path(dir.path(), me.1.as_str(), &sha256))?;

    assert_eq!(
        signed_empty(&state, &me, "GET", &content_uri(&resource_id))
            .await?
            .status(),
        StatusCode::NOT_FOUND
    );
    // The record itself still answers: it is the object that is missing.
    assert_eq!(
        signed_empty(
            &state,
            &me,
            "GET",
            &format!("/vault/resources/{resource_id}")
        )
        .await?
        .status(),
        StatusCode::OK
    );
    Ok(())
}

// ── the edges of the index ──────────────────────────────────────────────────

/// Who may upload **for whom**: an organization's administrator may put a
/// file and a record down on a member of it, a plain operator may not put one
/// down on anybody else, and a destination that does not exist is a `400` only
/// for a caller that could otherwise have reached it. The file and the
/// description answer alike.
#[rstest]
// An org admin hands down inside the roster it administers.
#[case::admin_for_member(true, true, true, StatusCode::CREATED)]
// A plain operator registers for nobody but itself.
#[case::operator_for_stranger(false, true, true, StatusCode::FORBIDDEN)]
// An org admin may not reach an account outside its roster.
#[case::admin_for_outsider(true, false, true, StatusCode::FORBIDDEN)]
// A server admin learns the account does not exist; scope passed first.
#[case::server_for_absent(true, true, false, StatusCode::BAD_REQUEST)]
#[tokio::test]
async fn uploading_for_another_account(
    #[case] caller_is_admin: bool,
    #[case] target_on_roster: bool,
    #[case] target_exists: bool,
    #[case] expected: StatusCode,
) -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let admin = server_admin(&state).await?;
    let org_id = create_org(&state, &admin, "acme").await?;

    let caller = if caller_is_admin {
        org_admin(&state, &org_id).await?
    } else {
        operator(&state).await?
    };
    let target = if target_exists {
        let target = registered_user(&state, &[]).await?;
        if target_on_roster {
            set_user_org(&state, &target.1, Some(org_id.parse()?)).await?;
        }
        target.1
    } else {
        UserId::mint()
    };
    // The absent-account case is the server admin's, because a caller that may
    // not hand there is refused before existence is consulted.
    let caller = if target_exists { caller } else { admin };

    let sha256 = sha256_hex(BLOB);
    let put = put_vault_blob(&state, &caller, target.as_str(), &sha256, BLOB).await?;
    assert_eq!(
        put.status(),
        if expected == StatusCode::CREATED {
            StatusCode::CREATED
        } else {
            expected
        }
    );
    assert_eq!(
        describe(&state, &caller, Some(target.as_str()), &sha256)
            .await?
            .status(),
        expected
    );
    Ok(())
}

/// A delete takes a file only with the last record naming it: two uploads of
/// one file share its bytes, and the first delete leaves them for the second.
#[tokio::test]
async fn a_file_goes_with_the_last_record_naming_it() -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let me = operator(&state).await?;
    let sha256 = upload_vault_blob(&state, &me, me.1.as_str(), BLOB).await?;
    let mut ids = Vec::new();
    for _ in 0..2 {
        let resp = describe(&state, &me, None, &sha256).await?;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let answer = body_json(resp).await?;
        ids.push(json_str(&answer["resources"][0], "resource_id")?.to_string());
    }

    for (resource_id, stored) in ids.iter().zip([StatusCode::OK, StatusCode::NOT_FOUND]) {
        let uri = format!("/api/v1/vault/resources/{resource_id}");
        let resp = signed_empty(&state, &me, "DELETE", &uri).await?;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        let probe = head_vault_blob(&state, &me, me.1.as_str(), &sha256).await?;
        assert_eq!(probe.status(), stored, "after deleting {resource_id}");
    }
    Ok(())
}

/// An upload an administrator makes for a member of its organization is the
/// member's: its owner and its plan entry's principal name the member, the
/// member reads and deletes it, and the administrator neither reads nor deletes it.
#[tokio::test]
async fn an_upload_for_a_member_belongs_to_the_member() -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let admin = server_admin(&state).await?;
    let org_id = create_org(&state, &admin, "acme").await?;
    let team_admin = org_admin(&state, &org_id).await?;
    let member = operator(&state).await?;
    set_user_org(&state, &member.1, Some(org_id.parse()?)).await?;

    let sha256 = upload_vault_blob(&state, &team_admin, member.1.as_str(), BLOB).await?;
    let resp = describe(&state, &team_admin, Some(member.1.as_str()), &sha256).await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let answer = body_json(resp).await?;
    assert_eq!(answer["resources"][0]["owner"], user_owner(&member.1));
    assert_eq!(answer["entry"]["model"]["principal_id"], json!(member.1));
    let resource_id = json_str(&answer["resources"][0], "resource_id")?.to_string();
    let uri = format!("/api/v1/vault/resources/{resource_id}");

    for read in [uri.clone(), content_uri(&resource_id)] {
        let resp = signed_empty(&state, &team_admin, "GET", &read).await?;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{read}");
        assert_eq!(
            json_str(&body_json(resp).await?, "error")?,
            "this vault resource belongs to another owner"
        );
    }

    // Uploading it for the member does not let the administrator delete it.
    let resp = signed_empty(&state, &team_admin, "DELETE", &uri).await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        json_str(&body_json(resp).await?, "error")?,
        "this vault resource belongs to another owner"
    );

    let resp = signed_empty(&state, &member, "GET", &uri).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await?["owner"], user_owner(&member.1));
    assert_eq!(
        signed_empty(&state, &member, "DELETE", &uri)
            .await?
            .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        signed_empty(&state, &member, "GET", &uri).await?.status(),
        StatusCode::NOT_FOUND
    );
    Ok(())
}

/// A malformed description is refused at the boundary with a `400`, before
/// any scope is consulted or any store is read. The body that registered an
/// object by its key is one of them.
#[rstest]
#[case::by_object_key(json!({"name": "x", "object_key": "{me}/lfm2.gguf", "sha256": "a".repeat(64)}))]
#[case::unknown_type(json!({"name": "x", "type": "docker_vllm", "model": {"filename": "m.gguf", "sha256": "a".repeat(64)}}))]
#[case::missing_file(json!({"name": "x", "type": "gguf_vision", "model": {"filename": "m.gguf", "sha256": "a".repeat(64)}}))]
#[case::bad_sha256(json!({"name": "x", "type": "gguf_text", "model": {"filename": "m.gguf", "sha256": "not-hex"}}))]
#[case::nested_filename(json!({"name": "x", "type": "gguf_text", "model": {"filename": "a/m.gguf", "sha256": "a".repeat(64)}}))]
#[case::owner_not_a_principal(json!({"name": "x", "owner": "acme", "type": "gguf_text", "model": {"filename": "m.gguf", "sha256": "a".repeat(64)}}))]
#[tokio::test]
async fn a_malformed_description_is_a_400(#[case] body: Value) -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let me = operator(&state).await?;
    let body: Value = serde_json::from_str(&body.to_string().replace("{me}", me.1.as_str()))?;
    assert_eq!(
        describe_vault_upload(&state, &me, &body).await?.status(),
        StatusCode::BAD_REQUEST
    );
    Ok(())
}

/// A description that is not JSON, not a description, or not sent as JSON, is a
/// `400` in the error envelope rather than axum's plain-text `400`, `422` or
/// `415`, and writes nothing. `None` sends a valid description of an uploaded
/// file.
#[rstest]
#[case::not_json(Some("application/json"), Some("{ not json"))]
#[case::wrong_shape(Some("application/json"), Some(r#"{"name": 7}"#))]
#[case::no_content_type(None, None)]
#[case::text_plain(Some("text/plain"), None)]
#[tokio::test]
async fn a_description_not_sent_as_json_is_a_400_in_the_envelope(
    #[case] content_type: Option<&str>,
    #[case] body: Option<&str>,
) -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let me = operator(&state).await?;
    let sha256 = upload_vault_blob(&state, &me, me.1.as_str(), BLOB).await?;
    let body = body.map_or_else(
        || gguf_text_upload(None, &sha256).to_string(),
        str::to_string,
    );

    let uri = "/api/v1/vault/resources";
    let headers = user_auth_headers(&me.0, &me.1, &me.2, "POST", uri)?;
    let resp = raw_request(&state, "POST", uri, &headers, content_type, body).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let error = body_json(resp).await?;
    assert!(!json_str(&error, "error")?.is_empty(), "{error}");

    let listing =
        body_json(signed_empty(&state, &me, "GET", "/api/v1/vault/resources").await?).await?;
    assert_eq!(listing["items"], json!([]), "a refusal writes no record");
    Ok(())
}

/// A refused file name is quoted with the whole rule. The variant's files are
/// flattened into the body, so the refusal names no field: the name has to say
/// which file it was.
#[tokio::test]
async fn a_refused_file_name_is_quoted_with_the_rule() -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let me = operator(&state).await?;
    let body = json!({"name": "x", "type": "gguf_text", "model": {"filename": "my model (1).gguf", "sha256": "a".repeat(64)}});
    let resp = describe_vault_upload(&state, &me, &body).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let error = body_json(resp).await?;
    let error = json_str(&error, "error")?;
    assert!(
        error.contains("file name `my model (1).gguf` must be one path segment of [A-Za-z0-9._-]")
            && error.contains("at most 255 characters"),
        "{error}"
    );
    Ok(())
}

/// An id that is not a vault id is a `400` at the boundary, not a lookup that
/// finds nothing: the prefix is what tells the keyspaces apart.
#[rstest]
#[case::campaign_id("campaign_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e")]
#[case::bare("checkpoint")]
#[tokio::test]
async fn a_foreign_id_in_the_path_is_a_400(#[case] id: &str) -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let me = operator(&state).await?;
    assert_eq!(
        signed_empty(&state, &me, "GET", &format!("/vault/resources/{id}"))
            .await?
            .status(),
        StatusCode::BAD_REQUEST
    );
    Ok(())
}

// ── a plan that names the vault ─────────────────────────────────────────────

/// One variant pinned to `device`, whose model is the inline TOML table
/// `model`: the body of a variant edit, and the variant of [`model_document`].
fn model_variant(device: &ClientId, model: &str) -> String {
    format!(
        r#"[[variants]]
clients  = ["{device}"]
models   = [{model}]
runtimes = [{{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", repository_version = "b5000", flavor = "macos-arm64" }}]
"#,
        device = device.as_str(),
    )
}

/// A plan document of one job pinned to `device`, whose model is the inline
/// TOML table `model`.
fn model_document(device: &ClientId, model: &str) -> String {
    format!(
        "benchmarks = [\"eval_test\"]\n\n{}",
        model_variant(device, model)
    )
}

/// A public model, for the variant a vault reference is filed beside.
const PUBLIC_MODEL: &str = r#"{ type = "gguf_text", source = "huggingface", org = "LiquidAI", repo_name = "LFM2-700M-GGUF", path = "LFM2-700M-Q4_0.gguf" }"#;

/// The inline TOML table of a model fetched from the vault at `url` on behalf
/// of `principal`, pinning `sha256` when given.
fn vault_model(url: &str, principal: &str, sha256: Option<&str>) -> String {
    let pin = sha256
        .map(|sha256| format!(", sha256 = \"{sha256}\""))
        .unwrap_or_default();
    format!(
        "{{ type = \"gguf_text\", source = \"vault_url\", url = \"{url}\", \
         principal_id = \"{principal}\"{pin} }}"
    )
}

/// File [`model_document`] into a campaign of `signer`'s own.
async fn file_model_document(
    state: &AppState,
    signer: &UserSigner,
    device: &ClientId,
    model: &str,
) -> anyhow::Result<axum::http::Response<axum::body::Body>> {
    let campaign_id = put_idle_campaign(state, &signer.1, "Vault").await?;
    post_plan_document(
        state,
        signer,
        &format!("/api/v1/campaigns/{campaign_id}/plans"),
        &model_document(device, model),
    )
    .await
}

/// File one job whose model is a vault fetch of `url` on behalf of
/// `principal`, as an operator holding `submit_plans`.
async fn file_vault_plan(
    state: &AppState,
    signer: &UserSigner,
    device: &ClientId,
    url: &str,
    principal: &str,
    sha256: Option<&str>,
) -> anyhow::Result<axum::http::Response<axum::body::Body>> {
    file_model_document(state, signer, device, &vault_model(url, principal, sha256)).await
}

/// Ingestion refuses a plan naming a resource that does not exist, is not the
/// claimed principal's, lost its object, or pins another digest, before
/// anything is queued and while the reason can still be named. The planner
/// refuses a vault fetch spelled as an absolute URL. The filer is on the owning
/// organization.
#[rstest]
#[case::owner_matches(true, true, true, true, None, StatusCode::CREATED, "")]
#[case::pins_the_record_s_digest(true, true, true, true, Some(true), StatusCode::CREATED, "")]
#[case::principal_mismatch(true, false, true, true, None, StatusCode::BAD_REQUEST, "belongs to")]
#[case::no_such_resource(
    false,
    true,
    true,
    true,
    None,
    StatusCode::BAD_REQUEST,
    "does not exist"
)]
#[case::object_removed(
    true,
    true,
    true,
    false,
    None,
    StatusCode::BAD_REQUEST,
    "no longer there"
)]
#[case::pins_another_digest(
    true,
    true,
    true,
    true,
    Some(false),
    StatusCode::BAD_REQUEST,
    "is pinned to"
)]
#[case::absolute_url(
    true,
    true,
    false,
    true,
    None,
    StatusCode::BAD_REQUEST,
    "relative to the server"
)]
#[tokio::test]
async fn a_plan_naming_the_vault_is_checked_at_ingestion(
    #[case] registered: bool,
    #[case] principal_is_owner: bool,
    #[case] url_is_this_server: bool,
    #[case] object_present: bool,
    #[case] pins_right_digest: Option<bool>,
    #[case] expected: StatusCode,
    #[case] reason: &str,
) -> anyhow::Result<()> {
    let (dir, state) = vault_state().await?;
    let admin = server_admin(&state).await?;
    let org_id = create_org(&state, &admin, "acme").await?;
    let team_admin = org_admin(&state, &org_id).await?;
    let sha256 = upload_vault_blob(&state, &team_admin, &org_id, BLOB).await?;
    let resource_id = if registered {
        describe_gguf_text_ok(&state, &team_admin, Some(&org_id), &sha256).await?
    } else {
        "vault_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e".to_string()
    };
    let principal = if principal_is_owner {
        org_id.clone()
    } else {
        // Another organization, real but not the owner.
        create_org(&state, &admin, "rival").await?
    };
    let url = if url_is_this_server {
        content_uri(&resource_id)
    } else {
        "https://elsewhere.example/vault/resources/vault_0195c3e2/content".to_string()
    };

    if !object_present {
        std::fs::remove_file(blob_path(dir.path(), &org_id, &sha256))?;
    }
    let sha256 = pins_right_digest.map(|right| {
        if right {
            sha256_hex(BLOB)
        } else {
            "b".repeat(64)
        }
    });

    let submitter = registered_user(&state, &[UserPermission::SubmitPlans]).await?;
    set_user_org(&state, &submitter.1, Some(org_id.parse()?)).await?;
    let (_, device) = owned_device(&state, &admin, Some(org_owner(&org_id))).await?;
    let resp = file_vault_plan(
        &state,
        &submitter,
        &device,
        &url,
        &principal,
        sha256.as_deref(),
    )
    .await?;
    assert_eq!(resp.status(), expected);
    if expected == StatusCode::BAD_REQUEST {
        let body = body_json(resp).await?;
        let error = json_str(&body, "error")?;
        assert!(error.contains(reason), "the refusal says why: {error}");
    }
    Ok(())
}

/// The plan page's jobs name the vault resource a plan cell names, only for a
/// reader the vault admits: reading a plan is not reading what its owner holds.
#[rstest]
#[case::a_member(&[UserPermission::ReadAnyPlans], true, true)]
#[case::a_server_admin(&[UserPermission::ReadAnyPlans, UserPermission::ManageAnyOrgs], false, true)]
#[case::an_auditor_outside(&[UserPermission::ReadAnyPlans], false, false)]
#[tokio::test]
async fn the_plan_page_names_a_vault_resource_in_the_readers_reach(
    #[case] grants: &[UserPermission],
    #[case] member: bool,
    #[case] named: bool,
) -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let admin = server_admin(&state).await?;
    let org_id = create_org(&state, &admin, "acme").await?;
    let team_admin = org_admin(&state, &org_id).await?;
    let resource_id = upload_gguf_text(&state, &team_admin, Some(&org_id), BLOB).await?;
    let submitter = registered_user(&state, &[UserPermission::SubmitPlans]).await?;
    set_user_org(&state, &submitter.1, Some(org_id.parse()?)).await?;
    let (_, device) = owned_device(&state, &admin, Some(org_owner(&org_id))).await?;
    let resp = file_vault_plan(
        &state,
        &submitter,
        &device,
        &content_uri(&resource_id),
        &org_id,
        None,
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let plan_id = PlanId::try_new(json_str(&body_json(resp).await?, "plan_id")?)?;

    let reader = registered_user(&state, grants).await?;
    if member {
        set_user_org(&state, &reader.1, Some(org_id.parse()?)).await?;
    }
    let resp = signed_empty(
        &state,
        &reader,
        "GET",
        &format!("/api/v1/plans/{plan_id}/jobs?results=true"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    crate::api_schema::assert_conforms("NamedPlanJobs", &body)?;
    // Either reader gets the cell, which names the resource by its id.
    assert_eq!(
        body.pointer("/jobs/0/planned/model/url"),
        Some(&json!(content_uri(&resource_id))),
        "{body}"
    );
    let expected = named.then(|| json!({ resource_id.as_str(): "acme private checkpoint" }));
    assert_eq!(body.get("names"), expected.as_ref(), "{body}");
    Ok(())
}

/// A vault source's `custom_label` is its record's name: a plan that pastes
/// the vault's entry shows it on its cell, one that rewords it is refused, and
/// one that leaves it out joins the same job.
#[tokio::test]
async fn a_plan_labels_a_vault_model_as_its_record_names_it() -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let admin = server_admin(&state).await?;
    let org_id = create_org(&state, &admin, "acme").await?;
    let team_admin = org_admin(&state, &org_id).await?;
    let resource_id = upload_gguf_text(&state, &team_admin, Some(&org_id), BLOB).await?;
    let submitter = registered_user(
        &state,
        &[UserPermission::SubmitPlans, UserPermission::ReadPlans],
    )
    .await?;
    set_user_org(&state, &submitter.1, Some(org_id.parse()?)).await?;
    let (_, device) = owned_device(&state, &admin, Some(org_owner(&org_id))).await?;

    let record = body_json(
        signed_empty(
            &state,
            &team_admin,
            "GET",
            &format!("/vault/resources/{resource_id}"),
        )
        .await?,
    )
    .await?;
    let entry = json_str(&record, "toml")?;
    assert!(
        entry.contains(r#"custom_label = "acme private checkpoint""#),
        "{entry}"
    );

    let filed = |model: String| {
        let (state, submitter, device) = (&state, &submitter, &device);
        async move {
            let resp = file_model_document(state, submitter, device, &model).await?;
            anyhow::ensure!(resp.status() == StatusCode::CREATED, "{}", resp.status());
            let plan_id = PlanId::try_new(json_str(&body_json(resp).await?, "plan_id")?)?;
            let resp = signed_empty(
                state,
                submitter,
                "GET",
                &format!("/api/v1/plans/{plan_id}/jobs?results=true"),
            )
            .await?;
            anyhow::ensure!(resp.status() == StatusCode::OK, "{}", resp.status());
            let body = body_json(resp).await?;
            crate::api_schema::assert_conforms("NamedPlanJobs", &body)?;
            anyhow::Ok((
                body.pointer("/jobs/0/job_id").cloned(),
                body.pointer("/jobs/0/planned/model/custom_label").cloned(),
            ))
        }
    };
    let (labelled_job, label) = filed(entry.to_owned()).await?;
    assert_eq!(label, Some(json!("acme private checkpoint")));

    let reworded = entry.replace("acme private checkpoint", "acme checkpoint v2");
    let resp = file_model_document(&state, &submitter, &device, &reworded).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await?;
    let error = json_str(&body, "error")?;
    assert!(
        error.contains(r#"not the custom_label "acme checkpoint v2""#),
        "{error}"
    );

    // The same entry without its label: every other field, the pin too.
    let label_field = r#", custom_label = "acme private checkpoint""#;
    anyhow::ensure!(entry.contains(label_field), "{entry}");
    let (plain_job, label) = filed(entry.replace(label_field, "")).await?;
    assert_eq!(plain_job, labelled_job, "a label is no part of the job");
    // The job keeps the body it was minted with.
    assert_eq!(label, Some(json!("acme private checkpoint")));
    Ok(())
}

/// A campaign that ran an organization's vault resource stays inside it, as
/// one that ran its private benchmark does. The job names the owner, so the
/// record's delete changes nothing; a filer who moves away cannot hand the
/// campaign to the new organization, and can hand it back inside.
#[tokio::test]
async fn a_campaign_that_ran_an_organization_s_resource_stays_in_it() -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let admin = server_admin(&state).await?;
    let acme = create_org(&state, &admin, "acme").await?;
    let labs = create_org(&state, &admin, "labs").await?;
    let team_admin = org_admin(&state, &acme).await?;
    let resource_id = upload_gguf_text(&state, &team_admin, Some(&acme), BLOB).await?;
    let filer = registered_user(&state, &[UserPermission::SubmitPlans]).await?;
    set_user_org(&state, &filer.1, Some(acme.parse()?)).await?;
    let (_, device) = owned_device(&state, &admin, Some(org_owner(&acme))).await?;
    let campaign_id = put_idle_campaign(&state, &filer.1, "Vault").await?;
    let model = vault_model(&content_uri(&resource_id), &acme, None);
    let resp = post_plan_document(
        &state,
        &filer,
        &format!("/api/v1/campaigns/{campaign_id}/plans"),
        &model_document(&device, &model),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let deleted = format!("/api/v1/vault/resources/{resource_id}");
    let resp = signed_empty(&state, &team_admin, "DELETE", &deleted).await?;
    assert!(resp.status().is_success(), "{}", resp.status());

    let uri = format!("/api/v1/campaigns/{campaign_id}/owner");
    set_user_org(&state, &filer.1, Some(labs.parse()?)).await?;
    let to_labs = json!({ "owner": org_owner(&labs) });
    let resp = signed_json(&state, &filer, "PUT", &uri, &to_labs).await?;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let error = json_str(&body_json(resp).await?, "error")?.to_string();
    assert!(error.contains(&resource_id), "{error}");

    set_user_org(&state, &filer.1, Some(acme.parse()?)).await?;
    let to_acme = json!({ "owner": org_owner(&acme) });
    let resp = signed_json(&state, &filer, "PUT", &uri, &to_acme).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    Ok(())
}

/// A job naming several resources is judged on all of them: one that fails
/// refuses the job, and the message names *that* one.
#[tokio::test]
async fn one_bad_reference_among_several_refuses_the_job() -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let admin = server_admin(&state).await?;
    let org_id = create_org(&state, &admin, "acme").await?;
    let team_admin = org_admin(&state, &org_id).await?;
    let good = upload_gguf_text(&state, &team_admin, Some(&org_id), BLOB).await?;
    let absent = "vault_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e";

    let submitter = registered_user(&state, &[UserPermission::SubmitPlans]).await?;
    set_user_org(&state, &submitter.1, Some(org_id.parse()?)).await?;
    let (_, device) = owned_device(&state, &admin, Some(org_owner(&org_id))).await?;
    let model = format!(
        "{{ type = \"gguf_vision\", source = \"vault_url\", model = \"{}\", \
         mmproj = \"{}\", principal_id = \"{org_id}\" }}",
        content_uri(&good),
        content_uri(absent),
    );
    let resp = file_model_document(&state, &submitter, &device, &model).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let error = body_json(resp).await?;
    let error = json_str(&error, "error")?;
    assert!(error.contains(absent), "names the absent one: {error}");
    assert!(!error.contains(&good), "and not the good one: {error}");
    Ok(())
}

/// A resource id no record holds: the refusal of one out of reach must read
/// as this one's.
const NO_SUCH_RESOURCE: &str = "vault_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e";

/// The `error` of a `400`.
async fn refusal(resp: axum::http::Response<axum::body::Body>) -> anyhow::Result<String> {
    anyhow::ensure!(
        resp.status() == StatusCode::BAD_REQUEST,
        "expected a 400, got {}",
        resp.status()
    );
    Ok(json_str(&body_json(resp).await?, "error")?.to_string())
}

/// The label of the principal that owns `resource_id`, as the record says.
async fn owner_label(state: &AppState, resource_id: &str) -> anyhow::Result<String> {
    Ok(state
        .vault_store
        .get_resource(&VaultResourceId::try_new(resource_id)?)
        .await?
        .context("the resource exists")?
        .owner
        .label())
}

async fn plan_count(state: &AppState) -> anyhow::Result<usize> {
    Ok(pipette_store::drain_plans(state.plan_store.as_ref(), None)
        .await?
        .items
        .len())
}

async fn head_of(state: &AppState, campaign_id: &str) -> anyhow::Result<Option<PlanId>> {
    Ok(state
        .campaign_store
        .get_campaign(&CampaignId::try_new(campaign_id)?)
        .await?
        .context("the campaign exists")?
        .current_plan_id)
}

/// **The filer model.** A plan names the filer's own resources and its
/// organization's, and `submit_any_plans` names every one. `manage_any_orgs`
/// reads every resource and names none it does not own. A resource outside the
/// reach is refused exactly as an absent one, so the refusal names no owner.
#[tokio::test]
async fn the_filer_matrix() -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let w = world(&state).await?;
    let (_, device) = owned_device(&state, &w.server, None).await?;
    let unaffiliated = registered_user(&state, &[UserPermission::SubmitPlans]).await?;
    let reader = registered_user(
        &state,
        &[UserPermission::ManageAnyOrgs, UserPermission::SubmitPlans],
    )
    .await?;
    grant_user(&state, &w.member.1, &[UserPermission::SubmitPlans]).await?;
    grant_user(
        &state,
        &w.acme_admin.1,
        &[UserPermission::ManageOrgs, UserPermission::SubmitPlans],
    )
    .await?;
    grant_user(&state, &w.rival_member.1, &[UserPermission::SubmitPlans]).await?;
    grant_user(
        &state,
        &w.server.1,
        &[
            UserPermission::ManageAnyOrgs,
            UserPermission::ManageAnyClients,
            UserPermission::SubmitAnyPlans,
        ],
    )
    .await?;
    let absent = refusal(
        file_vault_plan(
            &state,
            &w.member,
            &device,
            &content_uri(NO_SUCH_RESOURCE),
            &w.acme,
            None,
        )
        .await?,
    )
    .await?;

    // Rows are filers; columns are the artifacts, in this order.
    let columns = [
        &w.member_res,
        &w.colleague_res,
        &w.acme_res,
        &w.rival_res,
        &w.rival_member_res,
    ];
    let expected: [(&str, &UserSigner, [bool; 5]); 6] = [
        //                                    member  colleague acme   rival  rival-member
        (
            "member of acme",
            &w.member,
            [true, false, true, false, false],
        ),
        (
            "acme admin",
            &w.acme_admin,
            [false, false, true, false, false],
        ),
        (
            "member of rival",
            &w.rival_member,
            [false, false, false, true, true],
        ),
        ("in no org", &unaffiliated, [false; 5]),
        ("manage_any_orgs in no org", &reader, [false; 5]),
        ("server admin", &w.server, [true; 5]),
    ];

    for (who, signer, row) in expected {
        for (resource_id, want) in columns.iter().zip(row) {
            let owner = owner_label(&state, resource_id).await?;
            let resp = file_vault_plan(
                &state,
                signer,
                &device,
                &content_uri(resource_id),
                &owner,
                None,
            )
            .await?;
            if want {
                assert_eq!(resp.status(), StatusCode::CREATED, "{who} on {resource_id}");
                continue;
            }
            let error = refusal(resp)
                .await
                .with_context(|| format!("{who} on {resource_id}"))?;
            assert_eq!(
                error,
                absent.replace(NO_SUCH_RESOURCE, resource_id),
                "{who} on {resource_id}"
            );
            assert!(!error.contains(&owner), "{who} on {resource_id}: {error}");
        }
    }
    Ok(())
}

/// The principal a plan names beside a resource outside the filer's reach.
#[derive(Debug, Clone, Copy)]
enum Named {
    Owner,
    Filer,
    FilerOrg,
}

/// Whatever principal a plan names beside it, a resource outside the filer's
/// reach answers as an absent one: never `belongs to`, and never its owner.
#[rstest]
#[case::its_owner(Named::Owner)]
#[case::the_filer(Named::Filer)]
#[case::the_filer_s_organization(Named::FilerOrg)]
#[tokio::test]
async fn a_refusal_outside_the_reach_names_no_owner(#[case] named: Named) -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let w = world(&state).await?;
    let (_, device) = owned_device(&state, &w.server, None).await?;
    grant_user(&state, &w.member.1, &[UserPermission::SubmitPlans]).await?;
    let principal = match named {
        Named::Owner => w.rival.clone(),
        Named::Filer => w.member.1.to_string(),
        Named::FilerOrg => w.acme.clone(),
    };

    let mut answers = Vec::new();
    for resource_id in [NO_SUCH_RESOURCE, w.rival_res.as_str()] {
        let resp = file_vault_plan(
            &state,
            &w.member,
            &device,
            &content_uri(resource_id),
            &principal,
            None,
        )
        .await?;
        answers.push(refusal(resp).await?);
    }
    let [absent, error] = <[String; 2]>::try_from(answers)
        .map_err(|answers| anyhow::anyhow!("two answers: {answers:?}"))?;
    assert_eq!(error, absent.replace(NO_SUCH_RESOURCE, &w.rival_res));
    assert!(
        !error.contains(&w.rival) && !error.contains("belongs to"),
        "{error}"
    );
    Ok(())
}

/// A door a plan comes in by.
#[derive(Debug, Clone, Copy)]
enum Door {
    Create,
    File,
    FileDryRun,
    Variants,
    Document,
}

/// Every door that files a plan holds it to the filer's reach, a dry run too:
/// the filer's organization's resource files, and another organization's is
/// refused as absent, files nothing, and leaves the campaign where it was.
#[rstest]
#[tokio::test]
async fn every_door_holds_a_plan_to_the_filer_s_reach(
    #[values(
        Door::Create,
        Door::File,
        Door::FileDryRun,
        Door::Variants,
        Door::Document
    )]
    door: Door,
    #[values(true, false)] in_reach: bool,
) -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let w = world(&state).await?;
    let (_, device) = owned_device(&state, &w.server, None).await?;
    let filer = &w.member;
    grant_user(&state, &filer.1, FULL_EDITOR).await?;
    let (resource_id, owner) = if in_reach {
        (&w.acme_res, &w.acme)
    } else {
        (&w.rival_res, &w.rival)
    };
    let model = vault_model(&content_uri(resource_id), owner, None);
    let document = model_document(&device, &model);
    let campaign = match door {
        Door::File | Door::FileDryRun | Door::Variants | Door::Document => Some(
            created_campaign_from(
                &state,
                filer,
                &[("name", "Vault")],
                &model_document(&device, PUBLIC_MODEL),
            )
            .await?,
        ),
        Door::Create => None,
    };
    let plans = plan_count(&state).await?;

    let (uri, body, filed) = match (door, &campaign) {
        (Door::Create, _) => (
            "/api/v1/campaigns?name=Vault".to_string(),
            document,
            StatusCode::CREATED,
        ),
        (Door::File, Some((id, _))) => (
            format!("/api/v1/campaigns/{id}/plans"),
            document,
            StatusCode::CREATED,
        ),
        (Door::FileDryRun, Some((id, _))) => (
            format!("/api/v1/campaigns/{id}/plans?dry_run=true"),
            document,
            StatusCode::OK,
        ),
        (Door::Variants, Some((id, head))) => (
            format!("/api/v1/campaigns/{id}/variants?base_plan_id={head}"),
            model_variant(&device, &model),
            StatusCode::CREATED,
        ),
        (Door::Document, Some((id, head))) => (
            format!("/api/v1/campaigns/{id}/document?base_plan_id={head}"),
            document,
            StatusCode::CREATED,
        ),
        (door, None) => anyhow::bail!("{door:?} files into a campaign"),
    };
    let resp = match door {
        Door::Document => put_plan_document(&state, filer, &uri, &body).await?,
        Door::Create | Door::File | Door::FileDryRun | Door::Variants => {
            post_plan_document(&state, filer, &uri, &body).await?
        }
    };

    if in_reach {
        let status = resp.status();
        assert_eq!(status, filed, "{door:?}: {}", text(resp).await?);
        return Ok(());
    }
    let error = refusal(resp).await.with_context(|| format!("{door:?}"))?;
    assert!(
        error.contains(&format!("vault resource {resource_id} does not exist")),
        "{door:?}: {error}"
    );
    assert!(!error.contains(owner.as_str()), "{door:?}: {error}");
    assert_eq!(plan_count(&state).await?, plans, "{door:?} filed a plan");
    if let Some((id, head)) = &campaign {
        assert_eq!(
            head_of(&state, id).await?.as_ref(),
            Some(head),
            "{door:?} moved the campaign"
        );
    }
    Ok(())
}

/// A revision checks the whole document it files under the reviser's reach,
/// so a head that a server admin filed naming another organization's resource
/// is not revised by the campaign's owner while it names it.
#[tokio::test]
async fn a_revision_rechecks_what_the_head_names() -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let w = world(&state).await?;
    let (_, device) = owned_device(&state, &w.server, None).await?;
    grant_user(&state, &w.member.1, FULL_EDITOR).await?;
    grant_user(
        &state,
        &w.server.1,
        &[
            UserPermission::ManageAnyOrgs,
            UserPermission::ManageAnyClients,
            UserPermission::SubmitAnyPlans,
        ],
    )
    .await?;
    let (campaign_id, _) = created_campaign_from(
        &state,
        &w.member,
        &[("name", "Vault")],
        &model_document(&device, PUBLIC_MODEL),
    )
    .await?;
    let rival = vault_model(&content_uri(&w.rival_res), &w.rival, None);
    let resp = post_plan_document(
        &state,
        &w.server,
        &format!("/api/v1/campaigns/{campaign_id}/plans"),
        &model_document(&device, &rival),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let head = PlanId::try_new(json_str(&body_json(resp).await?, "plan_id")?)?;

    let resp = post_plan_document(
        &state,
        &w.member,
        &format!("/api/v1/campaigns/{campaign_id}/variants?base_plan_id={head}"),
        &model_variant(&device, PUBLIC_MODEL),
    )
    .await?;
    let error = refusal(resp).await?;
    assert!(
        error.contains(&format!("vault resource {} does not exist", w.rival_res)),
        "{error}"
    );
    assert!(!error.contains(&w.rival), "{error}");
    assert_eq!(head_of(&state, &campaign_id).await?, Some(head));
    Ok(())
}

/// A vault store that answers every read with a fault, standing in for an
/// object store that is down.
struct FailingVault;

#[async_trait::async_trait]
impl VaultStore for FailingVault {
    async fn get_resource(
        &self,
        _resource_id: &VaultResourceId,
    ) -> anyhow::Result<Option<VaultResource>> {
        anyhow::bail!("s3://example-vault-bucket/vault: connection refused")
    }
    async fn put_resource(&self, _resource: &VaultResource) -> anyhow::Result<()> {
        anyhow::bail!("down")
    }
    async fn list_resources(
        &self,
        _after: Option<&VaultResourceId>,
        _limit: std::num::NonZeroUsize,
    ) -> anyhow::Result<VaultResourcePage> {
        anyhow::bail!("down")
    }
    async fn delete_resource(&self, _resource_id: &VaultResourceId) -> anyhow::Result<()> {
        anyhow::bail!("down")
    }
    async fn stat_blob(&self, _key: &VaultObjectKey) -> anyhow::Result<Option<u64>> {
        anyhow::bail!("s3://example-vault-bucket/vault-resources/org_x/blobs: connection refused")
    }
    async fn delete_blob(&self, _key: &VaultObjectKey) -> anyhow::Result<()> {
        anyhow::bail!("not used")
    }
    async fn put_blob(
        &self,
        _key: &VaultObjectKey,
        _sha256: &Sha256,
        _body: BlobBody,
    ) -> anyhow::Result<BlobWrite> {
        anyhow::bail!("down")
    }
    async fn open_blob(
        &self,
        _key: &VaultObjectKey,
        _from: u64,
    ) -> anyhow::Result<Option<ArtifactBody>> {
        anyhow::bail!("down")
    }
    async fn put_staged_blob(
        &self,
        _owner: &pipette_core::vault::VaultOwner,
        _body: BlobBody,
        _limit: std::num::NonZeroU64,
    ) -> anyhow::Result<pipette_store::StagedWrite> {
        anyhow::bail!("down")
    }
    async fn get_import(
        &self,
        _owner: &pipette_core::vault::VaultOwner,
        _import: &Sha256,
    ) -> anyhow::Result<Option<pipette_core::vault::VaultImport>> {
        anyhow::bail!("down")
    }
    async fn put_import(
        &self,
        _owner: &pipette_core::vault::VaultOwner,
        _import: &Sha256,
        _entry: &pipette_core::vault::VaultImport,
    ) -> anyhow::Result<()> {
        anyhow::bail!("down")
    }
}

/// A store fault while a vault reference is being checked is a `500`, not a
/// rejection: the caller did nothing wrong, and the body must not carry the
/// store's own error text.
#[tokio::test]
async fn a_store_fault_during_the_vault_check_is_a_redacted_500() -> anyhow::Result<()> {
    let (_dir, mut state) = vault_state().await?;
    let admin = server_admin(&state).await?;
    let submitter = registered_user(&state, &[UserPermission::SubmitPlans]).await?;
    let (_, device) = owned_device(&state, &admin, None).await?;
    state.vault_store = std::sync::Arc::new(FailingVault);

    let resp = file_vault_plan(
        &state,
        &submitter,
        &device,
        &content_uri("vault_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e"),
        "org_0195c3e2",
        None,
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let error = body_json(resp).await?;
    let error = json_str(&error, "error")?;
    assert!(
        !error.contains("s3://") && !error.contains("connection refused"),
        "the store's error must not reach the caller: {error}"
    );
    Ok(())
}

/// A store fault while a file is uploaded or described is a `500` whose body
/// carries none of the store's own text: that text names buckets and keys.
#[tokio::test]
async fn a_store_fault_during_an_upload_is_a_redacted_500() -> anyhow::Result<()> {
    let (_dir, mut state) = vault_state().await?;
    let me = operator(&state).await?;
    state.vault_store = std::sync::Arc::new(FailingVault);
    let sha256 = sha256_hex(BLOB);

    let answers = [
        put_vault_blob(&state, &me, me.1.as_str(), &sha256, BLOB).await?,
        head_vault_blob(&state, &me, me.1.as_str(), &sha256).await?,
        describe(&state, &me, None, &sha256).await?,
    ];
    let bodies: Vec<(StatusCode, String)> = futures::stream::iter(answers)
        .then(|resp| async move {
            let status = resp.status();
            anyhow::Ok((status, text(resp).await?))
        })
        .try_collect()
        .await?;
    assert!(
        bodies
            .iter()
            .all(|(status, _)| *status == StatusCode::INTERNAL_SERVER_ERROR),
        "{bodies:?}"
    );
    let leaked: Vec<&str> = ["s3://", "connection refused", "vault-resources", "/blobs"]
        .into_iter()
        .filter(|internal| bodies.iter().any(|(_, body)| body.contains(internal)))
        .collect();
    assert!(leaked.is_empty(), "{leaked:?} in {bodies:?}");
    Ok(())
}

// ── what the app reads ──────────────────────────────────────────────────────

fn listed(body: &Value) -> anyhow::Result<Vec<String>> {
    Ok(body
        .pointer("/items")
        .and_then(Value::as_array)
        .context("the listing must carry items")?
        .iter()
        .filter_map(|item| item.get("resource_id").and_then(Value::as_str))
        .map(str::to_string)
        .collect())
}

/// `order=newest` pages from the newest record down, and its cursor bounds the
/// next page from above, so it holds after its own record is deleted. Unasked,
/// the order is ascending.
#[tokio::test]
async fn the_newest_come_first_when_asked() -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let me = operator(&state).await?;
    let stranger = operator(&state).await?;
    let sha256 = upload_vault_blob(&state, &me, me.1.as_str(), BLOB).await?;
    let first = describe_gguf_text_ok(&state, &me, None, &sha256).await?;
    own_resource(&state, &stranger).await?;
    let second = describe_gguf_text_ok(&state, &me, None, &sha256).await?;
    let third = describe_gguf_text_ok(&state, &me, None, &sha256).await?;
    let read = async |query: &str| {
        let resp = signed_empty(
            &state,
            &me,
            "GET",
            &format!("/api/v1/vault/resources?{query}"),
        )
        .await?;
        anyhow::ensure!(
            resp.status() == StatusCode::OK,
            "{query}: {}",
            resp.status()
        );
        body_json(resp).await
    };

    let page = read("order=newest&limit=2").await?;
    assert_eq!(listed(&page)?, [third.clone(), second.clone()]);
    assert_eq!(fields(&page)?, ["items", "next_cursor", "unreadable"]);
    crate::api_schema::assert_conforms("NamedVaultResourceDetailPage", &page)?;
    let cursor = json_str(&page, "next_cursor")?.to_string();
    assert_eq!(cursor, second);

    let rest = read(&format!("order=newest&limit=2&cursor={cursor}")).await?;
    assert_eq!(listed(&rest)?, std::slice::from_ref(&first));
    assert_eq!(rest["next_cursor"], Value::Null, "the last page");

    let resp = signed_empty(
        &state,
        &me,
        "DELETE",
        &format!("/api/v1/vault/resources/{second}"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let after_delete = read(&format!("order=newest&cursor={cursor}")).await?;
    assert_eq!(listed(&after_delete)?, std::slice::from_ref(&first));

    assert_eq!(listed(&read("").await?)?, [first.clone(), third.clone()]);
    assert_eq!(listed(&read("order=oldest").await?)?, [first, third]);
    Ok(())
}

/// Newest first, the unreadable records are counted on the first page alone,
/// so the pages still add up to the keyspace's count.
#[tokio::test]
async fn the_newest_first_count_the_unreadable_once() -> anyhow::Result<()> {
    let (dir, state) = vault_state().await?;
    let me = operator(&state).await?;
    let (first, _) = own_resource(&state, &me).await?;
    let (second, _) = own_resource(&state, &me).await?;
    std::fs::write(
        dir.path()
            .join("vault")
            .join("vault_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e.json"),
        b"{ this is not a record",
    )?;

    let page = body_json(
        signed_empty(
            &state,
            &me,
            "GET",
            "/api/v1/vault/resources?order=newest&limit=1",
        )
        .await?,
    )
    .await?;
    assert_eq!(listed(&page)?, [second]);
    assert_eq!(page["unreadable"], 1);
    let cursor = json_str(&page, "next_cursor")?;
    let rest = body_json(
        signed_empty(
            &state,
            &me,
            "GET",
            &format!("/api/v1/vault/resources?order=newest&cursor={cursor}"),
        )
        .await?,
    )
    .await?;
    assert_eq!(listed(&rest)?, [first]);
    assert_eq!(rest["unreadable"], 0);
    Ok(())
}

/// `names=true` names the owner of every row served, and no other: an
/// account by its address, an organization by its handle. Unasked, the body
/// carries no `names`.
#[tokio::test]
async fn names_are_the_served_rows_owners() -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let w = world(&state).await?;
    let member = state
        .user_store
        .get_user(&w.member.1)
        .await?
        .context("the member exists")?;
    let read = async |signer: &UserSigner, query: &str| {
        body_json(
            signed_empty(
                &state,
                signer,
                "GET",
                &format!("/api/v1/vault/resources?{query}"),
            )
            .await?,
        )
        .await
    };

    let page = read(&w.server, &format!("names=true&org_id={}", w.acme)).await?;
    assert_eq!(listed(&page)?, std::slice::from_ref(&w.acme_res));
    assert_eq!(page["names"], json!({ w.acme.as_str(): "acme" }));
    crate::api_schema::assert_conforms("NamedVaultResourceDetailPage", &page)?;

    let page = read(&w.member, "names=true&owner=mine").await?;
    assert_eq!(listed(&page)?, std::slice::from_ref(&w.member_res));
    assert_eq!(
        page["names"],
        json!({ w.member.1.as_str(): member.contact_email.to_string() })
    );

    // A filter naming an owner outside the reach serves no row, so names nobody.
    let page = read(&w.member, &format!("names=true&org_id={}", w.rival)).await?;
    assert_eq!(page["names"], json!({}));

    let page = read(&w.server, "").await?;
    assert!(page.get("names").is_none(), "{page}");

    let record = body_json(
        signed_empty(
            &state,
            &w.member,
            "GET",
            &format!("/vault/resources/{}?names=true", w.acme_res),
        )
        .await?,
    )
    .await?;
    assert_eq!(record["names"], json!({ w.acme.as_str(): "acme" }));
    crate::api_schema::assert_conforms("NamedVaultResourceDetail", &record)?;
    Ok(())
}

/// `object=true` says whether each file is still stored, from a stat of it:
/// present, resized behind the record, or missing. On the record too.
#[tokio::test]
async fn the_file_state_follows_the_store() -> anyhow::Result<()> {
    let (dir, state) = vault_state().await?;
    let me = operator(&state).await?;
    let (resource_id, sha256) = own_resource(&state, &me).await?;
    let record_uri = format!("/vault/resources/{resource_id}?object=true");
    let states = async || -> anyhow::Result<(Value, Value)> {
        let page = body_json(
            signed_empty(&state, &me, "GET", "/api/v1/vault/resources?object=true").await?,
        )
        .await?;
        crate::api_schema::assert_conforms("NamedVaultResourceDetailPage", &page)?;
        let record = body_json(signed_empty(&state, &me, "GET", &record_uri).await?).await?;
        crate::api_schema::assert_conforms("NamedVaultResourceDetail", &record)?;
        let row = page.pointer("/items/0/object").cloned();
        Ok((row.context("one row")?, record["object"].clone()))
    };

    let present = json!({ "state": "present" });
    assert_eq!(states().await?, (present.clone(), present));

    let stored = blob_path(dir.path(), me.1.as_str(), &sha256);
    std::fs::write(&stored, b"GGUF but longer than it was")?;
    let resized = json!({ "state": "resized", "bytes": 27 });
    assert_eq!(states().await?, (resized.clone(), resized));

    std::fs::remove_file(&stored)?;
    let missing = json!({ "state": "missing" });
    assert_eq!(states().await?, (missing.clone(), missing));

    let plain =
        body_json(signed_empty(&state, &me, "GET", "/api/v1/vault/resources").await?).await?;
    let item = plain.pointer("/items/0").context("one row")?;
    assert!(item.get("object").is_none(), "{item}");
    Ok(())
}

/// The local store, but a stat of any file fails, as an object store that
/// stops answering `HEAD`s would.
struct StatFailing(std::sync::Arc<dyn VaultStore>);

#[async_trait::async_trait]
impl VaultStore for StatFailing {
    async fn get_resource(
        &self,
        resource_id: &VaultResourceId,
    ) -> anyhow::Result<Option<VaultResource>> {
        self.0.get_resource(resource_id).await
    }
    async fn put_resource(&self, resource: &VaultResource) -> anyhow::Result<()> {
        self.0.put_resource(resource).await
    }
    async fn list_resources(
        &self,
        after: Option<&VaultResourceId>,
        limit: std::num::NonZeroUsize,
    ) -> anyhow::Result<VaultResourcePage> {
        self.0.list_resources(after, limit).await
    }
    async fn delete_resource(&self, resource_id: &VaultResourceId) -> anyhow::Result<()> {
        self.0.delete_resource(resource_id).await
    }
    async fn stat_blob(&self, _key: &VaultObjectKey) -> anyhow::Result<Option<u64>> {
        anyhow::bail!("s3://example-vault-bucket/vault-resources/blobs: connection refused")
    }
    async fn delete_blob(&self, _key: &VaultObjectKey) -> anyhow::Result<()> {
        anyhow::bail!("not used")
    }
    async fn put_blob(
        &self,
        key: &VaultObjectKey,
        sha256: &Sha256,
        body: BlobBody,
    ) -> anyhow::Result<BlobWrite> {
        self.0.put_blob(key, sha256, body).await
    }
    async fn open_blob(
        &self,
        key: &VaultObjectKey,
        from: u64,
    ) -> anyhow::Result<Option<ArtifactBody>> {
        self.0.open_blob(key, from).await
    }
    async fn put_staged_blob(
        &self,
        owner: &pipette_core::vault::VaultOwner,
        body: BlobBody,
        limit: std::num::NonZeroU64,
    ) -> anyhow::Result<pipette_store::StagedWrite> {
        self.0.put_staged_blob(owner, body, limit).await
    }
    async fn get_import(
        &self,
        owner: &pipette_core::vault::VaultOwner,
        import: &Sha256,
    ) -> anyhow::Result<Option<pipette_core::vault::VaultImport>> {
        self.0.get_import(owner, import).await
    }
    async fn put_import(
        &self,
        owner: &pipette_core::vault::VaultOwner,
        import: &Sha256,
        entry: &pipette_core::vault::VaultImport,
    ) -> anyhow::Result<()> {
        self.0.put_import(owner, import, entry).await
    }
}

/// A fault behind a name or a file's state costs its cell, not the listing:
/// the row's `object` reads `unknown` and the owner is left out of `names`. The
/// record, which has one file to show, is a redacted `500` instead.
#[tokio::test]
async fn a_store_fault_costs_the_listing_a_cell() -> anyhow::Result<()> {
    let (dir, mut state) = vault_state().await?;
    let admin = server_admin(&state).await?;
    let me = operator(&state).await?;
    let (resource_id, _) = own_resource(&state, &me).await?;
    // The owner's record no longer parses, so its address does not read.
    std::fs::write(
        dir.path().join("users").join(format!("{}.json", me.1)),
        b"{ not a user",
    )?;
    state.vault_store = std::sync::Arc::new(StatFailing(state.vault_store.clone()));

    let resp = signed_empty(
        &state,
        &admin,
        "GET",
        "/api/v1/vault/resources?names=true&object=true",
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let page = body_json(resp).await?;
    assert_eq!(listed(&page)?, std::slice::from_ref(&resource_id));
    assert_eq!(
        page.pointer("/items/0/object"),
        Some(&json!({"state": "unknown"}))
    );
    assert_eq!(page["names"], json!({}));
    crate::api_schema::assert_conforms("NamedVaultResourceDetailPage", &page)?;

    let named = signed_empty(
        &state,
        &admin,
        "GET",
        &format!("/vault/resources/{resource_id}?names=true"),
    )
    .await?;
    assert_eq!(named.status(), StatusCode::OK);
    assert_eq!(body_json(named).await?["names"], json!({}));

    let record = signed_empty(
        &state,
        &admin,
        "GET",
        &format!("/vault/resources/{resource_id}?object=true"),
    )
    .await?;
    assert_eq!(record.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = text(record).await?;
    assert!(
        !body.contains("s3://") && !body.contains("connection refused"),
        "the store's error must not reach the caller: {body}"
    );
    Ok(())
}

/// A device reading a record it may fetch is served the record alone, whatever
/// it asks: no stat for it to cost, and no account named to it.
#[tokio::test]
async fn a_device_read_takes_neither_names_nor_the_file_state() -> anyhow::Result<()> {
    let (_dir, state) = vault_state().await?;
    let admin = server_admin(&state).await?;
    let me = operator(&state).await?;
    let (resource_id, _) = own_resource(&state, &me).await?;
    let (key, client_id) = owned_device(&state, &admin, Some(user_owner(&me.1))).await?;

    let resp = authed_get(
        &state,
        &key,
        &client_id,
        &format!("/vault/resources/{resource_id}?names=true&object=true"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert!(body.get("names").is_none(), "{body}");
    assert!(body.get("object").is_none(), "{body}");
    Ok(())
}
