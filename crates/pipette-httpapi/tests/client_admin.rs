//! Device administration end to end: the writes on
//! `/api/v1/clients/{client_id}` and the bulk assignment at
//! `PUT /api/v1/orgs/{org}/clients`.

use anyhow::Context;
use axum::http::StatusCode;
use futures::TryStreamExt;
use rstest::rstest;
use serde_json::{Value, json};

use pipette_core::client::ClientStatus;
use pipette_core::client_permission::ClientPermissionSet;
use pipette_core::key_index::digest_of_client_id;
use pipette_httpapi::state::AppState;
use pipette_plan_types::{
    ClientId, ClientOwner, ClientPermission, OrgId, UserPermission, UserStatus,
};
use pipette_web_testkit::*;

async fn device_admin(state: &AppState) -> anyhow::Result<UserSigner> {
    registered_user(state, &[UserPermission::ManageAnyClients]).await
}

/// Administers every device and every organization's roster.
async fn fleet_admin(state: &AppState) -> anyhow::Result<UserSigner> {
    registered_user(
        state,
        &[
            UserPermission::ManageAnyClients,
            UserPermission::ManageAnyOrgs,
        ],
    )
    .await
}

/// The device as a no-op `PATCH` answers it.
async fn noop_patch(
    state: &AppState,
    admin: &UserSigner,
    client_id: &str,
) -> anyhow::Result<Value> {
    let resp = signed_json(
        state,
        admin,
        "PATCH",
        &format!("/api/v1/clients/{client_id}"),
        &json!({ "client_details": "test" }),
    )
    .await?;
    anyhow::ensure!(
        resp.status() == StatusCode::OK,
        "a no-op PATCH answered {}",
        resp.status()
    );
    body_json(resp).await
}

async fn stored_status(state: &AppState, client_id: &str) -> anyhow::Result<Option<ClientStatus>> {
    Ok(state
        .client_store
        .get_client(&ClientId::try_new(client_id)?)
        .await?
        .map(|c| c.status))
}

/// An approved device holding what a registration seeds, and no work right.
async fn registered_device(
    state: &AppState,
) -> anyhow::Result<(ed25519_dalek::SigningKey, ClientId)> {
    register_approve_and_grant(state, &[ClientPermission::SubmitResults]).await
}

async fn stored_permissions(state: &AppState, client_id: &ClientId) -> anyhow::Result<Vec<String>> {
    let stored = state.client_store.get_client_permissions(client_id).await?;
    Ok(stored
        .unwrap_or_default()
        .names()
        .into_iter()
        .map(str::to_string)
        .collect())
}

/// Refused before resolving what the write names: neither a fleet read nor the
/// roster grant is enough.
#[tokio::test]
async fn the_device_writes_are_gated_on_manage_clients() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let server_admin = fleet_admin(&state).await?;
    let acme = create_org(&state, &server_admin, "acme").await?;
    let rival = create_org(&state, &server_admin, "rival").await?;
    let (_, client_id) = register_pending(&state).await?;
    assign(&state, &server_admin, &acme, &[client_id.as_str()]).await?;
    state
        .todo_store
        .write_suspension(&client_id, chrono::Utc::now(), &job("job-stuck")?)
        .await?;
    let owner_before = state.org_store.get_ownership(&client_id).await?;
    assert!(owner_before.is_some(), "the device starts out in acme");

    // The widest *read* of the fleet, and the grant that administers every
    // organization's roster: neither writes a device.
    let reader = registered_user(&state, &[UserPermission::ReadAnyClients]).await?;
    let org_manager = registered_user(
        &state,
        &[
            UserPermission::ManageAnyOrgs,
            UserPermission::ReadAnyClients,
        ],
    )
    .await?;
    for (who, caller) in [
        ("a fleet reader", &reader),
        ("an org manager", &org_manager),
    ] {
        for (method, uri, body) in [
            (
                "PATCH",
                format!("/api/v1/clients/{client_id}"),
                json!({ "client_details": "renamed" }),
            ),
            (
                "POST",
                format!("/api/v1/clients/{client_id}/approve"),
                Value::Null,
            ),
            (
                "POST",
                format!("/api/v1/clients/{client_id}/reject"),
                Value::Null,
            ),
            (
                "POST",
                format!("/api/v1/clients/{client_id}/unsuspend"),
                Value::Null,
            ),
            (
                "DELETE",
                format!("/api/v1/clients/{client_id}"),
                Value::Null,
            ),
            (
                "PUT",
                format!("/api/v1/clients/{client_id}/permissions"),
                json!({ "permissions": ["perform_work"] }),
            ),
            (
                "PATCH",
                format!("/api/v1/clients/{client_id}/permissions"),
                json!({ "add": ["perform_work"] }),
            ),
            (
                "PUT",
                format!("/api/v1/clients/{client_id}/owner"),
                json!({ "owner": null }),
            ),
            // No such organization: the gate answers before the owner resolves.
            (
                "PUT",
                format!("/api/v1/clients/{client_id}/owner"),
                json!({ "owner": { "type": "org", "org_id": "org_nosuch" } }),
            ),
            (
                "PUT",
                format!("/api/v1/orgs/{rival}/clients"),
                json!({ "client_ids": [client_id.as_str()] }),
            ),
            // Nor does the path's organization resolve first: a 403, not a 404.
            (
                "PUT",
                "/api/v1/orgs/org_nosuch/clients".to_string(),
                json!({ "client_ids": [client_id.as_str()] }),
            ),
        ] {
            let resp = if body.is_null() {
                signed_empty(&state, caller, method, &uri).await?
            } else {
                signed_json(&state, caller, method, &uri, &body).await?
            };
            assert_eq!(
                resp.status(),
                StatusCode::FORBIDDEN,
                "{who}: {method} {uri} must refuse a caller without manage_clients"
            );
            assert_eq!(
                json_str(&body_json(resp).await?, "error")?,
                "user lacks the manage_clients permission",
                "{who}: {method} {uri} {body}"
            );
        }
    }

    let stored = state
        .client_store
        .get_client(&client_id)
        .await?
        .context("the device survives")?;
    assert_eq!(stored.status, ClientStatus::Pending);
    assert_eq!(stored.client_details.as_str(), "test");
    assert_eq!(
        state.org_store.get_ownership(&client_id).await?,
        owner_before
    );
    assert!(
        state
            .todo_store
            .read_suspension(&client_id)
            .await?
            .is_some(),
        "the suspension is still there"
    );
    assert_eq!(
        stored_permissions(&state, &client_id).await?,
        ["submit_results"]
    );
    Ok(())
}

