//! The organization surface end to end: the roster CRUD under `/api/v1/orgs`, and the
//! two assignment routes that write the relations hung off it.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Context;
use axum::http::StatusCode;
use futures::TryStreamExt;
use rstest::rstest;
use serde_json::{Value, json};

use pipette_core::organization::{Membership, Organization, Ownership};
use pipette_core::user_permission::UserPermissionGroup;
use pipette_httpapi::state::AppState;
use pipette_plan_types::{ClientId, ClientOwner, OrgId, OrgStatus, UserId, UserPermission};
use pipette_store::{OrgStore, SlugClaim};
use pipette_web_testkit::*;

/// The caller most of these tests act as. `manage_any_clients` implies the
/// `read_clients` the device inventory needs.
async fn manager(state: &AppState) -> anyhow::Result<UserSigner> {
    registered_user(
        state,
        &[
            // The unscoped forms throughout: these tests create organizations
            // and act across them, which is a server decision.
            UserPermission::ManageAnyOrgs,
            // Setting a device's owner is a *device* write, so it is
            // `manage_clients`, again unscoped.
            UserPermission::ManageAnyClients,
        ],
    )
    .await
}

/// Place `user_id` in `org_id` through the store, for tests that are not about
/// the membership route.
async fn join(state: &AppState, user_id: &UserId, org_id: &str) -> anyhow::Result<()> {
    set_user_org(state, user_id, Some(OrgId::try_new(org_id.to_owned())?)).await
}

/// An org-owned vault record; its blob is never written, the overview only counts it.
async fn put_org_resource(state: &AppState, org_id: &OrgId) -> anyhow::Result<()> {
    let owner = pipette_plan_types::PrincipalOwner::Org {
        org_id: org_id.clone(),
    };
    let sha = pipette_store_testkit::vault_blob_sha256()?;
    let resource = pipette_store_testkit::vault_resource_for(owner, sha, 1)?;
    state.vault_store.put_resource(&resource).await
}

/// The roster reads refuse a caller without `read_orgs`, and `read_orgs` leaves
/// the roster writes refused.
#[tokio::test]
async fn the_org_routes_are_gated_on_their_own_permissions() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let reader = registered_user(&state, &[UserPermission::ReadOrgs]).await?;
    // Someone holding a plan grant, which says nothing about organizations.
    let stranger = registered_user(&state, &[UserPermission::SubmitPlans]).await?;
    let manager = manager(&state).await?;

    let org_id = create_org(&state, &manager, "acme").await?;
    // The bare verb is scoped by membership, so the reader is placed first.
    join(&state, &reader.1, &org_id).await?;

    for uri in [
        "/api/v1/orgs".to_string(),
        format!("/api/v1/orgs/{org_id}"),
        format!("/api/v1/orgs/{org_id}/users"),
        format!("/api/v1/orgs/{org_id}/overview"),
    ] {
        let (key, user_id, key_id) = &stranger;
        let resp = authed_user_get(&state, key, user_id, key_id, &uri).await?;
        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "GET {uri} must refuse a caller without read_orgs"
        );

        let (key, user_id, key_id) = &reader;
        let resp = authed_user_get(&state, key, user_id, key_id, &uri).await?;
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "GET {uri} must admit read_orgs"
        );
    }

    // Reading who is on a roster and deciding who is on it are different
    // decisions.
    for (method, uri, body) in [
        (
            "POST",
            "/api/v1/orgs".to_string(),
            json!({ "slug": "other" }),
        ),
        (
            "PATCH",
            format!("/api/v1/orgs/{org_id}"),
            json!({ "display_name": "Renamed" }),
        ),
        (
            "PUT",
            format!("/api/v1/users/{}/org", reader.1),
            json!({ "org_id": org_id }),
        ),
    ] {
        let resp = signed_json(&state, &reader, method, &uri, &body).await?;
        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "{method} {uri} must refuse a caller holding only read_orgs"
        );
    }

    // `manage_orgs` implies `read_orgs`, so the manager needs no second grant
    // to read what it just wrote.
    let (key, user_id, key_id) = &manager;
    let resp = authed_user_get(&state, key, user_id, key_id, "/api/v1/orgs").await?;
    assert_eq!(resp.status(), StatusCode::OK);
    Ok(())
}

/// Another organization reads as absent, not refused, as with
/// `GET /api/v1/users/{user_id}`, so the two roster surfaces cannot be played
/// against each other.
#[tokio::test]
async fn a_scoped_reader_sees_its_own_organization_only() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let manager = manager(&state).await?;
    let acme = create_org(&state, &manager, "acme").await?;
    let rival = create_org(&state, &manager, "rival").await?;

    // Holds the fleet read as well, so the inventory route is decided by the
    // roster scope alone.
    let reader = registered_user(
        &state,
        &[UserPermission::ReadOrgs, UserPermission::ReadClients],
    )
    .await?;
    join(&state, &reader.1, &acme).await?;
    let wide = registered_user(
        &state,
        &[UserPermission::ReadAnyOrgs, UserPermission::ReadClients],
    )
    .await?;

    let listed = body_json(signed_empty(&state, &reader, "GET", "/api/v1/orgs").await?).await?;
    crate::api_schema::assert_conforms("OrgViewPage", &listed)?;
    assert_eq!(
        listed["items"].as_array().map(|orgs| orgs
            .iter()
            .filter_map(|o| o["org_id"].as_str())
            .collect::<Vec<_>>()),
        Some(vec![acme.as_str()]),
        "a scoped reader lists its own organization and no other"
    );

    for uri in [
        format!("/api/v1/orgs/{rival}"),
        "/api/v1/orgs/rival".to_string(),
        format!("/api/v1/orgs/{rival}/users"),
        format!("/api/v1/orgs/{rival}/clients"),
    ] {
        let resp = signed_empty(&state, &reader, "GET", &uri).await?;
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "GET {uri} must read as absent to a reader outside the organization"
        );
        let resp = signed_empty(&state, &wide, "GET", &uri).await?;
        assert_ne!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "GET {uri} must resolve for read_any_orgs"
        );
    }

    // A reader belonging to no organization sees nothing at all.
    let stray = registered_user(&state, &[UserPermission::ReadOrgs]).await?;
    let listed = body_json(signed_empty(&state, &stray, "GET", "/api/v1/orgs").await?).await?;
    assert_eq!(listed["items"].as_array().map(Vec::len), Some(0));
    Ok(())
}

#[tokio::test]
async fn an_unaffiliated_reader_reads_every_organization_as_absent() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let manager = manager(&state).await?;
    let acme = create_org(&state, &manager, "acme").await?;
    // `read_clients` too, so the inventory's 404 is the roster scope's alone.
    let stray = registered_user(
        &state,
        &[UserPermission::ReadOrgs, UserPermission::ReadClients],
    )
    .await?;

    for (uri, reference) in [
        ("/api/v1/orgs/acme".to_string(), "acme"),
        (format!("/api/v1/orgs/{acme}"), acme.as_str()),
        ("/api/v1/orgs/acme/users".to_string(), "acme"),
        ("/api/v1/orgs/acme/clients".to_string(), "acme"),
    ] {
        let resp = signed_empty(&state, &stray, "GET", &uri).await?;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "GET {uri}");
        assert_eq!(
            json_str(&body_json(resp).await?, "error")?,
            format!("no organization {reference}"),
            "GET {uri}"
        );
    }
    Ok(())
}

/// The rows are whole client records, key material included; `read_orgs`
/// promises a roster of people, not the fleet's hardware.
#[tokio::test]
async fn the_device_inventory_needs_a_fleet_grant_as_well() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let manager = manager(&state).await?;
    let org_id = create_org(&state, &manager, "acme").await?;
    let uri = format!("/api/v1/orgs/{org_id}/clients");

    // Administers the roster, holds no fleet grant.
    let roster_only = registered_user(&state, &[UserPermission::ManageOrgs]).await?;
    let (key, user_id, key_id) = &roster_only;
    let resp = authed_user_get(&state, key, user_id, key_id, &uri).await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert!(
        json_str(&body_json(resp).await?, "error")?.contains("read_clients"),
        "the refusal must name the grant that is missing"
    );

    // Reads the fleet, holds no roster grant: refused on the other side.
    let fleet_only = registered_user(&state, &[UserPermission::ReadClients]).await?;
    let (key, user_id, key_id) = &fleet_only;
    let resp = authed_user_get(&state, key, user_id, key_id, &uri).await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // Both, so the filter and the rows are each accounted for.
    let (key, user_id, key_id) = &manager;
    let resp = authed_user_get(&state, key, user_id, key_id, &uri).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    Ok(())
}

