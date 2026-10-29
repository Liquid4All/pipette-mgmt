//! Account administration and permission granting over HTTP, and the delegation
//! this makes possible.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::http::StatusCode;
use serde_json::json;

use pipette_core::types::DeriveFromKey;
use pipette_core::user::{User, UserKey};
use pipette_core::user_permission::{UserPermissionGroup, UserPermissionSet};
use pipette_core::user_preauth::{UserPreauthConsumeOutcome, UserPreauthKey, UserPreauthSecret};
use pipette_httpapi::state::AppState;
use pipette_plan_types::{
    NonEmptyTrimmedString, PublicKeyHex, UserAccountType, UserId, UserKeyId, UserPermission,
    UserPreauthKeyId, UserStatus,
};
use pipette_store::{EmailClaim, UserPermissionMutation, UserStore};
use pipette_web_testkit::*;

/// A well-formed user key id no account holds.
const UNHELD_KEY: &str = "userkey_0195c3e2a4f37b8c";

async fn server_admin(state: &AppState) -> anyhow::Result<UserSigner> {
    registered_user(state, UserPermissionGroup::ServerAdmin.permissions()).await
}

/// Put an existing account into an organization and give it a permission set.
async fn place(
    state: &AppState,
    admin: &UserSigner,
    user_id: &UserId,
    org_id: &str,
    permissions: &[UserPermission],
) -> anyhow::Result<()> {
    let resp = signed_json(
        state,
        admin,
        "PUT",
        &format!("/api/v1/users/{user_id}/org"),
        &json!({ "org_id": org_id }),
    )
    .await?;
    anyhow::ensure!(resp.status() == StatusCode::OK, "placement failed");
    let names: Vec<&str> = permissions.iter().map(AsRef::as_ref).collect();
    let resp = signed_json(
        state,
        admin,
        "PUT",
        &format!("/api/v1/users/{user_id}/permissions"),
        &json!({ "permissions": names }),
    )
    .await?;
    anyhow::ensure!(resp.status() == StatusCode::OK, "grant failed");
    Ok(())
}

/// The names `user_id`'s stored permission object holds, empty when it has none.
async fn stored_names(state: &AppState, user_id: &UserId) -> anyhow::Result<Vec<String>> {
    Ok(state
        .user_store
        .get_user_permissions(user_id)
        .await?
        .map(|set| set.names().iter().map(|n| n.to_string()).collect())
        .unwrap_or_default())
}

/// The delegation the org-admin role exists for, with no server admin involved.
#[tokio::test]
async fn an_org_admin_can_appoint_another_for_its_own_organization() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let root = server_admin(&state).await?;
    let acme = create_org(&state, &root, "acme").await?;

    // The last time the server admin appears.
    let lead = registered_user(&state, &[]).await?;
    place(
        &state,
        &root,
        &lead.1,
        &acme,
        UserPermissionGroup::OrgAdmin.permissions(),
    )
    .await?;

    // It never names an organization: its own is implied.
    let resp = signed_json(
        &state,
        &lead,
        "POST",
        "/api/v1/users",
        &json!({
            "contact_email": "second@example.test",
            "permissions": UserPermissionGroup::OrgAdmin
                .permissions()
                .iter()
                .map(AsRef::as_ref)
                .collect::<Vec<&str>>(),
        }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await?;
    assert_eq!(
        json_str(&body, "org_id")?,
        acme,
        "a scoped create lands in the caller's own organization"
    );
    let second = UserId::try_new(json_str(&body, "user_id")?)?;
    assert!(
        body["permissions"]
            .as_array()
            .is_some_and(|names| names.iter().any(|n| n == "manage_orgs")),
        "the colleague holds the delegation grant too: {body}"
    );

    // And the new admin can administer the organization's people in turn.
    let listed =
        body_json(authed_user_get(&state, &lead.0, &lead.1, &lead.2, "/api/v1/users").await?)
            .await?;
    assert_eq!(
        listed["items"].as_array().map(Vec::len),
        Some(2),
        "the listing is scoped to the organization, not the server"
    );
    assert!(state.user_store.get_user(&second).await?.is_some());
    Ok(())
}

/// Belonging to the organization is not that grant: refused, not scoped to
/// nothing.
#[rstest::rstest]
#[case::member_of_the_organization(true)]
#[case::unaffiliated(false)]
#[tokio::test]
async fn the_roster_reads_are_refused_without_read_orgs(
    #[case] member_of_acme: bool,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (root, acme, _lead) = acme_with_admin(&state).await?;
    let stranger = registered_user(&state, &[UserPermission::SubmitPlans]).await?;
    if member_of_acme {
        place(
            &state,
            &root,
            &stranger.1,
            &acme,
            &[UserPermission::SubmitPlans],
        )
        .await?;
    }
    let member = keyless(&state, &acme, &[]).await?;

    for uri in [
        "/api/v1/users".to_string(),
        format!("/api/v1/users/{member}"),
        format!("/api/v1/users/{}", stranger.1),
    ] {
        let resp = signed_empty(&state, &stranger, "GET", &uri).await?;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "GET {uri}");
        assert_eq!(
            json_str(&body_json(resp).await?, "error")?,
            "user lacks the read_orgs permission",
            "GET {uri}"
        );
    }
    Ok(())
}

/// Exactly its organization's roster; one record matches its listing row.
#[tokio::test]
async fn a_read_orgs_member_reads_its_organizations_accounts() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (root, acme, lead) = acme_with_admin(&state).await?;
    let rival = create_org(&state, &root, "rival").await?;
    keyless(&state, &rival, &[]).await?;
    let reader = registered_user(&state, &[]).await?;
    place(&state, &root, &reader.1, &acme, &[UserPermission::ReadOrgs]).await?;
    // Stored as `manage_orgs` alone, so `read_orgs` in the answer is resolution.
    let target = keyless(&state, &acme, &[UserPermission::ManageOrgs]).await?;
    let stored = state
        .user_store
        .get_user(&target)
        .await?
        .ok_or_else(|| anyhow::anyhow!("the target was not written"))?;

    let resp = signed_empty(&state, &reader, "GET", &format!("/api/v1/users/{target}")).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let got = body_json(resp).await?;
    assert_eq!(got["user_id"], json!(target.as_str()));
    assert_eq!(got["contact_email"], json!(stored.contact_email.as_str()));
    assert_eq!(got["status"], json!("active"));
    json_str(&got, "created_at")?.parse::<chrono::DateTime<chrono::Utc>>()?;
    assert_eq!(got["org_id"], json!(acme));
    assert_eq!(
        got["permissions"],
        json!(["manage_orgs", "read_orgs"]),
        "the answer carries the resolved set, not the stored one"
    );

    let resp = signed_empty(&state, &reader, "GET", "/api/v1/users").await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let listed = body_json(resp).await?;
    let rows = listed["items"].as_array().cloned().unwrap_or_default();
    let mut ids: Vec<&str> = rows
        .iter()
        .filter_map(|row| row["user_id"].as_str())
        .collect();
    ids.sort_unstable();
    let mut roster = vec![lead.1.as_str(), reader.1.as_str(), target.as_str()];
    roster.sort_unstable();
    assert_eq!(
        ids, roster,
        "the listing is the organization's roster and nothing past it"
    );
    let row = rows
        .iter()
        .find(|row| row["user_id"] == json!(target.as_str()))
        .ok_or_else(|| anyhow::anyhow!("the target is not listed: {listed}"))?;
    assert_eq!(
        row,
        &account_of(got),
        "a listing row is the record one read returns"
    );
    Ok(())
}

/// A `400` naming the prefix it lacks once signed in, `401` before; nothing is
/// revoked.
#[rstest::rstest]
#[case::a_bare_word("GET", "/api/v1/users/nobody", "user id must start with `user_`")]
#[case::a_client_id("GET", "/api/v1/users/ev1_abc", "user id must start with `user_`")]
#[case::a_key_without_its_prefix(
    "DELETE",
    "/api/v1/users/me/keys/not-a-key",
    "user key id must start with `userkey_`"
)]
#[case::an_accounts_key_without_its_prefix(
    "DELETE",
    "/api/v1/users/user_0195c3e2a4f37b8c/keys/not-a-key",
    "user key id must start with `userkey_`"
)]
#[tokio::test]
async fn a_malformed_id_in_the_path_is_refused_after_sign_in(
    #[case] method: &str,
    #[case] uri: &str,
    #[case] refusal: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let root = server_admin(&state).await?;

    let resp = signed_empty(&state, &root, method, uri).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{method} {uri}");
    assert_eq!(
        json_str(&body_json(resp).await?, "error")?,
        refusal,
        "{method} {uri}"
    );

    // Unsigned, the same request is refused for the missing sign-in first.
    let resp = request_with_headers(&state, method, uri, &[]).await?;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{method} {uri}");
    assert_eq!(
        json_str(&body_json(resp).await?, "error")?,
        "missing X-Client-Id header"
    );

    let (key, user_id, key_id) = &root;
    assert_eq!(state.user_store.list_user_keys(user_id).await?.len(), 1);
    let resp = authed_user_get(&state, key, user_id, key_id, "/api/v1/users/me").await?;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "the signer's key still signs"
    );
    Ok(())
}

/// **Nobody may grant what they do not hold**, on every path that grants.
/// Without this, delegation is privilege escalation with extra steps.
#[tokio::test]
async fn a_grant_cannot_exceed_what_the_caller_holds() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let root = server_admin(&state).await?;
    let acme = create_org(&state, &root, "acme").await?;

    let lead = registered_user(&state, &[]).await?;
    place(
        &state,
        &root,
        &lead.1,
        &acme,
        UserPermissionGroup::OrgAdmin.permissions(),
    )
    .await?;
    let colleague = registered_user(&state, &[]).await?;
    place(&state, &root, &colleague.1, &acme, &[]).await?;

    // Create, set and amend all refuse the same widening, and each names what
    // it refused.
    let wider = "manage_any_orgs";
    for (method, uri, body) in [
        (
            "POST",
            "/api/v1/users".to_string(),
            json!({ "contact_email": "new@example.test", "permissions": [wider] }),
        ),
        (
            "PUT",
            format!("/api/v1/users/{}/permissions", colleague.1),
            json!({ "permissions": [wider] }),
        ),
        (
            "PATCH",
            format!("/api/v1/users/{}/permissions", colleague.1),
            json!({ "add": [wider] }),
        ),
    ] {
        let resp = signed_json(&state, &lead, method, &uri, &body).await?;
        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "{method} {uri} must refuse a grant beyond the caller's set"
        );
        assert!(
            json_str(&body_json(resp).await?, "error")?.contains(wider),
            "the refusal must name what it refused"
        );
    }

    // Including granting it to itself, which is the escalation this guards.
    let resp = signed_json(
        &state,
        &lead,
        "PATCH",
        &format!("/api/v1/users/{}/permissions", lead.1),
        &json!({ "add": [wider] }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // A name it does hold passes, so the ceiling is a bound and not a wall.
    let resp = signed_json(
        &state,
        &lead,
        "PATCH",
        &format!("/api/v1/users/{}/permissions", colleague.1),
        &json!({ "add": ["read_clients"] }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        body_json(resp).await?["permissions"]
            .as_array()
            .is_some_and(|names| names.iter().any(|n| n == "read_clients"))
    );
    Ok(())
}

/// Each account write and pre-auth key route is refused, granting itself
/// `manage_orgs` included.
#[tokio::test]
async fn a_read_orgs_member_writes_nothing_on_the_roster() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (root, acme, lead) = acme_with_admin(&state).await?;
    let reader = registered_user(&state, &[]).await?;
    place(&state, &root, &reader.1, &acme, &[UserPermission::ReadOrgs]).await?;
    let member = keyless(&state, &acme, &[UserPermission::ReadClients]).await?;
    let minted = body_json(mint(&state, &lead, &member).await?).await?;
    let key_id = json_str(&minted, "key_id")?;

    for (method, uri, body) in [
        (
            "POST",
            "/api/v1/users".to_string(),
            json!({ "contact_email": "new@example.test" }),
        ),
        (
            "PATCH",
            format!("/api/v1/users/{member}"),
            json!({ "status": "disabled" }),
        ),
        (
            "PUT",
            format!("/api/v1/users/{member}/permissions"),
            json!({ "permissions": [] }),
        ),
        (
            "PATCH",
            format!("/api/v1/users/{member}/permissions"),
            json!({ "remove": ["read_clients"] }),
        ),
        (
            "PUT",
            format!("/api/v1/users/{}/permissions", reader.1),
            json!({ "permissions": ["manage_orgs"] }),
        ),
        (
            "PATCH",
            format!("/api/v1/users/{}/permissions", reader.1),
            json!({ "add": ["manage_orgs"] }),
        ),
        (
            "GET",
            format!("/api/v1/users/{member}/preauth-keys"),
            json!({}),
        ),
        (
            "POST",
            format!("/api/v1/users/{member}/preauth-keys"),
            json!({}),
        ),
        (
            "DELETE",
            format!("/api/v1/users/{member}/preauth-keys/{key_id}"),
            json!({}),
        ),
        (
            "DELETE",
            format!("/api/v1/users/{}/keys/{}", lead.1, lead.2),
            json!({}),
        ),
    ] {
        let resp = signed_json(&state, &reader, method, &uri, &body).await?;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{method} {uri}");
        assert_eq!(
            json_str(&body_json(resp).await?, "error")?,
            "user lacks the manage_orgs permission",
            "{method} {uri}"
        );
    }

    assert!(
        state
            .user_store
            .lookup_user_by_email("new@example.test")
            .await?
            .is_none(),
        "no account was created"
    );
    let kept = state
        .user_store
        .get_user(&member)
        .await?
        .ok_or_else(|| anyhow::anyhow!("the member was deleted"))?;
    assert_eq!(kept.status, UserStatus::Active);
    assert_eq!(stored_names(&state, &member).await?, vec!["read_clients"]);
    assert_eq!(stored_names(&state, &reader.1).await?, vec!["read_orgs"]);
    assert_eq!(
        present(&state, json_str(&minted, "token")?).await?.status(),
        StatusCode::CREATED,
        "the pre-auth key was not revoked"
    );
    let (key, user_id, key_id) = &lead;
    let resp = authed_user_get(&state, key, user_id, key_id, "/api/v1/users/me").await?;
    assert_eq!(resp.status(), StatusCode::OK, "the lead's key still signs");
    Ok(())
}

/// Standing is settled before any grant, even the server-wide ones.
#[rstest::rstest]
#[case::pending(UserStatus::Pending, "user is pending an operator's approval")]
#[case::disabled(UserStatus::Disabled, "user is disabled")]
#[tokio::test]
async fn an_account_out_of_good_standing_administers_nothing(
    #[case] status: UserStatus,
    #[case] refusal: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let caller = server_admin(&state).await?;
    let target = create_user(&state, "target@example.test").await?.user_id;
    grant_user(&state, &target, &[UserPermission::ReadPlans]).await?;
    let minted = body_json(mint(&state, &caller, &target).await?).await?;
    let key_id = json_str(&minted, "key_id")?;
    set_user_status(&state, &caller.1, status).await?;
    let accounts = state.user_store.list_users().await?.len();

    for (method, uri, body) in [
        ("GET", "/api/v1/users".to_string(), json!({})),
        (
            "POST",
            "/api/v1/users".to_string(),
            json!({ "contact_email": "new@example.test" }),
        ),
        ("GET", format!("/api/v1/users/{target}"), json!({})),
        (
            "PATCH",
            format!("/api/v1/users/{target}"),
            json!({ "status": "disabled" }),
        ),
        (
            "PUT",
            format!("/api/v1/users/{target}/permissions"),
            json!({ "permissions": [] }),
        ),
        (
            "PATCH",
            format!("/api/v1/users/{target}/permissions"),
            json!({ "remove": ["read_plans"] }),
        ),
        ("GET", format!("/api/v1/users/{target}/keys"), json!({})),
        (
            "DELETE",
            format!("/api/v1/users/{target}/keys/{UNHELD_KEY}"),
            json!({}),
        ),
        (
            "GET",
            format!("/api/v1/users/{target}/preauth-keys"),
            json!({}),
        ),
        (
            "POST",
            format!("/api/v1/users/{target}/preauth-keys"),
            json!({}),
        ),
        (
            "DELETE",
            format!("/api/v1/users/{target}/preauth-keys/{key_id}"),
            json!({}),
        ),
    ] {
        let resp = signed_json(&state, &caller, method, &uri, &body).await?;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{method} {uri}");
        assert_eq!(
            json_str(&body_json(resp).await?, "error")?,
            refusal,
            "{method} {uri}"
        );
    }

    assert_eq!(state.user_store.list_users().await?.len(), accounts);
    assert!(
        state
            .user_store
            .lookup_user_by_email("new@example.test")
            .await?
            .is_none(),
        "no account was created"
    );
    let kept = state
        .user_store
        .get_user(&target)
        .await?
        .ok_or_else(|| anyhow::anyhow!("the target was deleted"))?;
    assert_eq!(kept.status, UserStatus::Active);
    assert_eq!(stored_names(&state, &target).await?, vec!["read_plans"]);
    assert!(
        state
            .user_store
            .list_user_preauth_keys()
            .await?
            .iter()
            .any(|key| key.key_id.as_str() == key_id),
        "the pre-auth key was neither revoked nor replaced"
    );
    Ok(())
}

/// To a server-wide administrator too; no permission object or pre-auth key is
/// written for it.
#[tokio::test]
async fn an_id_naming_no_account_is_absent_on_every_account_route() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let root = server_admin(&state).await?;
    let absent = UserId::try_new("user_00000000-0000-0000-0000-000000000000")?;
    let accounts = state.user_store.list_users().await?.len();
    let account = format!("/api/v1/users/{absent}");

    for (method, uri, body) in [
        ("GET", account.clone(), json!({})),
        ("PATCH", account.clone(), json!({ "status": "disabled" })),
        (
            "PUT",
            format!("{account}/permissions"),
            json!({ "permissions": ["read_plans"] }),
        ),
        (
            "PATCH",
            format!("{account}/permissions"),
            json!({ "add": ["read_plans"] }),
        ),
        ("GET", format!("{account}/keys"), json!({})),
        ("DELETE", format!("{account}/keys/{UNHELD_KEY}"), json!({})),
        ("GET", format!("{account}/preauth-keys"), json!({})),
        ("POST", format!("{account}/preauth-keys"), json!({})),
        (
            "DELETE",
            format!("{account}/preauth-keys/0195c3e2a4f37b8c"),
            json!({}),
        ),
    ] {
        let resp = signed_json(&state, &root, method, &uri, &body).await?;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{method} {uri}");
        assert_eq!(
            json_str(&body_json(resp).await?, "error")?,
            "no such user",
            "{method} {uri}"
        );
    }

    assert!(state.user_store.get_user(&absent).await?.is_none());
    assert!(
        state
            .user_store
            .get_user_permissions(&absent)
            .await?
            .is_none(),
        "no permission object names the absent id"
    );
    assert!(
        state.user_store.list_user_preauth_keys().await?.is_empty(),
        "no pre-auth key was minted"
    );
    assert_eq!(state.user_store.list_users().await?.len(), accounts);
    Ok(())
}