/// The account's standing outranks every grant, `manage_any_clients` included.
#[rstest::rstest]
#[case::pending(
    Some(UserStatus::Pending),
    StatusCode::FORBIDDEN,
    "user is pending an operator's approval"
)]
#[case::disabled(Some(UserStatus::Disabled), StatusCode::FORBIDDEN, "user is disabled")]
// `None` deletes the record and keeps the key, so the request reaches the lookup.
#[case::gone(None, StatusCode::UNAUTHORIZED, "user not found")]
#[tokio::test]
async fn an_account_out_of_good_standing_is_refused_every_device_write(
    #[case] standing: Option<UserStatus>,
    #[case] status: StatusCode,
    #[case] refusal: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let server_admin = fleet_admin(&state).await?;
    let acme = create_org(&state, &server_admin, "acme").await?;
    let rival = create_org(&state, &server_admin, "rival").await?;
    let (_, client_id) = register_pending(&state).await?;
    assign(&state, &server_admin, &acme, &[client_id.as_str()]).await?;
    state
        .todo_store
        .write_suspension(&client_id, chrono::Utc::now(), &job("job-stuck")?)
        .await?;
    let owner_before = state.org_store.get_ownership(&client_id).await?;
    assert!(owner_before.is_some(), "the device starts out in acme");

    let caller = fleet_admin(&state).await?;
    match standing {
        Some(standing) => set_user_status(&state, &caller.1, standing).await?,
        None => state.user_store.delete_user(&caller.1).await?,
    }
    for (method, uri, body) in [
        (
            "PATCH",
            format!("/api/v1/clients/{client_id}"),
            json!({ "client_details": "renamed" }),
        ),
        (
            "POST",
            format!("/api/v1/clients/{client_id}/approve"),
            Value::Null,
        ),
        (
            "POST",
            format!("/api/v1/clients/{client_id}/reject"),
            Value::Null,
        ),
        (
            "POST",
            format!("/api/v1/clients/{client_id}/unsuspend"),
            Value::Null,
        ),
        (
            "DELETE",
            format!("/api/v1/clients/{client_id}"),
            Value::Null,
        ),
        (
            "PUT",
            format!("/api/v1/clients/{client_id}/permissions"),
            json!({ "permissions": ["perform_work"] }),
        ),
        (
            "PATCH",
            format!("/api/v1/clients/{client_id}/permissions"),
            json!({ "add": ["perform_work"] }),
        ),
        (
            "PUT",
            format!("/api/v1/clients/{client_id}/owner"),
            json!({ "owner": null }),
        ),
        (
            "PUT",
            format!("/api/v1/orgs/{rival}/clients"),
            json!({ "client_ids": [client_id.as_str()] }),
        ),
    ] {
        let resp = if body.is_null() {
            signed_empty(&state, &caller, method, &uri).await?
        } else {
            signed_json(&state, &caller, method, &uri, &body).await?
        };
        assert_eq!(resp.status(), status, "{method} {uri}");
        assert_eq!(
            json_str(&body_json(resp).await?, "error")?,
            refusal,
            "{method} {uri} must refuse on the account's status"
        );
    }

    let stored = state
        .client_store
        .get_client(&client_id)
        .await?
        .context("the device survives")?;
    assert_eq!(stored.status, ClientStatus::Pending);
    assert_eq!(stored.client_details.as_str(), "test");
    assert_eq!(
        state.org_store.get_ownership(&client_id).await?,
        owner_before
    );
    assert!(
        state
            .todo_store
            .read_suspension(&client_id)
            .await?
            .is_some(),
        "the suspension is still there"
    );
    assert_eq!(
        stored_permissions(&state, &client_id).await?,
        ["submit_results"],
        "no permission write landed"
    );

    // Admitted, the same caller's grants reach the device: the standing was
    // the only thing in the way. A deleted account has nothing to admit.
    if standing.is_none() {
        return Ok(());
    }
    set_user_status(&state, &caller.1, UserStatus::Active).await?;
    let resp = signed_empty(
        &state,
        &caller,
        "POST",
        &format!("/api/v1/clients/{client_id}/approve"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await?["changed"], true);
    Ok(())
}

/// Refused at the path before any lookup; else the fleet-scope unsuspend, which
/// skips the record, would answer `204`.
#[rstest]
#[case::rename("PATCH", "/api/v1/clients/a.b", json!({ "client_details": "renamed" }))]
#[case::approve("POST", "/api/v1/clients/a.b/approve", Value::Null)]
#[case::reject("POST", "/api/v1/clients/a.b/reject", Value::Null)]
#[case::unsuspend("POST", "/api/v1/clients/a.b/unsuspend", Value::Null)]
#[case::delete("DELETE", "/api/v1/clients/a.b", Value::Null)]
#[case::owner("PUT", "/api/v1/clients/a.b/owner", json!({ "owner": null }))]
#[case::set_permissions("PUT", "/api/v1/clients/a.b/permissions", json!({ "permissions": [] }))]
#[case::amend_permissions(
    "PATCH",
    "/api/v1/clients/a.b/permissions",
    json!({ "add": [], "remove": [] })
)]
#[tokio::test]
async fn a_client_id_outside_the_id_charset_is_a_400_on_every_device_write(
    #[case] method: &str,
    #[case] uri: &str,
    #[case] body: Value,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let admin = device_admin(&state).await?;

    let resp = if body.is_null() {
        signed_empty(&state, &admin, method, uri).await?
    } else {
        signed_json(&state, &admin, method, uri, &body).await?
    };
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        body_json(resp).await?,
        json!({"error": "client id contains '.'; allowed: [A-Za-z0-9_-]"})
    );
    Ok(())
}

/// Not axum's plain-text `415`/`422`, and nothing is written.
#[tokio::test]
async fn a_body_that_is_not_json_is_a_400_in_the_envelope() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let admin = fleet_admin(&state).await?;
    let acme = create_org(&state, &admin, "acme").await?;
    let (_, client_id) = register_and_approve(&state).await?;
    let (key, user_id, key_id) = &admin;

    // Each body would write if it arrived as JSON.
    let routes = [
        (
            "PATCH",
            format!("/api/v1/clients/{client_id}"),
            json!({ "client_details": "renamed" }),
        ),
        (
            "PUT",
            format!("/api/v1/clients/{client_id}/owner"),
            json!({ "owner": { "type": "org", "org_id": acme } }),
        ),
        (
            "PUT",
            format!("/api/v1/orgs/{acme}/clients"),
            json!({ "client_ids": [client_id.as_str()] }),
        ),
    ];
    // The message is axum's wording passed through, so only its presence is pinned.
    for (method, uri, valid) in &routes {
        for (content_type, body) in [
            (Some("application/json"), "{ not json".to_string()),
            (Some("text/plain"), valid.to_string()),
            (None, valid.to_string()),
        ] {
            let headers = user_auth_headers(key, user_id, key_id, method, uri)?;
            let resp = raw_request(&state, method, uri, &headers, content_type, body).await?;
            assert_eq!(
                resp.status(),
                StatusCode::BAD_REQUEST,
                "{method} {uri} as {content_type:?}"
            );
            let error = json_str(&body_json(resp).await?, "error")?.to_string();
            assert!(!error.is_empty(), "{method} {uri} as {content_type:?}");
        }
    }

    let stored = state
        .client_store
        .get_client(&client_id)
        .await?
        .context("the device survives")?;
    assert_eq!(stored.client_details.as_str(), "test");
    assert!(
        state.org_store.get_ownership(&client_id).await?.is_none(),
        "no assignment landed"
    );
    Ok(())
}

/// A repeat reports `changed: false` rather than refusing, so a retry whose
/// response was lost costs nothing. Each answer is the device as `PATCH`
/// answers it; a move adds the status it left.
#[tokio::test]
async fn approving_a_device_is_idempotent() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let admin = fleet_admin(&state).await?;
    let acme = create_org(&state, &admin, "acme").await?;
    let (_, client_id) = register_pending(&state).await?;
    assign(&state, &admin, &acme, &[client_id.as_str()]).await?;
    let uri = format!("/api/v1/clients/{client_id}/approve");

    let resp = signed_empty(&state, &admin, "POST", &uri).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(json_str(&body, "client_id")?, client_id.as_str());
    assert_eq!(body["changed"], true);
    assert_eq!(json_str(&body, "status")?, "approved");
    assert_eq!(json_str(&body, "previous_status")?, "pending");
    assert_eq!(json_str(&body, "client_details")?, "test");
    assert_eq!(body["owner"], json!({ "type": "org", "org_id": acme }));
    assert_eq!(
        stored_status(&state, client_id.as_str()).await?,
        Some(ClientStatus::Approved)
    );

    let resp = signed_empty(&state, &admin, "POST", &uri).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(json_str(&body, "client_id")?, client_id.as_str());
    assert_eq!(body["changed"], false);
    assert_eq!(json_str(&body, "status")?, "approved");
    assert!(
        body.get("previous_status").is_none(),
        "nothing moved, so there is no previous status to report"
    );

    assert_eq!(
        noop_patch(&state, &admin, client_id.as_str()).await?,
        body,
        "a no-op approval answers what a no-op PATCH does"
    );
    Ok(())
}