/// The second create is a `409` naming the holder, and leaves no second record.
#[tokio::test]
async fn a_handle_names_one_organization() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let manager = manager(&state).await?;

    let org_id = create_org(&state, &manager, "acme").await?;

    // Typed differently, folded to the same key: why the handle is folded.
    let resp = signed_json(
        &state,
        &manager,
        "POST",
        "/api/v1/orgs",
        &json!({ "slug": " ACME " }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body = body_json(resp).await?;
    assert!(
        json_str(&body, "error")?.contains(&org_id),
        "the refusal must name the holder: {body}"
    );
    assert_eq!(state.org_store.list_orgs().await?.len(), 1);
    Ok(())
}

/// Refused before any handle is claimed.
#[rstest]
#[case::bare_manage_orgs(&[UserPermission::ManageOrgs])]
#[case::org_admin_group(UserPermissionGroup::OrgAdmin.permissions())]
#[tokio::test]
async fn an_org_admin_cannot_create_an_organization(
    #[case] grants: &[UserPermission],
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let manager = manager(&state).await?;
    let acme = create_org(&state, &manager, "acme").await?;
    let admin = registered_user(&state, grants).await?;
    join(&state, &admin.1, &acme).await?;

    let resp = signed_json(
        &state,
        &admin,
        "POST",
        "/api/v1/orgs",
        &json!({ "slug": "other" }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        json_str(&body_json(resp).await?, "error")?,
        "user lacks the manage_any_orgs permission"
    );
    assert_eq!(
        state.org_store.list_orgs().await?.len(),
        1,
        "no record is written"
    );
    assert!(
        state.org_store.lookup_org_by_slug("other").await?.is_none(),
        "no handle is claimed"
    );
    Ok(())
}

/// Otherwise a record could exist that no path segment, or only its minted id,
/// reaches.
#[rstest]
#[case::blank("   ", "organization slug must not be empty")]
#[case::outside_the_charset("a b", "organization slug \"a b\" contains invalid character ' '")]
#[case::id_prefixed(
    "ORG_acme",
    "organization slug \"org_acme\" may not start with \"org_\""
)]
#[tokio::test]
async fn an_invalid_slug_is_refused_and_writes_nothing(
    #[case] slug: &str,
    #[case] error: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let manager = manager(&state).await?;

    let resp = signed_json(
        &state,
        &manager,
        "POST",
        "/api/v1/orgs",
        &json!({ "slug": slug }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{slug:?}");
    let body = body_json(resp).await?;
    assert!(
        json_str(&body, "error")?.contains(error),
        "{slug:?}: {body}"
    );
    assert!(state.org_store.list_orgs().await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn a_slug_is_at_most_64_characters() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let manager = manager(&state).await?;

    let resp = signed_json(
        &state,
        &manager,
        "POST",
        "/api/v1/orgs",
        &json!({ "slug": "a".repeat(65) }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await?;
    assert!(
        json_str(&body, "error")?.contains("is too long (65 chars, max 64)"),
        "{body}"
    );
    assert!(state.org_store.list_orgs().await?.is_empty());

    let longest = "a".repeat(64);
    let resp = signed_json(
        &state,
        &manager,
        "POST",
        "/api/v1/orgs",
        &json!({ "slug": longest }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(json_str(&body_json(resp).await?, "slug")?, longest);
    Ok(())
}

#[rstest]
#[case::named_after_the_handle(json!({ "slug": " Beta " }), "beta", "beta")]
#[case::named_explicitly(
    json!({ "slug": " Acme ", "display_name": " Acme Corp., Inc. " }),
    "acme",
    "Acme Corp., Inc."
)]
#[tokio::test]
async fn a_create_answers_the_new_organization(
    #[case] request: Value,
    #[case] slug: &str,
    #[case] display_name: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let manager = manager(&state).await?;

    crate::api_schema::assert_conforms("CreateOrgRequest", &request)?;
    let resp = signed_json(&state, &manager, "POST", "/api/v1/orgs", &request).await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await?;
    crate::api_schema::assert_conforms("OrgView", &body)?;
    assert!(json_str(&body, "org_id")?.starts_with("org_"), "{body}");
    assert_eq!(json_str(&body, "slug")?, slug);
    assert_eq!(json_str(&body, "display_name")?, display_name);
    assert_eq!(json_str(&body, "status")?, "active");
    assert!(body["created_at"].is_string(), "{body}");
    assert_eq!(body["member_count"], 0, "{body}");
    assert_eq!(body["client_count"], 0, "{body}");

    let (key, user_id, key_id) = &manager;
    let read = body_json(
        authed_user_get(
            &state,
            key,
            user_id,
            key_id,
            &format!("/api/v1/orgs/{slug}"),
        )
        .await?,
    )
    .await?;
    assert_eq!(read, body, "the answer is what was written");
    Ok(())
}

#[tokio::test]
async fn an_organization_is_addressable_by_handle_or_id() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let manager = manager(&state).await?;
    let org_id = create_org(&state, &manager, "acme").await?;
    let (key, user_id, key_id) = &manager;

    for uri in [
        format!("/api/v1/orgs/{org_id}"),
        "/api/v1/orgs/acme".to_string(),
    ] {
        let resp = authed_user_get(&state, key, user_id, key_id, &uri).await?;
        assert_eq!(resp.status(), StatusCode::OK, "{uri}");
        let body = body_json(resp).await?;
        crate::api_schema::assert_conforms("OrgView", &body)?;
        assert_eq!(json_str(&body, "org_id")?, org_id);
    }

    let resp = authed_user_get(&state, key, user_id, key_id, "/api/v1/orgs/nobody").await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    Ok(())
}

/// The rosters echo no id, so a caller that typed `acme` learns the one to
/// store from the record or a member row.
#[tokio::test]
async fn a_roster_read_by_handle_answers_the_minted_id() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let manager = manager(&state).await?;
    let org_id = create_org(&state, &manager, "acme").await?;
    let member = registered_user(&state, &[]).await?;
    join(&state, &member.1, &org_id).await?;
    let (key, user_id, key_id) = &manager;

    // The inventory is empty: no device is enrolled here.
    for (uri, first_row_org) in [
        ("/api/v1/orgs/acme/users", Some(json!(org_id))),
        ("/api/v1/orgs/acme/clients", None),
    ] {
        let resp = authed_user_get(&state, key, user_id, key_id, uri).await?;
        assert_eq!(resp.status(), StatusCode::OK, "{uri}");
        let body = body_json(resp).await?;
        assert!(body["items"].is_array(), "{uri}: {body}");
        assert_eq!(body["next_cursor"], Value::Null, "{uri}: {body}");
        assert_eq!(body.get("org_id"), None, "{uri}: {body}");
        assert_eq!(
            body.pointer("/items/0/org_id"),
            first_row_org.as_ref(),
            "{uri}: {body}"
        );
    }
    Ok(())
}

/// How the refusal of `bad.slug` starts: it fails as a handle.
const BAD_HANDLE: &str = "invalid organization reference: invalid organization handle:";
/// How the refusal of `org_bad.id` starts: its prefix makes it an id.
const BAD_ID: &str = "invalid organization reference: invalid organization id:";
/// How a mutation refuses `org_bad.id`: it takes an id only, never a handle.
const NOT_AN_ID: &str = "organization id contains '.'";
/// How a mutation refuses a handle a read still resolves.
const A_HANDLE: &str = "organization id must start with `org_`";

/// Saying which spelling failed, rather than a `404` for a reference that could
/// never exist.
#[rstest]
#[case::read_by_handle("GET", "/api/v1/orgs/bad.slug", None, BAD_HANDLE)]
#[case::read_by_id("GET", "/api/v1/orgs/org_bad.id", None, BAD_ID)]
#[case::members("GET", "/api/v1/orgs/bad.slug/users", None, BAD_HANDLE)]
#[case::devices("GET", "/api/v1/orgs/bad.slug/clients", None, BAD_HANDLE)]
#[case::overview("GET", "/api/v1/orgs/bad.slug/overview", None, BAD_HANDLE)]
#[case::rename("PATCH", "/api/v1/orgs/org_bad.id", Some(json!({ "display_name": "Renamed" })), NOT_AN_ID)]
#[case::rename_by_handle("PATCH", "/api/v1/orgs/acme", Some(json!({ "display_name": "Renamed" })), A_HANDLE)]
#[case::assign_by_handle("PUT", "/api/v1/orgs/acme/clients", Some(json!({ "client_ids": ["rack"] })), A_HANDLE)]
#[case::put_benchmark_by_handle("PUT", "/api/v1/orgs/acme/benchmarks/gsm8k", Some(json!({})), A_HANDLE)]
#[case::delete_benchmark_by_handle("DELETE", "/api/v1/orgs/acme/benchmarks/gsm8k", None, A_HANDLE)]
#[case::mint_key_by_handle("POST", "/api/v1/orgs/acme/client-preauth-keys", Some(json!({})), A_HANDLE)]
#[case::revoke_key_by_handle(
    "DELETE",
    "/api/v1/orgs/acme/client-preauth-keys/0195c3e2a4f37b8d",
    None,
    A_HANDLE
)]
#[case::catalog("GET", "/api/v1/orgs/bad.slug/benchmarks", None, BAD_HANDLE)]
#[case::preauth_keys("GET", "/api/v1/orgs/bad.slug/client-preauth-keys", None, BAD_HANDLE)]
#[tokio::test]
async fn a_malformed_organization_reference_is_a_400(
    #[case] method: &str,
    #[case] uri: &str,
    #[case] body: Option<Value>,
    #[case] prefix: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let manager = manager(&state).await?;
    create_org(&state, &manager, "acme").await?;

    let resp = match &body {
        Some(body) => signed_json(&state, &manager, method, uri, body).await?,
        None => signed_empty(&state, &manager, method, uri).await?,
    };
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{method} {uri}");
    let body = body_json(resp).await?;
    assert!(
        json_str(&body, "error")?.starts_with(prefix),
        "{method} {uri}: {body}"
    );
    Ok(())
}

/// Why the two are separate fields: every stored reference to the organization
/// keeps resolving.
#[tokio::test]
async fn a_rename_leaves_the_handle_alone() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let manager = manager(&state).await?;
    let org_id = create_org(&state, &manager, "acme").await?;
    let uri = format!("/api/v1/orgs/{org_id}");

    let request = json!({ "display_name": "Acme Holdings", "status": "disabled" });
    crate::api_schema::assert_conforms("UpdateOrgRequest", &request)?;
    let resp = signed_json(&state, &manager, "PATCH", &uri, &request).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    crate::api_schema::assert_conforms("ChangedOrgView", &body)?;
    assert_eq!(body["changed"], true, "{body}");
    assert_eq!(json_str(&body, "display_name")?, "Acme Holdings");
    assert_eq!(json_str(&body, "status")?, "disabled");
    assert_eq!(json_str(&body, "slug")?, "acme");
    assert_eq!(json_str(&body, "org_id")?, org_id);

    let (key, user_id, key_id) = &manager;
    let resp = authed_user_get(&state, key, user_id, key_id, "/api/v1/orgs/acme").await?;
    assert_eq!(resp.status(), StatusCode::OK);

    // A body naming no field is a `400` rather than a silent no-op write.
    let resp = signed_json(&state, &manager, "PATCH", &uri, &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    Ok(())
}

/// The admitting side of the scoped gate. Its status is a server decision, so
/// setting it is refused and changes nothing.
#[tokio::test]
async fn an_org_admin_renames_its_own_organization() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let manager = manager(&state).await?;
    let acme = create_org(&state, &manager, "acme").await?;
    let admin = registered_user(&state, &[UserPermission::ManageOrgs]).await?;
    join(&state, &admin.1, &acme).await?;
    let uri = format!("/api/v1/orgs/{acme}");

    let resp = signed_json(
        &state,
        &admin,
        "PATCH",
        &uri,
        &json!({ "display_name": "Acme Refused", "status": "disabled" }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        json_str(&body_json(resp).await?, "error")?,
        "user lacks the manage_any_orgs permission"
    );
    let stored = state
        .org_store
        .get_org(&OrgId::try_new(acme.clone())?)
        .await?;
    assert!(
        stored.is_some_and(|org| {
            org.display_name.as_str() != "Acme Refused" && org.status == OrgStatus::Active
        }),
        "a refused write changes nothing"
    );

    // The app sends back the status it drew; unchanged, it takes no server grant.
    let resp = signed_json(
        &state,
        &admin,
        "PATCH",
        &uri,
        &json!({ "display_name": "Acme Holdings", "status": "active" }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let (body, changed) = changed_record(body_json(resp).await?)?;
    assert!(changed, "the name moved: {body}");
    assert_eq!(json_str(&body, "org_id")?, acme);
    assert_eq!(json_str(&body, "slug")?, "acme");
    assert_eq!(json_str(&body, "display_name")?, "Acme Holdings");
    assert_eq!(json_str(&body, "status")?, "active");
    assert_eq!(body["member_count"], 1, "the admin itself: {body}");
    assert_eq!(body["client_count"], 0, "{body}");

    let (key, user_id, key_id) = &manager;
    let read = body_json(authed_user_get(&state, key, user_id, key_id, "/api/v1/orgs/acme").await?)
        .await?;
    assert_eq!(read, body, "the answer is what was written");

    // Nor does it re-enable the organization a server admin disabled.
    let disabled = json!({ "display_name": "Acme Holdings", "status": "disabled" });
    let resp = signed_json(&state, &manager, "PATCH", &uri, &disabled).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let enabled = json!({ "display_name": "Acme Holdings", "status": "active" });
    let resp = signed_json(&state, &admin, "PATCH", &uri, &enabled).await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let stored = state.org_store.get_org(&OrgId::try_new(acme)?).await?;
    assert_eq!(stored.map(|org| org.status), Some(OrgStatus::Disabled));
    Ok(())
}

/// An org store that counts the writes a no-op must skip.
struct WatchedOrgStore {
    inner: Arc<dyn OrgStore>,
    org_puts: AtomicUsize,
    membership_sets: AtomicUsize,
    ownership_sets: AtomicUsize,
}

impl WatchedOrgStore {
    fn over(inner: Arc<dyn OrgStore>) -> Self {
        Self {
            inner,
            org_puts: AtomicUsize::new(0),
            membership_sets: AtomicUsize::new(0),
            ownership_sets: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl OrgStore for WatchedOrgStore {
    async fn get_org(&self, org_id: &OrgId) -> anyhow::Result<Option<Organization>> {
        self.inner.get_org(org_id).await
    }
    async fn put_org(&self, org: &Organization) -> anyhow::Result<()> {
        self.org_puts.fetch_add(1, Ordering::SeqCst);
        self.inner.put_org(org).await
    }
    async fn delete_org(&self, org_id: &OrgId) -> anyhow::Result<()> {
        self.inner.delete_org(org_id).await
    }
    async fn list_orgs(&self) -> anyhow::Result<Vec<Organization>> {
        self.inner.list_orgs().await
    }
    async fn claim_org_slug(&self, slug: &str, org_id: &OrgId) -> anyhow::Result<SlugClaim> {
        self.inner.claim_org_slug(slug, org_id).await
    }
    async fn lookup_org_by_slug(&self, slug: &str) -> anyhow::Result<Option<OrgId>> {
        self.inner.lookup_org_by_slug(slug).await
    }
    async fn release_org_slug(&self, slug: &str) -> anyhow::Result<()> {
        self.inner.release_org_slug(slug).await
    }
    async fn get_membership(&self, user_id: &UserId) -> anyhow::Result<Option<OrgId>> {
        self.inner.get_membership(user_id).await
    }
    async fn put_membership(&self, membership: &Membership) -> anyhow::Result<()> {
        self.inner.put_membership(membership).await
    }
    async fn delete_membership(&self, user_id: &UserId) -> anyhow::Result<()> {
        self.inner.delete_membership(user_id).await
    }
    async fn set_membership(&self, user_id: &UserId, org_id: Option<OrgId>) -> anyhow::Result<()> {
        self.membership_sets.fetch_add(1, Ordering::SeqCst);
        self.inner.set_membership(user_id, org_id).await
    }
    async fn list_memberships(&self) -> anyhow::Result<Vec<Membership>> {
        self.inner.list_memberships().await
    }
    async fn get_ownership(&self, client_id: &ClientId) -> anyhow::Result<Option<ClientOwner>> {
        self.inner.get_ownership(client_id).await
    }
    async fn put_ownership(&self, ownership: &Ownership) -> anyhow::Result<()> {
        self.inner.put_ownership(ownership).await
    }
    async fn delete_ownership(&self, client_id: &ClientId) -> anyhow::Result<()> {
        self.inner.delete_ownership(client_id).await
    }
    async fn set_ownership(
        &self,
        client_id: &ClientId,
        owner: Option<ClientOwner>,
    ) -> anyhow::Result<()> {
        self.ownership_sets.fetch_add(1, Ordering::SeqCst);
        self.inner.set_ownership(client_id, owner).await
    }
    async fn list_ownerships(&self) -> anyhow::Result<Vec<Ownership>> {
        self.inner.list_ownerships().await
    }
}

/// A `PATCH` naming the values an organization holds writes nothing, so it
/// cannot revert a concurrent edit with the copy it read.
#[tokio::test]
async fn a_patch_to_the_stored_values_writes_nothing() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let mut state = make_state(dir.path()).await?;
    let manager = manager(&state).await?;
    let acme = create_org(&state, &manager, "acme").await?;
    let uri = format!("/api/v1/orgs/{acme}");
    let watched = Arc::new(WatchedOrgStore::over(state.org_store.clone()));
    state.org_store = watched.clone();

    let answers = check_write_steps(
        &state,
        &manager,
        ("PATCH", &uri),
        || watched.org_puts.load(Ordering::SeqCst),
        [
            (json!({ "display_name": "acme" }), false, 0),
            (
                json!({ "display_name": "acme", "status": "active" }),
                false,
                0,
            ),
            (json!({ "status": "disabled" }), true, 1),
        ],
    )
    .await?;
    answers
        .iter()
        .try_for_each(|answer| crate::api_schema::assert_conforms("ChangedOrgView", answer))?;
    let stored = state.org_store.get_org(&OrgId::try_new(acme)?).await?;
    assert_eq!(stored.map(|org| org.status), Some(OrgStatus::Disabled));
    Ok(())
}

/// Naming the organization an account already belongs to, or clearing a
/// membership it does not hold, answers `changed: false` and writes nothing.
#[tokio::test]
async fn a_membership_put_to_the_stored_org_writes_nothing() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let mut state = make_state(dir.path()).await?;
    let manager = manager(&state).await?;
    let acme = create_org(&state, &manager, "acme").await?;
    let member = create_user(&state, "member@example.test").await?;
    let uri = format!("/api/v1/users/{}/org", member.user_id);
    let watched = Arc::new(WatchedOrgStore::over(state.org_store.clone()));
    state.org_store = watched.clone();

    let answers = check_write_steps(
        &state,
        &manager,
        ("PUT", &uri),
        || watched.membership_sets.load(Ordering::SeqCst),
        [
            (json!({ "org_id": acme }), true, 1),
            (json!({ "org_id": acme }), false, 1),
            (json!({ "org_id": null }), true, 2),
            (json!({ "org_id": null }), false, 2),
        ],
    )
    .await?;
    answers
        .iter()
        .try_for_each(|answer| crate::api_schema::assert_conforms("ChangedOrgUserRow", answer))?;
    assert_eq!(state.org_store.get_membership(&member.user_id).await?, None);
    Ok(())
}

/// Naming the owner a device already has, or detaching one with none, answers
/// `changed: false` and writes nothing.
#[tokio::test]
async fn an_owner_put_to_the_stored_owner_writes_nothing() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let mut state = make_state(dir.path()).await?;
    let manager = manager(&state).await?;
    let acme = create_org(&state, &manager, "acme").await?;
    let (_, client_id) = register_and_approve(&state).await?;
    let uri = format!("/api/v1/clients/{client_id}/owner");
    let watched = Arc::new(WatchedOrgStore::over(state.org_store.clone()));
    state.org_store = watched.clone();

    let owner = json!({ "type": "org", "org_id": acme });
    let steps = [
        (owner.clone(), true, 1),
        (owner, false, 1),
        (Value::Null, true, 2),
        (Value::Null, false, 2),
    ]
    .map(|(owner, changed, writes)| (json!({ "owner": owner }), changed, writes));
    let answers = check_write_steps(
        &state,
        &manager,
        ("PUT", &uri),
        || watched.ownership_sets.load(Ordering::SeqCst),
        steps.clone(),
    )
    .await?;
    steps.iter().zip(&answers).for_each(|((body, ..), answer)| {
        assert_eq!(
            answer.get("owner").unwrap_or(&Value::Null),
            &body["owner"],
            "{answer}"
        );
    });
    assert_eq!(state.org_store.get_ownership(&client_id).await?, None);
    Ok(())
}

/// Another organization reads as absent and is left as it was; an admin in no
/// organization reaches none.
#[tokio::test]
async fn an_org_admin_cannot_patch_another_organization() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let manager = manager(&state).await?;
    let acme = create_org(&state, &manager, "acme").await?;
    let rival = create_org(&state, &manager, "rival").await?;
    let admin = registered_user(&state, &[UserPermission::ManageOrgs]).await?;
    join(&state, &admin.1, &acme).await?;
    let stray = registered_user(&state, &[UserPermission::ManageOrgs]).await?;

    // Names a field, or the empty-body 400 would answer first.
    let hijack = json!({ "display_name": "Hijacked", "status": "disabled" });
    for (signer, reference) in [(&admin, &rival), (&stray, &acme)] {
        let uri = format!("/api/v1/orgs/{reference}");
        let resp = signed_json(&state, signer, "PATCH", &uri, &hijack).await?;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "PATCH {uri}");
        assert_eq!(
            json_str(&body_json(resp).await?, "error")?,
            format!("no organization {reference}"),
            "PATCH {uri}"
        );
    }

    let (key, user_id, key_id) = &manager;
    for slug in ["acme", "rival"] {
        let uri = format!("/api/v1/orgs/{slug}");
        let body = body_json(authed_user_get(&state, key, user_id, key_id, &uri).await?).await?;
        assert_eq!(json_str(&body, "display_name")?, slug, "{body}");
        assert_eq!(json_str(&body, "status")?, "active", "{body}");
    }
    Ok(())
}

/// A membership is an authority: moving an org admin moves what it
/// administers, with no grant written.
#[tokio::test]
async fn test_moving_an_org_admin_moves_what_it_administers() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let manager = manager(&state).await?;
    let acme = create_org(&state, &manager, "acme").await?;
    let rival = create_org(&state, &manager, "rival").await?;
    let admin = registered_user(&state, UserPermissionGroup::OrgAdmin.permissions()).await?;
    join(&state, &admin.1, &acme).await?;
    let held = state
        .user_store
        .get_user_permissions(&admin.1)
        .await?
        .context("the admin holds a set")?;

    let resp = signed_json(
        &state,
        &manager,
        "PUT",
        &format!("/api/v1/users/{}/org", admin.1),
        &json!({ "org_id": rival }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);

    let rename = json!({ "display_name": "Renamed" });
    let resp = signed_json(
        &state,
        &admin,
        "PATCH",
        &format!("/api/v1/orgs/{acme}"),
        &rename,
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND, "acme left its reach");
    let resp = signed_json(
        &state,
        &admin,
        "PATCH",
        &format!("/api/v1/orgs/{rival}"),
        &rename,
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK, "rival came into it");
    assert_eq!(
        state.user_store.get_user_permissions(&admin.1).await?,
        Some(held),
        "the move wrote no grant"
    );
    Ok(())
}

/// A disabled organization still scopes its members: the status refuses only
/// Clerk provisioning (clerk_keys.rs `disabled_organization`).
#[tokio::test]
async fn test_a_disabled_organization_still_scopes_its_members() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let manager = manager(&state).await?;
    let acme = create_org(&state, &manager, "acme").await?;
    let reader = registered_user(&state, &[UserPermission::ReadOrgs]).await?;
    join(&state, &reader.1, &acme).await?;
    let member = registered_user(&state, &[]).await?;
    join(&state, &member.1, &acme).await?;

    let resp = signed_json(
        &state,
        &manager,
        "PATCH",
        &format!("/api/v1/orgs/{acme}"),
        &json!({ "status": "disabled" }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);

    for (signer, uri) in [
        (&reader, "/api/v1/orgs/acme"),
        (&reader, "/api/v1/orgs/acme/users"),
        // On membership alone, with no grant.
        (&member, "/api/v1/orgs/acme/benchmarks"),
    ] {
        let resp = signed_empty(&state, signer, "GET", uri).await?;
        assert_eq!(resp.status(), StatusCode::OK, "GET {uri}");
    }
    Ok(())
}

/// An org id is minted, so a membership naming one never created would never
/// resolve. `null` clears it.
#[tokio::test]
async fn a_membership_names_an_organization_that_exists() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let manager = manager(&state).await?;
    let org_id = create_org(&state, &manager, "acme").await?;
    let member = create_user(&state, "member@example.test").await?;
    let uri = format!("/api/v1/users/{}/org", member.user_id);
    let account = serde_json::to_value(&member)?;

    let resp = signed_json(
        &state,
        &manager,
        "PUT",
        &uri,
        &json!({ "org_id": "org_00000000-0000-0000-0000-000000000000" }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let request = json!({ "org_id": org_id });
    crate::api_schema::assert_conforms("SetUserOrgRequest", &request)?;
    let resp = signed_json(&state, &manager, "PUT", &uri, &request).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let mut placed = account.clone();
    placed["org_id"] = json!(org_id);
    let body = body_json(resp).await?;
    crate::api_schema::assert_conforms("ChangedOrgUserRow", &body)?;
    assert_eq!(changed_record(body)?, (placed, true), "the account, joined");

    // The roster route is the read side of the same fact.
    let (key, user_id, key_id) = &manager;
    let resp = authed_user_get(
        &state,
        key,
        user_id,
        key_id,
        &format!("/api/v1/orgs/{org_id}/users"),
    )
    .await?;
    let body = body_json(resp).await?;
    assert_eq!(body["items"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        json_str(&body["items"][0], "user_id")?,
        member.user_id.as_str()
    );

    // `null` is a value this field takes, not an absent field.
    let request = json!({ "org_id": null });
    crate::api_schema::assert_conforms("SetUserOrgRequest", &request)?;
    let resp = signed_json(&state, &manager, "PUT", &uri, &request).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    crate::api_schema::assert_conforms("ChangedOrgUserRow", &body)?;
    assert_eq!(
        changed_record(body)?,
        (account, true),
        "a clear omits org_id"
    );
    assert_eq!(state.org_store.get_membership(&member.user_id).await?, None);
    Ok(())
}

/// Only an explicit `null` clears a membership: `{}` or a misspelt `org_id` is
/// a `400` and the account stays, as a scoped admin could not put it back.
#[tokio::test]
async fn a_membership_is_cleared_by_null_only() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let manager = manager(&state).await?;
    let org_id = create_org(&state, &manager, "acme").await?;
    let member = create_user(&state, "member@example.test").await?;
    let uri = format!("/api/v1/users/{}/org", member.user_id);
    let resp = signed_json(&state, &manager, "PUT", &uri, &json!({ "org_id": org_id })).await?;
    assert_eq!(resp.status(), StatusCode::OK);

    for (body, key) in [
        (json!({}), "org_id"),
        (json!({ "orgid": org_id }), "orgid"),
        (json!({ "org_id": null, "owner": null }), "owner"),
    ] {
        assert!(
            crate::api_schema::assert_conforms("SetUserOrgRequest", &body).is_err(),
            "the schema refuses it too: {body}"
        );
        let resp = signed_json(&state, &manager, "PUT", &uri, &body).await?;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{body}");
        let error = json_str(&body_json(resp).await?, "error")?.to_string();
        assert!(error.contains(key), "the refusal names {key}: {error}");
        assert_eq!(
            state
                .org_store
                .get_membership(&member.user_id)
                .await?
                .as_ref()
                .map(|org| org.as_str()),
            Some(org_id.as_str()),
            "a refused body leaves the membership alone: {body}"
        );
    }

    let resp = signed_json(&state, &manager, "PUT", &uri, &json!({ "org_id": null })).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(state.org_store.get_membership(&member.user_id).await?, None);
    Ok(())
}

/// A `404` whether the body sets an organization or clears one.
#[tokio::test]
async fn a_membership_names_an_account_that_exists() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let manager = manager(&state).await?;
    let org_id = create_org(&state, &manager, "acme").await?;
    let uri = "/api/v1/users/user_00000000-0000-0000-0000-000000000000/org";

    // The body organization is resolved first, so it names one that exists.
    for body in [json!({ "org_id": org_id }), json!({ "org_id": null })] {
        let resp = signed_json(&state, &manager, "PUT", uri, &body).await?;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{body}");
        assert_eq!(
            json_str(&body_json(resp).await?, "error")?,
            "no such user",
            "{body}"
        );
    }
    assert!(state.org_store.list_memberships().await?.is_empty());
    Ok(())
}

/// A handle, which the path would accept, is a `400` and the membership stays.
#[tokio::test]
async fn a_membership_body_takes_an_id_not_a_handle() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let manager = manager(&state).await?;
    create_org(&state, &manager, "acme").await?;
    let labs = create_org(&state, &manager, "labs").await?;
    let member = create_user(&state, "member@example.test").await?;
    let uri = format!("/api/v1/users/{}/org", member.user_id);
    let resp = signed_json(&state, &manager, "PUT", &uri, &json!({ "org_id": labs })).await?;
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = signed_json(&state, &manager, "PUT", &uri, &json!({ "org_id": "acme" })).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await?;
    assert!(
        json_str(&body, "error")?.contains("organization id must start with `org_`"),
        "{body}"
    );
    assert_eq!(
        state
            .org_store
            .get_membership(&member.user_id)
            .await?
            .map(|id| id.to_string()),
        Some(labs),
        "the member did not move"
    );
    Ok(())
}