/// Nor can it pull an unaffiliated one in: that may be a server admin, and
/// adopting one would be a takeover.
#[tokio::test]
async fn an_org_admin_cannot_reach_outside_its_organization() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let root = server_admin(&state).await?;
    let acme = create_org(&state, &root, "acme").await?;
    let rival = create_org(&state, &root, "rival").await?;

    let lead = registered_user(&state, &[]).await?;
    place(
        &state,
        &root,
        &lead.1,
        &acme,
        UserPermissionGroup::OrgAdmin.permissions(),
    )
    .await?;

    let outsider = registered_user(&state, &[]).await?;
    place(&state, &root, &outsider.1, &rival, &[]).await?;
    // Belongs to nobody, and holds the widest grant there is.
    let unaffiliated = &root.1;

    // Another organization's account reads as absent rather than refused, so
    // the route cannot be used to probe the server's roster.
    let (key, user_id, key_id) = &lead;
    let resp = authed_user_get(
        &state,
        key,
        user_id,
        key_id,
        &format!("/api/v1/users/{}", outsider.1),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    for (method, uri, body) in [
        (
            "PATCH",
            format!("/api/v1/users/{}", outsider.1),
            json!({ "status": "disabled" }),
        ),
        (
            "PUT",
            format!("/api/v1/users/{}/permissions", outsider.1),
            json!({ "permissions": [] }),
        ),
    ] {
        let resp = signed_json(&state, &lead, method, &uri, &body).await?;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{method} {uri}");
    }

    let resp = signed_json(
        &state,
        &lead,
        "PUT",
        &format!("/api/v1/users/{unaffiliated}/org"),
        &json!({ "org_id": acme }),
    )
    .await?;
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "adopting an unaffiliated account would be a takeover"
    );
    Ok(())
}

/// Absent before the body is weighed: a remove is not ceiling-bounded, so scope
/// alone stops a strip.
#[tokio::test]
async fn an_org_admin_cannot_amend_a_set_in_another_organization() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (root, _acme, lead) = acme_with_admin(&state).await?;
    let rival = create_org(&state, &root, "rival").await?;
    let outsider = keyless(&state, &rival, &[UserPermission::ReadPlans]).await?;
    let uri = format!("/api/v1/users/{outsider}/permissions");

    for body in [
        json!({ "add": ["read_clients"] }),
        json!({ "remove": ["read_plans"] }),
        json!({}),
    ] {
        let resp = signed_json(&state, &lead, "PATCH", &uri, &body).await?;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{body}");
        assert_eq!(
            json_str(&body_json(resp).await?, "error")?,
            "no such user",
            "{body}"
        );
    }
    assert_eq!(stored_names(&state, &outsider).await?, vec!["read_plans"]);
    Ok(())
}

/// A server admin is one, so a strip or a disable here would be a takeover.
#[tokio::test]
async fn an_org_admin_cannot_reach_an_unaffiliated_account() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (root, _acme, lead) = acme_with_admin(&state).await?;
    let held = stored_names(&state, &root.1).await?;
    let account = format!("/api/v1/users/{}", root.1);

    for (method, uri, body) in [
        ("GET", account.clone(), json!({})),
        ("PATCH", account.clone(), json!({ "status": "disabled" })),
        (
            "PUT",
            format!("{account}/permissions"),
            json!({ "permissions": [] }),
        ),
        (
            "PATCH",
            format!("{account}/permissions"),
            json!({ "remove": ["manage_any_orgs"] }),
        ),
    ] {
        let resp = signed_json(&state, &lead, method, &uri, &body).await?;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{method} {uri}");
        assert_eq!(
            json_str(&body_json(resp).await?, "error")?,
            "no such user",
            "{method} {uri}"
        );
    }

    let kept = state
        .user_store
        .get_user(&root.1)
        .await?
        .ok_or_else(|| anyhow::anyhow!("the server admin was deleted"))?;
    assert_eq!(kept.status, UserStatus::Active);
    assert_eq!(
        stored_names(&state, &root.1).await?,
        held,
        "the server admin's grants are intact"
    );
    let (key, user_id, key_id) = &root;
    let resp = authed_user_get(&state, key, user_id, key_id, "/api/v1/users/me").await?;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "the server admin still signs in"
    );
    Ok(())
}

/// Another organization reads as absent, as one that does not exist.
#[rstest::rstest]
#[case::another_organization(true)]
#[case::no_such_organization(false)]
#[tokio::test]
async fn an_org_admin_cannot_move_a_member_into_another_organization(
    #[case] exists: bool,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (root, acme, lead) = acme_with_admin(&state).await?;
    let rival = create_org(&state, &root, "rival").await?;
    let member = keyless(&state, &acme, &[]).await?;
    let target = if exists {
        rival
    } else {
        "org_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e".to_string()
    };

    let resp = signed_json(
        &state,
        &lead,
        "PUT",
        &format!("/api/v1/users/{member}/org"),
        &json!({ "org_id": target }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        json_str(&body_json(resp).await?, "error")?,
        format!("no organization {target}")
    );
    assert_eq!(
        state
            .org_store
            .get_membership(&member)
            .await?
            .map(|org| org.to_string()),
        Some(acme),
        "the member stays in the organization"
    );
    Ok(())
}

/// By a create or a move, whoever asks; it keeps its members.
#[tokio::test]
async fn a_disabled_organization_takes_no_new_members() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (root, acme, lead) = acme_with_admin(&state).await?;
    let resp = signed_json(
        &state,
        &root,
        "PATCH",
        &format!("/api/v1/orgs/{acme}"),
        &json!({ "status": "disabled" }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);

    let refusal = "organization acme is disabled and takes no new members";
    for signer in [&lead, &root] {
        let resp = signed_json(
            &state,
            signer,
            "POST",
            "/api/v1/users",
            &json!({ "contact_email": "new@example.test", "org_id": acme }),
        )
        .await?;
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        assert_eq!(json_str(&body_json(resp).await?, "error")?, refusal);
    }
    let loner = create_user(&state, "loner@example.test").await?;
    let resp = signed_json(
        &state,
        &root,
        "PUT",
        &format!("/api/v1/users/{}/org", loner.user_id),
        &json!({ "org_id": acme }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(json_str(&body_json(resp).await?, "error")?, refusal);
    assert!(
        state
            .org_store
            .get_membership(&loner.user_id)
            .await?
            .is_none()
    );

    // Naming the organization a member already belongs to adds nobody.
    let resp = signed_json(
        &state,
        &root,
        "PUT",
        &format!("/api/v1/users/{}/org", lead.1),
        &json!({ "org_id": acme }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        state
            .org_store
            .get_membership(&lead.1)
            .await?
            .map(|org| org.to_string()),
        Some(acme),
        "its members stay"
    );
    Ok(())
}

/// A release is one-way: the account is then out of reach.
#[tokio::test]
async fn an_org_admin_places_and_releases_its_own_members() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (_root, acme, lead) = acme_with_admin(&state).await?;
    let member = keyless(&state, &acme, &[]).await?;
    let uri = format!("/api/v1/users/{member}/org");

    let resp = signed_json(&state, &lead, "PUT", &uri, &json!({ "org_id": acme })).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["user_id"], json!(member.as_str()));
    assert_eq!(body["org_id"], json!(acme));
    assert_eq!(body["changed"], false, "already a member: {body}");

    let resp = signed_json(&state, &lead, "PUT", &uri, &json!({ "org_id": null })).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["user_id"], json!(member.as_str()));
    assert!(body.get("org_id").is_none(), "released: {body}");
    assert_eq!(body["changed"], true, "{body}");
    assert!(
        state.org_store.get_membership(&member).await?.is_none(),
        "the membership is removed"
    );

    let resp = signed_json(&state, &lead, "PUT", &uri, &json!({ "org_id": acme })).await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(json_str(&body_json(resp).await?, "error")?, "no such user");
    assert!(state.org_store.get_membership(&member).await?.is_none());
    Ok(())
}

/// So a caller cannot lock itself out of the surface that would undo it.
#[tokio::test]
async fn an_account_cannot_retire_itself() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let root = server_admin(&state).await?;

    let resp = signed_json(
        &state,
        &root,
        "PATCH",
        &format!("/api/v1/users/{}", root.1),
        &json!({ "status": "disabled" }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    Ok(())
}

#[tokio::test]
async fn a_disabled_account_is_shut_out_until_it_is_restored() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let root = server_admin(&state).await?;
    let member = registered_user(&state, &[UserPermission::ReadPlans]).await?;
    let email = state
        .user_store
        .get_user(&member.1)
        .await?
        .ok_or_else(|| anyhow::anyhow!("the member was not written"))?
        .contact_email;
    let uri = format!("/api/v1/users/{}", member.1);
    let (key, user_id, key_id) = &member;

    let resp = signed_json(
        &state,
        &root,
        "PATCH",
        &uri,
        &json!({ "status": "disabled" }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["user_id"], json!(user_id.as_str()));
    assert_eq!(body["contact_email"], json!(email.as_str()));
    assert_eq!(body["status"], json!("disabled"));
    assert_eq!(body["permissions"], json!(["read_plans"]));

    let resp = authed_user_get(&state, key, user_id, key_id, "/api/v1/users/me").await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        json_str(&body_json(resp).await?, "error")?,
        "user is disabled"
    );

    let resp = signed_json(&state, &root, "PATCH", &uri, &json!({ "status": "active" })).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await?["status"], json!("active"));
    let resp = authed_user_get(&state, key, user_id, key_id, "/api/v1/users/me").await?;
    assert_eq!(resp.status(), StatusCode::OK, "the same key signs in again");
    Ok(())
}