/// Matches `clients reject`: the verb for turning away a stranger must not
/// remove a machine that is doing work.
#[tokio::test]
async fn rejecting_deletes_a_pending_device_and_refuses_an_approved_one() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let admin = device_admin(&state).await?;

    let (_, working) = register_and_approve(&state).await?;
    let resp = signed_empty(
        &state,
        &admin,
        "POST",
        &format!("/api/v1/clients/{working}/reject"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert!(
        stored_status(&state, working.as_str()).await?.is_some(),
        "an approved device survives a reject"
    );

    let (_, pending) = register_pending(&state).await?;
    let resp = signed_empty(
        &state,
        &admin,
        "POST",
        &format!("/api/v1/clients/{pending}/reject"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert_eq!(stored_status(&state, pending.as_str()).await?, None);
    Ok(())
}

/// The `404` names the id, and the lookup comes before the empty-body check.
#[tokio::test]
async fn an_unknown_device_is_a_404_rather_than_a_write() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let admin = device_admin(&state).await?;

    for (method, uri, body) in [
        (
            "PATCH",
            "/api/v1/clients/ev1_nobody",
            json!({ "client_details": "x" }),
        ),
        // Names nothing, yet the missing device is what the caller hears.
        ("PATCH", "/api/v1/clients/ev1_nobody", json!({})),
        ("POST", "/api/v1/clients/ev1_nobody/approve", Value::Null),
        ("POST", "/api/v1/clients/ev1_nobody/reject", Value::Null),
    ] {
        let resp = if body.is_null() {
            signed_empty(&state, &admin, method, uri).await?
        } else {
            signed_json(&state, &admin, method, uri, &body).await?
        };
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "{method} {uri} {body}"
        );
        assert_eq!(
            json_str(&body_json(resp).await?, "error")?,
            "no client ev1_nobody",
            "{method} {uri} {body}"
        );
    }
    assert_eq!(stored_status(&state, "ev1_nobody").await?, None);
    Ok(())
}

/// A repeat, or a device never suspended, is a `200` with `changed: false`.
/// Each answer is the device as `PATCH` answers it.
#[tokio::test]
async fn unsuspending_clears_the_suspension_and_reports_whether_there_was_one() -> anyhow::Result<()>
{
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let admin = device_admin(&state).await?;
    let (_, client_id) = register_and_approve(&state).await?;
    let (_, never) = register_and_approve(&state).await?;
    state
        .todo_store
        .write_suspension(&client_id, chrono::Utc::now(), &job("job-stuck")?)
        .await?;
    let uri = format!("/api/v1/clients/{client_id}/unsuspend");
    let (key, user_id, key_id) = &admin;
    let row = format!("/api/v1/clients/{client_id}");
    let resp = authed_user_get(&state, key, user_id, key_id, &row).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let read = body_json(resp).await?;
    assert_eq!(read["suspended"], true, "the fleet read sees the marker");

    let resp = signed_empty(&state, &admin, "POST", &uri).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(json_str(&body, "client_id")?, client_id.as_str());
    assert_eq!(body["changed"], true);
    assert_eq!(json_str(&body, "status")?, "approved");
    assert!(
        state
            .todo_store
            .read_suspension(&client_id)
            .await?
            .is_none(),
        "the suspension is cleared"
    );
    let resp = authed_user_get(&state, key, user_id, key_id, &row).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let read = body_json(resp).await?;
    assert_eq!(read["suspended"], false, "the fleet read sees it cleared");
    assert_eq!(
        stored_status(&state, client_id.as_str()).await?,
        Some(ClientStatus::Approved),
        "the identity record is not this verb's concern"
    );

    for (id, why) in [
        (&client_id, "a repeat"),
        (&never, "a device never suspended"),
    ] {
        let resp = signed_empty(
            &state,
            &admin,
            "POST",
            &format!("/api/v1/clients/{id}/unsuspend"),
        )
        .await?;
        assert_eq!(resp.status(), StatusCode::OK, "{why}");
        let body = body_json(resp).await?;
        assert_eq!(json_str(&body, "client_id")?, id.as_str(), "{why}");
        assert_eq!(body["changed"], false, "{why} is a successful no-op");
        assert_eq!(
            noop_patch(&state, &admin, id.as_str()).await?,
            body,
            "{why} answers what a no-op PATCH does"
        );
    }
    Ok(())
}

/// Its own device clears; an unowned or another organization's is a `403`, and
/// an id with no record a `404`.
#[tokio::test]
async fn a_scoped_unsuspend_is_held_to_the_device_record() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let server_admin = fleet_admin(&state).await?;
    let acme = create_org(&state, &server_admin, "acme").await?;
    let rival = create_org(&state, &server_admin, "rival").await?;
    let org_admin = registered_user(
        &state,
        &[UserPermission::ManageClients, UserPermission::ReadClients],
    )
    .await?;
    let resp = signed_json(
        &state,
        &server_admin,
        "PUT",
        &format!("/api/v1/users/{}/org", org_admin.1),
        &json!({ "org_id": acme }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK, "the org admin joins acme");
    let unaffiliated = registered_user(
        &state,
        &[UserPermission::ManageClients, UserPermission::ReadClients],
    )
    .await?;

    let (_, ours) = register_and_approve(&state).await?;
    let (_, theirs) = register_and_approve(&state).await?;
    let (_, unowned) = register_and_approve(&state).await?;
    assign(&state, &server_admin, &acme, &[ours.as_str()]).await?;
    assign(&state, &server_admin, &rival, &[theirs.as_str()]).await?;
    // A marker with no device record behind it.
    let nobody = ClientId::try_new("ev1_nobody")?;
    for client_id in [&ours, &theirs, &unowned, &nobody] {
        state
            .todo_store
            .write_suspension(client_id, chrono::Utc::now(), &job("job-stuck")?)
            .await?;
    }

    let outside = "this device is outside your organization";
    for (who, caller, client_id, status, error) in [
        (
            "an org admin",
            &org_admin,
            &theirs,
            StatusCode::FORBIDDEN,
            outside,
        ),
        (
            "an org admin",
            &org_admin,
            &unowned,
            StatusCode::FORBIDDEN,
            outside,
        ),
        (
            "an org admin",
            &org_admin,
            &nobody,
            StatusCode::NOT_FOUND,
            "no client ev1_nobody",
        ),
        (
            "an unaffiliated admin",
            &unaffiliated,
            &unowned,
            StatusCode::FORBIDDEN,
            outside,
        ),
    ] {
        let uri = format!("/api/v1/clients/{client_id}/unsuspend");
        let resp = signed_empty(&state, caller, "POST", &uri).await?;
        assert_eq!(resp.status(), status, "{who}: {uri}");
        assert_eq!(
            json_str(&body_json(resp).await?, "error")?,
            error,
            "{who}: {uri}"
        );
        assert!(
            state.todo_store.read_suspension(client_id).await?.is_some(),
            "{who}: {uri} must leave the suspension in place"
        );
    }

    let uri = format!("/api/v1/clients/{ours}/unsuspend");
    for changed in [true, false] {
        let resp = signed_empty(&state, &org_admin, "POST", &uri).await?;
        assert_eq!(resp.status(), StatusCode::OK, "{uri} changed: {changed}");
        let body = body_json(resp).await?;
        assert_eq!(json_str(&body, "client_id")?, ours.as_str());
        assert_eq!(body["changed"], changed, "{uri}");
        assert_eq!(body["owner"], json!({ "type": "org", "org_id": acme }));
    }
    assert!(
        state.todo_store.read_suspension(&ours).await?.is_none(),
        "{uri} clears the suspension"
    );
    Ok(())
}

/// Without consulting the record, so a stranded marker needs no shell access;
/// with no device to answer with, it and its repeat are `204`, not `404`.
#[tokio::test]
async fn the_fleet_scope_clears_a_suspension_no_device_record_answers_to() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let admin = device_admin(&state).await?;
    let gone = ClientId::try_new("ev1_gone")?;
    state
        .todo_store
        .write_suspension(&gone, chrono::Utc::now(), &job("job-stuck")?)
        .await?;

    let (state, admin) = (&state, &admin);
    futures::stream::iter(["the clearing", "a repeat"].map(anyhow::Ok))
        .try_for_each(|attempt| async move {
            let resp =
                signed_empty(state, admin, "POST", "/api/v1/clients/ev1_gone/unsuspend").await?;
            assert_eq!(resp.status(), StatusCode::NO_CONTENT, "{attempt}");
            Ok(())
        })
        .await?;
    assert!(
        state.todo_store.read_suspension(&gone).await?.is_none(),
        "the stranded marker is cleared"
    );
    assert_eq!(
        stored_status(state, "ev1_gone").await?,
        None,
        "clearing a marker invents no device"
    );
    Ok(())
}