/// `me` included, once the caller is authenticated; a `401` before.
#[rstest]
#[case::device_id("ev1_0123456789abcdef")]
#[case::bare_word("nobody")]
#[case::own_alias("me")]
#[tokio::test]
async fn a_membership_path_that_is_not_a_user_id_is_a_400(
    #[case] segment: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let manager = manager(&state).await?;
    let uri = format!("/api/v1/users/{segment}/org");
    let clear = json!({ "org_id": null });

    let resp = signed_json(&state, &manager, "PUT", &uri, &clear).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{uri}");
    let body = body_json(resp).await?;
    assert!(
        json_str(&body, "error")?.contains("user id must start with `user_`"),
        "{uri}: {body}"
    );

    let resp = json_request(&state, "PUT", &uri, &[], &clear).await?;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{uri}");
    assert_eq!(
        json_str(&body_json(resp).await?, "error")?,
        "missing X-Client-Id header"
    );
    Ok(())
}

/// The join stored once: moving the account moves the device, and the org-owned
/// device stays put.
#[tokio::test]
async fn a_user_owned_device_follows_its_owners_organization() -> anyhow::Result<()> {
    async fn owned(
        state: &AppState,
        signer: &UserSigner,
        org: &str,
    ) -> anyhow::Result<Vec<String>> {
        let (key, user_id, key_id) = signer;
        let uri = format!("/api/v1/orgs/{org}/clients");
        let body = body_json(authed_user_get(state, key, user_id, key_id, &uri).await?).await?;
        let mut ids: Vec<String> = body["items"]
            .as_array()
            .map(|rows| {
                rows.iter()
                    .filter_map(|row| row["client_id"].as_str().map(str::to_string))
                    .collect()
            })
            .ok_or_else(|| anyhow::anyhow!("no rows: {body}"))?;
        ids.sort();
        Ok(ids)
    }

    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let manager = manager(&state).await?;
    let acme = create_org(&state, &manager, "acme").await?;
    let labs = create_org(&state, &manager, "labs").await?;

    let member = create_user(&state, "member@example.test").await?;
    join(&state, &member.user_id, &acme).await?;

    let (_, rack) = register_and_approve(&state).await?;
    let (_, laptop) = register_and_approve(&state).await?;
    signed_json(
        &state,
        &manager,
        "PUT",
        &format!("/api/v1/clients/{rack}/owner"),
        &json!({ "owner": { "type": "org", "org_id": acme } }),
    )
    .await?;
    signed_json(
        &state,
        &manager,
        "PUT",
        &format!("/api/v1/clients/{laptop}/owner"),
        &json!({ "owner": { "type": "user", "user_id": member.user_id } }),
    )
    .await?;

    let mut both = vec![rack.to_string(), laptop.to_string()];
    both.sort();
    assert_eq!(owned(&state, &manager, &acme).await?, both);
    assert_eq!(owned(&state, &manager, &labs).await?, Vec::<String>::new());

    // The member moves. The laptop goes with them; the rack does not.
    signed_json(
        &state,
        &manager,
        "PUT",
        &format!("/api/v1/users/{}/org", member.user_id),
        &json!({ "org_id": labs }),
    )
    .await?;
    assert_eq!(
        owned(&state, &manager, &acme).await?,
        vec![rack.to_string()]
    );
    assert_eq!(
        owned(&state, &manager, &labs).await?,
        vec![laptop.to_string()]
    );
    Ok(())
}