/// A requested account belongs to no organization, so an org admin reads it as
/// absent.
#[tokio::test]
async fn only_a_server_wide_admin_admits_a_requested_account() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (root, _acme, lead) = acme_with_admin(&state).await?;
    let key = signing_key()?;
    let resp = request_user_account(&state, "asker@example.test", &key, "work laptop").await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let asked = body_json(resp).await?;
    let user_id = UserId::try_new(json_str(&asked, "user_id")?)?;
    let key_id = UserKeyId::try_new(json_str(&asked, "key_id")?)?;
    let uri = format!("/api/v1/users/{user_id}");
    let admit = json!({ "status": "active" });

    let resp = signed_json(&state, &lead, "PATCH", &uri, &admit).await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(json_str(&body_json(resp).await?, "error")?, "no such user");
    let waiting = state
        .user_store
        .get_user(&user_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("the requested account was not written"))?;
    assert_eq!(
        waiting.status,
        UserStatus::Pending,
        "the org admin admitted nothing"
    );

    let resp = signed_json(&state, &root, "PATCH", &uri, &admit).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["status"], json!("active"));
    assert!(
        body.get("org_id").is_none(),
        "admitting places it nowhere: {body}"
    );

    let second = signing_key()?;
    let resp = authed_user_post(
        &state,
        &key,
        &user_id,
        &key_id,
        "/api/v1/users/me/keys",
        &json!({
            "public_key": hex::encode(second.verifying_key().as_bytes()),
            "key_details": "a second laptop",
        }),
    )
    .await?;
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "the key that asked is admitted"
    );
    Ok(())
}

/// Nor may it create into an organization it names, which would seed a roster
/// it is not on.
#[tokio::test]
async fn an_unaffiliated_admin_has_nowhere_to_act() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let root = server_admin(&state).await?;
    let acme = create_org(&state, &root, "acme").await?;
    let stray = registered_user(&state, UserPermissionGroup::OrgAdmin.permissions()).await?;

    for body in [
        json!({ "contact_email": "nobody@example.test" }),
        json!({ "contact_email": "nobody@example.test", "org_id": acme }),
    ] {
        let resp = signed_json(&state, &stray, "POST", "/api/v1/users", &body).await?;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{body}");
    }

    let listed =
        body_json(authed_user_get(&state, &stray.0, &stray.1, &stray.2, "/api/v1/users").await?)
            .await?;
    assert_eq!(
        listed["items"].as_array().map(Vec::len),
        Some(0),
        "it sees no accounts, including its own"
    );
    Ok(())
}

/// The rule `PUT /api/v1/users/{id}/org` keeps, held on the create path too.
#[tokio::test]
async fn a_create_names_an_organization_that_exists() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let root = server_admin(&state).await?;

    let resp = signed_json(
        &state,
        &root,
        "POST",
        "/api/v1/users",
        &json!({
            "contact_email": "new@example.test",
            "org_id": "org_00000000-0000-0000-0000-000000000000",
        }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert!(
        state
            .user_store
            .lookup_user_by_email("new@example.test")
            .await?
            .is_none(),
        "nothing is written for a membership that would not resolve"
    );
    Ok(())
}

/// Only its own organization or an operator creates an address a Clerk rule provisions.
#[rstest::rstest]
#[case::another_orgs_domain(false, "victim@rival.test", StatusCode::FORBIDDEN)]
#[case::a_rule_naming_no_organization(false, "victim@ghost.test", StatusCode::FORBIDDEN)]
#[case::its_own_domain(false, "hire@acme.test", StatusCode::CREATED)]
#[case::an_unprovisioned_domain(false, "hire@elsewhere.test", StatusCode::CREATED)]
#[case::an_operator(true, "victim@rival.test", StatusCode::CREATED)]
#[tokio::test]
async fn an_org_admin_cannot_create_an_address_another_organization_provisions(
    #[case] as_operator: bool,
    #[case] email: &str,
    #[case] status: StatusCode,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let clerk = ClerkInstance::new()?;
    let provision = [
        ("acme.test", "acme"),
        ("rival.test", "rival"),
        ("ghost.test", "ghost"),
    ];
    let state = make_state_with_clerk(dir.path(), &clerk, &provision).await?;
    let (root, _, lead) = acme_with_admin(&state).await?;
    create_org(&state, &root, "rival").await?;
    let signer = if as_operator { &root } else { &lead };

    let body = json!({ "contact_email": email });
    let resp = signed_json(&state, signer, "POST", "/api/v1/users", &body).await?;
    assert_eq!(resp.status(), status, "{email}");
    let claimed = state.user_store.lookup_user_by_email(email).await?;
    assert_eq!(claimed.is_some(), status == StatusCode::CREATED, "{email}");
    Ok(())
}

/// Naming another is refused before the address is claimed.
#[tokio::test]
async fn an_org_admin_creates_only_into_its_own_organization() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (root, acme, lead) = acme_with_admin(&state).await?;
    let rival = create_org(&state, &root, "rival").await?;
    let accounts = state.user_store.list_users().await?.len();

    let resp = signed_json(
        &state,
        &lead,
        "POST",
        "/api/v1/users",
        &json!({ "contact_email": "hire@example.test", "org_id": rival }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        json_str(&body_json(resp).await?, "error")?,
        "you may only create accounts in your own organization"
    );
    assert!(
        state
            .user_store
            .lookup_user_by_email("hire@example.test")
            .await?
            .is_none(),
        "the address was not claimed"
    );
    assert_eq!(state.user_store.list_users().await?.len(), accounts);

    let resp = signed_json(
        &state,
        &lead,
        "POST",
        "/api/v1/users",
        &json!({ "contact_email": "hire@example.test", "org_id": acme }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await?;
    assert_eq!(json_str(&body, "org_id")?, acme);
    let hire = UserId::try_new(json_str(&body, "user_id")?)?;
    assert_eq!(
        state
            .org_store
            .get_membership(&hire)
            .await?
            .map(|org| org.to_string()),
        Some(acme)
    );
    Ok(())
}

/// An account is a person's unless the create asks for a job's.
#[tokio::test]
async fn a_create_may_ask_for_a_service_account() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (_, acme, lead) = acme_with_admin(&state).await?;

    let mut created = Vec::new();
    for body in [
        json!({ "contact_email": "hire@example.test", "org_id": acme }),
        json!({ "contact_email": "ops+ci@example.test", "account_type": "service" }),
    ] {
        let resp = signed_json(&state, &lead, "POST", "/api/v1/users", &body).await?;
        assert_eq!(resp.status(), StatusCode::CREATED);
        created.push(body_json(resp).await?);
    }
    let types: Vec<&str> = created
        .iter()
        .map(|body| json_str(body, "account_type"))
        .collect::<anyhow::Result<_>>()?;
    assert_eq!(types, ["human", "service"]);
    let robot = UserId::try_new(json_str(&created[1], "user_id")?)?;
    assert_eq!(
        state
            .user_store
            .get_user(&robot)
            .await?
            .map(|user| user.account_type),
        Some(UserAccountType::Service)
    );

    let resp = signed_json(
        &state,
        &lead,
        "POST",
        "/api/v1/users",
        &json!({ "contact_email": "bot@example.test", "account_type": "robot" }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(
        state
            .user_store
            .lookup_user_by_email("bot@example.test")
            .await?
            .is_none()
    );
    Ok(())
}

/// Only a server-wide administrator learns the holder: a scoped caller may not
/// learn ids outside its roster.
#[rstest::rstest]
#[case::server_wide(true)]
#[case::scoped(false)]
#[tokio::test]
async fn a_create_at_a_held_address_is_a_conflict(#[case] server_wide: bool) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (root, _acme, lead) = acme_with_admin(&state).await?;
    let holder = create_user(&state, "taken@example.test").await?.user_id;
    let (caller, refusal) = if server_wide {
        (
            &root,
            format!("an account already holds that address: {holder}"),
        )
    } else {
        (&lead, "an account already holds that address".to_string())
    };
    let accounts = state.user_store.list_users().await?.len();

    // Addresses are folded, so another spelling is the same address.
    for email in ["taken@example.test", "Taken@Example.TEST"] {
        let resp = signed_json(
            &state,
            caller,
            "POST",
            "/api/v1/users",
            &json!({ "contact_email": email }),
        )
        .await?;
        assert_eq!(resp.status(), StatusCode::CONFLICT, "{email}");
        assert_eq!(
            json_str(&body_json(resp).await?, "error")?,
            refusal,
            "{email}"
        );
    }

    assert_eq!(
        state.user_store.list_users().await?.len(),
        accounts,
        "no second record"
    );
    assert_eq!(
        state
            .user_store
            .lookup_user_by_email("taken@example.test")
            .await?,
        Some(holder),
        "the claim still names the holder"
    );
    Ok(())
}

/// Active, holding nothing, and no membership written.
#[tokio::test]
async fn a_server_wide_create_without_an_organization_is_unaffiliated() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let root = server_admin(&state).await?;

    let resp = signed_json(
        &state,
        &root,
        "POST",
        "/api/v1/users",
        &json!({ "contact_email": "loose@example.test" }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await?;
    assert_eq!(body["contact_email"], json!("loose@example.test"));
    assert_eq!(body["status"], json!("active"));
    assert_eq!(body["permissions"], json!([]));
    assert!(body.get("org_id").is_none(), "{body}");
    let created = UserId::try_new(json_str(&body, "user_id")?)?;
    assert!(
        state.org_store.get_membership(&created).await?.is_none(),
        "no membership is written"
    );

    let resp = signed_empty(&state, &root, "GET", &format!("/api/v1/users/{created}")).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        account_of(body_json(resp).await?),
        body,
        "a read returns the record the create answered"
    );
    Ok(())
}

/// A `400` before anything is weighed: `{}` neither clears a set nor writes a
/// status.
#[rstest::rstest]
#[case::create_without_an_address(
    "POST",
    "/api/v1/users",
    json!({}),
    "missing field `contact_email`"
)]
#[case::create_at_a_malformed_address(
    "POST",
    "/api/v1/users",
    json!({ "contact_email": "not-an-address" }),
    "contact_email is not a valid email address"
)]
#[case::nothing_named(
    "PATCH",
    "/api/v1/users/TARGET",
    json!({}),
    "name a status, an account_type or a campaign_scope to change"
)]
#[case::status_unknown(
    "PATCH",
    "/api/v1/users/TARGET",
    json!({ "status": "banned" }),
    "unknown variant `banned`"
)]
#[case::set_missing(
    "PUT",
    "/api/v1/users/TARGET/permissions",
    json!({}),
    "missing field `permissions`"
)]
#[tokio::test]
async fn a_write_whose_body_does_not_parse_is_refused(
    #[case] method: &str,
    #[case] path: &str,
    #[case] body: serde_json::Value,
    #[case] reason: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let root = server_admin(&state).await?;
    let target = registered_user(&state, &[UserPermission::ReadClients]).await?;
    let accounts = state.user_store.list_users().await?.len();
    let uri = path.replace("TARGET", target.1.as_str());

    let resp = signed_json(&state, &root, method, &uri, &body).await?;
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "{method} {uri} {body}"
    );
    let refused = body_json(resp).await?;
    let error = json_str(&refused, "error")?;
    assert!(
        error.contains(reason),
        "the refusal names its reason: {refused}"
    );

    assert_eq!(state.user_store.list_users().await?.len(), accounts);
    let kept = state
        .user_store
        .get_user(&target.1)
        .await?
        .ok_or_else(|| anyhow::anyhow!("the target was deleted"))?;
    assert_eq!(kept.status, UserStatus::Active);
    assert_eq!(stored_names(&state, &target.1).await?, vec!["read_clients"]);
    Ok(())
}

/// A device's name or a made-up one is refused at the body, so it is never
/// stored to lie inert.
#[rstest::rstest]
#[case::create_with_a_devices_name(
    "POST",
    "/api/v1/users",
    json!({ "contact_email": "new@example.test", "permissions": ["perform_work"] }),
    "perform_work"
)]
#[case::set_with_a_made_up_name(
    "PUT",
    "/api/v1/users/TARGET/permissions",
    json!({ "permissions": ["nonsense"] }),
    "nonsense"
)]
#[case::set_with_a_devices_name(
    "PUT",
    "/api/v1/users/TARGET/permissions",
    json!({ "permissions": ["submit_results"] }),
    "submit_results"
)]
#[case::amend_with_a_devices_name(
    "PATCH",
    "/api/v1/users/TARGET/permissions",
    json!({ "add": ["submit_results"] }),
    "submit_results"
)]
#[tokio::test]
async fn a_grant_outside_the_user_vocabulary_is_refused(
    #[case] method: &str,
    #[case] path: &str,
    #[case] body: serde_json::Value,
    #[case] name: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let root = server_admin(&state).await?;
    let target = registered_user(&state, &[UserPermission::ReadClients]).await?;
    let accounts = state.user_store.list_users().await?.len();
    let uri = path.replace("TARGET", target.1.as_str());

    let resp = signed_json(&state, &root, method, &uri, &body).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{method} {uri}");
    let refused = body_json(resp).await?;
    let error = json_str(&refused, "error")?;
    assert!(
        error.contains(&format!("unknown variant `{name}`")),
        "the refusal names the name: {refused}"
    );

    assert!(
        state
            .user_store
            .lookup_user_by_email("new@example.test")
            .await?
            .is_none(),
        "the address was not claimed"
    );
    assert_eq!(state.user_store.list_users().await?.len(), accounts);
    assert_eq!(stored_names(&state, &target.1).await?, vec!["read_clients"]);
    Ok(())
}

/// The answer carries the resolved set, and an empty list clears it.
#[tokio::test]
async fn a_set_replaces_every_name_the_account_held() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let root = server_admin(&state).await?;
    let target = registered_user(
        &state,
        &[UserPermission::CancelPlans, UserPermission::ReadClients],
    )
    .await?;
    let uri = format!("/api/v1/users/{}/permissions", target.1);

    let resp = signed_json(
        &state,
        &root,
        "PUT",
        &uri,
        &json!({ "permissions": ["submit_plans"] }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["user_id"], json!(target.1.as_str()));
    assert_eq!(body["changed"], true, "{body}");
    assert_eq!(
        body["permissions"],
        json!(["read_plans", "submit_plans"]),
        "the resolved set, and nothing the body left out"
    );
    assert_eq!(
        stored_names(&state, &target.1).await?,
        vec!["submit_plans"],
        "stored as stated, not resolved"
    );

    // A set naming one its wider name implies normalizes to the stored one.
    let resp = signed_json(
        &state,
        &root,
        "PUT",
        &uri,
        &json!({ "permissions": ["submit_plans", "read_plans"] }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["changed"], false, "{body}");

    let resp = signed_json(&state, &root, "PUT", &uri, &json!({ "permissions": [] })).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["permissions"], json!([]));
    assert_eq!(body["changed"], true, "{body}");
    assert!(
        stored_names(&state, &target.1).await?.is_empty(),
        "an empty list clears the set"
    );

    // The set it already holds is a successful no-op.
    let resp = signed_json(&state, &root, "PUT", &uri, &json!({ "permissions": [] })).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["changed"], false, "{body}");
    Ok(())
}