/// It also reports whether anything moved, and refuses a body naming nothing.
#[tokio::test]
async fn an_update_writes_only_what_it_names() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let admin = device_admin(&state).await?;
    let (_, client_id) = register_and_approve(&state).await?;
    let uri = format!("/api/v1/clients/{client_id}");

    let resp = signed_json(&state, &admin, "PATCH", &uri, &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let body = body_json(
        signed_json(
            &state,
            &admin,
            "PATCH",
            &uri,
            &json!({ "client_details": "rack 3, shelf 2" }),
        )
        .await?,
    )
    .await?;
    assert_eq!(body["changed"], true);
    assert_eq!(json_str(&body, "client_details")?, "rack 3, shelf 2");
    // Untouched fields survive, and the status is not a detail this route sets.
    assert_eq!(json_str(&body, "status")?, "approved");

    let body = body_json(
        signed_json(
            &state,
            &admin,
            "PATCH",
            &uri,
            &json!({ "client_details": "rack 3, shelf 2" }),
        )
        .await?,
    )
    .await?;
    assert_eq!(body["changed"], false);
    Ok(())
}

/// Each lands in its own field, answered and stored; `client_details` stays.
#[tokio::test]
async fn an_update_writes_the_organization_label_and_contact_email() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let admin = device_admin(&state).await?;
    let (_, client_id) = register_and_approve(&state).await?;

    let resp = signed_json(
        &state,
        &admin,
        "PATCH",
        &format!("/api/v1/clients/{client_id}"),
        &json!({ "organization": "acme lab", "contact_email": "ops@example.test" }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["changed"], true);
    assert_eq!(json_str(&body, "organization")?, "acme lab");
    assert_eq!(json_str(&body, "contact_email")?, "ops@example.test");
    assert_eq!(json_str(&body, "client_details")?, "test");

    let stored = state
        .client_store
        .get_client(&client_id)
        .await?
        .context("the device survives")?;
    assert_eq!(stored.organization.as_str(), "acme lab");
    assert_eq!(stored.contact_email.as_str(), "ops@example.test");
    assert_eq!(stored.client_details.as_str(), "test");
    Ok(())
}

/// Matches `clients delete`: an absent device is a `204`, so a teardown that
/// stopped part-way is finished by repeating the call.
#[tokio::test]
async fn a_delete_removes_the_identity_and_is_idempotent() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let admin = device_admin(&state).await?;
    let (_, client_id) = register_and_approve(&state).await?;
    let uri = format!("/api/v1/clients/{client_id}");
    let (key, user_id, key_id) = &admin;

    let resp = authed_user_delete(&state, key, user_id, key_id, &uri).await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert_eq!(stored_status(&state, client_id.as_str()).await?, None);

    let resp = authed_user_delete(&state, key, user_id, key_id, &uri).await?;
    assert_eq!(
        resp.status(),
        StatusCode::NO_CONTENT,
        "a second delete converges rather than 404ing"
    );
    Ok(())
}

/// Both free the keypair claim and drop the ownership; a delete also purges the
/// device's queue state.
#[tokio::test]
async fn a_reject_or_a_delete_frees_the_keypair_ownership_and_queue_state() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let admin = fleet_admin(&state).await?;
    let acme = create_org(&state, &admin, "acme").await?;
    let (_, pending) = register_pending(&state).await?;
    let (_, working) = register_and_approve(&state).await?;
    assign(&state, &admin, &acme, &[pending.as_str(), working.as_str()]).await?;
    state
        .todo_store
        .write_suspension(&working, chrono::Utc::now(), &job("job-1")?)
        .await?;
    state.todo_store.write_pending_reindex(&working).await?;
    // Read back through the same calls the purge is checked with below.
    assert!(state.todo_store.read_suspension(&working).await?.is_some());
    assert!(
        state
            .todo_store
            .list_pending_reindex()
            .await?
            .iter()
            .any(|(c, _)| c == &working),
        "the pending-reindex flag is listed"
    );

    for (client_id, method, uri) in [
        (
            &pending,
            "POST",
            format!("/api/v1/clients/{pending}/reject"),
        ),
        (&working, "DELETE", format!("/api/v1/clients/{working}")),
    ] {
        let digest =
            digest_of_client_id(client_id).context("a registered id carries its key digest")?;
        assert!(
            state
                .key_index_store
                .holder_of_public_key(&digest)
                .await?
                .is_some(),
            "registration claims the keypair"
        );
        assert!(state.org_store.get_ownership(client_id).await?.is_some());

        let resp = signed_empty(&state, &admin, method, &uri).await?;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT, "{method} {uri}");
        assert!(
            state
                .key_index_store
                .holder_of_public_key(&digest)
                .await?
                .is_none(),
            "{method} {uri} must release the keypair claim"
        );
        assert!(
            state.org_store.get_ownership(client_id).await?.is_none(),
            "{method} {uri} must drop the ownership"
        );
    }

    assert!(
        state.todo_store.read_suspension(&working).await?.is_none(),
        "a delete purges the suspension"
    );
    assert!(
        !state
            .todo_store
            .list_pending_reindex()
            .await?
            .iter()
            .any(|(c, _)| c == &working),
        "a delete purges the pending-reindex flags"
    );
    Ok(())
}

/// One unknown id leaves the whole rack alone rather than half-assigned. A
/// device named twice is assigned once, where it first appears.
#[tokio::test]
async fn a_bulk_assignment_is_all_or_nothing_on_resolution() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    // Assignment writes client records and names an organization, so it needs
    // both grants.
    let admin = registered_user(
        &state,
        &[
            UserPermission::ManageAnyClients,
            UserPermission::ManageAnyOrgs,
            UserPermission::ReadClients,
        ],
    )
    .await?;

    let org = body_json(
        signed_json(
            &state,
            &admin,
            "POST",
            "/api/v1/orgs",
            &json!({ "slug": "acme" }),
        )
        .await?,
    )
    .await?;
    let org_id = json_str(&org, "org_id")?.to_string();
    let uri = format!("/api/v1/orgs/{org_id}/clients");

    let (_, first) = register_and_approve(&state).await?;
    let (_, second) = register_and_approve(&state).await?;

    let resp = signed_json(
        &state,
        &admin,
        "PUT",
        &uri,
        &json!({ "client_ids": [first.as_str(), "ev1_nobody"] }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let listed =
        body_json(authed_user_get(&state, &admin.0, &admin.1, &admin.2, &uri).await?).await?;
    assert_eq!(
        listed["items"].as_array().map(Vec::len),
        Some(0),
        "a refused assignment writes nothing"
    );

    // The ids are random, so the greater one leads: a sorted dedupe would
    // answer them the other way round.
    let (lead, next) = if first > second {
        (&first, &second)
    } else {
        (&second, &first)
    };
    let body = body_json(
        signed_json(
            &state,
            &admin,
            "PUT",
            &uri,
            &json!({ "client_ids": [lead.as_str(), next.as_str(), lead.as_str()] }),
        )
        .await?,
    )
    .await?;
    assert_eq!(
        body["clients"].as_array().map(|rows| rows
            .iter()
            .filter_map(|row| row["client_id"].as_str())
            .collect::<Vec<_>>()),
        Some(vec![lead.as_str(), next.as_str()])
    );
    assert!(
        body["clients"]
            .as_array()
            .is_some_and(|rows| rows.iter().all(|row| row["changed"] == true))
    );

    let body = body_json(
        signed_json(
            &state,
            &admin,
            "PUT",
            &uri,
            &json!({ "client_ids": [first.as_str()] }),
        )
        .await?,
    )
    .await?;
    assert_eq!(body["clients"][0]["changed"], false);

    // An empty list is a refusal rather than a successful no-op, so a caller
    // that built the list wrong hears about it.
    let resp = signed_json(&state, &admin, "PUT", &uri, &json!({ "client_ids": [] })).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    Ok(())
}

/// The answer carries the minted id the owner is written under.
#[tokio::test]
async fn a_bulk_assignment_answers_the_minted_org_id() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let admin = fleet_admin(&state).await?;
    let org_id = create_org(&state, &admin, "acme").await?;
    let (_, client_id) = register_and_approve(&state).await?;

    let resp = signed_json(
        &state,
        &admin,
        "PUT",
        &format!("/api/v1/orgs/{org_id}/clients"),
        &json!({ "client_ids": [client_id.as_str()] }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(json_str(&body, "org_id")?, org_id, "{body}");
    assert_eq!(
        body["clients"],
        json!([{ "client_id": client_id.as_str(), "changed": true }])
    );
    assert_eq!(
        state.org_store.get_ownership(&client_id).await?,
        Some(ClientOwner::Org {
            org_id: OrgId::try_new(org_id)?
        })
    );
    Ok(())
}

/// Not the `404` of a well-formed id matching nothing, so a typo does not read
/// as a missing organization. Prefixes for the 400s: the charset wording
/// belongs to the validators' own tests.
#[rstest]
#[case::bad_id("org_bad.id", StatusCode::BAD_REQUEST, "organization id contains")]
#[case::bad_handle("bad.handle", StatusCode::BAD_REQUEST, "organization id contains")]
#[case::handle(
    "nosuch",
    StatusCode::BAD_REQUEST,
    "organization id must start with `org_`"
)]
#[case::missing("org_nosuch", StatusCode::NOT_FOUND, "no organization org_nosuch")]
#[tokio::test]
async fn a_malformed_organization_reference_is_a_400_on_the_bulk_assignment(
    #[case] reference: &str,
    #[case] status: StatusCode,
    #[case] error: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let admin = device_admin(&state).await?;
    let (_, client_id) = register_and_approve(&state).await?;

    let resp = signed_json(
        &state,
        &admin,
        "PUT",
        &format!("/api/v1/orgs/{reference}/clients"),
        &json!({ "client_ids": [client_id.as_str()] }),
    )
    .await?;
    assert_eq!(resp.status(), status, "{reference}");
    let body = body_json(resp).await?;
    let refused = json_str(&body, "error")?;
    if status == StatusCode::NOT_FOUND {
        assert_eq!(refused, error, "{reference}");
    } else {
        assert!(refused.starts_with(error), "{reference}: {refused}");
    }
    assert!(
        state.org_store.get_ownership(&client_id).await?.is_none(),
        "no assignment landed"
    );
    Ok(())
}