/// Each row's `owner` names the member a device came through, or the
/// organization for one it owns directly.
#[tokio::test]
async fn the_inventory_names_the_member_a_device_came_through() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let manager = manager(&state).await?;
    let acme = create_org(&state, &manager, "acme").await?;
    let member = create_user(&state, "member@example.test").await?;
    join(&state, &member.user_id, &acme).await?;

    let (_, rack) = register_and_approve(&state).await?;
    let (_, laptop) = register_and_approve(&state).await?;
    let rack_owner = json!({ "type": "org", "org_id": acme });
    let laptop_owner = json!({ "type": "user", "user_id": member.user_id });
    for (client_id, owner) in [(&rack, &rack_owner), (&laptop, &laptop_owner)] {
        let resp = signed_json(
            &state,
            &manager,
            "PUT",
            &format!("/api/v1/clients/{client_id}/owner"),
            &json!({ "owner": owner }),
        )
        .await?;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    let (key, user_id, key_id) = &manager;
    let uri = format!("/api/v1/orgs/{acme}/clients");
    let resp = authed_user_get(&state, key, user_id, key_id, &uri).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    let row = |client_id: &ClientId| {
        body["items"]
            .as_array()
            .and_then(|rows| {
                rows.iter()
                    .find(|row| row["client_id"] == json!(client_id.as_str()))
            })
            .ok_or_else(|| anyhow::anyhow!("the inventory lists {client_id}: {body}"))
    };

    assert_eq!(row(&rack)?["owner"], rack_owner);
    assert_eq!(row(&laptop)?["owner"], laptop_owner);
    Ok(())
}