/// Removing a wide name drops the narrower ones it implied, and an unknown name
/// is carried through untouched.
#[tokio::test]
async fn amending_permissions_matches_the_cli() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let root = server_admin(&state).await?;
    let target = registered_user(&state, &[UserPermission::ManageAnyOrgs]).await?;
    // A name this binary does not know, as a later release might write.
    let stored: UserPermissionSet =
        serde_json::from_value(json!(["manage_any_orgs", "rescore_plans"]))?;
    state
        .user_store
        .put_user_permissions(&target.1, &stored)
        .await?;

    let resp = signed_json(
        &state,
        &root,
        "PATCH",
        &format!("/api/v1/users/{}/permissions", target.1),
        &json!({ "remove": ["manage_any_orgs"], "add": ["read_plans"] }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["changed"], json!(true));
    assert_eq!(body["still_granted"], json!([]));

    let names: Vec<String> = state
        .user_store
        .get_user_permissions(&target.1)
        .await?
        .map(|set| set.names().iter().map(|n| n.to_string()).collect())
        .unwrap_or_default();
    assert_eq!(
        names,
        vec!["read_plans", "rescore_plans"],
        "the wide name goes without leaving its implied names, and the unknown one stays"
    );
    Ok(())
}

/// The answer names what is still granted, as `users permission remove` prints
/// it.
#[tokio::test]
async fn removing_an_implied_name_answers_that_it_is_still_granted() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let root = server_admin(&state).await?;
    let target = registered_user(&state, &[UserPermission::ManageAnyOrgs]).await?;

    let resp = signed_json(
        &state,
        &root,
        "PATCH",
        &format!("/api/v1/users/{}/permissions", target.1),
        &json!({ "remove": ["manage_orgs", "read_orgs", "read_plans"] }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["changed"], json!(false));
    assert_eq!(
        body["still_granted"],
        json!(["manage_orgs", "read_orgs"]),
        "a name the account never held is not still granted"
    );
    assert!(
        body["permissions"]
            .as_array()
            .is_some_and(|names| names.contains(&json!("manage_orgs"))),
        "{body}"
    );
    Ok(())
}

/// Refused rather than answered as a no-op.
#[rstest::rstest]
#[case::an_empty_object(json!({}))]
#[case::two_empty_lists(json!({ "add": [], "remove": [] }))]
#[tokio::test]
async fn an_amend_naming_nothing_is_refused(#[case] body: serde_json::Value) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let root = server_admin(&state).await?;
    let target = registered_user(&state, &[UserPermission::ReadPlans]).await?;

    let uri = format!("/api/v1/users/{}/permissions", target.1);
    let resp = signed_json(&state, &root, "PATCH", &uri, &body).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        json_str(&body_json(resp).await?, "error")?,
        "nothing to amend: send add, remove, or both"
    );
    assert_eq!(stored_names(&state, &target.1).await?, vec!["read_plans"]);
    Ok(())
}

/// Removed first, then added back.
#[rstest::rstest]
#[case::already_held(true)]
#[case::not_yet_held(false)]
#[tokio::test]
async fn a_name_both_added_and_removed_stays_granted(#[case] held: bool) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let root = server_admin(&state).await?;
    let grants: &[UserPermission] = if held {
        &[UserPermission::ReadPlans]
    } else {
        &[]
    };
    let target = registered_user(&state, grants).await?;

    let resp = signed_json(
        &state,
        &root,
        "PATCH",
        &format!("/api/v1/users/{}/permissions", target.1),
        &json!({ "add": ["read_plans"], "remove": ["read_plans"] }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["still_granted"], json!(["read_plans"]), "{body}");
    assert_eq!(body["permissions"], json!(["read_plans"]), "{body}");
    assert_eq!(
        body["changed"],
        json!(!held),
        "changed only when the account did not hold it before"
    );
    assert_eq!(stored_names(&state, &target.1).await?, vec!["read_plans"]);
    Ok(())
}

/// The account a `GET /api/v1/users/{user_id}` answers, without the fields that
/// depend on the reader.
fn account_of(mut record: serde_json::Value) -> serde_json::Value {
    if let Some(fields) = record.as_object_mut() {
        [
            "may_administer",
            "may_grant",
            "may_revoke",
            "awaits_sign_in",
        ]
        .iter()
        .for_each(|key| {
            fields.remove(*key);
        });
    }
    record
}

/// As the writes judge it: an org admin grants what it holds and revokes what
/// is stored, except its own administration.
#[tokio::test]
async fn the_record_says_what_the_reader_may_grant_and_revoke() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (root, acme, lead) = acme_with_admin(&state).await?;
    let member = keyless(&state, &acme, &[UserPermission::ReadAnyPlans]).await?;
    let record = |signer: &UserSigner, id: String| {
        let (key, user_id, key_id) = signer.clone();
        let state = state.clone();
        async move {
            let uri = format!("/api/v1/users/{id}");
            let resp = authed_user_get(&state, &key, &user_id, &key_id, &uri).await?;
            anyhow::ensure!(resp.status() == StatusCode::OK, "{uri}: {}", resp.status());
            let body = body_json(resp).await?;
            crate::api_schema::assert_conforms("UserDetail", &body)?;
            anyhow::Ok(body)
        }
    };

    // Outranked: the member holds a server-wide name the org admin lacks.
    let seen = record(&lead, member.to_string()).await?;
    assert_eq!(seen["may_administer"], json!(false), "{seen}");
    assert_eq!(seen["may_grant"], json!([]), "{seen}");

    // Its own record: it grants what it holds, and keeps its administration.
    let own = record(&lead, lead.1.to_string()).await?;
    assert_eq!(own["may_administer"], json!(true), "{own}");
    let held = [
        "submit_plans",
        "read_plans",
        "cancel_plans",
        "read_clients",
        "read_orgs",
        "manage_clients",
        "edit_catalog",
    ];
    assert_eq!(own["may_grant"], json!(held), "{own}");
    assert_eq!(
        own["may_revoke"],
        json!([
            "cancel_plans",
            "edit_catalog",
            "manage_clients",
            "submit_plans"
        ]),
        "{own}"
    );

    // A service account is offered no admin name.
    let robot = registered_user(&state, &[]).await?;
    let org_id = pipette_plan_types::OrgId::try_new(acme.clone())?;
    make_service_account(&state, &robot.1, Some(org_id)).await?;
    let seen = record(&lead, robot.1.to_string()).await?;
    assert_eq!(
        seen["may_grant"],
        json!([
            "submit_plans",
            "read_plans",
            "cancel_plans",
            "read_clients",
            "read_orgs",
            "edit_catalog"
        ]),
        "{seen}"
    );

    // The server's admin reaches the member, and grants every name.
    let seen = record(&root, member.to_string()).await?;
    assert_eq!(seen["may_administer"], json!(true), "{seen}");
    let every: Vec<String> = <UserPermission as strum::IntoEnumIterator>::iter()
        .map(|name| name.as_ref().to_string())
        .collect();
    assert_eq!(seen["may_grant"], json!(every), "{seen}");
    assert_eq!(seen["may_revoke"], json!(["read_any_plans"]), "{seen}");
    Ok(())
}

/// By either permission write, an implying name, or leaving the organization;
/// any other change to itself goes through.
#[rstest::rstest]
#[case::amend(false, "PATCH", "permissions", json!({ "remove": ["manage_orgs"] }))]
#[case::set(false, "PUT", "permissions", json!({ "permissions": ["read_plans"] }))]
#[case::implied(true, "PATCH", "permissions", json!({ "remove": ["manage_any_orgs"] }))]
#[case::release(false, "PUT", "org", json!({ "org_id": null }))]
#[tokio::test]
async fn an_account_keeps_its_own_administration(
    #[case] server_wide: bool,
    #[case] method: &str,
    #[case] route: &str,
    #[case] body: serde_json::Value,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (root, acme, lead) = acme_with_admin(&state).await?;
    let caller = if server_wide { &root } else { &lead };
    let uri = format!("/api/v1/users/{}/{route}", caller.1);
    let stored = state.user_store.get_user_permissions(&caller.1).await?;

    let resp = signed_json(&state, caller, method, &uri, &body).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let error = json_str(&body_json(resp).await?, "error")?.to_string();
    assert!(error.starts_with("an account cannot "), "{error}");
    assert_eq!(
        state.user_store.get_user_permissions(&caller.1).await?,
        stored,
        "nothing was written"
    );
    assert_eq!(
        state
            .org_store
            .get_membership(&lead.1)
            .await?
            .map(|org| org.to_string()),
        Some(acme)
    );

    let other = json!({ "remove": ["cancel_plans"] });
    let uri = format!("/api/v1/users/{}/permissions", caller.1);
    let resp = signed_json(&state, caller, "PATCH", &uri, &other).await?;
    assert_eq!(resp.status(), StatusCode::OK, "a change that keeps it");
    Ok(())
}

/// Whether the `field` list of a `may` view holds `name`.
fn lists(view: &serde_json::Value, field: &str, name: &str) -> bool {
    view[field]
        .as_array()
        .is_some_and(|names| names.contains(&json!(name)))
}

/// As its routes would judge it; an account still pending reaches nothing.
#[tokio::test]
async fn the_own_record_says_how_far_each_authority_reaches() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (root, acme, lead) = acme_with_admin(&state).await?;
    let may = |signer: &UserSigner| {
        let (key, user_id, key_id) = signer.clone();
        let state = state.clone();
        async move {
            let resp = authed_user_get(&state, &key, &user_id, &key_id, "/api/v1/users/me").await?;
            let body = body_json(resp).await?;
            crate::api_schema::assert_conforms("OwnUserView", &body)?;
            anyhow::Ok(body["may"].clone())
        }
    };

    let scoped = may(&lead).await?;
    let granted = [
        "submit_plans",
        "read_plans",
        "cancel_plans",
        "read_clients",
        "read_orgs",
        "manage_orgs",
        "manage_clients",
        "edit_catalog",
    ];
    assert_eq!(
        scoped,
        json!({
            "read_roster": "own",
            "administer_roster": "own",
            "read_fleet": true,
            "administer_fleet": "own",
            "edit_catalog": "own",
            "read_plans": "own",
            "file_plans": true,
            "create_orgs": false,
            "grant": granted,
            "grant_service": ["submit_plans", "read_plans", "cancel_plans", "read_clients", "read_orgs", "edit_catalog"],
        })
    );

    let server = may(&root).await?;
    [
        "read_roster",
        "administer_roster",
        "administer_fleet",
        "read_plans",
    ]
    .into_iter()
    .for_each(|field| assert_eq!(server[field], json!("every"), "{field}: {server}"));
    assert_eq!(server["create_orgs"], json!(true), "{server}");
    assert_eq!(server["edit_catalog"], json!("every"), "{server}");
    assert!(lists(&server, "grant", "edit_any_catalog"), "{server}");
    assert!(
        !lists(&server, "grant_service", "edit_any_catalog"),
        "{server}"
    );

    // Accounts in acme, by what they hold and whether each is a service account.
    let cases: [(&str, &[UserPermission], bool, &str); 3] = [
        (
            "member",
            UserPermissionGroup::OrgMember.permissions(),
            false,
            "own",
        ),
        ("ungranted", &[], false, "none"),
        ("service", &[UserPermission::EditCatalog], true, "own"),
    ];
    for (label, held, service, expected) in cases {
        let account = registered_user(&state, held).await?;
        if service {
            make_service_account(&state, &account.1, Some(acme.parse()?)).await?;
        } else {
            set_user_org(&state, &account.1, Some(acme.parse()?)).await?;
        }
        let view = may(&account).await?;
        assert_eq!(view["edit_catalog"], json!(expected), "{label}: {view}");
        assert!(
            !lists(&view, "grant", "edit_any_catalog"),
            "{label}: {view}"
        );
    }

    let member = registered_user(&state, UserPermissionGroup::OrgMember.permissions()).await?;
    set_user_org(&state, &member.1, Some(acme.parse()?)).await?;
    grant_user(&state, &member.1, &[UserPermission::EditAnyCatalog]).await?;
    assert_eq!(may(&member).await?["edit_catalog"], json!("every"));

    set_user_status(&state, &lead.1, UserStatus::Pending).await?;
    let pending = may(&lead).await?;
    assert_eq!(pending["administer_roster"], json!("none"), "{pending}");
    assert_eq!(pending["edit_catalog"], json!("none"), "{pending}");
    assert_eq!(pending["grant"], json!([]), "{pending}");
    Ok(())
}

// ── the credentials an account signs in with ────────────────────────────────

/// Narrow `admin` to the roster and device names, so a member holding a plan
/// name holds what it does not.
async fn roster_only(state: &AppState, admin: &UserSigner) -> anyhow::Result<()> {
    grant_user(
        state,
        &admin.1,
        &[UserPermission::ManageOrgs, UserPermission::ManageClients],
    )
    .await
}

/// An org admin and the organization it administers, placed there by a server
/// admin.
async fn acme_with_admin(state: &AppState) -> anyhow::Result<(UserSigner, String, UserSigner)> {
    let root = server_admin(state).await?;
    let acme = create_org(state, &root, "acme").await?;
    let lead = registered_user(state, &[]).await?;
    place(
        state,
        &root,
        &lead.1,
        &acme,
        UserPermissionGroup::OrgAdmin.permissions(),
    )
    .await?;
    Ok((root, acme, lead))
}

/// An account in `org_id` holding `grants` and **no key**: nobody signs in as
/// it yet, which is the account a pre-auth key minted here exists for.
async fn keyless(
    state: &AppState,
    org_id: &str,
    grants: &[UserPermission],
) -> anyhow::Result<UserId> {
    let email = format!(
        "keyless-{}@example.test",
        hex::encode(uuid::Uuid::now_v7().as_bytes())
    );
    let user = create_user(state, &email).await?;
    set_user_org(
        state,
        &user.user_id,
        Some(pipette_plan_types::OrgId::try_new(org_id)?),
    )
    .await?;
    grant_user(state, &user.user_id, grants).await?;
    Ok(user.user_id)
}

async fn mint(
    state: &AppState,
    admin: &UserSigner,
    user_id: &UserId,
) -> anyhow::Result<axum::http::Response<axum::body::Body>> {
    signed_json(
        state,
        admin,
        "POST",
        &format!("/api/v1/users/{user_id}/preauth-keys"),
        &json!({}),
    )
    .await
}

/// The raw response, since half the callers expect a refusal.
async fn present(
    state: &AppState,
    token: &str,
) -> anyhow::Result<axum::http::Response<axum::body::Body>> {
    let key = signing_key()?;
    unauthed_post(
        state,
        "/api/v1/users/register",
        &json!({
            "public_key": hex::encode(key.verifying_key().as_bytes()),
            "user_preauth_key": token,
            "key_details": "another machine",
        }),
    )
    .await
}