/// Refused whole: a check made mid-write would leave a rack half-assigned.
#[tokio::test]
async fn a_bulk_assignment_with_one_device_outside_the_scope_writes_none() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let server_admin = fleet_admin(&state).await?;
    let acme = create_org(&state, &server_admin, "acme").await?;
    let rival = create_org(&state, &server_admin, "rival").await?;
    let org_admin = registered_user(
        &state,
        &[UserPermission::ManageClients, UserPermission::ReadClients],
    )
    .await?;
    let resp = signed_json(
        &state,
        &server_admin,
        "PUT",
        &format!("/api/v1/users/{}/org", org_admin.1),
        &json!({ "org_id": acme }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK, "the org admin joins acme");
    // A member's own device rolls up to acme, so acme's admin may take it.
    let (_, member_owned) = register_pending(&state).await?;
    let resp = signed_json(
        &state,
        &server_admin,
        "PUT",
        &format!("/api/v1/clients/{member_owned}/owner"),
        &json!({ "owner": { "type": "user", "user_id": org_admin.1 } }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK, "a member owns the device");
    let member_owner = state.org_store.get_ownership(&member_owned).await?;
    let (_, theirs) = register_and_approve(&state).await?;
    assign(&state, &server_admin, &rival, &[theirs.as_str()]).await?;
    let theirs_owner = state.org_store.get_ownership(&theirs).await?;

    // The reachable device first, so a check made inside the write loop would
    // already have assigned it.
    let resp = signed_json(
        &state,
        &org_admin,
        "PUT",
        &format!("/api/v1/orgs/{acme}/clients"),
        &json!({ "client_ids": [member_owned.as_str(), theirs.as_str()] }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        json_str(&body_json(resp).await?, "error")?,
        "this device is outside your organization"
    );
    assert_eq!(
        state.org_store.get_ownership(&member_owned).await?,
        member_owner,
        "the reachable device is not assigned"
    );
    assert_eq!(
        state.org_store.get_ownership(&theirs).await?,
        theirs_owner,
        "the other organization keeps its device"
    );

    // Named alone, the reachable device lands: the rival one refused the call.
    assign(&state, &org_admin, &acme, &[member_owned.as_str()]).await?;
    assert_eq!(
        state.org_store.get_ownership(&member_owned).await?,
        Some(ClientOwner::Org {
            org_id: OrgId::try_new(acme.as_str())?
        })
    );
    Ok(())
}

#[tokio::test]
async fn an_org_admin_reaches_its_own_organizations_devices_only() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let server_admin = registered_user(
        &state,
        &[
            UserPermission::ManageAnyClients,
            UserPermission::ManageAnyOrgs,
            UserPermission::ReadClients,
        ],
    )
    .await?;

    let acme = create_org(&state, &server_admin, "acme").await?;
    let rival = create_org(&state, &server_admin, "rival").await?;

    let admin = registered_user(
        &state,
        &[UserPermission::ManageClients, UserPermission::ReadClients],
    )
    .await?;
    signed_json(
        &state,
        &server_admin,
        "PUT",
        &format!("/api/v1/users/{}/org", admin.1),
        &json!({ "org_id": acme }),
    )
    .await?;

    let (_, ours) = register_and_approve(&state).await?;
    let (_, theirs) = register_and_approve(&state).await?;
    let (_, unowned) = register_pending(&state).await?;
    assign(&state, &server_admin, &acme, &[ours.as_str()]).await?;
    assign(&state, &server_admin, &rival, &[theirs.as_str()]).await?;

    let resp = signed_json(
        &state,
        &admin,
        "PATCH",
        &format!("/api/v1/clients/{ours}"),
        &json!({ "client_details": "rack 1" }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);

    // Refused without saying who holds it.
    for (method, uri) in [
        ("PATCH", format!("/api/v1/clients/{theirs}")),
        ("DELETE", format!("/api/v1/clients/{theirs}")),
    ] {
        let (key, user_id, key_id) = &admin;
        let headers = user_auth_headers(key, user_id, key_id, method, &uri)?;
        let resp = json_request(
            &state,
            method,
            &uri,
            &headers,
            &json!({ "client_details": "x" }),
        )
        .await?;
        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "{method} {uri} must refuse another organization's device"
        );
        let error = json_str(&body_json(resp).await?, "error")?.to_string();
        assert!(
            !error.contains("rival"),
            "the refusal must not disclose which organization holds it: {error}"
        );
    }

    // An unowned device is the server's: an organization onboards through its
    // own pre-auth keys, so it cannot take another tenant's registration.
    let resp = signed_json(
        &state,
        &admin,
        "PUT",
        &format!("/api/v1/orgs/{acme}/clients"),
        &json!({ "client_ids": [unowned.as_str()] }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    for (method, uri) in [
        ("POST", format!("/api/v1/clients/{unowned}/approve")),
        ("POST", format!("/api/v1/clients/{unowned}/reject")),
        ("DELETE", format!("/api/v1/clients/{unowned}")),
    ] {
        let resp = signed_empty(&state, &admin, method, &uri).await?;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{method} {uri}");
    }
    assert_eq!(
        owner_org(&state, &server_admin, &acme).await?.len(),
        1,
        "the unowned device stays out of the organization"
    );

    // But it cannot hand a device to another organization, in either route.
    // The bulk route names the organization in its path, so another one reads
    // exactly as a missing one: an id cannot be probed for existence.
    for org in [rival.as_str(), "org_nosuch"] {
        let resp = signed_json(
            &state,
            &admin,
            "PUT",
            &format!("/api/v1/orgs/{org}/clients"),
            &json!({ "client_ids": [ours.as_str()] }),
        )
        .await?;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{org}");
        assert_eq!(
            json_str(&body_json(resp).await?, "error")?,
            format!("no organization {org}")
        );
    }
    let resp = signed_json(
        &state,
        &admin,
        "PUT",
        &format!("/api/v1/clients/{ours}/owner"),
        &json!({ "owner": { "type": "org", "org_id": rival } }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // Nor to a person outside its roster: the device would leave the
    // organization pointing at a principal the caller does not administer.
    let outsider = registered_user(&state, &[]).await?;
    let resp = signed_json(
        &state,
        &admin,
        "PUT",
        &format!("/api/v1/clients/{ours}/owner"),
        &json!({ "owner": { "type": "user", "user_id": outsider.1 } }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // Detaching outright, and handing to one of its own members, both stay
    // within the scope.
    let resp = signed_json(
        &state,
        &admin,
        "PUT",
        &format!("/api/v1/clients/{ours}/owner"),
        &json!({ "owner": { "type": "user", "user_id": admin.1 } }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = signed_json(
        &state,
        &admin,
        "PUT",
        &format!("/api/v1/clients/{ours}/owner"),
        &json!({ "owner": null }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    Ok(())
}

/// Only an explicit `null` detaches a device: `{}` or a misspelt `owner` is a
/// `400` and the owner stays, as a detached device leaves its organization.
#[tokio::test]
async fn a_device_is_detached_by_null_only() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let admin = registered_user(
        &state,
        &[
            UserPermission::ManageAnyClients,
            UserPermission::ManageAnyOrgs,
            UserPermission::ReadClients,
        ],
    )
    .await?;
    let acme = create_org(&state, &admin, "acme").await?;
    let (_, client_id) = register_and_approve(&state).await?;
    let uri = format!("/api/v1/clients/{client_id}/owner");
    let owner = json!({ "type": "org", "org_id": acme });
    let resp = signed_json(&state, &admin, "PUT", &uri, &json!({ "owner": owner })).await?;
    assert_eq!(resp.status(), StatusCode::OK);

    for (body, key) in [
        (json!({}), "owner"),
        (json!({ "ownr": owner }), "ownr"),
        (json!({ "org_id": acme }), "org_id"),
        // The tag an owner carried before `type`, which nothing reads now.
        (
            json!({ "owner": { "kind": "org", "org_id": acme } }),
            "`type`",
        ),
    ] {
        let resp = signed_json(&state, &admin, "PUT", &uri, &body).await?;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{body}");
        let error = json_str(&body_json(resp).await?, "error")?.to_string();
        assert!(error.contains(key), "the refusal names {key}: {error}");
        assert_eq!(
            owner_org(&state, &admin, &acme).await?,
            vec![client_id.to_string()],
            "a refused body leaves the device with its owner: {body}"
        );
    }

    let resp = signed_json(&state, &admin, "PUT", &uri, &json!({ "owner": null })).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(state.org_store.get_ownership(&client_id).await?, None);
    Ok(())
}

/// A scoped caller cannot confirm an organization or account id by naming it as
/// a device's owner; only the fleet scope hears that an owner is missing.
#[tokio::test]
async fn an_owner_move_does_not_confirm_an_owner_outside_the_scope() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let server_admin = registered_user(
        &state,
        &[
            UserPermission::ManageAnyClients,
            UserPermission::ManageAnyOrgs,
            UserPermission::ReadClients,
        ],
    )
    .await?;
    let acme = create_org(&state, &server_admin, "acme").await?;
    let rival = create_org(&state, &server_admin, "rival").await?;
    let admin = registered_user(
        &state,
        &[UserPermission::ManageClients, UserPermission::ReadClients],
    )
    .await?;
    signed_json(
        &state,
        &server_admin,
        "PUT",
        &format!("/api/v1/users/{}/org", admin.1),
        &json!({ "org_id": acme }),
    )
    .await?;
    let outsider = create_user(&state, "outsider@example.test").await?;
    let (_, ours) = register_and_approve(&state).await?;
    assign(&state, &server_admin, &acme, &[ours.as_str()]).await?;

    let real_org = json!({ "type": "org", "org_id": rival });
    let missing_org =
        json!({ "type": "org", "org_id": "org_00000000-0000-0000-0000-000000000000" });
    let real_user = json!({ "type": "user", "user_id": outsider.user_id });
    let missing_user =
        json!({ "type": "user", "user_id": "user_00000000-0000-0000-0000-000000000000" });
    let uri = format!("/api/v1/clients/{ours}/owner");

    // A real owner outside the scope and a made-up one read alike.
    futures::stream::iter(
        [
            (&real_org, &missing_org, "this move is outside"),
            (&real_user, &missing_user, "another person's"),
        ]
        .map(anyhow::Ok),
    )
    .try_for_each(|(real, missing, token)| {
        let (state, admin, uri) = (&state, &admin, &uri);
        async move {
            let resp = signed_json(state, admin, "PUT", uri, &json!({ "owner": real })).await?;
            assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{real}");
            let refused = body_json(resp).await?;
            assert!(json_str(&refused, "error")?.contains(token), "{refused}");
            let resp = signed_json(state, admin, "PUT", uri, &json!({ "owner": missing })).await?;
            assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{missing}");
            assert_eq!(
                body_json(resp).await?,
                refused,
                "{missing} must read as {real}"
            );
            Ok(())
        }
    })
    .await?;
    assert_eq!(
        owner_org(&state, &server_admin, &acme).await?,
        vec![ours.to_string()],
        "a refused move writes nothing"
    );

    // The fleet scope may hand a device anywhere, so it is told the owner is
    // missing rather than refused.
    for (missing, token) in [
        (&missing_org, "no organization"),
        (&missing_user, "no user"),
    ] {
        let resp = signed_json(
            &state,
            &server_admin,
            "PUT",
            &uri,
            &json!({ "owner": missing }),
        )
        .await?;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{missing}");
        let error = json_str(&body_json(resp).await?, "error")?.to_string();
        assert!(error.contains(token), "{missing}: {error}");
    }

    // An unknown device is the device's `404` whatever the body names, the
    // answer every other device write gives.
    for signer in [&admin, &server_admin] {
        for owner in [
            &Value::Null,
            &real_org,
            &missing_org,
            &real_user,
            &missing_user,
        ] {
            let resp = signed_json(
                &state,
                signer,
                "PUT",
                "/api/v1/clients/ev1_nobody/owner",
                &json!({ "owner": owner }),
            )
            .await?;
            assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{owner}");
            let error = json_str(&body_json(resp).await?, "error")?.to_string();
            assert!(error.contains("no client ev1_nobody"), "{owner}: {error}");
        }
    }
    Ok(())
}

#[tokio::test]
async fn an_org_admin_deletes_a_device_its_organization_holds() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let server_admin = fleet_admin(&state).await?;
    let acme = create_org(&state, &server_admin, "acme").await?;
    let admin = registered_user(
        &state,
        &[UserPermission::ManageClients, UserPermission::ReadClients],
    )
    .await?;
    let resp = signed_json(
        &state,
        &server_admin,
        "PUT",
        &format!("/api/v1/users/{}/org", admin.1),
        &json!({ "org_id": acme }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK, "the admin joins acme");
    let (_, ours) = register_and_approve(&state).await?;
    assign(&state, &server_admin, &acme, &[ours.as_str()]).await?;

    let resp = signed_empty(&state, &admin, "DELETE", &format!("/api/v1/clients/{ours}")).await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert_eq!(stored_status(&state, ours.as_str()).await?, None);
    assert!(
        state.org_store.get_ownership(&ours).await?.is_none(),
        "the device leaves acme's roster with its record"
    );

    // Scoped, the delete needs the record, so neither an unknown id nor a repeat answers `204`.
    let resp = signed_empty(&state, &admin, "DELETE", "/api/v1/clients/ev1_nobody").await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        json_str(&body_json(resp).await?, "error")?,
        "no client ev1_nobody"
    );
    let resp = signed_empty(&state, &admin, "DELETE", &format!("/api/v1/clients/{ours}")).await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    Ok(())
}

/// A state rather than an error: the grant can come before the account joins an
/// organization. Every organization reads as missing to it.
#[tokio::test]
async fn an_unaffiliated_admin_administers_nothing() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let admin = registered_user(
        &state,
        &[UserPermission::ManageClients, UserPermission::ReadClients],
    )
    .await?;
    let (_, client_id) = register_and_approve(&state).await?;

    let resp = signed_json(
        &state,
        &admin,
        "PATCH",
        &format!("/api/v1/clients/{client_id}"),
        &json!({ "client_details": "mine now" }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let server_admin = registered_user(&state, &[UserPermission::ManageAnyOrgs]).await?;
    let resp = signed_json(
        &state,
        &server_admin,
        "POST",
        "/api/v1/orgs",
        &json!({ "slug": "acme" }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let acme = json_str(&body_json(resp).await?, "org_id")?.to_string();
    for org in [acme.as_str(), "org_nosuch"] {
        let resp = signed_json(
            &state,
            &admin,
            "PUT",
            &format!("/api/v1/orgs/{org}/clients"),
            &json!({ "client_ids": [client_id.as_str()] }),
        )
        .await?;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{org}");
        assert_eq!(
            json_str(&body_json(resp).await?, "error")?,
            format!("no organization {org}")
        );
    }
    Ok(())
}

/// A claim is refused before `perform_work`, served after, and refused once it
/// is revoked. An amend asking for what it holds reports no change.
#[tokio::test]
async fn a_grant_is_what_lets_a_device_claim_work() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let admin = device_admin(&state).await?;
    let (key, client_id) = registered_device(&state).await?;
    let uri = format!("/api/v1/clients/{client_id}/permissions");

    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let grant = json!({ "add": ["perform_work"] });
    let body = body_json(signed_json(&state, &admin, "PATCH", &uri, &grant).await?).await?;
    assert_eq!(json_str(&body, "client_id")?, client_id.as_str());
    assert_eq!(body["changed"], true);
    assert_eq!(
        body["permissions"],
        json!(["perform_work", "submit_results"])
    );
    let body = body_json(signed_json(&state, &admin, "PATCH", &uri, &grant).await?).await?;
    assert_eq!(body["changed"], false);
    assert_eq!(
        body["permissions"],
        json!(["perform_work", "submit_results"])
    );

    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let body = body_json(
        signed_json(
            &state,
            &admin,
            "PATCH",
            &uri,
            &json!({ "remove": ["perform_work"] }),
        )
        .await?,
    )
    .await?;
    assert_eq!(body["changed"], true);
    assert_eq!(body["permissions"], json!(["submit_results"]));
    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let resp = signed_json(
        &state,
        &admin,
        "PATCH",
        "/api/v1/clients/ev1_nobody/permissions",
        &grant,
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    Ok(())
}

/// A `PUT` states the whole set, so it drops a name this build does not know
/// and an empty one clears the device; an amend leaves such a name where it is.
#[tokio::test]
async fn a_set_states_the_whole_set_and_an_amend_only_what_it_names() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let admin = device_admin(&state).await?;
    let (_, client_id) = registered_device(&state).await?;
    let uri = format!("/api/v1/clients/{client_id}/permissions");
    let newer: ClientPermissionSet = serde_json::from_value(json!(["rescore_plans"]))?;
    state
        .client_store
        .put_client_permissions(&client_id, &newer)
        .await?;

    let amend = json!({ "add": ["submit_results"] });
    crate::api_schema::assert_conforms("AmendClientPermissionsRequest", &amend)?;
    assert!(
        crate::api_schema::assert_conforms(
            "AmendClientPermissionsRequest",
            &json!({ "add": ["submit_plans"] })
        )
        .is_err(),
        "a user's permission is no device's"
    );
    let resp = signed_json(&state, &admin, "PATCH", &uri, &amend).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        stored_permissions(&state, &client_id).await?,
        ["rescore_plans", "submit_results"]
    );

    let set = json!({ "permissions": ["perform_work"] });
    let body = body_json(signed_json(&state, &admin, "PUT", &uri, &set).await?).await?;
    assert_eq!(body["changed"], true);
    assert_eq!(body["permissions"], json!(["perform_work"]));
    assert_eq!(
        stored_permissions(&state, &client_id).await?,
        ["perform_work"]
    );

    let clear = json!({ "permissions": [] });
    let body = body_json(signed_json(&state, &admin, "PUT", &uri, &clear).await?).await?;
    assert_eq!(body["changed"], true);
    assert_eq!(body["permissions"], json!([]));
    assert_eq!(
        state
            .client_store
            .get_client_permissions(&client_id)
            .await?,
        Some(ClientPermissionSet::default()),
        "cleared, which an operator tells apart from never granted"
    );
    Ok(())
}

#[rstest]
#[case::an_empty_amend("PATCH", json!({}))]
#[case::a_misspelt_amend("PATCH", json!({ "ad": ["perform_work"] }))]
#[case::both_ways("PATCH", json!({ "add": ["perform_work"], "remove": ["perform_work"] }))]
#[case::a_user_permission("PATCH", json!({ "add": ["submit_plans"] }))]
#[case::no_set("PUT", json!({}))]
#[case::an_unknown_name("PUT", json!({ "permissions": ["perform_work", "nonsense"] }))]
#[tokio::test]
async fn a_permission_write_refuses_what_it_cannot_apply(
    #[case] method: &str,
    #[case] body: Value,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let admin = device_admin(&state).await?;
    let (_, client_id) = registered_device(&state).await?;

    let uri = format!("/api/v1/clients/{client_id}/permissions");
    let resp = signed_json(&state, &admin, method, &uri, &body).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        stored_permissions(&state, &client_id).await?,
        ["submit_results"]
    );
    Ok(())
}

/// An org admin grants its own organization's devices and no others: one
/// nobody owns and another organization's are out of reach.
#[tokio::test]
async fn an_org_admin_grants_within_its_device_scope_only() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let server_admin = fleet_admin(&state).await?;
    let acme = create_org(&state, &server_admin, "acme").await?;
    let rival = create_org(&state, &server_admin, "rival").await?;
    let admin = registered_user(&state, &[UserPermission::ManageClients]).await?;
    signed_json(
        &state,
        &server_admin,
        "PUT",
        &format!("/api/v1/users/{}/org", admin.1),
        &json!({ "org_id": acme }),
    )
    .await?;

    let (_, ours) = registered_device(&state).await?;
    let (_, theirs) = registered_device(&state).await?;
    let (_, unowned) = register_pending(&state).await?;
    assign(&state, &server_admin, &acme, &[ours.as_str()]).await?;
    assign(&state, &server_admin, &rival, &[theirs.as_str()]).await?;

    let grant = json!({ "add": ["perform_work"] });
    let uri = format!("/api/v1/clients/{ours}/permissions");
    let resp = signed_json(&state, &admin, "PATCH", &uri, &grant).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        stored_permissions(&state, &ours).await?,
        ["perform_work", "submit_results"]
    );

    for device in [&unowned, &theirs] {
        for (method, body) in [
            ("PATCH", json!({ "add": ["perform_work"] })),
            ("PUT", json!({ "permissions": [] })),
        ] {
            let uri = format!("/api/v1/clients/{device}/permissions");
            let resp = signed_json(&state, &admin, method, &uri, &body).await?;
            assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{method} {uri}");
        }
        assert_eq!(
            stored_permissions(&state, device).await?,
            ["submit_results"]
        );
    }

    // The fleet scope reaches it.
    let uri = format!("/api/v1/clients/{theirs}/permissions");
    let resp = signed_json(&state, &server_admin, "PATCH", &uri, &grant).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    Ok(())
}

/// An org admin reaches neither another organization's device nor an unowned
/// one, and an unaffiliated admin reaches none.
#[tokio::test]
async fn approve_and_reject_refuse_a_device_outside_the_callers_scope() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let server_admin = fleet_admin(&state).await?;
    let acme = create_org(&state, &server_admin, "acme").await?;
    let rival = create_org(&state, &server_admin, "rival").await?;
    let org_admin = registered_user(
        &state,
        &[UserPermission::ManageClients, UserPermission::ReadClients],
    )
    .await?;
    let resp = signed_json(
        &state,
        &server_admin,
        "PUT",
        &format!("/api/v1/users/{}/org", org_admin.1),
        &json!({ "org_id": acme }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK, "the org admin joins acme");
    let unaffiliated = registered_user(
        &state,
        &[UserPermission::ManageClients, UserPermission::ReadClients],
    )
    .await?;

    let (_, theirs) = register_pending(&state).await?;
    let (_, unowned) = register_pending(&state).await?;
    assign(&state, &server_admin, &rival, &[theirs.as_str()]).await?;

    for (who, caller, client_id) in [
        ("an org admin", &org_admin, &theirs),
        ("an org admin", &org_admin, &unowned),
        ("an unaffiliated admin", &unaffiliated, &unowned),
    ] {
        for verb in ["reject", "approve"] {
            let uri = format!("/api/v1/clients/{client_id}/{verb}");
            let resp = signed_empty(&state, caller, "POST", &uri).await?;
            assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{who}: {uri}");
            // The exact message also proves the refusal does not name rival.
            assert_eq!(
                json_str(&body_json(resp).await?, "error")?,
                "this device is outside your organization",
                "{who}: {uri}"
            );
        }
    }

    for client_id in [&theirs, &unowned] {
        assert_eq!(
            stored_status(&state, client_id.as_str()).await?,
            Some(ClientStatus::Pending),
            "{client_id} is neither admitted nor turned away"
        );
    }
    Ok(())
}

async fn owners(
    state: &AppState,
    signer: &UserSigner,
    client_id: &str,
    query: &str,
) -> anyhow::Result<(StatusCode, Value)> {
    let (key, user_id, key_id) = signer;
    let uri = format!("/api/v1/clients/{client_id}/owners?{query}");
    let resp = authed_user_get(state, key, user_id, key_id, &uri).await?;
    Ok((resp.status(), body_json(resp).await?))
}

/// The values an owner lookup answered, in order, after checking its schema.
fn offered(body: &Value) -> anyhow::Result<Vec<String>> {
    crate::api_schema::assert_conforms("OwnerMatches", body)?;
    Ok(body
        .get("owners")
        .and_then(Value::as_array)
        .context("an answer carries owners")?
        .iter()
        .filter_map(|owner| {
            owner
                .get("value")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect())
}

/// What the app's owner field offers: an organization's device goes to it, to
/// the caller, or (for the fleet scope) to its members, organizations first,
/// never to one the roster grant hides.
#[tokio::test]
async fn the_owner_lookup_offers_where_the_device_may_go() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let acme = seed_org(&state, "acme").await?.org_id;
    let rival = seed_org(&state, "rival").await?.org_id;
    let lead = registered_user(
        &state,
        &[UserPermission::ManageClients, UserPermission::ReadOrgs],
    )
    .await?;
    let member = registered_user(&state, &[]).await?;
    let outsider = registered_user(&state, &[]).await?;
    set_user_org(&state, &lead.1, Some(acme.clone())).await?;
    set_user_org(&state, &member.1, Some(acme.clone())).await?;
    set_user_org(&state, &outsider.1, Some(rival.clone())).await?;
    let (_, ours) = register_and_approve(&state).await?;
    let (_, theirs) = register_and_approve(&state).await?;
    own_client_by_org(&state, &ours, &acme).await?;
    own_client_by_org(&state, &theirs, &rival).await?;

    let (status, body) = owners(&state, &lead, ours.as_str(), "q=").await?;
    assert_eq!(status, StatusCode::OK);
    let values = offered(&body)?;
    assert_eq!(values.first(), Some(&acme.to_string()), "{body}");
    // A scoped caller names only itself: a colleague's device fetches their vault.
    [
        (lead.1.to_string(), true),
        (member.1.to_string(), false),
        (rival.to_string(), false),
        (outsider.1.to_string(), false),
    ]
    .into_iter()
    .for_each(|(principal, wanted)| {
        assert_eq!(values.contains(&principal), wanted, "{principal}: {body}");
    });

    let (_, body) = owners(&state, &lead, ours.as_str(), "q=&type=org").await?;
    assert_eq!(offered(&body)?, [acme.to_string()]);
    let address = state
        .user_store
        .get_user(&member.1)
        .await?
        .context("the member exists")?
        .contact_email
        .to_string();

    // Every roster stops at the device's organization.
    let admin = registered_user(
        &state,
        &[
            UserPermission::ManageAnyClients,
            UserPermission::ReadAnyOrgs,
        ],
    )
    .await?;
    let (_, body) = owners(
        &state,
        &admin,
        ours.as_str(),
        &format!("q={}", address.to_uppercase()),
    )
    .await?;
    assert_eq!(offered(&body)?, [member.1.to_string()]);
    let values = offered(&owners(&state, &admin, ours.as_str(), "q=").await?.1)?;
    for (principal, wanted) in [
        (acme.to_string(), true),
        (member.1.to_string(), true),
        (rival.to_string(), false),
        (outsider.1.to_string(), false),
    ] {
        assert_eq!(values.contains(&principal), wanted, "{principal}");
    }

    // Without the roster grant nobody is named, whatever is typed, so an id
    // never resolves to an address.
    let fleet_only = registered_user(&state, &[UserPermission::ManageAnyClients]).await?;
    let scoped_only = registered_user(&state, &[UserPermission::ManageClients]).await?;
    set_user_org(&state, &scoped_only.1, Some(acme.clone())).await?;
    let (_, unowned) = register_and_approve(&state).await?;
    let member_id = format!("q={}", member.1);
    let member_address = format!("q={address}");
    for (signer, client_id, query) in [
        (&fleet_only, ours.as_str(), "q="),
        (&fleet_only, ours.as_str(), member_id.as_str()),
        (&fleet_only, ours.as_str(), member_address.as_str()),
        (&fleet_only, unowned.as_str(), "q="),
        (&fleet_only, unowned.as_str(), member_id.as_str()),
        (&scoped_only, ours.as_str(), "q="),
        (&scoped_only, ours.as_str(), member_id.as_str()),
    ] {
        let (status, body) = owners(&state, signer, client_id, query).await?;
        assert_eq!(status, StatusCode::OK, "{client_id} {query}: {body}");
        assert!(offered(&body)?.is_empty(), "{client_id} {query}: {body}");
    }

    // Refused in order: the fleet read, the device, then the device scope.
    let unknown = "ev1_0000000000000000000000000000000000000000000000000000000000000000";
    let reader = registered_user(&state, &[UserPermission::ReadClients]).await?;
    let nobody = registered_user(&state, &[]).await?;
    for (signer, client_id, query, status, error) in [
        (
            &nobody,
            unknown,
            "q=",
            StatusCode::FORBIDDEN,
            "user lacks the read_clients permission",
        ),
        (
            &reader,
            unknown,
            "q=",
            StatusCode::NOT_FOUND,
            "client not found",
        ),
        (
            &reader,
            ours.as_str(),
            "q=",
            StatusCode::FORBIDDEN,
            "this device is outside your device scope",
        ),
        (
            &lead,
            theirs.as_str(),
            "q=",
            StatusCode::FORBIDDEN,
            "this device is outside your device scope",
        ),
    ] {
        let (answered, body) = owners(&state, signer, client_id, query).await?;
        assert_eq!(
            (answered, json_str(&body, "error")?),
            (status, error),
            "{client_id}"
        );
    }
    let (status, _) = owners(&state, &lead, ours.as_str(), "type=team").await?;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    Ok(())
}

/// An answer is cut at 20, and what the reader typed exactly comes first, so a
/// typed handle resolves however many others hold it.
#[tokio::test]
async fn an_exact_match_leads_the_owner_lookup_past_its_cut() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let admin = registered_user(
        &state,
        &[
            UserPermission::ManageAnyClients,
            UserPermission::ReadAnyOrgs,
        ],
    )
    .await?;
    for n in 1..=21 {
        seed_org(&state, &format!("a-zz-{n:02}")).await?;
    }
    let zz = seed_org(&state, "zz").await?.org_id;
    let (_, unowned) = register_and_approve(&state).await?;

    let (status, body) = owners(&state, &admin, unowned.as_str(), "q=zz&type=org").await?;
    assert_eq!(status, StatusCode::OK);
    let values = offered(&body)?;
    assert_eq!(values.len(), 20, "{body}");
    assert_eq!(values.first(), Some(&zz.to_string()), "{body}");

    let (_, body) = owners(&state, &admin, unowned.as_str(), "q=zz-2&type=org").await?;
    assert_eq!(offered(&body)?.len(), 2, "a-zz-20 and a-zz-21");
    Ok(())
}

async fn assign(
    state: &AppState,
    signer: &UserSigner,
    org: &str,
    client_ids: &[&str],
) -> anyhow::Result<()> {
    let resp = signed_json(
        state,
        signer,
        "PUT",
        &format!("/api/v1/orgs/{org}/clients"),
        &json!({ "client_ids": client_ids }),
    )
    .await?;
    anyhow::ensure!(
        resp.status() == StatusCode::OK,
        "assignment failed with {}",
        resp.status()
    );
    Ok(())
}

/// The client ids an organization holds.
async fn owner_org(
    state: &AppState,
    signer: &UserSigner,
    org: &str,
) -> anyhow::Result<Vec<String>> {
    let (key, user_id, key_id) = signer;
    let uri = format!("/api/v1/orgs/{org}/clients");
    let body = body_json(authed_user_get(state, key, user_id, key_id, &uri).await?).await?;
    body.get("items")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| row["client_id"].as_str().map(str::to_string))
                .collect()
        })
        .ok_or_else(|| anyhow::anyhow!("no rows: {body}"))
}