/// The relations are their own records, yet every response still carries them.
#[tokio::test]
async fn the_relations_answer_where_the_records_carried_them() -> anyhow::Result<()> {
    async fn get(state: &AppState, signer: &UserSigner, uri: &str) -> anyhow::Result<Value> {
        let (key, user_id, key_id) = signer;
        body_json(authed_user_get(state, key, user_id, key_id, uri).await?).await
    }

    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let manager = manager(&state).await?;
    let acme = create_org(&state, &manager, "acme").await?;
    let member = registered_user(&state, &[]).await?;
    let member_id = member.1.clone();
    let resp = signed_json(
        &state,
        &manager,
        "PUT",
        &format!("/api/v1/users/{member_id}/org"),
        &json!({ "org_id": acme }),
    )
    .await?;
    assert_eq!(body_json(resp).await?["org_id"], json!(acme));
    let (_, rack) = register_and_approve(&state).await?;
    let owner = json!({ "type": "org", "org_id": acme });
    let request = json!({ "owner": owner });
    crate::api_schema::assert_conforms("SetClientOwnerRequest", &request)?;
    let resp = signed_json(
        &state,
        &manager,
        "PUT",
        &format!("/api/v1/clients/{rack}/owner"),
        &request,
    )
    .await?;
    assert_eq!(body_json(resp).await?["owner"], owner);

    let members = get(&state, &manager, &format!("/api/v1/orgs/{acme}/users")).await?;
    assert_eq!(members["items"][0]["org_id"], json!(acme));
    let account = get(&state, &manager, &format!("/api/v1/users/{member_id}")).await?;
    assert_eq!(account["org_id"], json!(acme));
    let accounts = get(&state, &manager, "/api/v1/users").await?;
    crate::api_schema::assert_conforms("UserViewPage", &accounts)?;
    let listed = accounts["items"]
        .as_array()
        .and_then(|rows| {
            rows.iter()
                .find(|row| row["user_id"] == json!(member_id.as_str()))
        })
        .ok_or_else(|| anyhow::anyhow!("the roster lists the member"))?;
    assert_eq!(listed["org_id"], json!(acme));
    let inventory = get(&state, &manager, &format!("/api/v1/orgs/{acme}/clients")).await?;
    assert_eq!(inventory["items"][0]["owner"], owner);
    assert_eq!(
        get(&state, &manager, &format!("/api/v1/clients/{rack}")).await?["owner"],
        owner
    );
    let fleet = get(&state, &manager, "/api/v1/clients").await?;
    crate::api_schema::assert_conforms("ClientListRowPage", &fleet)?;
    let row = fleet["items"]
        .as_array()
        .and_then(|rows| {
            rows.iter()
                .find(|row| row["client_id"] == json!(rack.as_str()))
        })
        .ok_or_else(|| anyhow::anyhow!("the fleet lists the rack"))?;
    assert_eq!(row["owner"], owner);

    let account_uri = format!("/api/v1/users/{member_id}");
    let cases = [
        (
            &member,
            "GET",
            "/api/v1/users/me".to_string(),
            None,
            "/org_id",
            json!(acme),
        ),
        (
            &manager,
            "PATCH",
            account_uri.clone(),
            Some(json!({ "status": "active" })),
            "/org_id",
            json!(acme),
        ),
        (
            &manager,
            "PUT",
            format!("{account_uri}/permissions"),
            Some(json!({ "permissions": ["read_clients"] })),
            "/org_id",
            json!(acme),
        ),
        (
            &manager,
            "PATCH",
            format!("{account_uri}/permissions"),
            Some(json!({ "remove": ["read_clients"] })),
            "/org_id",
            json!(acme),
        ),
        (
            &manager,
            "POST",
            "/api/v1/users".to_string(),
            Some(json!({ "contact_email": "new@example.test", "org_id": acme })),
            "/org_id",
            json!(acme),
        ),
        (
            &manager,
            "PATCH",
            format!("/api/v1/clients/{rack}"),
            Some(json!({ "client_details": "rack 3" })),
            "/owner",
            owner.clone(),
        ),
    ];
    let state = &state;
    futures::stream::iter(cases.into_iter().map(Ok::<_, anyhow::Error>))
        .try_for_each(
            |(signer, method, uri, body, pointer, expected)| async move {
                let resp = match &body {
                    Some(body) => signed_json(state, signer, method, &uri, body).await?,
                    None => signed_empty(state, signer, method, &uri).await?,
                };
                let answer = body_json(resp).await?;
                assert_eq!(
                    answer.pointer(pointer),
                    Some(&expected),
                    "{method} {uri}: {answer}"
                );
                Ok(())
            },
        )
        .await?;

    // Neither relation is written onto the auth record it used to live on.
    let raw = |path: String| -> anyhow::Result<Value> {
        Ok(serde_json::from_slice(&std::fs::read(
            dir.path().join(path),
        )?)?)
    };
    assert!(
        raw(format!("users/{member_id}.json"))?
            .get("org_id")
            .is_none()
    );
    assert!(raw(format!("clients/{rack}.json"))?.get("owner").is_none());
    assert_eq!(
        raw(format!("user-orgs/{member_id}.json"))?,
        json!({ "user_id": member_id.as_str(), "org_id": acme })
    );
    assert_eq!(
        raw(format!("client-owners/{rack}.json"))?,
        json!({ "client_id": rack.as_str(), "owner": owner })
    );
    Ok(())
}