/// A single-use key the host's `users preauth create` would mint for `user_id`,
/// not yet stored.
fn host_minted(user_id: &UserId) -> anyhow::Result<pipette_core::user_preauth::UserMintedKey> {
    Ok(pipette_core::user_preauth::mint(
        pipette_core::user_preauth::UserPreauthParams {
            user_id: user_id.clone(),
            usage: pipette_plan_types::UserPreauthUsage::SingleUse,
            expires_at: None,
            note: None,
        },
        chrono::Utc::now(),
    )?)
}

#[tokio::test]
async fn an_org_admin_onboards_a_member_with_a_preauth_key() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (_, acme, lead) = acme_with_admin(&state).await?;

    let resp = signed_json(
        &state,
        &lead,
        "POST",
        "/api/v1/users",
        &json!({ "contact_email": "hire@example.test", "permissions": ["read_clients"] }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let member = UserId::try_new(json_str(&body_json(resp).await?, "user_id")?)?;

    let uri = format!("/api/v1/users/{member}/preauth-keys");
    let resp = signed_json(
        &state,
        &lead,
        "POST",
        &uri,
        &json!({ "expires_in_days": 7, "note": "for the new hire" }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(
        resp.headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok()),
        Some("no-store"),
        "a credential is not cached"
    );
    let minted = body_json(resp).await?;
    let token = json_str(&minted, "token")?.to_string();
    let secret = token
        .split_once('.')
        .map(|(_, secret)| secret.to_string())
        .ok_or_else(|| anyhow::anyhow!("a token carries its secret after the dot"))?;
    assert!(token.starts_with("user_preauth_"), "{token}");
    assert_eq!(minted["usage"], json!("single_use"));
    assert_eq!(minted["user_id"], json!(member.as_str()));
    assert_eq!(minted["minted_by"], json!(lead.1.as_str()));
    assert_eq!(minted["note"], json!("for the new hire"));
    assert!(minted.get("secret_hash").is_none(), "{minted}");
    let expires_at: chrono::DateTime<chrono::Utc> = json_str(&minted, "expires_at")?.parse()?;
    let hours = (expires_at - chrono::Utc::now()).num_hours();
    assert!((6 * 24..=7 * 24).contains(&hours), "expires in {hours}h");

    // Listed by its id and its minter, never by its secret.
    let listed = body_json(signed_empty(&state, &lead, "GET", &uri).await?).await?;
    assert_eq!(listed["items"].as_array().map(Vec::len), Some(1));
    assert_eq!(listed["items"][0]["key_id"], minted["key_id"]);
    assert_eq!(listed["items"][0]["minted_by"], json!(lead.1.as_str()));
    let text = listed.to_string();
    assert!(
        !text.contains(&secret) && !text.contains("secret_hash"),
        "{text}"
    );

    // The member's machine registers with it: active, and in acme.
    let key = signing_key()?;
    let key_id = register_user_key(&state, &token, &key, "new hire laptop").await?;
    let me = body_json(authed_user_get(&state, &key, &member, &key_id, "/api/v1/users/me").await?)
        .await?;
    assert_eq!(me["status"], json!("active"));
    assert_eq!(me["org_id"], json!(acme));

    // Spent, gone from the listing, and the account's keys show who signs in.
    let listed = body_json(signed_empty(&state, &lead, "GET", &uri).await?).await?;
    assert_eq!(listed["items"], json!([]));
    assert_eq!(
        present(&state, &token).await?.status(),
        StatusCode::UNAUTHORIZED
    );
    let keys = body_json(
        signed_empty(
            &state,
            &lead,
            "GET",
            &format!("/api/v1/users/{member}/keys"),
        )
        .await?,
    )
    .await?;
    assert_eq!(keys["items"][0]["key_details"], json!("new hire laptop"));
    Ok(())
}

/// A pre-auth key onboards; it never takes over.
#[tokio::test]
async fn a_preauth_key_is_minted_only_for_an_account_nobody_signed_in_as() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (root, acme, lead) = acme_with_admin(&state).await?;
    let member = registered_user(&state, &[]).await?;
    place(
        &state,
        &root,
        &member.1,
        &acme,
        &[UserPermission::ReadPlans],
    )
    .await?;

    // Stripping the member first changes nothing: its sign-in is the obstacle.
    let resp = signed_json(
        &state,
        &lead,
        "PUT",
        &format!("/api/v1/users/{}/permissions", member.1),
        &json!({ "permissions": [] }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    for target in [&member.1, &lead.1] {
        let resp = mint(&state, &lead, target).await?;
        assert_eq!(resp.status(), StatusCode::CONFLICT, "{target}");
        assert!(json_str(&body_json(resp).await?, "error")?.contains("has signed in"));
    }
    assert!(state.user_store.list_user_preauth_keys().await?.is_empty());
    Ok(())
}

/// Its administrator may mint a service account keys, so retyping a colleague
/// would be a takeover. Back to a person's is always open.
#[tokio::test]
async fn an_account_becomes_a_service_only_before_a_sign_in() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (root, acme, lead) = acme_with_admin(&state).await?;
    let retype = |user_id: UserId, body: serde_json::Value| {
        let (state, lead) = (&state, &lead);
        async move {
            signed_json(
                state,
                lead,
                "PATCH",
                &format!("/api/v1/users/{user_id}"),
                &body,
            )
            .await
        }
    };

    let fresh = keyless(&state, &acme, &[]).await?;
    for wanted in ["service", "human"] {
        let resp = retype(fresh.clone(), json!({ "account_type": wanted })).await?;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(json_str(&body_json(resp).await?, "account_type")?, wanted);
    }

    let member = registered_user(&state, &[]).await?;
    place(
        &state,
        &root,
        &member.1,
        &acme,
        &[UserPermission::ReadPlans],
    )
    .await?;
    let resp = retype(member.1.clone(), json!({ "account_type": "service" })).await?;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert!(json_str(&body_json(resp).await?, "error")?.contains("has signed in"));
    let kept = state
        .user_store
        .get_user(&member.1)
        .await?
        .ok_or_else(|| anyhow::anyhow!("the member still exists"))?;
    assert_eq!(kept.account_type, UserAccountType::Human);

    state
        .user_store
        .put_user(&pipette_core::user::User {
            account_type: UserAccountType::Service,
            ..kept
        })
        .await?;
    let resp = retype(member.1.clone(), json!({ "account_type": "human" })).await?;
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = retype(member.1.clone(), json!({})).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(
        json_str(&body_json(resp).await?, "error")?
            .contains("name a status, an account_type or a campaign_scope")
    );
    Ok(())
}

/// Still held to the minter's grants; a person's account takes one only before
/// its first sign-in.
#[tokio::test]
async fn a_service_account_is_minted_a_key_at_any_time() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (_root, acme, lead) = acme_with_admin(&state).await?;
    roster_only(&state, &lead).await?;
    let robot = keyless(&state, &acme, &[]).await?;
    let resp = signed_json(
        &state,
        &lead,
        "PATCH",
        &format!("/api/v1/users/{robot}"),
        &json!({ "account_type": "service" }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);

    for _ in 0..2 {
        let minted = body_json(mint(&state, &lead, &robot).await?).await?;
        assert_eq!(
            present(&state, json_str(&minted, "token")?).await?.status(),
            StatusCode::CREATED
        );
    }
    let keys = state.user_store.list_user_keys(&robot).await?;
    assert_eq!(keys.len(), 2);
    let resp = signed_empty(
        &state,
        &lead,
        "DELETE",
        &format!("/api/v1/users/{robot}/keys/{}", keys[0].key_id),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert_eq!(state.user_store.list_user_keys(&robot).await?, keys[1..]);

    grant_user(&state, &robot, &[UserPermission::ReadPlans]).await?;
    let resp = mint(&state, &lead, &robot).await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert!(json_str(&body_json(resp).await?, "error")?.contains("holds what you do not"));
    grant_user(&state, &robot, &[]).await?;

    let resp = signed_json(
        &state,
        &lead,
        "PATCH",
        &format!("/api/v1/users/{robot}"),
        &json!({ "account_type": "human" }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        mint(&state, &lead, &robot).await?.status(),
        StatusCode::CONFLICT
    );
    Ok(())
}

/// Holding no key is not the same as nobody having signed in as it.
#[rstest::rstest]
#[case::by_its_holder(false)]
#[case::by_its_administrator(true)]
#[tokio::test]
async fn an_account_that_revoked_its_keys_is_not_onboarded_again(
    #[case] by_administrator: bool,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (_root, acme, lead) = acme_with_admin(&state).await?;
    let member = keyless(&state, &acme, &[]).await?;
    let token = json_str(
        &body_json(mint(&state, &lead, &member).await?).await?,
        "token",
    )?
    .to_string();
    let key = signing_key()?;
    let key_id = register_user_key(&state, &token, &key, "only laptop").await?;

    let resp = if by_administrator {
        signed_empty(
            &state,
            &lead,
            "DELETE",
            &format!("/api/v1/users/{member}/keys/{key_id}"),
        )
        .await?
    } else {
        authed_user_delete(
            &state,
            &key,
            &member,
            &key_id,
            &format!("/api/v1/users/me/keys/{key_id}"),
        )
        .await?
    };
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert!(state.user_store.list_user_keys(&member).await?.is_empty());
    assert_eq!(
        mint(&state, &lead, &member).await?.status(),
        StatusCode::CONFLICT
    );
    Ok(())
}

/// It carries no mark, so whether anybody signed in as it is unknown: refused,
/// not trusted.
#[tokio::test]
async fn an_account_older_than_the_mark_is_not_onboarded() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (_root, acme, lead) = acme_with_admin(&state).await?;
    let member = keyless(&state, &acme, &[]).await?;
    state
        .user_store
        .clear_user_awaiting_sign_in(&member)
        .await?;

    let resp = mint(&state, &lead, &member).await?;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert!(json_str(&body_json(resp).await?, "error")?.contains("users preauth create"));
    Ok(())
}

/// A key signs in as the account, so none is minted for one holding more than
/// the caller or a name this build cannot compare. A server-wide name is
/// refused by rank first; a keyless peer holding the same set is within reach.
#[tokio::test]
async fn a_preauth_key_is_held_to_the_callers_own_grants() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (_root, acme, lead) = acme_with_admin(&state).await?;
    roster_only(&state, &lead).await?;

    let wide = keyless(&state, &acme, &[UserPermission::SubmitPlans]).await?;
    let resp = mint(&state, &lead, &wide).await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let error = json_str(&body_json(resp).await?, "error")?.to_string();
    assert!(
        error.starts_with("a pre-auth key signs in as the account, and it holds what you do not: ")
            && error.contains("submit_plans"),
        "{error}"
    );

    let server_wide = keyless(&state, &acme, &[UserPermission::ReadAnyPlans]).await?;
    let resp = mint(&state, &lead, &server_wide).await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        json_str(&body_json(resp).await?, "error")?,
        "you cannot change an account holding server-wide names you do not: read_any_plans"
    );

    let newer = keyless(&state, &acme, &[]).await?;
    let stored: UserPermissionSet = serde_json::from_value(json!(["rescore_plans"]))?;
    state
        .user_store
        .put_user_permissions(&newer, &stored)
        .await?;
    let resp = mint(&state, &lead, &newer).await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert!(json_str(&body_json(resp).await?, "error")?.contains("rescore_plans"));
    assert!(
        state.user_store.list_user_preauth_keys().await?.is_empty(),
        "a refused mint stores nothing"
    );

    let peer = keyless(
        &state,
        &acme,
        &[UserPermission::ManageOrgs, UserPermission::ManageClients],
    )
    .await?;
    assert_eq!(
        mint(&state, &lead, &peer).await?.status(),
        StatusCode::CREATED
    );
    Ok(())
}

/// Outside it they read as absent; `read_orgs` alone reads the roster, not the
/// credentials.
#[tokio::test]
async fn credentials_are_administered_inside_the_organization() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (root, acme, lead) = acme_with_admin(&state).await?;
    let rival = create_org(&state, &root, "rival").await?;
    let outsider = keyless(&state, &rival, &[]).await?;
    let unaffiliated = create_user(&state, "loose@example.test").await?.user_id;

    for target in [&outsider, &unaffiliated] {
        for (method, uri) in [
            ("GET", format!("/api/v1/users/{target}/keys")),
            (
                "DELETE",
                format!("/api/v1/users/{target}/keys/{UNHELD_KEY}"),
            ),
            ("GET", format!("/api/v1/users/{target}/preauth-keys")),
            ("POST", format!("/api/v1/users/{target}/preauth-keys")),
        ] {
            let resp = signed_json(&state, &lead, method, &uri, &json!({})).await?;
            assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{method} {uri}");
        }
    }

    let reader = registered_user(&state, &[]).await?;
    place(&state, &root, &reader.1, &acme, &[UserPermission::ReadOrgs]).await?;
    let member = keyless(&state, &acme, &[]).await?;
    let wide = registered_user(&state, &[]).await?;
    place(
        &state,
        &root,
        &wide.1,
        &acme,
        &[UserPermission::ManageOrgs, UserPermission::ReadAnyOrgs],
    )
    .await?;
    // A roster reader, and an administrator reading a rival's account, administer
    // neither, so neither learns what only an administrator reads.
    for (signer, target) in [(&reader, &member), (&wide, &outsider)] {
        let uri = format!("/api/v1/users/{target}");
        let resp = signed_empty(&state, signer, "GET", &uri).await?;
        assert_eq!(resp.status(), StatusCode::OK, "{uri}");
        let read = body_json(resp).await?;
        assert_eq!(read.get("awaits_sign_in"), None, "{uri}: {read}");
        assert_eq!(read.get("stored_permissions"), None, "{uri}: {read}");
    }

    for (method, uri) in [
        ("GET", format!("/api/v1/users/{member}/keys")),
        (
            "DELETE",
            format!("/api/v1/users/{member}/keys/{UNHELD_KEY}"),
        ),
        ("POST", format!("/api/v1/users/{member}/preauth-keys")),
    ] {
        let resp = signed_json(&state, &reader, method, &uri, &json!({})).await?;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{method} {uri}");
    }
    Ok(())
}