/// So a missing owner reads as "belongs to no organization", not as a dangling
/// reference.
#[rstest]
#[case::missing_org(json!({ "type": "org", "org_id": "org_00000000-0000-0000-0000-000000000000" }))]
#[case::missing_user(
    json!({ "type": "user", "user_id": "user_00000000-0000-0000-0000-000000000000" })
)]
#[tokio::test]
async fn an_ownership_names_a_record_that_exists(#[case] missing: Value) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let manager = manager(&state).await?;
    create_org(&state, &manager, "acme").await?;
    let (_, client_id) = register_and_approve(&state).await?;
    let uri = format!("/api/v1/clients/{client_id}/owner");

    let resp = signed_json(&state, &manager, "PUT", &uri, &json!({ "owner": missing })).await?;
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "an ownership must not name a record that does not exist"
    );

    // A missing device is a `404` too, not a write that invents one.
    let resp = signed_json(
        &state,
        &manager,
        "PUT",
        "/api/v1/clients/ev1_nobody/owner",
        &json!({ "owner": null }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // Clearing an unowned device is accepted and leaves it unowned.
    let resp = signed_json(&state, &manager, "PUT", &uri, &json!({ "owner": null })).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        state
            .org_store
            .get_ownership(&ClientId::try_new(client_id.as_str())?)
            .await?,
        None
    );
    Ok(())
}

/// A well-formed organization id no organization was minted with.
const NEVER_MINTED: &str = "org_00000000-0000-0000-0000-000000000000";

/// A rename must not resurrect a deleted record. Only the CLI deletes one, so
/// the store does it here.
#[tokio::test]
async fn renaming_an_organization_that_does_not_exist_is_a_404() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let manager = manager(&state).await?;
    let acme = create_org(&state, &manager, "acme").await?;
    let gone = create_org(&state, &manager, "gone").await?;
    state
        .org_store
        .delete_org(&OrgId::try_new(gone.as_str())?)
        .await?;
    state.org_store.release_org_slug("gone").await?;

    let body = json!({ "display_name": "X" });
    for reference in [NEVER_MINTED, gone.as_str()] {
        let uri = format!("/api/v1/orgs/{reference}");
        let error = format!("no organization {reference}");
        let resp = signed_json(&state, &manager, "PATCH", &uri, &body).await?;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "PATCH {uri}");
        assert_eq!(
            json_str(&body_json(resp).await?, "error")?,
            error,
            "PATCH {uri}"
        );
    }

    let left: Vec<String> = state
        .org_store
        .list_orgs()
        .await?
        .into_iter()
        .map(|org| org.org_id.to_string())
        .collect();
    assert_eq!(left, vec![acme], "nothing must come back into existence");
    assert!(state.org_store.lookup_org_by_slug("gone").await?.is_none());
    Ok(())
}

/// The counts follow the join the per-organization routes do, and an update
/// answers the same counted view.
#[tokio::test]
async fn the_listing_counts_members_and_devices() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let manager = manager(&state).await?;
    let acme = create_org(&state, &manager, "acme").await?;
    create_org(&state, &manager, "labs").await?;

    let member = create_user(&state, "member@example.test").await?;
    join(&state, &member.user_id, &acme).await?;
    let (_, client_id) = register_and_approve(&state).await?;
    signed_json(
        &state,
        &manager,
        "PUT",
        &format!("/api/v1/clients/{client_id}/owner"),
        &json!({ "owner": { "type": "user", "user_id": member.user_id } }),
    )
    .await?;

    let (key, user_id, key_id) = &manager;
    let body =
        body_json(authed_user_get(&state, key, user_id, key_id, "/api/v1/orgs").await?).await?;
    let rows = body["items"].as_array().cloned().unwrap_or_default();
    assert_eq!(rows.len(), 2);
    let acme_row = rows
        .iter()
        .find(|row| row["org_id"] == acme.as_str())
        .ok_or_else(|| anyhow::anyhow!("acme must be listed"))?;
    assert_eq!(acme_row["member_count"], 1);
    assert_eq!(acme_row["client_count"], 1);
    let labs_row = rows
        .iter()
        .find(|row| row["slug"] == "labs")
        .ok_or_else(|| anyhow::anyhow!("labs must be listed"))?;
    assert_eq!(labs_row["member_count"], 0);
    assert_eq!(labs_row["client_count"], 0);

    // The single-organization read agrees with its row.
    let body = body_json(
        authed_user_get(
            &state,
            key,
            user_id,
            key_id,
            &format!("/api/v1/orgs/{acme}"),
        )
        .await?,
    )
    .await?;
    assert_eq!(body["member_count"], 1);
    assert_eq!(body["client_count"], 1);

    // And an update answers what the read then serves, counts included.
    let resp = signed_json(
        &state,
        &manager,
        "PATCH",
        &format!("/api/v1/orgs/{acme}"),
        &json!({ "display_name": "Acme Holdings" }),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let (updated, changed) = changed_record(body_json(resp).await?)?;
    assert!(changed, "the name moved: {updated}");
    let read = body_json(
        authed_user_get(
            &state,
            key,
            user_id,
            key_id,
            &format!("/api/v1/orgs/{acme}"),
        )
        .await?,
    )
    .await?;
    assert_eq!(updated, read);
    assert_eq!(updated["member_count"], 1);
    Ok(())
}

/// `counts=false` lists the same records, scope included, without the counts:
/// if it kept them, a caller after names would still pay for every relation.
#[tokio::test]
async fn the_listing_without_counts_lists_the_records_alone() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let manager = manager(&state).await?;
    let acme = create_org(&state, &manager, "acme").await?;
    create_org(&state, &manager, "labs").await?;
    let member = create_user(&state, "member@example.test").await?;
    join(&state, &member.user_id, &acme).await?;

    let counted = body_json(signed_empty(&state, &manager, "GET", "/api/v1/orgs").await?).await?;
    let asked =
        body_json(signed_empty(&state, &manager, "GET", "/api/v1/orgs?counts=true").await?).await?;
    assert_eq!(asked, counted, "counts=true is the default");

    let bare = body_json(signed_empty(&state, &manager, "GET", "/api/v1/orgs?counts=false").await?)
        .await?;
    crate::api_schema::assert_conforms("OrgViewPage", &bare)?;
    let ids = |body: &Value| {
        body["items"]
            .as_array()
            .map(|orgs| orgs.iter().map(|o| o["org_id"].clone()).collect::<Vec<_>>())
    };
    assert_eq!(ids(&bare).map(|ids| ids.len()), Some(2), "{bare}");
    assert_eq!(
        ids(&bare),
        ids(&counted),
        "the same rows, in the same order"
    );
    let rows = bare["items"]
        .as_array()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no rows: {bare}"))?;
    assert!(
        !rows.is_empty()
            && rows
                .iter()
                .all(|row| row.get("member_count").is_none() && row.get("client_count").is_none()),
        "{bare}"
    );

    let reader = registered_user(&state, &[UserPermission::ReadOrgs]).await?;
    join(&state, &reader.1, &acme).await?;
    let scoped =
        body_json(signed_empty(&state, &reader, "GET", "/api/v1/orgs?counts=false").await?).await?;
    assert_eq!(
        scoped["items"].as_array().map(|orgs| orgs
            .iter()
            .filter_map(|o| o["org_id"].as_str())
            .collect::<Vec<_>>()),
        Some(vec![acme.as_str()]),
        "a scoped reader still lists its own organization only"
    );

    let resp = signed_empty(&state, &manager, "GET", "/api/v1/orgs?counts=maybe").await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    Ok(())
}