/// A revoked key registers nothing, and a second mint replaces the first.
#[tokio::test]
async fn a_preauth_key_is_revoked_through_its_own_account() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (_root, acme, lead) = acme_with_admin(&state).await?;
    let (a, b) = (
        keyless(&state, &acme, &[]).await?,
        keyless(&state, &acme, &[]).await?,
    );

    let first = body_json(mint(&state, &lead, &a).await?).await?;
    let minted = body_json(mint(&state, &lead, &a).await?).await?;
    assert_eq!(
        present(&state, json_str(&first, "token")?).await?.status(),
        StatusCode::UNAUTHORIZED,
        "the second key revoked the first"
    );
    let key_id = json_str(&minted, "key_id")?;
    let token = json_str(&minted, "token")?;

    let resp = signed_empty(
        &state,
        &lead,
        "DELETE",
        &format!("/api/v1/users/{b}/preauth-keys/{key_id}"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let revoke = format!("/api/v1/users/{a}/preauth-keys/{key_id}");
    let resp = signed_empty(&state, &lead, "DELETE", &revoke).await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        present(&state, token).await?.status(),
        StatusCode::UNAUTHORIZED
    );
    let resp = signed_empty(&state, &lead, "DELETE", &revoke).await?;
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "a revocation is reported once"
    );
    Ok(())
}

#[tokio::test]
async fn a_mint_leaves_a_host_minted_key_standing() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (_root, acme, lead) = acme_with_admin(&state).await?;
    let member = keyless(&state, &acme, &[]).await?;
    let host = host_minted(&member)?;
    state.user_store.put_user_preauth_key(&host.key).await?;

    let resp = mint(&state, &lead, &member).await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let minted = body_json(resp).await?;

    let resp = signed_empty(
        &state,
        &lead,
        "GET",
        &format!("/api/v1/users/{member}/preauth-keys"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let listed = body_json(resp).await?;
    let mut ids: Vec<&str> = listed["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|key| key["key_id"].as_str())
        .collect();
    ids.sort_unstable();
    let mut both = vec![host.key.key_id.as_str(), json_str(&minted, "key_id")?];
    both.sort_unstable();
    assert_eq!(ids, both, "the host's key stands beside the new one");
    let kept = listed["items"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|key| key["key_id"] == json!(host.key.key_id.as_str()))
        .ok_or_else(|| anyhow::anyhow!("the host's key is not listed: {listed}"))?;
    assert!(kept.get("minted_by").is_none(), "{listed}");
    assert_eq!(state.user_store.list_user_preauth_keys().await?.len(), 2);

    assert_eq!(
        present(&state, host.token.expose()).await?.status(),
        StatusCode::CREATED,
        "the host's key still registers"
    );
    Ok(())
}

/// The account reads as absent before any key is looked up.
#[tokio::test]
async fn a_preauth_key_in_another_organization_is_not_revoked() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (root, _acme, lead) = acme_with_admin(&state).await?;
    let rival = create_org(&state, &root, "rival").await?;
    let outsider = keyless(&state, &rival, &[]).await?;
    let minted = body_json(mint(&state, &root, &outsider).await?).await?;
    let key_id = json_str(&minted, "key_id")?;

    let resp = signed_empty(
        &state,
        &lead,
        "DELETE",
        &format!("/api/v1/users/{outsider}/preauth-keys/{key_id}"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        json_str(&body_json(resp).await?, "error")?,
        "no such user",
        "absent as an account, not as a key"
    );
    assert!(
        state
            .user_store
            .list_user_preauth_keys()
            .await?
            .iter()
            .any(|key| key.key_id.as_str() == key_id)
    );
    assert_eq!(
        present(&state, json_str(&minted, "token")?).await?.status(),
        StatusCode::CREATED,
        "the key still registers"
    );
    Ok(())
}

#[tokio::test]
async fn the_preauth_listing_shows_each_keys_provenance_newest_first() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (_root, acme, lead) = acme_with_admin(&state).await?;
    let member = keyless(&state, &acme, &[]).await?;
    let resp = mint(&state, &lead, &member).await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let minted = body_json(resp).await?;

    // Minted on the host two days ago, expired since yesterday.
    let now = chrono::Utc::now();
    let mut host = pipette_core::user_preauth::mint(
        pipette_core::user_preauth::UserPreauthParams {
            user_id: member.clone(),
            usage: pipette_plan_types::UserPreauthUsage::SingleUse,
            expires_at: Some(now - chrono::Duration::days(1)),
            note: Some(pipette_plan_types::NonEmptyTrimmedString::try_new(
                "from the host",
            )?),
        },
        now - chrono::Duration::days(2),
    )?
    .key;
    // Sorts first by name, so only the date puts it last.
    host.key_id = pipette_plan_types::UserPreauthKeyId::try_new("0000000000000000")?;
    state.user_store.put_user_preauth_key(&host).await?;

    let resp = signed_empty(
        &state,
        &lead,
        "GET",
        &format!("/api/v1/users/{member}/preauth-keys"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let listed = body_json(resp).await?;
    crate::api_schema::assert_conforms("UserPreauthKeyViewPage", &listed)?;
    assert_eq!(listed["next_cursor"], json!(null), "one page: {listed}");
    let keys = &listed["items"];
    assert_eq!(keys.as_array().map(Vec::len), Some(2), "{listed}");

    assert_eq!(
        keys[0]["key_id"], minted["key_id"],
        "newest first: {listed}"
    );
    assert_eq!(keys[0]["minted_by"], json!(lead.1.as_str()));
    assert_eq!(keys[0]["expired"], json!(false));

    assert_eq!(keys[1]["key_id"], json!("0000000000000000"));
    assert_eq!(
        keys[1]["expired"],
        json!(true),
        "an expired key stays, marked"
    );
    assert!(keys[1].get("minted_by").is_none(), "{listed}");
    assert_eq!(keys[1]["note"], json!("from the host"));
    Ok(())
}

/// Refused rather than dropped.
#[rstest::rstest]
#[case::template(json!({ "permissions": ["read_plans"] }), true)]
#[case::usage(json!({ "usage": "multi_use" }), true)]
#[case::multi_use(json!({ "multi_use": true }), true)]
#[case::no_days(json!({ "expires_in_days": 0 }), false)]
#[case::past_the_ceiling(json!({ "expires_in_days": 91 }), false)]
#[tokio::test]
async fn a_mint_refuses_what_an_onboarding_key_does_not_carry(
    #[case] body: serde_json::Value,
    #[case] schema_refuses: bool,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (_root, acme, lead) = acme_with_admin(&state).await?;
    let member = keyless(&state, &acme, &[]).await?;

    let uri = format!("/api/v1/users/{member}/preauth-keys");
    let resp = signed_json(&state, &lead, "POST", &uri, &body).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{body}");
    assert!(state.user_store.list_user_preauth_keys().await?.is_empty());
    assert_eq!(
        crate::api_schema::assert_conforms("MintUserPreauthRequest", &body).is_err(),
        schema_refuses,
        "{body}"
    );
    Ok(())
}

#[tokio::test]
async fn a_mint_note_is_at_most_4096_characters() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (_root, acme, lead) = acme_with_admin(&state).await?;
    let member = keyless(&state, &acme, &[]).await?;
    let uri = format!("/api/v1/users/{member}/preauth-keys");

    let resp = signed_json(
        &state,
        &lead,
        "POST",
        &uri,
        &json!({ "note": "a".repeat(4097) }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        json_str(&body_json(resp).await?, "error")?,
        "note is longer than 4096 characters"
    );
    assert!(
        state.user_store.list_user_preauth_keys().await?.is_empty(),
        "a refused mint stores nothing"
    );

    // Two bytes a character, so a byte bound would refuse it.
    let note = "é".repeat(4096);
    let resp = signed_json(&state, &lead, "POST", &uri, &json!({ "note": note })).await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(body_json(resp).await?["note"], json!(note));
    assert_eq!(state.user_store.list_user_preauth_keys().await?.len(), 1);
    Ok(())
}

/// A copy kept back is refused once the account holds a key. The default expiry
/// is the 90-day ceiling.
#[tokio::test]
async fn a_key_minted_here_admits_only_the_first_machine() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (_root, acme, lead) = acme_with_admin(&state).await?;
    let member = keyless(&state, &acme, &[]).await?;

    let minted = body_json(mint(&state, &lead, &member).await?).await?;
    let expires_at: chrono::DateTime<chrono::Utc> = json_str(&minted, "expires_at")?.parse()?;
    assert_eq!((expires_at - chrono::Utc::now()).num_days(), 89);

    // A second key of the same minter, stored beside it as a race or a
    // failed replacement would leave it.
    let mut kept = host_minted(&member)?;
    kept.key.minted_by = Some(lead.1.clone());
    state.user_store.put_user_preauth_key(&kept.key).await?;

    assert_eq!(
        present(&state, json_str(&minted, "token")?).await?.status(),
        StatusCode::CREATED
    );
    let resp = present(&state, kept.token.expose()).await?;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(
        state.user_store.list_user_keys(&member).await?.len(),
        1,
        "one machine signs in"
    );
    assert_eq!(
        mint(&state, &lead, &member).await?.status(),
        StatusCode::CONFLICT
    );
    Ok(())
}

/// The key carries its minter's authority. It is spent either way, and the
/// refusal names no reason.
#[rstest::rstest]
#[case::minter_lost_the_grant("lost_grant")]
#[case::minter_disabled("disabled")]
#[case::account_moved_out("moved")]
#[case::account_widened("widened")]
#[case::account_holds_an_unknown_name("unknown")]
#[tokio::test]
async fn a_key_admits_nothing_once_its_minter_could_not_mint_it(
    #[case] change: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (root, acme, lead) = acme_with_admin(&state).await?;
    let member = keyless(&state, &acme, &[]).await?;
    let minted = body_json(mint(&state, &lead, &member).await?).await?;

    match change {
        "lost_grant" => grant_user(&state, &lead.1, &[UserPermission::ReadOrgs]).await?,
        "disabled" => {
            set_user_status(&state, &lead.1, pipette_plan_types::UserStatus::Disabled).await?
        }
        "moved" => {
            let rival = create_org(&state, &root, "rival").await?;
            set_user_org(
                &state,
                &member,
                Some(pipette_plan_types::OrgId::try_new(rival)?),
            )
            .await?;
        }
        "unknown" => {
            // A name a later release might write, which this build cannot compare.
            let stored: UserPermissionSet = serde_json::from_value(json!(["rescore_plans"]))?;
            state
                .user_store
                .put_user_permissions(&member, &stored)
                .await?
        }
        _ => grant_user(&state, &member, &[UserPermission::ReadAnyPlans]).await?,
    }

    let resp = present(&state, json_str(&minted, "token")?).await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{change}");
    assert_eq!(
        json_str(&body_json(resp).await?, "error")?,
        "this pre-auth key no longer admits the account; ask its administrator for a new one",
        "{change}"
    );
    assert!(state.user_store.list_user_keys(&member).await?.is_empty());
    assert_eq!(
        present(&state, json_str(&minted, "token")?).await?.status(),
        StatusCode::UNAUTHORIZED,
        "the refused key is spent"
    );
    Ok(())
}

/// A second revocation is reported, not answered `204`. The stolen-key recovery
/// revokes while disabled, and enabling brings back only the other key.
#[rstest::rstest]
#[case::active(false)]
#[case::while_disabled(true)]
#[tokio::test]
async fn an_org_admin_revokes_one_of_a_members_keys(
    #[case] while_disabled: bool,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (root, acme, lead) = acme_with_admin(&state).await?;
    let member = registered_user(&state, &[]).await?;
    place(
        &state,
        &root,
        &member.1,
        &acme,
        &[UserPermission::ReadPlans],
    )
    .await?;
    let (key, user_id, key_id) = &member;
    let second = signing_key()?;
    let resp = authed_user_post(
        &state,
        key,
        user_id,
        key_id,
        "/api/v1/users/me/keys",
        &json!({
            "public_key": hex::encode(second.verifying_key().as_bytes()),
            "key_details": "second laptop",
        }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let second_id = UserKeyId::try_new(json_str(&body_json(resp).await?, "key_id")?)?;

    let listed = body_json(
        signed_empty(
            &state,
            &lead,
            "GET",
            &format!("/api/v1/users/{user_id}/keys"),
        )
        .await?,
    )
    .await?;
    let mut ids: Vec<&str> = listed["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|key| key["key_id"].as_str())
        .collect();
    ids.sort_unstable();
    let mut both = vec![key_id.as_str(), second_id.as_str()];
    both.sort_unstable();
    assert_eq!(ids, both, "{listed}");

    let account = format!("/api/v1/users/{user_id}");
    let revoke = format!("{account}/keys/{key_id}");
    if while_disabled {
        let resp = signed_json(
            &state,
            &lead,
            "PATCH",
            &account,
            &json!({ "status": "disabled" }),
        )
        .await?;
        assert_eq!(resp.status(), StatusCode::OK);
    }
    let resp = signed_empty(&state, &lead, "DELETE", &revoke).await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    if while_disabled {
        let resp = signed_json(
            &state,
            &lead,
            "PATCH",
            &account,
            &json!({ "status": "active" }),
        )
        .await?;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    let resp = authed_user_get(&state, key, user_id, key_id, "/api/v1/users/me").await?;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(json_str(&body_json(resp).await?, "error")?, "key not found");
    let resp = authed_user_get(&state, &second, user_id, &second_id, "/api/v1/users/me").await?;
    assert_eq!(resp.status(), StatusCode::OK, "the other key still signs");
    let me = body_json(resp).await?;
    assert_eq!(me["keys"].as_array().map(Vec::len), Some(1), "{me}");
    assert_eq!(me["keys"][0]["key_id"], json!(second_id.as_str()));

    let resp = signed_empty(&state, &lead, "DELETE", &revoke).await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        json_str(&body_json(resp).await?, "error")?,
        "no such key on this account"
    );

    // A claim left behind would refuse this registration with `409`.
    let successor = create_user(&state, "successor@example.test").await?;
    let token = mint_user_preauth(&state, &successor.user_id).await?;
    assert_eq!(
        register_user_key(&state, &token, key, "handed on").await?,
        *key_id,
        "the freed keypair registers to another account"
    );
    Ok(())
}

#[tokio::test]
async fn a_key_is_revoked_only_through_the_account_holding_it() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (root, acme, lead) = acme_with_admin(&state).await?;
    let (a, b) = (
        registered_user(&state, &[]).await?,
        registered_user(&state, &[]).await?,
    );
    for member in [&a, &b] {
        place(&state, &root, &member.1, &acme, &[]).await?;
    }

    let resp = signed_empty(
        &state,
        &lead,
        "DELETE",
        &format!("/api/v1/users/{}/keys/{}", a.1, b.2),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        json_str(&body_json(resp).await?, "error")?,
        "no such key on this account"
    );
    for (key, user_id, key_id) in [&a, &b] {
        let resp = authed_user_get(&state, key, user_id, key_id, "/api/v1/users/me").await?;
        assert_eq!(resp.status(), StatusCode::OK, "{user_id}'s key still signs");
    }
    Ok(())
}

#[rstest::rstest]
#[case::in_a_rival(true)]
#[case::unaffiliated(false)]
#[tokio::test]
async fn only_a_server_wide_admin_revokes_a_key_outside_the_organization(
    #[case] in_a_rival: bool,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (root, _acme, lead) = acme_with_admin(&state).await?;
    let target = registered_user(&state, &[]).await?;
    if in_a_rival {
        let rival = create_org(&state, &root, "rival").await?;
        place(&state, &root, &target.1, &rival, &[]).await?;
    }
    let (key, user_id, key_id) = &target;

    let revoke = format!("/api/v1/users/{user_id}/keys/{key_id}");
    let resp = signed_empty(&state, &lead, "DELETE", &revoke).await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        json_str(&body_json(resp).await?, "error")?,
        "no such user",
        "absent as an account, not as a key"
    );
    let resp = authed_user_get(&state, key, user_id, key_id, "/api/v1/users/me").await?;
    assert_eq!(resp.status(), StatusCode::OK, "the key still signs");

    let resp = signed_empty(&state, &root, "DELETE", &revoke).await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let resp = authed_user_get(&state, key, user_id, key_id, "/api/v1/users/me").await?;
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "the key is revoked"
    );
    Ok(())
}

/// The key signing the request included.
#[tokio::test]
async fn an_administrator_revokes_its_own_key_as_its_holder_would() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (_root, _acme, lead) = acme_with_admin(&state).await?;
    let (key, user_id, key_id) = &lead;

    let resp = signed_empty(
        &state,
        &lead,
        "DELETE",
        &format!("/api/v1/users/{user_id}/keys/{key_id}"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let resp = authed_user_get(&state, key, user_id, key_id, "/api/v1/users/me").await?;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    Ok(())
}

/// So a machine added since sits at the bottom, where it gets noticed.
#[tokio::test]
async fn an_accounts_keys_are_listed_oldest_first() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (root, acme, lead) = acme_with_admin(&state).await?;
    let member = registered_user(&state, &[]).await?;
    place(&state, &root, &member.1, &acme, &[]).await?;
    // Two older keys whose ids sort against their dates, whatever id the
    // registered key drew: only the date orders them.
    let mut candidates: Vec<PublicKeyHex> = (0..=u8::MAX)
        .filter_map(|byte| PublicKeyHex::try_new(hex::encode([byte; PublicKeyHex::BYTE_LEN])).ok())
        .collect();
    candidates.sort_by_key(UserKeyId::derive);
    let highest = candidates
        .pop()
        .ok_or_else(|| anyhow::anyhow!("no candidate public key"))?;
    let lowest = candidates
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("a single candidate public key"))?;
    let now = chrono::Utc::now();
    let oldest = UserKey::new(
        highest,
        NonEmptyTrimmedString::try_new("old desktop")?,
        now - chrono::Duration::days(2),
    );
    let older = UserKey::new(
        lowest,
        NonEmptyTrimmedString::try_new("spare tablet")?,
        now - chrono::Duration::days(1),
    );
    state.user_store.put_user_key(&member.1, &older).await?;
    state.user_store.put_user_key(&member.1, &oldest).await?;

    let resp = signed_empty(
        &state,
        &lead,
        "GET",
        &format!("/api/v1/users/{}/keys", member.1),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let listed = body_json(resp).await?;
    crate::api_schema::assert_conforms("UserKeyPage", &listed)?;
    assert_eq!(listed["next_cursor"], json!(null), "one page: {listed}");
    let ids: Vec<&str> = listed["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|key| key["key_id"].as_str())
        .collect();
    assert_eq!(
        ids,
        vec![
            oldest.key_id.as_str(),
            older.key_id.as_str(),
            member.2.as_str()
        ],
        "oldest first: {listed}"
    );
    assert_eq!(listed["items"][0]["key_details"], json!("old desktop"));
    assert_eq!(listed["items"][1]["key_details"], json!("spare tablet"));
    assert_eq!(listed["items"][2]["key_details"], json!("test laptop"));
    Ok(())
}

// ── rank: an account wider than its administrator ───────────────────────────

/// A server admin who belongs to `org_id`, as a Clerk-provisioned account
/// promoted later does, and the id of a pre-auth key the host minted for it.
async fn server_admin_in(
    state: &AppState,
    root: &UserSigner,
    org_id: &str,
) -> anyhow::Result<(UserSigner, String)> {
    let boss = registered_user(state, &[]).await?;
    place(
        state,
        root,
        &boss.1,
        org_id,
        UserPermissionGroup::ServerAdmin.permissions(),
    )
    .await?;
    let host = host_minted(&boss.1)?;
    state.user_store.put_user_preauth_key(&host.key).await?;
    Ok((boss, host.key.key_id.as_str().to_string()))
}

/// Every write is refused with the names the org admin lacks; the account stays
/// on its roster to read.
#[tokio::test]
async fn an_org_admin_changes_nothing_on_a_server_admin_in_its_organization() -> anyhow::Result<()>
{
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (root, acme, lead) = acme_with_admin(&state).await?;
    let (boss, host_key) = server_admin_in(&state, &root, &acme).await?;
    let held = stored_names(&state, &boss.1).await?;
    let account = format!("/api/v1/users/{}", boss.1);

    for (method, uri, body) in [
        ("PATCH", account.clone(), json!({ "status": "disabled" })),
        ("PATCH", account.clone(), json!({ "status": "pending" })),
        (
            "PUT",
            format!("{account}/permissions"),
            json!({ "permissions": [] }),
        ),
        // Inside the caller's ceiling, so rank and not the ceiling refuses it.
        (
            "PUT",
            format!("{account}/permissions"),
            json!({ "permissions": ["read_clients"] }),
        ),
        (
            "PATCH",
            format!("{account}/permissions"),
            json!({ "remove": ["manage_any_orgs"] }),
        ),
        (
            "PATCH",
            format!("{account}/permissions"),
            json!({ "add": ["read_clients"] }),
        ),
        ("PUT", format!("{account}/org"), json!({ "org_id": null })),
        ("PUT", format!("{account}/org"), json!({ "org_id": acme })),
        ("POST", format!("{account}/preauth-keys"), json!({})),
        (
            "DELETE",
            format!("{account}/preauth-keys/{host_key}"),
            json!({}),
        ),
        ("DELETE", format!("{account}/keys/{}", boss.2), json!({})),
    ] {
        let resp = signed_json(&state, &lead, method, &uri, &body).await?;
        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "{method} {uri} {body}"
        );
        let error = json_str(&body_json(resp).await?, "error")?.to_string();
        assert!(
            error
                .starts_with("you cannot change an account holding server-wide names you do not: ")
                && error.contains("manage_any_orgs"),
            "{method} {uri} {body}: {error}"
        );
    }

    let kept = state
        .user_store
        .get_user(&boss.1)
        .await?
        .ok_or_else(|| anyhow::anyhow!("the server admin was deleted"))?;
    assert_eq!(kept.status, UserStatus::Active);
    assert_eq!(stored_names(&state, &boss.1).await?, held);
    assert_eq!(
        state
            .org_store
            .get_membership(&boss.1)
            .await?
            .map(|org| org.to_string()),
        Some(acme.clone())
    );
    assert_eq!(state.user_store.list_user_keys(&boss.1).await?.len(), 1);
    let standing: Vec<String> = state
        .user_store
        .list_user_preauth_keys()
        .await?
        .into_iter()
        .filter(|key| key.user_id == boss.1)
        .map(|key| key.key_id.as_str().to_string())
        .collect();
    assert_eq!(standing, vec![host_key], "no key minted, none revoked");
    let (key, user_id, key_id) = &boss;
    let resp = authed_user_get(&state, key, user_id, key_id, "/api/v1/users/me").await?;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "the server admin still signs in"
    );

    // The reads are unchanged: the account and its names are on lead's roster.
    let resp = signed_empty(&state, &lead, "GET", &account).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let shown = body_json(resp).await?;
    assert!(
        shown["permissions"]
            .as_array()
            .is_some_and(|names| names.contains(&json!("manage_any_orgs"))),
        "{shown}"
    );
    assert!(shown.get("stored_permissions").is_some(), "{shown}");
    assert_eq!(shown["may_administer"], json!(false), "{shown}");
    assert_eq!(
        shown["awaits_sign_in"],
        json!(false),
        "an outranked account's sign-in state still shows: {shown}"
    );
    let listed = body_json(signed_empty(&state, &lead, "GET", "/api/v1/users").await?).await?;
    assert!(
        listed["items"]
            .as_array()
            .is_some_and(|users| users.iter().any(|u| u["user_id"] == json!(boss.1.as_str()))),
        "{listed}"
    );
    for uri in [format!("{account}/keys"), format!("{account}/preauth-keys")] {
        let resp = signed_empty(&state, &lead, "GET", &uri).await?;
        assert_eq!(resp.status(), StatusCode::OK, "{uri}");
    }
    Ok(())
}

#[tokio::test]
async fn a_server_wide_admin_still_administers_a_server_admin_in_an_organization()
-> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (root, acme, _lead) = acme_with_admin(&state).await?;
    let (boss, host_key) = server_admin_in(&state, &root, &acme).await?;
    let account = format!("/api/v1/users/{}", boss.1);

    for status in ["disabled", "active"] {
        let resp = signed_json(
            &state,
            &root,
            "PATCH",
            &account,
            &json!({ "status": status }),
        )
        .await?;
        assert_eq!(resp.status(), StatusCode::OK, "{status}");
    }
    let resp = signed_json(
        &state,
        &root,
        "PATCH",
        &format!("{account}/permissions"),
        &json!({ "remove": ["cancel_any_plans"] }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await?["changed"], json!(true));
    let resp = signed_empty(
        &state,
        &root,
        "DELETE",
        &format!("{account}/preauth-keys/{host_key}"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let (key, user_id, key_id) = &boss;
    let resp = signed_empty(&state, &root, "DELETE", &format!("{account}/keys/{key_id}")).await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let resp = authed_user_get(&state, key, user_id, key_id, "/api/v1/users/me").await?;
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "the key is revoked"
    );
    let resp = signed_json(
        &state,
        &root,
        "PUT",
        &format!("{account}/org"),
        &json!({ "org_id": null }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(state.org_store.get_membership(&boss.1).await?.is_none());
    Ok(())
}

/// Rank weighs server-wide names alone, so plan names an org admin lacks do not
/// stop it offboarding a member.
#[tokio::test]
async fn an_org_admin_still_offboards_a_member_holding_names_it_lacks() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (_root, acme, lead) = acme_with_admin(&state).await?;
    roster_only(&state, &lead).await?;
    let member = keyless(&state, &acme, UserPermissionGroup::OrgMember.permissions()).await?;
    let account = format!("/api/v1/users/{member}");

    for (method, uri, body) in [
        (
            "PATCH",
            format!("{account}/permissions"),
            json!({ "remove": ["submit_plans"] }),
        ),
        (
            "PUT",
            format!("{account}/permissions"),
            json!({ "permissions": [] }),
        ),
        ("PATCH", account.clone(), json!({ "status": "disabled" })),
        ("PUT", format!("{account}/org"), json!({ "org_id": null })),
    ] {
        let resp = signed_json(&state, &lead, method, &uri, &body).await?;
        assert_eq!(resp.status(), StatusCode::OK, "{method} {uri} {body}");
    }
    assert!(stored_names(&state, &member).await?.is_empty());
    assert_eq!(
        state.user_store.get_user(&member).await?.map(|u| u.status),
        Some(UserStatus::Disabled)
    );
    assert!(state.org_store.get_membership(&member).await?.is_none());
    Ok(())
}

/// One server-wide name refuses a scoped caller, unless the caller holds it too.
#[rstest::rstest]
#[case::the_caller_lacks_it(
    &[],
    StatusCode::FORBIDDEN,
    Some("you cannot change an account holding server-wide names you do not: read_any_plans"),
    UserStatus::Active
)]
#[case::the_caller_holds_it_too(
    &[UserPermission::ReadAnyPlans],
    StatusCode::OK,
    None,
    UserStatus::Disabled
)]
#[tokio::test]
async fn a_lone_server_wide_name_is_weighed_against_the_callers_own(
    #[case] caller_extra: &[UserPermission],
    #[case] expected: StatusCode,
    #[case] error: Option<&str>,
    #[case] after: UserStatus,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (_root, acme, lead) = acme_with_admin(&state).await?;
    let member = keyless(&state, &acme, &[UserPermission::ReadAnyPlans]).await?;
    grant_user(
        &state,
        &lead.1,
        &[UserPermissionGroup::OrgAdmin.permissions(), caller_extra].concat(),
    )
    .await?;

    let resp = signed_json(
        &state,
        &lead,
        "PATCH",
        &format!("/api/v1/users/{member}"),
        &json!({ "status": "disabled" }),
    )
    .await?;
    assert_eq!(resp.status(), expected);
    assert_eq!(
        body_json(resp)
            .await?
            .get("error")
            .and_then(serde_json::Value::as_str),
        error
    );
    assert_eq!(
        state.user_store.get_user(&member).await?.map(|u| u.status),
        Some(after)
    );
    Ok(())
}

/// A user store that counts its account writes and, given `grant_mid_write`,
/// stores that grant on its account just before each swap of the account's set:
/// a server admin's grant landing between a write's rank check and its swap.
struct WatchedUserStore {
    inner: Arc<dyn UserStore>,
    user_puts: AtomicUsize,
    grant_mid_write: Option<(UserId, UserPermissionSet)>,
}

impl WatchedUserStore {
    fn over(inner: Arc<dyn UserStore>) -> Self {
        Self {
            inner,
            user_puts: AtomicUsize::new(0),
            grant_mid_write: None,
        }
    }
}

#[async_trait::async_trait]
impl UserStore for WatchedUserStore {
    async fn get_user(&self, user_id: &UserId) -> anyhow::Result<Option<User>> {
        self.inner.get_user(user_id).await
    }
    async fn put_user(&self, user: &User) -> anyhow::Result<()> {
        self.user_puts.fetch_add(1, Ordering::SeqCst);
        self.inner.put_user(user).await
    }
    async fn delete_user(&self, user_id: &UserId) -> anyhow::Result<()> {
        self.inner.delete_user(user_id).await
    }
    async fn list_users(&self) -> anyhow::Result<Vec<User>> {
        self.inner.list_users().await
    }
    async fn claim_user_email(&self, email: &str, user_id: &UserId) -> anyhow::Result<EmailClaim> {
        self.inner.claim_user_email(email, user_id).await
    }
    async fn lookup_user_by_email(&self, email: &str) -> anyhow::Result<Option<UserId>> {
        self.inner.lookup_user_by_email(email).await
    }
    async fn release_user_email(&self, email: &str) -> anyhow::Result<()> {
        self.inner.release_user_email(email).await
    }
    async fn put_user_key(&self, user_id: &UserId, key: &UserKey) -> anyhow::Result<()> {
        self.inner.put_user_key(user_id, key).await
    }
    async fn get_user_key(
        &self,
        user_id: &UserId,
        key_id: &UserKeyId,
    ) -> anyhow::Result<Option<UserKey>> {
        self.inner.get_user_key(user_id, key_id).await
    }
    async fn list_user_keys(&self, user_id: &UserId) -> anyhow::Result<Vec<UserKey>> {
        self.inner.list_user_keys(user_id).await
    }
    async fn delete_user_key(&self, user_id: &UserId, key_id: &UserKeyId) -> anyhow::Result<()> {
        self.inner.delete_user_key(user_id, key_id).await
    }
    async fn get_user_permissions(
        &self,
        user_id: &UserId,
    ) -> anyhow::Result<Option<UserPermissionSet>> {
        self.inner.get_user_permissions(user_id).await
    }
    async fn put_user_permissions(
        &self,
        user_id: &UserId,
        set: &UserPermissionSet,
    ) -> anyhow::Result<()> {
        self.inner.put_user_permissions(user_id, set).await
    }
    async fn delete_user_permissions(&self, user_id: &UserId) -> anyhow::Result<()> {
        self.inner.delete_user_permissions(user_id).await
    }
    async fn mutate_user_permissions(
        &self,
        user_id: &UserId,
        f: &(dyn for<'s> Fn(&'s mut UserPermissionSet) -> bool + Send + Sync),
    ) -> anyhow::Result<UserPermissionMutation> {
        if let Some((granted, grant)) = &self.grant_mid_write
            && granted == user_id
        {
            self.inner.put_user_permissions(user_id, grant).await?;
        }
        self.inner.mutate_user_permissions(user_id, f).await
    }
    async fn put_user_preauth_key(&self, key: &UserPreauthKey) -> anyhow::Result<()> {
        self.inner.put_user_preauth_key(key).await
    }
    async fn consume_user_preauth_key(
        &self,
        key_id: &UserPreauthKeyId,
        secret: &UserPreauthSecret,
    ) -> anyhow::Result<UserPreauthConsumeOutcome> {
        self.inner.consume_user_preauth_key(key_id, secret).await
    }
    async fn list_user_preauth_keys(&self) -> anyhow::Result<Vec<UserPreauthKey>> {
        self.inner.list_user_preauth_keys().await
    }
    async fn delete_user_preauth_key(&self, key_id: &UserPreauthKeyId) -> anyhow::Result<()> {
        self.inner.delete_user_preauth_key(key_id).await
    }
    async fn list_spent_user_preauth_markers(&self) -> anyhow::Result<Vec<UserPreauthKeyId>> {
        self.inner.list_spent_user_preauth_markers().await
    }
    async fn delete_spent_user_preauth_marker(
        &self,
        key_id: &UserPreauthKeyId,
    ) -> anyhow::Result<()> {
        self.inner.delete_spent_user_preauth_marker(key_id).await
    }
    async fn mark_user_awaiting_sign_in(&self, user_id: &UserId) -> anyhow::Result<()> {
        self.inner.mark_user_awaiting_sign_in(user_id).await
    }
    async fn user_awaits_sign_in(&self, user_id: &UserId) -> anyhow::Result<bool> {
        self.inner.user_awaits_sign_in(user_id).await
    }
    async fn clear_user_awaiting_sign_in(&self, user_id: &UserId) -> anyhow::Result<()> {
        self.inner.clear_user_awaiting_sign_in(user_id).await
    }
    async fn record_user_first_clerk_sign_in(
        &self,
        user_id: &UserId,
        first: &pipette_plan_types::FirstClerkSignIn,
    ) -> anyhow::Result<bool> {
        self.inner
            .record_user_first_clerk_sign_in(user_id, first)
            .await
    }
    async fn get_user_first_clerk_sign_in(
        &self,
        user_id: &UserId,
    ) -> anyhow::Result<Option<pipette_plan_types::FirstClerkSignIn>> {
        self.inner.get_user_first_clerk_sign_in(user_id).await
    }
    async fn delete_user_first_clerk_sign_in(&self, user_id: &UserId) -> anyhow::Result<()> {
        self.inner.delete_user_first_clerk_sign_in(user_id).await
    }
}