/// Every device registered before ownership existed has no owner, so the
/// default has to be "belongs to nobody".
#[tokio::test]
async fn an_unowned_device_belongs_to_no_organization() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let manager = manager(&state).await?;
    let org_id = create_org(&state, &manager, "acme").await?;
    let (_, client_id) = register_and_approve(&state).await?;

    let id = ClientId::try_new(client_id.as_str())?;
    state
        .client_store
        .get_client(&id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("the registered client must exist"))?;
    assert_eq!(
        state.org_store.get_ownership(&id).await?,
        None,
        "a self-registration names nobody: the operator vouches for a device"
    );

    let (key, user_id, key_id) = &manager;
    let body = body_json(
        authed_user_get(
            &state,
            key,
            user_id,
            key_id,
            &format!("/api/v1/orgs/{org_id}/clients"),
        )
        .await?,
    )
    .await?;
    assert_eq!(body["items"].as_array().map(Vec::len), Some(0));
    Ok(())
}

/// The sorted ids of the fleet page `query` reads, checked against its schema.
async fn narrowed_ids(
    state: &AppState,
    signer: &UserSigner,
    query: &str,
) -> anyhow::Result<Vec<String>> {
    let (key, user_id, key_id) = signer;
    let resp = authed_user_get(
        state,
        key,
        user_id,
        key_id,
        &format!("/api/v1/clients?{query}"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK, "{query}");
    let body = body_json(resp).await?;
    crate::api_schema::assert_conforms("ClientListRowPage", &body)?;
    let mut ids: Vec<String> = body
        .pointer("/items")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| row.get("client_id").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    ids.sort();
    Ok(ids)
}

#[tokio::test]
async fn test_the_fleet_narrows_to_owned_or_unclaimed_devices() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let manager = manager(&state).await?;
    let acme = create_org(&state, &manager, "acme").await?;
    let member = create_user(&state, "member@example.test").await?;
    join(&state, &member.user_id, &acme).await?;

    let (_, rack) = register_and_approve(&state).await?;
    let (_, laptop) = register_and_approve(&state).await?;
    let (_, loose) = register_and_approve(&state).await?;
    for (client_id, owner) in [
        (&rack, json!({ "type": "org", "org_id": acme })),
        (
            &laptop,
            json!({ "type": "user", "user_id": member.user_id }),
        ),
    ] {
        let uri = format!("/api/v1/clients/{client_id}/owner");
        let resp = signed_json(&state, &manager, "PUT", &uri, &json!({ "owner": owner })).await?;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    let mut owned = vec![rack.to_string(), laptop.to_string()];
    owned.sort();
    let listed = |query| narrowed_ids(&state, &manager, query);
    assert_eq!(listed("owned=true").await?, owned);
    assert_eq!(listed("owned=false").await?, vec![loose.to_string()]);
    assert_eq!(
        listed("owned=false&status=pending").await?,
        Vec::<String>::new()
    );
    assert_eq!(
        listed("owned=").await?.len(),
        3,
        "an empty value clears the filter"
    );
    Ok(())
}

/// `org_id` takes the organization's roster read: `404` outside it, as
/// `GET /api/v1/orgs/{org_id}/clients` answers.
#[tokio::test]
async fn test_the_fleet_narrows_to_my_devices_or_an_organizations() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let manager = manager(&state).await?;
    let acme = create_org(&state, &manager, "acme").await?;
    let rival = create_org(&state, &manager, "rival").await?;
    let member = registered_user(
        &state,
        &[UserPermission::ReadOrgs, UserPermission::ReadClients],
    )
    .await?;
    join(&state, &member.1, &acme).await?;

    let (_, rack) = register_and_approve(&state).await?;
    let (_, laptop) = register_and_approve(&state).await?;
    let (_, theirs) = register_and_approve(&state).await?;
    let (_, kept) = register_and_approve(&state).await?;
    let (_, _loose) = register_and_approve(&state).await?;
    // The fleet grant alone: `owner=mine` must not need a roster read.
    let fleet_only = registered_user(&state, &[UserPermission::ReadClients]).await?;
    for (client_id, owner) in [
        (&rack, json!({ "type": "org", "org_id": acme })),
        (&laptop, json!({ "type": "user", "user_id": member.1 })),
        (&theirs, json!({ "type": "org", "org_id": rival })),
        (&kept, json!({ "type": "user", "user_id": fleet_only.1 })),
    ] {
        let uri = format!("/api/v1/clients/{client_id}/owner");
        let resp = signed_json(&state, &manager, "PUT", &uri, &json!({ "owner": owner })).await?;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    let mut acme_owns = vec![rack.to_string(), laptop.to_string()];
    acme_owns.sort();
    assert_eq!(
        narrowed_ids(&state, &member, "owner=mine").await?,
        vec![laptop.to_string()]
    );
    assert_eq!(
        narrowed_ids(&state, &manager, "owner=mine").await?,
        Vec::<String>::new(),
        "the manager owns nothing as a person"
    );
    assert_eq!(
        narrowed_ids(&state, &fleet_only, "owner=mine").await?,
        vec![kept.to_string()]
    );
    assert_eq!(
        narrowed_ids(&state, &member, &format!("org_id={acme}")).await?,
        acme_owns
    );
    assert_eq!(
        narrowed_ids(&state, &member, &format!("org_id={acme}&status=pending")).await?,
        Vec::<String>::new()
    );
    assert_eq!(
        narrowed_ids(&state, &manager, &format!("org_id={rival}")).await?,
        vec![theirs.to_string()]
    );
    // A device a page: the cursor walks the organization's own ids in order and
    // ends on the last of them, not at the end of the fleet.
    let mut walked = Vec::new();
    let mut cursor = String::new();
    for _ in 0..acme_owns.len() {
        let uri = format!("/api/v1/clients?org_id={acme}&limit=1{cursor}");
        let resp = signed_empty(&state, &member, "GET", &uri).await?;
        assert_eq!(resp.status(), StatusCode::OK, "{uri}");
        let body = body_json(resp).await?;
        walked.extend(
            body.pointer("/items/0/client_id")
                .and_then(Value::as_str)
                .map(str::to_string),
        );
        cursor = match body.pointer("/next_cursor").and_then(Value::as_str) {
            Some(next) => format!("&cursor={next}"),
            None => String::new(),
        };
    }
    assert_eq!(walked, acme_owns);
    assert_eq!(
        cursor, "",
        "the last of the organization's devices ends the listing"
    );

    // Outside the roster read, and an id naming no organization, read alike,
    // also beside a filter that matches nobody without a scan.
    let nobody = format!("org_id={rival}&any_of=gpu:a17pro");
    let untagged = format!("org_id={rival}&tag=none-such");
    let unknown = "org_id=org_00000000-0000-0000-0000-000000000000".to_string();
    for query in [format!("org_id={rival}"), nobody, untagged, unknown] {
        let resp =
            signed_empty(&state, &member, "GET", &format!("/api/v1/clients?{query}")).await?;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{query}");
    }
    // The fleet read alone does not reach the roster the filter is made of.
    for query in [
        format!("org_id={acme}"),
        format!("org_id={acme}&any_of=gpu:a17pro"),
    ] {
        let resp = signed_empty(
            &state,
            &fleet_only,
            "GET",
            &format!("/api/v1/clients?{query}"),
        )
        .await?;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{query}");
    }
    Ok(())
}

/// What the console's page draws, in one read; either spelling reaches it.
#[tokio::test]
async fn test_the_overview_answers_what_waits_and_the_latest_campaigns() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let root = registered_user(&state, UserPermissionGroup::ServerAdmin.permissions()).await?;
    let acme = create_org(&state, &root, "acme").await?;
    let org_id = OrgId::try_new(acme.clone())?;

    let mut applicant = create_user(&state, "applicant@example.test").await?;
    applicant.status = pipette_plan_types::UserStatus::Pending;
    state.user_store.put_user(&applicant).await?;
    set_user_org(&state, &applicant.user_id, Some(org_id.clone())).await?;
    for _ in 0..6 {
        let (_key, client_id) = register_pending(&state).await?;
        own_client_by_org(&state, &client_id, &org_id).await?;
    }
    let (_key, stuck) = register_and_approve(&state).await?;
    own_client_by_org(&state, &stuck, &org_id).await?;
    state
        .todo_store
        .write_suspension(&stuck, chrono::Utc::now(), &job("job-stuck")?)
        .await?;
    // Another organization's registration waits on somebody else.
    let rival = OrgId::try_new(create_org(&state, &root, "rival").await?)?;
    let (_key, theirs) = register_pending(&state).await?;
    own_client_by_org(&state, &theirs, &rival).await?;
    // One benchmark and one vault resource each, so a count is seen narrowing.
    futures::stream::iter([("acme", &org_id), ("rival", &rival)].map(Ok::<_, anyhow::Error>))
        .try_for_each(|(slug, owner)| {
            let (state, dir) = (&state, dir.path());
            async move {
                setup_private_benchmark(dir, &format!("eval_{slug}_internal_v3"), owner)?;
                put_org_resource(state, owner).await
            }
        })
        .await?;

    // Filed before the team's, so the overview's five leave the oldest out.
    let older: Vec<String> = futures::stream::iter((0..5).map(Ok::<_, anyhow::Error>))
        .and_then(|n| {
            let state = &state;
            let owner = pipette_plan_types::PrincipalOwner::Org {
                org_id: org_id.clone(),
            };
            async move {
                let campaign = put_idle_campaign_of(state, owner, &format!("Older {n}")).await?;
                Ok::<_, anyhow::Error>(campaign.campaign_id.to_string())
            }
        })
        .try_collect()
        .await?;
    let author = registered_user(&state, &[UserPermission::SubmitPlans]).await?;
    join(&state, &author.1, &acme).await?;
    let (team, plan_id) = created_campaign(
        &state,
        &author,
        &[("name", "Team"), ("owner", acme.as_str())],
    )
    .await?;
    created_campaign(&state, &author, &[("name", "Its author's own")]).await?;
    add_starved_snapshot(&state, &plan_id).await?;

    let (key, user_id, key_id) = &root;
    let mut answers = Vec::new();
    for uri in [
        "/api/v1/orgs/acme/overview".to_string(),
        format!("/api/v1/orgs/{acme}/overview"),
    ] {
        let resp = authed_user_get(&state, key, user_id, key_id, &uri).await?;
        assert_eq!(resp.status(), StatusCode::OK, "{uri}");
        answers.push(body_json(resp).await?);
    }
    let body = answers.first().context("an answer")?;
    assert_eq!(
        answers.last(),
        Some(body),
        "either spelling reads one overview"
    );
    crate::api_schema::assert_conforms("OrgOverview", body)?;

    assert_eq!(json_str(body, "org_id")?, acme);
    assert_eq!(
        body["member_status_counts"],
        json!({ "active": 1, "pending": 1 })
    );
    assert_eq!(body["pending_members"]["total"], 1, "{body}");
    assert_eq!(
        json_str(&body["pending_members"]["first"][0], "user_id")?,
        applicant.user_id.as_str()
    );
    assert_eq!(
        body["client_status_counts"],
        json!({ "pending": 6, "approved": 1 })
    );
    assert_eq!(body["pending_clients"]["total"], 6, "{body}");
    assert_eq!(
        body["pending_clients"]["first"].as_array().map(Vec::len),
        Some(5),
        "the first five: {body}"
    );
    assert!(!body.to_string().contains(theirs.as_str()), "{body}");
    assert_eq!(body["suspended_clients"]["total"], 1, "{body}");
    assert_eq!(
        json_str(&body["suspended_clients"]["first"][0], "client_id")?,
        stuck.as_str()
    );

    assert_eq!(body["campaigns"]["total"], 6, "acme's alone: {body}");
    let shown: Vec<&str> = body["campaigns"]["first"]
        .as_array()
        .context("a `first` array")?
        .iter()
        .filter_map(|row| row["campaign_id"].as_str())
        .collect();
    let newest: Vec<&str> = std::iter::once(team.as_str())
        .chain(older.iter().rev().take(4).map(String::as_str))
        .collect();
    assert_eq!(shown, newest, "the five newest, newest first: {body}");
    let row = &body["campaigns"]["first"][0];
    assert_eq!(json_str(row, "campaign_id")?, team);
    assert_eq!(json_str(row, "name")?, "Team");
    assert_eq!(json_str(row, "current_plan_id")?, plan_id.as_str());
    assert!(row["updated_at"].is_string(), "{row}");
    assert!(row["plan"]["status"].is_string(), "{row}");
    assert_eq!(row["plan"]["cancel_requested"], false, "{row}");
    assert_eq!(row["plan"]["counts"]["total"], 1, "{row}");
    let uri = format!("/api/v1/campaigns/{team}?plan=true");
    let resp = authed_user_get(&state, key, user_id, key_id, &uri).await?;
    assert_eq!(resp.status(), StatusCode::OK, "{uri}");
    assert_eq!(row, &body_json(resp).await?);
    assert_eq!(json_str(&row["plan"], "plan_id")?, plan_id.as_str());
    assert_eq!(body["vault_resource_count"], 1, "acme's alone: {body}");
    assert_eq!(body["benchmark_count"], 1, "acme's alone: {body}");
    assert_eq!(body["client_preauth_key_count"], 0, "{body}");
    assert_eq!(body["may_administer"], true, "{body}");
    assert_eq!(body["may_delete"], true, "{body}");
    Ok(())
}