/// A permission write weighs rank again inside its swap, so a server-wide grant
/// landing after its first check is kept and the write refused.
#[rstest::rstest]
#[case::replace("PUT", json!({ "permissions": [] }))]
#[case::amend("PATCH", json!({ "remove": ["manage_any_orgs"] }))]
#[tokio::test]
async fn a_server_wide_grant_landing_mid_write_is_not_overwritten(
    #[case] method: &str,
    #[case] body: serde_json::Value,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let mut state = make_state(dir.path()).await?;
    let (_root, acme, lead) = acme_with_admin(&state).await?;
    let member = keyless(&state, &acme, UserPermissionGroup::OrgMember.permissions()).await?;
    let promoted: UserPermissionSet = UserPermissionGroup::ServerAdmin
        .permissions()
        .iter()
        .copied()
        .collect();
    state.user_store = Arc::new(WatchedUserStore {
        grant_mid_write: Some((member.clone(), promoted.clone())),
        ..WatchedUserStore::over(state.user_store.clone())
    });

    let resp = signed_json(
        &state,
        &lead,
        method,
        &format!("/api/v1/users/{member}/permissions"),
        &body,
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let error = json_str(&body_json(resp).await?, "error")?.to_string();
    assert!(
        error.starts_with("you cannot change an account holding server-wide names you do not: ")
            && error.contains("manage_any_orgs"),
        "{error}"
    );
    assert_eq!(
        state.user_store.get_user_permissions(&member).await?,
        Some(promoted)
    );
    Ok(())
}

/// A `PATCH` naming the values an account holds writes nothing, so it cannot
/// revert a concurrent edit with the copy it read.
#[tokio::test]
async fn a_patch_to_the_stored_values_writes_nothing() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let mut state = make_state(dir.path()).await?;
    let (_root, acme, lead) = acme_with_admin(&state).await?;
    let member = keyless(&state, &acme, UserPermissionGroup::OrgMember.permissions()).await?;
    let watched = Arc::new(WatchedUserStore::over(state.user_store.clone()));
    state.user_store = watched.clone();

    let answers = check_write_steps(
        &state,
        &lead,
        ("PATCH", &format!("/api/v1/users/{member}")),
        || watched.user_puts.load(Ordering::SeqCst),
        [
            (json!({ "status": "active" }), false, 0),
            (
                json!({ "status": "active", "account_type": "human" }),
                false,
                0,
            ),
            (json!({ "status": "disabled" }), true, 1),
        ],
    )
    .await?;
    answers
        .iter()
        .try_for_each(|answer| crate::api_schema::assert_conforms("ChangedUserView", answer))?;
    assert_eq!(
        state.user_store.get_user(&member).await?.map(|u| u.status),
        Some(UserStatus::Disabled)
    );
    Ok(())
}

// ── the bodies the app's account pages send and read ────────────────────────

/// So a permission form tells a stored name, which an untick revokes, from one
/// held only through a wider one. Only its administrator reads the stored set.
#[tokio::test]
async fn an_account_reads_with_its_stored_set_beside_the_resolved_one() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (root, acme, lead) = acme_with_admin(&state).await?;
    let member = keyless(&state, &acme, &[]).await?;
    let uri = format!("/api/v1/users/{member}");

    let request = json!({ "permissions": ["manage_orgs", "read_orgs"] });
    crate::api_schema::assert_conforms("SetUserPermissionsRequest", &request)?;
    let resp = signed_json(
        &state,
        &lead,
        "PUT",
        &format!("{uri}/permissions"),
        &request,
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let set = body_json(resp).await?;
    crate::api_schema::assert_conforms("ChangedUserView", &set)?;
    assert_eq!(set["stored_permissions"], json!(["manage_orgs"]), "{set}");

    let read = body_json(signed_empty(&state, &lead, "GET", &uri).await?).await?;
    crate::api_schema::assert_conforms("UserView", &read)?;
    assert_eq!(read["stored_permissions"], json!(["manage_orgs"]), "{read}");
    assert_eq!(read["permissions"], json!(["manage_orgs", "read_orgs"]));

    let listed = body_json(signed_empty(&state, &lead, "GET", "/api/v1/users").await?).await?;
    crate::api_schema::assert_conforms("UserViewPage", &listed)?;
    let row = listed
        .pointer("/items")
        .and_then(serde_json::Value::as_array)
        .and_then(|rows| {
            rows.iter()
                .find(|row| row["user_id"] == json!(member.as_str()))
        })
        .ok_or_else(|| anyhow::anyhow!("the member is on the roster: {listed}"))?;
    assert_eq!(row["stored_permissions"], json!(["manage_orgs"]), "{row}");

    let stored: UserPermissionSet = serde_json::from_value(json!(["read_plans", "rescore_plans"]))?;
    state
        .user_store
        .put_user_permissions(&member, &stored)
        .await?;
    let request = json!({ "remove": ["read_plans"] });
    crate::api_schema::assert_conforms("AmendUserPermissionsRequest", &request)?;
    let resp = signed_json(
        &state,
        &lead,
        "PATCH",
        &format!("{uri}/permissions"),
        &request,
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let amended = body_json(resp).await?;
    crate::api_schema::assert_conforms("UserPermissionsAmended", &amended)?;
    assert_eq!(
        amended["stored_permissions"],
        json!(["rescore_plans"]),
        "{amended}"
    );

    // A reader of the roster alone gets the resolved set, not the stored one.
    let reader = registered_user(&state, &[]).await?;
    place(&state, &root, &reader.1, &acme, &[UserPermission::ReadOrgs]).await?;
    let read = body_json(signed_empty(&state, &reader, "GET", &uri).await?).await?;
    crate::api_schema::assert_conforms("UserView", &read)?;
    assert_eq!(read.get("stored_permissions"), None, "{read}");
    let listed = body_json(signed_empty(&state, &reader, "GET", "/api/v1/users").await?).await?;
    let rows = listed
        .pointer("/items")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("a roster: {listed}"))?;
    assert!(!rows.is_empty(), "{listed}");
    assert!(
        rows.iter()
            .all(|row| row.get("stored_permissions").is_none()),
        "{listed}"
    );

    // An account holding no object stores nothing.
    let bare = keyless(&state, &acme, &[]).await?;
    let read =
        body_json(signed_empty(&state, &lead, "GET", &format!("/api/v1/users/{bare}")).await?)
            .await?;
    assert_eq!(read["stored_permissions"], json!([]), "{read}");
    assert_eq!(read["permissions"], json!([]), "{read}");
    Ok(())
}

/// As the console's page decides whether to offer the form.
#[tokio::test]
async fn the_account_record_says_whether_it_awaits_its_first_sign_in() -> anyhow::Result<()> {
    async fn awaits(
        state: &AppState,
        admin: &UserSigner,
        user_id: &UserId,
    ) -> anyhow::Result<bool> {
        let resp = signed_empty(state, admin, "GET", &format!("/api/v1/users/{user_id}")).await?;
        anyhow::ensure!(resp.status() == StatusCode::OK, "{}", resp.status());
        let record = body_json(resp).await?;
        crate::api_schema::assert_conforms("UserDetail", &record)?;
        record
            .get("awaits_sign_in")
            .and_then(serde_json::Value::as_bool)
            .ok_or_else(|| anyhow::anyhow!("no awaits_sign_in: {record}"))
    }

    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (root, acme, lead) = acme_with_admin(&state).await?;
    let member = keyless(&state, &acme, &[]).await?;
    assert!(awaits(&state, &lead, &member).await?);

    let request = json!({ "expires_in_days": 7, "note": "for the new hire" });
    crate::api_schema::assert_conforms("MintUserPreauthRequest", &request)?;
    let resp = signed_json(
        &state,
        &lead,
        "POST",
        &format!("/api/v1/users/{member}/preauth-keys"),
        &request,
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let minted = body_json(resp).await?;
    crate::api_schema::assert_conforms("UserPreauthKeyMinted", &minted)?;
    assert!(
        awaits(&state, &lead, &member).await?,
        "a key outstanding admits nobody yet"
    );

    let key = signing_key()?;
    register_user_key(&state, json_str(&minted, "token")?, &key, "laptop").await?;
    assert!(!awaits(&state, &lead, &member).await?, "signed in");

    let older = keyless(&state, &acme, &[]).await?;
    state.user_store.clear_user_awaiting_sign_in(&older).await?;
    assert!(!awaits(&state, &lead, &older).await?, "predates the mark");

    // A key the holder registered some other way clears it too.
    let keyed = registered_user(&state, &[]).await?;
    place(&state, &root, &keyed.1, &acme, &[]).await?;
    state
        .user_store
        .mark_user_awaiting_sign_in(&keyed.1)
        .await?;
    assert!(!awaits(&state, &lead, &keyed.1).await?, "holds a key");
    Ok(())
}

#[tokio::test]
async fn the_account_forms_conform_to_their_schemas() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (_root, acme, lead) = acme_with_admin(&state).await?;

    let request = json!({ "contact_email": "hire@example.test", "permissions": ["read_clients"] });
    crate::api_schema::assert_conforms("CreateUserRequest", &request)?;
    let resp = signed_json(&state, &lead, "POST", "/api/v1/users", &request).await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let created = body_json(resp).await?;
    crate::api_schema::assert_conforms("UserView", &created)?;
    assert_eq!(created["org_id"], json!(acme));
    assert_eq!(created["stored_permissions"], json!(["read_clients"]));
    let user_id = json_str(&created, "user_id")?.to_string();

    let request = json!({ "status": "disabled" });
    crate::api_schema::assert_conforms("UpdateUserRequest", &request)?;
    let resp = signed_json(
        &state,
        &lead,
        "PATCH",
        &format!("/api/v1/users/{user_id}"),
        &request,
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let updated = body_json(resp).await?;
    crate::api_schema::assert_conforms("ChangedUserView", &updated)?;
    assert_eq!(updated["status"], json!("disabled"));

    let resp = signed_empty(
        &state,
        &lead,
        "GET",
        &format!("/api/v1/users/{}/keys", lead.1),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    crate::api_schema::assert_conforms("UserKeyPage", &body_json(resp).await?)?;

    // A status the route does not know, the schema does not either.
    assert!(
        crate::api_schema::assert_conforms("UpdateUserRequest", &json!({ "status": "gone" }))
            .is_err()
    );
    Ok(())
}