/// As the page does. An org admin reads all of it, administers its organization
/// and does not delete it.
#[tokio::test]
async fn test_the_overview_leaves_out_what_the_reader_may_not_read() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let root = registered_user(&state, UserPermissionGroup::ServerAdmin.permissions()).await?;
    let acme = create_org(&state, &root, "acme").await?;
    let (_key, client_id) = register_and_approve(&state).await?;
    own_client_by_org(&state, &client_id, &OrgId::try_new(acme.clone())?).await?;
    let author = registered_user(&state, &[UserPermission::SubmitPlans]).await?;
    join(&state, &author.1, &acme).await?;
    let (team, plan_id) = created_campaign(
        &state,
        &author,
        &[("name", "Team"), ("owner", acme.as_str())],
    )
    .await?;

    let reader = registered_user(&state, &[UserPermission::ReadOrgs]).await?;
    join(&state, &reader.1, &acme).await?;
    let (key, user_id, key_id) = &reader;
    let resp = authed_user_get(&state, key, user_id, key_id, "/api/v1/orgs/acme/overview").await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    crate::api_schema::assert_conforms("OrgOverview", &body)?;
    assert_eq!(body["client_count"], 1, "the count stands: {body}");
    [
        "client_status_counts",
        "pending_clients",
        "suspended_clients",
    ]
    .into_iter()
    .for_each(|field| {
        assert_eq!(
            body.get(field),
            Some(&Value::Null),
            "{field} is null: {body}"
        )
    });
    assert!(
        !body.to_string().contains(client_id.as_str()),
        "a device leaked: {body}"
    );
    assert!(
        body["campaigns"].is_null(),
        "the campaign listing refuses it: {body}"
    );
    assert!(body["client_preauth_key_count"].is_null(), "{body}");
    assert_eq!(body["may_administer"], false, "{body}");
    assert_eq!(body["may_delete"], false, "{body}");

    let lead = registered_user(&state, UserPermissionGroup::OrgAdmin.permissions()).await?;
    join(&state, &lead.1, &acme).await?;
    let (key, user_id, key_id) = &lead;
    let resp = authed_user_get(&state, key, user_id, key_id, "/api/v1/orgs/acme/overview").await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    crate::api_schema::assert_conforms("OrgOverview", &body)?;
    assert_eq!(body["client_status_counts"], json!({ "approved": 1 }));
    // The campaign reads on membership, its plan with the `read_plans` an
    // org admin carries.
    let row = &body["campaigns"]["first"][0];
    assert_eq!(json_str(row, "campaign_id")?, team);
    assert_eq!(json_str(row, "current_plan_id")?, plan_id.as_str());
    assert!(row.get("plan").is_some(), "the plan is read: {row}");
    assert_eq!(body["client_preauth_key_count"], 0, "{body}");
    assert_eq!(body["may_administer"], true, "{body}");
    assert_eq!(body["may_delete"], false, "{body}");
    Ok(())
}

/// The vault count reaches as far as the vault does, the catalog's as far as
/// the roster read.
#[rstest]
#[case::member(&[UserPermission::ReadOrgs], true, 1, 1)]
#[case::org_admin(UserPermissionGroup::OrgAdmin.permissions(), true, 1, 1)]
#[case::roster_wide(&[UserPermission::ReadAnyOrgs], false, 0, 1)]
#[tokio::test]
async fn the_overview_counts_within_each_reach(
    #[case] permissions: &[UserPermission],
    #[case] joins: bool,
    #[case] vault_resource_count: u64,
    #[case] benchmark_count: u64,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let root = registered_user(&state, UserPermissionGroup::ServerAdmin.permissions()).await?;
    let acme = OrgId::try_new(create_org(&state, &root, "acme").await?)?;
    setup_private_benchmark(dir.path(), "eval_acme_internal_v3", &acme)?;
    put_org_resource(&state, &acme).await?;
    let reader = registered_user(&state, permissions).await?;
    if joins {
        join(&state, &reader.1, acme.as_str()).await?;
    }
    let (key, user_id, key_id) = &reader;
    let resp = authed_user_get(&state, key, user_id, key_id, "/api/v1/orgs/acme/overview").await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["vault_resource_count"], vault_resource_count, "{body}");
    assert_eq!(body["benchmark_count"], benchmark_count, "{body}");
    Ok(())
}

#[tokio::test]
async fn test_the_overview_is_gated_as_the_organization_is() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let manager = manager(&state).await?;
    let acme = create_org(&state, &manager, "acme").await?;
    let rival = create_org(&state, &manager, "rival").await?;
    let stranger = registered_user(&state, &[UserPermission::SubmitPlans]).await?;
    join(&state, &stranger.1, &acme).await?;
    let reader = registered_user(&state, &[UserPermission::ReadOrgs]).await?;
    join(&state, &reader.1, &acme).await?;

    let (key, user_id, key_id) = &stranger;
    let resp = authed_user_get(&state, key, user_id, key_id, "/api/v1/orgs/acme/overview").await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let (key, user_id, key_id) = &reader;
    for uri in [
        format!("/api/v1/orgs/{rival}/overview"),
        "/api/v1/orgs/rival/overview".to_string(),
        "/api/v1/orgs/nobody/overview".to_string(),
    ] {
        let resp = authed_user_get(&state, key, user_id, key_id, &uri).await?;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{uri}");
    }
    Ok(())
}
