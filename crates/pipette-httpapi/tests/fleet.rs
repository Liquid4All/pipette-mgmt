//! `GET /api/v1/clients`, `GET /api/v1/clients/{client_id}` and `GET /api/v1/clients/capabilities`:
//! the fleet, read by a user: who may read it and what a row carries.

use anyhow::Context;
use axum::http::StatusCode;
use ed25519_dalek::SigningKey;
use rstest::rstest;
use serde_json::{Value, json};

use pipette_core::types::Mint;
use pipette_httpapi::state::AppState;
use pipette_plan_types::{
    CampaignId, ClientId, ClientOwner, ClientPermission, ExpiresAt, NonEmptyTrimmedString, OrgId,
    Tag, UserId, UserKeyId, UserPermission,
};
use pipette_store::{CampaignBacklog, CampaignBacklogSnapshot};
use pipette_web_testkit::{
    approved_client, authed_get, authed_post, authed_user_get, body_json, filter_fleet, fleet_ids,
    job, put_idle_campaign, register_and_approve, registered_user, registered_user_with_detail,
    run_qm, seed_avail, submit_plan_as, unauthed_get,
};

fn row<'a>(body: &'a Value, client_id: &str) -> anyhow::Result<&'a Value> {
    body["items"]
        .as_array()
        .context("a listing carries an `items` array")?
        .iter()
        .find(|row| row["client_id"] == json!(client_id))
        .with_context(|| format!("the listing carries {client_id}"))
}

async fn read_conforming(
    state: &AppState,
    (key, user_id, key_id): &(SigningKey, UserId, UserKeyId),
    path: &str,
    schema: &str,
) -> anyhow::Result<Value> {
    let resp = authed_user_get(state, key, user_id, key_id, path).await?;
    anyhow::ensure!(resp.status() == StatusCode::OK, "{}", resp.status());
    let body = body_json(resp).await?;
    crate::api_schema::assert_conforms(schema, &body)?;
    Ok(body)
}

/// The server returns `400` when a client repeats a scalar key, even if one value is empty.
/// It also returns `400` for a filter value that does not parse.
#[rstest]
#[case::status("status=approved&status=pending")]
#[case::empty_status("status=&status=approved")]
#[case::limit("limit=1&limit=2")]
#[case::empty_limit("limit=&limit=1")]
#[case::cursor("cursor=first&cursor=second")]
#[case::empty_cursor("cursor=&cursor=first")]
#[case::tag("tag=bad/tag")]
#[case::empty_tag("tag=")]
#[case::noncanonical("requires=OS:iOS")]
#[case::empty_requires("requires=")]
#[case::empty_any_of("requires=os:ios&any_of=")]
#[case::empty_excludes("requires=os:ios&excludes=")]
#[case::empty_not_all_of("requires=os:ios&not_all_of=")]
#[case::whitespace("requires=+%09+")]
#[case::empty_repeat("requires=os:ios&requires=")]
#[case::noncanonical_group("requires=os:ios&any_of=gpu:A17Pro")]
#[case::invalid_limit("limit=abc")]
#[case::owned("owned=nobody")]
#[case::owned_repeat("owned=true&owned=false")]
#[case::owner_unknown("owner=org")]
#[case::owner_repeat("owner=mine&owner=mine")]
#[case::org_handle("org_id=acme")]
#[case::owned_and_owner("owned=true&owner=mine")]
#[case::owned_and_org("owned=false&org_id=org_00000000-0000-0000-0000-000000000000")]
#[case::owner_and_org("owner=mine&org_id=org_00000000-0000-0000-0000-000000000000")]
#[tokio::test]
async fn test_fleet_rejects_bad_query(#[case] query: &str) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, _client_id) = approved_client(dir.path()).await?;
    let (key, user_id, key_id) = registered_user(&state, &[UserPermission::ReadClients]).await?;
    let resp = authed_user_get(
        &state,
        &key,
        &user_id,
        &key_id,
        &format!("/api/v1/clients?{query}"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{query}");
    Ok(())
}

/// A published listing: narrowing it by default would quietly drop rows from a
/// caller that was reading them all.
#[rstest]
#[case::absent("", 2)]
#[case::cleared("?status=", 2)]
#[case::approved("?status=approved", 1)]
#[case::pending("?status=pending", 1)]
#[tokio::test]
async fn test_the_fleet_listing_filters_on_status(
    #[case] query: &str,
    #[case] expected: usize,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, approved) = approved_client(dir.path()).await?;
    let (_pending_key, pending) = pipette_web_testkit::register_pending(&state).await?;
    let (key, user_id, key_id) = registered_user(&state, &[UserPermission::ReadClients]).await?;

    let resp = authed_user_get(
        &state,
        &key,
        &user_id,
        &key_id,
        &format!("/api/v1/clients{query}"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK, "{query}");
    let body = body_json(resp).await?;

    let rows = body["items"]
        .as_array()
        .context("a listing carries an `items` array")?;
    assert_eq!(rows.len(), expected, "{query}");

    match query {
        "?status=approved" => {
            assert_eq!(rows[0]["client_id"], json!(approved.as_str()));
        }
        "?status=pending" => {
            assert_eq!(rows[0]["client_id"], json!(pending.as_str()));
        }
        _ => {}
    }
    Ok(())
}

/// A status nobody registers under is a `400`, as a bad `status` is on the plan
/// listing: answering with the whole fleet would answer a question nobody asked.
#[tokio::test]
async fn test_an_unknown_status_is_refused() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, _client_id) = approved_client(dir.path()).await?;
    let (key, user_id, key_id) = registered_user(&state, &[UserPermission::ReadClients]).await?;

    let resp = authed_user_get(
        &state,
        &key,
        &user_id,
        &key_id,
        "/api/v1/clients?status=decommissioned",
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    Ok(())
}

/// The error names the parameter at fault; a bad cursor is refused as a cursor,
/// not as the id it decodes to.
#[rstest]
#[case("/api/v1/clients?limit=0", "limit")]
#[case("/api/v1/clients?limit=201", "limit")]
#[case("/api/v1/clients?limit=-1", "limit")]
#[case("/api/v1/clients?cursor=bad.dot", "`cursor`")]
#[case("/api/v1/clients/capabilities?status=decommissioned", "status")]
#[case("/api/v1/plans?limit=0", "limit")]
#[case("/api/v1/plans?limit=201", "limit")]
#[case("/api/v1/plans?limit=-1", "limit")]
#[case("/api/v1/plans?cursor=bad.dot", "`cursor`")]
#[case("/api/v1/plans/plan-0/jobs?offset=-1", "offset")]
#[case("/api/v1/campaigns?limit=-1", "limit")]
#[case("/api/v1/campaigns?cursor=garbage", "`cursor`")]
#[case("/api/v1/vault/resources?limit=-1", "limit")]
#[case("/api/v1/vault/resources?cursor=garbage", "`cursor`")]
#[tokio::test]
async fn test_list_routes_refuse_a_bad_query_in_the_envelope(
    #[case] path: &str,
    #[case] names: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, _client_id) = approved_client(dir.path()).await?;
    let (key, user_id, key_id) = registered_user(
        &state,
        &[UserPermission::ReadClients, UserPermission::ReadPlans],
    )
    .await?;

    let resp = authed_user_get(&state, &key, &user_id, &key_id, path).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{path}");
    let body = body_json(resp).await?;
    let error = body["error"]
        .as_str()
        .context("a refusal carries an `error` string")?;
    assert!(error.contains(names), "{path}: {error}");
    Ok(())
}

/// A filtered cursor continues after the last examined key, including keys for
/// records that did not match the filter.
#[tokio::test]
async fn test_a_filtered_page_counts_matches_not_ids() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, first) = approved_client(dir.path()).await?;
    let (_second_key, second) = pipette_web_testkit::register_and_approve(&state).await?;
    // A pending machine between them in id order or not, it is never a match.
    let (_pending_key, _pending) = pipette_web_testkit::register_pending(&state).await?;
    let (key, user_id, key_id) = registered_user(&state, &[UserPermission::ReadClients]).await?;

    let mut approved = [first, second];
    approved.sort();

    let mut cursor = None;
    for id in &approved {
        let cursor_query = cursor
            .as_ref()
            .map_or_else(String::new, |cursor| format!("&cursor={cursor}"));
        let resp = authed_user_get(
            &state,
            &key,
            &user_id,
            &key_id,
            &format!("/api/v1/clients?status=approved&limit=1{cursor_query}"),
        )
        .await?;
        let body = body_json(resp).await?;
        let rows = body["items"]
            .as_array()
            .context("a listing carries an `items` array")?;
        assert_eq!(rows.len(), 1, "one match per page of one");
        assert_eq!(rows[0]["client_id"], json!(id.as_str()));
        cursor = body["next_cursor"].as_str().map(str::to_string);
    }
    Ok(())
}

#[tokio::test]
async fn test_the_listing_pages_ascending_by_client_id() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, first) = approved_client(dir.path()).await?;
    let (_second_key, second) = pipette_web_testkit::register_and_approve(&state).await?;
    let (key, user_id, key_id) = registered_user(&state, &[UserPermission::ReadClients]).await?;

    let mut expected = [first, second];
    expected.sort();

    let mut cursor = None;
    for id in &expected {
        let cursor_query = cursor
            .as_ref()
            .map_or_else(String::new, |cursor| format!("&cursor={cursor}"));
        let resp = authed_user_get(
            &state,
            &key,
            &user_id,
            &key_id,
            &format!("/api/v1/clients?limit=1{cursor_query}"),
        )
        .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await?;

        let rows = body["items"]
            .as_array()
            .context("a listing carries an `items` array")?;
        assert_eq!(rows.len(), 1, "one row per page of one");
        assert_eq!(rows[0]["client_id"], json!(id.as_str()));
        cursor = body["next_cursor"].as_str().map(str::to_string);
    }

    let resp = authed_user_get(
        &state,
        &key,
        &user_id,
        &key_id,
        "/api/v1/clients?cursor=zzzz",
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert!(
        body["items"]
            .as_array()
            .context("a listing carries an `items` array")?
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn test_the_listing_counts_an_unreadable_record_and_lists_the_rest() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, approved) = approved_client(dir.path()).await?;
    let (_pending_key, pending) = pipette_web_testkit::register_pending(&state).await?;
    std::fs::write(
        dir.path().join("clients").join(format!("{pending}.json")),
        b"{",
    )?;
    let (key, user_id, key_id) = registered_user(&state, &[UserPermission::ReadClients]).await?;

    let resp = authed_user_get(&state, &key, &user_id, &key_id, "/api/v1/clients").await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(fleet_ids(&body)?, [approved.as_str()]);
    assert_eq!(body["unreadable"], json!(1));
    Ok(())
}

/// Tags come from one fleet-wide marker listing, so a second tagged client
/// catches a join that leaks one row's markers into another or drops them.
#[tokio::test]
async fn test_a_listing_gives_each_row_its_own_tags() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, first) = approved_client(dir.path()).await?;
    let (_second_key, second) = pipette_web_testkit::register_and_approve(&state).await?;
    let (key, user_id, key_id) = registered_user(&state, &[UserPermission::ReadClients]).await?;

    state
        .tag_store
        .add_client_tag(&first, &Tag::try_new("lab")?)
        .await?;
    state
        .tag_store
        .add_client_tag(&second, &Tag::try_new("bench")?)
        .await?;
    state
        .tag_store
        .add_client_tag(&second, &Tag::try_new("lab")?)
        .await?;

    let resp = authed_user_get(&state, &key, &user_id, &key_id, "/api/v1/clients").await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;

    assert_eq!(row(&body, first.as_str())?["tags"], json!(["lab"]));
    // Sorted, because the view carries a `BTreeSet`.
    assert_eq!(
        row(&body, second.as_str())?["tags"],
        json!(["bench", "lab"])
    );
    Ok(())
}

/// A row carries the record, its tags, its resolved permissions, and the two
/// flags that say why an approved machine still takes no work.
#[tokio::test]
async fn test_list_fleet_carries_the_operator_view() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let (key, user_id, key_id) = registered_user(&state, &[UserPermission::ReadClients]).await?;

    state
        .tag_store
        .add_client_tag(&client_id, &Tag::try_new("lab")?)
        .await?;

    let resp = authed_user_get(&state, &key, &user_id, &key_id, "/api/v1/clients").await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;

    let client = row(&body, client_id.as_str())?;
    assert_eq!(client["status"], json!("approved"));
    assert_eq!(client["tags"], json!(["lab"]));
    // The **resolved** set, which is the standing an operator is looking for: a
    // machine holding `perform_work` is one the claim path admits.
    assert_eq!(
        client["permissions"],
        json!(["perform_work", "submit_results"])
    );
    assert_eq!(client["suspended"], json!(false));
    // The fixture registers the client, which queues a reindex.
    assert!(client["reindex_pending"].is_boolean());
    Ok(())
}

#[tokio::test]
async fn test_list_fleet_surfaces_why_a_client_takes_no_work() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let (key, user_id, key_id) = registered_user(&state, &[UserPermission::ReadClients]).await?;

    pipette_web_testkit::clear_pending_reindex(state.todo_store.as_ref(), &client_id).await?;
    state
        .todo_store
        .write_suspension(
            &client_id,
            chrono::Utc::now(),
            &pipette_web_testkit::job("job-stuck")?,
        )
        .await?;

    let resp = authed_user_get(&state, &key, &user_id, &key_id, "/api/v1/clients").await?;
    let body = body_json(resp).await?;
    let client = row(&body, client_id.as_str())?;
    assert_eq!(client["suspended"], json!(true));
    assert_eq!(client["reindex_pending"], json!(false));
    Ok(())
}

/// It includes `read_clients`, so the wider grant needs no narrower one too.
#[tokio::test]
async fn test_read_any_clients_admits() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, _client_id) = approved_client(dir.path()).await?;
    let (key, user_id, key_id) = registered_user(&state, &[UserPermission::ReadAnyClients]).await?;

    let resp = authed_user_get(&state, &key, &user_id, &key_id, "/api/v1/clients").await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        body_json(resp).await?["items"]
            .as_array()
            .context("a listing carries an `items` array")?
            .len(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn test_list_fleet_refuses_an_account_with_no_fleet_grant() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, _client_id) = approved_client(dir.path()).await?;
    let (key, user_id, key_id) = registered_user(&state, &[UserPermission::ReadAnyPlans]).await?;

    let resp = authed_user_get(&state, &key, &user_id, &key_id, "/api/v1/clients").await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert!(
        pipette_web_testkit::json_str(&body_json(resp).await?, "error")?.contains("read_clients"),
        "the refusal did not name the permission"
    );
    Ok(())
}

/// At the extractor: `read_clients` is a user's right, and a device reads its
/// own record at `/clients/me`.
#[tokio::test]
async fn test_list_fleet_refuses_a_client() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;
    let resp = authed_get(&state, &key, &client_id, "/api/v1/clients").await?;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    Ok(())
}

/// Unauthenticated reads nothing: only the catalog is public.
#[tokio::test]
async fn test_list_fleet_refuses_an_unauthenticated_caller() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, _client_id) = approved_client(dir.path()).await?;
    assert_eq!(
        unauthed_get(&state, "/api/v1/clients").await?.status(),
        StatusCode::UNAUTHORIZED
    );
    Ok(())
}

/// Permissions resolved through the implication closure, not echoed as stored.
#[tokio::test]
async fn test_get_fleet_client() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let (key, user_id, key_id) = registered_user(&state, &[UserPermission::ReadClients]).await?;

    state
        .client_store
        .put_client_permissions(
            &client_id,
            &[ClientPermission::PerformWork].into_iter().collect(),
        )
        .await?;

    let resp = authed_user_get(
        &state,
        &key,
        &user_id,
        &key_id,
        &format!("/api/v1/clients/{client_id}"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["client_id"], json!(client_id.as_str()));
    assert_eq!(body["permissions"], json!(["perform_work"]));
    assert_eq!(body["tags"], json!([]));
    Ok(())
}

/// The static segment wins over `{client_id}`; the routes take different
/// principals, so a collision would `401` the route the whole fleet calls.
#[tokio::test]
async fn test_clients_me_still_routes_to_the_device_view() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;
    let resp = authed_get(&state, &key, &client_id, "/clients/me").await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        body_json(resp).await?["client_id"],
        json!(client_id.as_str())
    );
    Ok(())
}

/// Worded for scope: "out of reach" and "does not exist" are one answer.
#[tokio::test]
async fn test_get_unknown_fleet_client_is_a_404() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, _client_id) = approved_client(dir.path()).await?;
    let (key, user_id, key_id) = registered_user(&state, &[UserPermission::ReadClients]).await?;

    let resp = authed_user_get(
        &state,
        &key,
        &user_id,
        &key_id,
        "/api/v1/clients/ev1_0000000000000000000000000000000000000000000000000000000000000000",
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    Ok(())
}

/// Refused before any record is read: a real, an absent and an unparseable id
/// all get the same `403`, never a `404` or a `500`.
#[tokio::test]
async fn test_get_fleet_client_refuses_an_account_with_no_fleet_grant() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let (_broken_key, broken) = pipette_web_testkit::register_and_approve(&state).await?;
    std::fs::write(
        dir.path().join("clients").join(format!("{broken}.json")),
        b"{",
    )?;
    let (key, user_id, key_id) = registered_user(&state, &[UserPermission::ReadAnyPlans]).await?;

    for client in [
        client_id.as_str(),
        "ev1_0000000000000000000000000000000000000000000000000000000000000000",
        broken.as_str(),
    ] {
        let path = format!("/api/v1/clients/{client}");
        let resp = authed_user_get(&state, &key, &user_id, &key_id, &path).await?;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{path}");
        assert_eq!(
            body_json(resp).await?,
            json!({"error": "user lacks the read_clients permission"}),
            "{path}"
        );
    }
    Ok(())
}

/// Not a `404` from a lookup under that id.
#[tokio::test]
async fn test_get_fleet_client_refuses_an_id_outside_the_charset() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, _client_id) = approved_client(dir.path()).await?;
    let (key, user_id, key_id) = registered_user(&state, &[UserPermission::ReadClients]).await?;

    let resp = authed_user_get(&state, &key, &user_id, &key_id, "/api/v1/clients/a.b").await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        body_json(resp).await?,
        json!({"error": "client id contains '.'; allowed: [A-Za-z0-9_-]"})
    );
    Ok(())
}

/// A stored record that does not parse is a redacted `500`, not a `404` or a
/// partial row: the cause stays in the log.
#[tokio::test]
async fn test_get_fleet_client_with_an_unreadable_record_is_a_redacted_500() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    std::fs::write(
        dir.path().join("clients").join(format!("{client_id}.json")),
        b"{",
    )?;
    let (key, user_id, key_id) = registered_user(&state, &[UserPermission::ReadClients]).await?;

    let resp = authed_user_get(
        &state,
        &key,
        &user_id,
        &key_id,
        &format!("/api/v1/clients/{client_id}"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        body_json(resp).await?,
        json!({"error": "internal server error"})
    );
    Ok(())
}

/// Two clients in opposite states catch a swap of the flags or one client's
/// marker read as another's.
#[tokio::test]
async fn test_get_fleet_client_says_why_it_takes_no_work() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, suspended) = approved_client(dir.path()).await?;
    let (_second_key, reindexing) = pipette_web_testkit::register_and_approve(&state).await?;
    let (key, user_id, key_id) = registered_user(&state, &[UserPermission::ReadClients]).await?;

    pipette_web_testkit::clear_pending_reindex(state.todo_store.as_ref(), &suspended).await?;
    state
        .todo_store
        .write_suspension(
            &suspended,
            chrono::Utc::now(),
            &pipette_web_testkit::job("job-stuck")?,
        )
        .await?;
    state.todo_store.write_pending_reindex(&reindexing).await?;

    for (client_id, is_suspended, is_reindexing) in
        [(&suspended, true, false), (&reindexing, false, true)]
    {
        let resp = authed_user_get(
            &state,
            &key,
            &user_id,
            &key_id,
            &format!("/api/v1/clients/{client_id}"),
        )
        .await?;
        assert_eq!(resp.status(), StatusCode::OK, "{client_id}");
        let body = body_json(resp).await?;
        assert_eq!(body["suspended"], json!(is_suspended), "{client_id}");
        assert_eq!(body["reindex_pending"], json!(is_reindexing), "{client_id}");
    }
    Ok(())
}

/// Even a request the route then refuses, from a pending client; only the
/// signer's time moves.
#[rstest]
#[case::approved(false, StatusCode::NO_CONTENT)]
#[case::pending(true, StatusCode::FORBIDDEN)]
#[tokio::test]
async fn test_a_signed_request_sets_last_seen_at(
    #[case] pending: bool,
    #[case] claim_status: StatusCode,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, approved_key, approved) = approved_client(dir.path()).await?;
    let (pending_key, pending_id) = pipette_web_testkit::register_pending(&state).await?;
    let user = registered_user(&state, &[UserPermission::ReadClients]).await?;
    let (signer_key, signer, other) = if pending {
        (&pending_key, &pending_id, &approved)
    } else {
        (&approved_key, &approved, &pending_id)
    };

    let listing = || {
        read_conforming(
            &state,
            &user,
            "/api/v1/clients?status=",
            "ClientListRowPage",
        )
    };
    let before = listing().await?;
    assert_eq!(row(&before, signer.as_str())?["last_seen_at"], Value::Null);

    let resp = authed_post(&state, signer_key, signer, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), claim_status);

    let after = listing().await?;
    assert!(row(&after, signer.as_str())?["last_seen_at"].is_string());
    assert_eq!(row(&after, other.as_str())?["last_seen_at"], Value::Null);

    let record = read_conforming(
        &state,
        &user,
        &format!("/api/v1/clients/{signer}"),
        "ClientDetail",
    )
    .await?;
    assert_eq!(
        record["last_seen_at"],
        row(&after, signer.as_str())?["last_seen_at"]
    );
    Ok(())
}

/// A device the count does not name reads zero; before the first run, `null`.
#[tokio::test]
async fn test_a_maintenance_run_sets_the_backlog() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _, listed) = approved_client(dir.path()).await?;
    let (_, unlisted) = register_and_approve(&state).await?;
    let user = registered_user(&state, &[UserPermission::ReadClients]).await?;
    let listing = || {
        read_conforming(
            &state,
            &user,
            "/api/v1/clients?status=",
            "ClientListRowPage",
        )
    };

    let before = listing().await?;
    assert_eq!(row(&before, listed.as_str())?["backlog"], Value::Null);
    let record_path = format!("/api/v1/clients/{listed}");
    let record = || read_conforming(&state, &user, &record_path, "ClientDetail");
    assert_eq!(record().await?["backlog_by_campaign"], Value::Null);

    seed_avail(
        dir.path(),
        "job-1",
        ExpiresAt::Never,
        &json!({"clients": [listed.as_str()]}),
    )?;
    run_qm(&state).await?;

    let after = listing().await?;
    let backlog = &row(&after, listed.as_str())?["backlog"];
    assert_eq!(backlog["job_count"], json!(1));
    assert!(backlog["counted_at"].is_string());
    assert_eq!(
        row(&after, unlisted.as_str())?["backlog"]["job_count"],
        json!(0)
    );

    let record = record().await?;
    assert_eq!(&record["backlog"], backlog);
    // No plan holds the seeded job.
    assert_eq!(
        record["backlog_by_campaign"],
        json!({
            "campaigns": [],
            "outside_campaign_job_count": 1,
            "unreadable_job_count": 0,
            "counted_at": backlog["counted_at"],
        })
    );
    Ok(())
}

/// A maintenance run counts a queued job of a campaign's plan in that campaign.
#[tokio::test]
async fn test_a_maintenance_run_counts_the_backlog_by_campaign() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _, client) = approved_client(dir.path()).await?;
    run_qm(&state).await?;
    let (key, user_id, key_id) = registered_user_with_detail(&state, true).await?;
    let plan_id = submit_plan_as(&state, &key, &user_id, &key_id, &client, "Queued", 1).await?;
    let campaign_id = state
        .plan_store
        .get_plan(&plan_id)
        .await?
        .context("the filed plan exists")?
        .campaign_id
        .context("the filed plan names its campaign")?;
    run_qm(&state).await?;

    let record = read_conforming(
        &state,
        &(key, user_id, key_id),
        &format!("/api/v1/clients/{client}"),
        "ClientDetail",
    )
    .await?;
    assert_eq!(record["backlog"]["job_count"], json!(1));
    let backlog = &record["backlog_by_campaign"];
    assert_eq!(
        backlog["campaigns"],
        json!([{"campaign_id": campaign_id.as_str(), "name": "Queued", "job_count": 1}])
    );
    assert_eq!(backlog["outside_campaign_job_count"], json!(0));
    assert_eq!(backlog["unreadable_job_count"], json!(0));
    Ok(())
}

/// The server sums each campaign that is outside the reach of the reader, or
/// that has no record, into `unreadable`. A reader with no campaign grant has
/// every campaign in that sum.
#[rstest]
#[case::campaign_reader(&[UserPermission::ReadClients, UserPermission::ReadPlans], &["Mine"], 4)]
#[case::org_admin_without_an_org(&[UserPermission::ReadClients, UserPermission::ManageOrgs], &["Mine"], 4)]
#[case::any_plan_reader(&[UserPermission::ReadClients, UserPermission::ReadAnyPlans], &["Theirs", "Mine"], 1)]
#[case::fleet_reader_only(&[UserPermission::ReadClients], &[], 6)]
#[tokio::test]
async fn test_the_backlog_by_campaign_shows_the_readable_campaigns(
    #[case] permissions: &[UserPermission],
    #[case] readable: &[&str],
    #[case] unreadable: u64,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _, client) = approved_client(dir.path()).await?;
    let user = registered_user(&state, permissions).await?;
    let (_, other, _) = registered_user(&state, &[UserPermission::ReadPlans]).await?;
    let own = put_idle_campaign(&state, &user.1, "Mine").await?;
    let theirs = put_idle_campaign(&state, &other, "Theirs").await?;
    let deleted = CampaignId::mint();
    let counted_at = chrono::DateTime::from_timestamp(1_800_000_000, 0).context("timestamp")?;
    state
        .todo_store
        .write_campaign_backlog(&CampaignBacklogSnapshot {
            counted_at,
            devices: [(
                client.clone(),
                CampaignBacklog {
                    by_campaign: [(own.clone(), 2), (theirs.clone(), 3), (deleted, 1)]
                        .into_iter()
                        .collect(),
                    outside_campaigns: 4,
                },
            )]
            .into_iter()
            .collect(),
        })
        .await?;

    let record = read_conforming(
        &state,
        &user,
        &format!("/api/v1/clients/{client}"),
        "ClientDetail",
    )
    .await?;
    let campaigns: Vec<Value> = readable
        .iter()
        .map(|name| match *name {
            "Mine" => json!({"campaign_id": own.as_str(), "name": "Mine", "job_count": 2}),
            _ => json!({"campaign_id": theirs.as_str(), "name": "Theirs", "job_count": 3}),
        })
        .collect();
    assert_eq!(
        record["backlog_by_campaign"],
        json!({
            "campaigns": campaigns,
            "outside_campaign_job_count": 4,
            "unreadable_job_count": unreadable,
            "counted_at": "2027-01-15T08:00:00Z",
        })
    );
    Ok(())
}

/// The count does not check suspension (`docs/queue.md`, "Device backlog").
#[tokio::test]
async fn test_the_backlog_counts_a_suspended_device() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _, client) = approved_client(dir.path()).await?;
    let user = registered_user(&state, &[UserPermission::ReadClients]).await?;
    seed_avail(
        dir.path(),
        "job-1",
        ExpiresAt::Never,
        &json!({"clients": [client.as_str()]}),
    )?;
    state
        .todo_store
        .write_suspension(&client, chrono::Utc::now(), &job("job-stuck")?)
        .await?;
    run_qm(&state).await?;

    let listing = read_conforming(
        &state,
        &user,
        "/api/v1/clients?status=",
        "ClientListRowPage",
    )
    .await?;
    assert_eq!(
        row(&listing, client.as_str())?["backlog"]["job_count"],
        json!(1)
    );
    Ok(())
}

/// Read leniently: both reads still answer `200`. A directory at the snapshot's
/// path fails the read.
#[tokio::test]
async fn test_an_unreadable_backlog_shows_no_backlog() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _, client) = approved_client(dir.path()).await?;
    let user = registered_user(&state, &[UserPermission::ReadClients]).await?;
    std::fs::create_dir_all(dir.path().join("todo").join(".backlog"))?;
    std::fs::create_dir_all(dir.path().join("todo").join(".backlog-campaigns"))?;

    let listing = read_conforming(
        &state,
        &user,
        "/api/v1/clients?status=",
        "ClientListRowPage",
    )
    .await?;
    assert_eq!(row(&listing, client.as_str())?["backlog"], Value::Null);

    let record = read_conforming(
        &state,
        &user,
        &format!("/api/v1/clients/{client}"),
        "ClientDetail",
    )
    .await?;
    assert_eq!(record["backlog"], Value::Null);
    assert_eq!(record["backlog_by_campaign"], Value::Null);
    Ok(())
}

/// After a failed stamp the request is still served, and the retry waits too.
#[rstest]
#[case::after_a_stamp(false)]
#[case::after_a_failed_stamp(true)]
#[tokio::test]
async fn test_a_second_request_within_the_wait_does_not_stamp(
    #[case] fail_first: bool,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, client_key, client) = approved_client(dir.path()).await?;
    let (key, user_id, key_id) = registered_user(&state, &[UserPermission::ReadClients]).await?;
    let last_seen_at = || {
        let (state, key, user_id, key_id) =
            (state.clone(), key.clone(), user_id.clone(), key_id.clone());
        let client = client.clone();
        async move {
            let resp =
                authed_user_get(&state, &key, &user_id, &key_id, "/api/v1/clients?status=").await?;
            anyhow::ensure!(resp.status() == StatusCode::OK, "{}", resp.status());
            anyhow::Ok(row(&body_json(resp).await?, client.as_str())?["last_seen_at"].clone())
        }
    };
    // The stamp writes into `liveness/` without creating it, so a missing
    // directory fails the write.
    let liveness_dir = dir.path().join("todo").join("liveness");

    if fail_first {
        std::fs::remove_dir_all(&liveness_dir)?;
    }
    let resp = authed_post(&state, &client_key, &client, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let first = last_seen_at().await?;
    assert_eq!(first.is_null(), fail_first);

    if fail_first {
        std::fs::create_dir_all(&liveness_dir)?;
    }
    let resp = authed_post(&state, &client_key, &client, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert_eq!(last_seen_at().await?, first);
    Ok(())
}

/// What one reader may do to a device, and the organization it rolls up to.
#[derive(Debug, PartialEq)]
struct Reach {
    may_administer: bool,
    owning_org_id: Option<String>,
}

async fn reach_of(
    state: &AppState,
    signer: &pipette_web_testkit::UserSigner,
    client_id: &ClientId,
) -> anyhow::Result<Reach> {
    let (key, user_id, key_id) = signer;
    let resp = authed_user_get(
        state,
        key,
        user_id,
        key_id,
        &format!("/api/v1/clients/{client_id}"),
    )
    .await?;
    anyhow::ensure!(resp.status() == StatusCode::OK, "{}", resp.status());
    let body = body_json(resp).await?;
    crate::api_schema::assert_conforms("ClientDetail", &body)?;
    let flag = |field: &str| {
        body.get(field)
            .and_then(Value::as_bool)
            .context("a flag is always there")
    };
    Ok(Reach {
        may_administer: flag("may_administer")?,
        owning_org_id: body
            .get("owning_org_id")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

fn reach(may_administer: bool, owning_org_id: Option<&OrgId>) -> Reach {
    Reach {
        may_administer,
        owning_org_id: owning_org_id.map(ToString::to_string),
    }
}

/// The forms follow the device scope; the owning organization is named only to
/// a reader of that roster or that organization's own device administrator.
#[tokio::test]
async fn test_the_single_read_says_what_the_reader_may_do() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, ours) = approved_client(dir.path()).await?;
    let acme = pipette_web_testkit::seed_org(&state, "acme").await?.org_id;
    let rival = pipette_web_testkit::seed_org(&state, "rival").await?.org_id;
    let (_key, members) = pipette_web_testkit::register_and_approve(&state).await?;
    let (_key, theirs) = pipette_web_testkit::register_and_approve(&state).await?;
    let (_key, unowned) = pipette_web_testkit::register_pending(&state).await?;

    let member = registered_user(&state, &[]).await?;
    pipette_web_testkit::set_user_org(&state, &member.1, Some(acme.clone())).await?;
    pipette_web_testkit::own_client_by_org(&state, &ours, &acme).await?;
    pipette_web_testkit::own_client(
        &state,
        &members,
        ClientOwner::User {
            user_id: member.1.clone(),
        },
    )
    .await?;
    pipette_web_testkit::own_client_by_org(&state, &theirs, &rival).await?;

    // acme's device administrator, who reads its roster too.
    let lead = registered_user(
        &state,
        &[UserPermission::ManageClients, UserPermission::ReadOrgs],
    )
    .await?;
    pipette_web_testkit::set_user_org(&state, &lead.1, Some(acme.clone())).await?;
    for (client_id, expected) in [
        (&ours, reach(true, Some(&acme))),
        (&members, reach(true, Some(&acme))),
        (&theirs, reach(false, None)),
        // A server admin's alone.
        (&unowned, reach(false, None)),
    ] {
        assert_eq!(
            reach_of(&state, &lead, client_id).await?,
            expected,
            "{client_id}"
        );
    }

    // Scoped to acme without its roster: the rollup is what its forms reach.
    let scoped = registered_user(&state, &[UserPermission::ManageClients]).await?;
    pipette_web_testkit::set_user_org(&state, &scoped.1, Some(acme.clone())).await?;
    assert_eq!(
        reach_of(&state, &scoped, &members).await?,
        reach(true, Some(&acme))
    );

    // The fleet's administrator without the roster grant learns no membership.
    let fleet = registered_user(&state, &[UserPermission::ManageAnyClients]).await?;
    assert_eq!(reach_of(&state, &fleet, &members).await?, reach(true, None));

    // A reader of every roster is told it, and may do nothing.
    let roster = registered_user(
        &state,
        &[UserPermission::ReadClients, UserPermission::ReadAnyOrgs],
    )
    .await?;
    for (client_id, owning) in [
        (&members, Some(&acme)),
        (&theirs, Some(&rival)),
        (&unowned, None),
    ] {
        assert_eq!(
            reach_of(&state, &roster, client_id).await?,
            reach(false, owning),
            "{client_id}"
        );
    }

    // The fleet grant alone reads the device and nothing past it.
    let reader = registered_user(&state, &[UserPermission::ReadClients]).await?;
    assert_eq!(
        reach_of(&state, &reader, &members).await?,
        reach(false, None)
    );
    Ok(())
}

/// Status is checked before permissions.
#[tokio::test]
async fn test_a_pending_account_reads_no_fleet() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let (key, user_id, key_id) = registered_user(&state, &[UserPermission::ReadAnyClients]).await?;
    pipette_web_testkit::set_user_status(&state, &user_id, pipette_plan_types::UserStatus::Pending)
        .await?;

    for path in [
        "/api/v1/clients".to_string(),
        format!("/api/v1/clients/{client_id}"),
    ] {
        let resp = authed_user_get(&state, &key, &user_id, &key_id, &path).await?;
        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "{path} admitted a pending account"
        );
    }
    Ok(())
}

/// Writes the record directly: registration refuses reported flags in reserved
/// namespaces, so the profile supplies `os:ios`.
async fn describe(
    state: &AppState,
    client_id: &ClientId,
    os: Option<&str>,
    capabilities: &[&str],
) -> anyhow::Result<()> {
    let mut client = state
        .client_store
        .get_client(client_id)
        .await?
        .context("the client registered")?;
    client.device_profile.device_os_name = os.map(NonEmptyTrimmedString::try_new).transpose()?;
    client.capabilities = capabilities.iter().map(|flag| flag.to_string()).collect();
    state.client_store.put_client(&client).await?;
    Ok(())
}

/// Every flag the vocabulary names, namespaced and free-form alike, ascending.
fn vocabulary_flags(body: &Value) -> anyhow::Result<Vec<String>> {
    let mut flags: Vec<String> = body["namespaces"]
        .as_array()
        .context("a vocabulary carries a `namespaces` array")?
        .iter()
        .flat_map(|namespace| namespace["flags"].as_array().into_iter().flatten())
        .chain(body["free_form"].as_array().into_iter().flatten())
        .filter_map(|flag| flag["flag"].as_str().map(str::to_string))
        .collect();
    flags.sort();
    Ok(flags)
}

/// A fleet of three: two approved machines that share one flag, and a pending
/// one whose flag nobody else reports.
async fn describe_fleet(state: &AppState, approved: &ClientId) -> anyhow::Result<()> {
    describe(
        state,
        approved,
        Some("iOS"),
        &["runtime:llama_cpp", "experimental"],
    )
    .await?;
    let (_second_key, second) = pipette_web_testkit::register_and_approve(state).await?;
    describe(state, &second, None, &["runtime:llama_cpp"]).await?;
    let (_pending_key, pending) = pipette_web_testkit::register_pending(state).await?;
    describe(state, &pending, None, &["runtime:onnx"]).await
}

/// Grouped by the prefix before the first `:`, flags without one listed apart.
#[rstest]
#[case::read_clients(UserPermission::ReadClients)]
#[case::read_any_clients(UserPermission::ReadAnyClients)]
#[tokio::test]
async fn test_the_capability_vocabulary_groups_and_counts(
    #[case] permission: UserPermission,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, approved) = approved_client(dir.path()).await?;
    describe_fleet(&state, &approved).await?;
    let (key, user_id, key_id) = registered_user(&state, &[permission]).await?;

    let resp = authed_user_get(
        &state,
        &key,
        &user_id,
        &key_id,
        "/api/v1/clients/capabilities",
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);

    assert_eq!(
        body_json(resp).await?,
        json!({
            "namespaces": [
                {
                    "namespace": "os",
                    "source": "derived",
                    "flags": [{"flag": "os:ios", "client_count": 1}]
                },
                {
                    "namespace": "runtime",
                    "source": "declared",
                    "flags": [
                        {"flag": "runtime:llama_cpp", "client_count": 2},
                        {"flag": "runtime:onnx", "client_count": 1}
                    ]
                }
            ],
            "free_form": [{"flag": "experimental", "client_count": 1}],
            "client_count": 3,
            "unreadable_client_count": 0
        })
    );
    Ok(())
}

#[rstest]
#[case::absent("", 3, &["experimental", "os:ios", "runtime:llama_cpp", "runtime:onnx"])]
#[case::cleared("?status=", 3, &["experimental", "os:ios", "runtime:llama_cpp", "runtime:onnx"])]
#[case::approved("?status=approved", 2, &["experimental", "os:ios", "runtime:llama_cpp"])]
#[case::pending("?status=pending", 1, &["runtime:onnx"])]
#[tokio::test]
async fn test_the_capability_vocabulary_filters_on_status(
    #[case] query: &str,
    #[case] clients: usize,
    #[case] expected: &[&str],
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, approved) = approved_client(dir.path()).await?;
    describe_fleet(&state, &approved).await?;
    let (key, user_id, key_id) = registered_user(&state, &[UserPermission::ReadClients]).await?;

    let resp = authed_user_get(
        &state,
        &key,
        &user_id,
        &key_id,
        &format!("/api/v1/clients/capabilities{query}"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK, "{query}");
    let body = body_json(resp).await?;

    assert_eq!(body["client_count"], json!(clients), "{query}");
    assert_eq!(vocabulary_flags(&body)?, expected, "{query}");
    Ok(())
}

#[rstest]
#[case::absent("", 1)]
#[case::cleared("?status=", 1)]
#[case::approved("?status=approved", 1)]
#[case::pending("?status=pending", 0)]
#[tokio::test]
async fn test_the_capability_vocabulary_reports_unreadable_records(
    #[case] query: &str,
    #[case] expected_clients: usize,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, approved) = approved_client(dir.path()).await?;
    describe(&state, &approved, None, &["runtime:llama_cpp"]).await?;
    let (_pending_key, pending) = pipette_web_testkit::register_pending(&state).await?;
    describe(&state, &pending, None, &["runtime:onnx"]).await?;
    std::fs::write(
        dir.path().join("clients").join(format!("{pending}.json")),
        b"{",
    )?;
    let (key, user_id, key_id) = registered_user(&state, &[UserPermission::ReadClients]).await?;

    let resp = authed_user_get(
        &state,
        &key,
        &user_id,
        &key_id,
        &format!("/api/v1/clients/capabilities{query}"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let namespaces = if expected_clients == 0 {
        json!([])
    } else {
        json!([{
            "namespace": "runtime",
            "source": "declared",
            "flags": [{"flag": "runtime:llama_cpp", "client_count": 1}]
        }])
    };
    assert_eq!(
        body_json(resp).await?,
        json!({
            "namespaces": namespaces,
            "free_form": [],
            "client_count": expected_clients,
            "unreadable_client_count": 1
        })
    );
    Ok(())
}

/// Flag counts disclose the fleet, as client lists do.
#[tokio::test]
async fn test_the_capability_vocabulary_refuses_an_account_with_no_fleet_grant()
-> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, _client_id) = approved_client(dir.path()).await?;
    let (key, user_id, key_id) = registered_user(&state, &[UserPermission::ReadAnyPlans]).await?;

    let resp = authed_user_get(
        &state,
        &key,
        &user_id,
        &key_id,
        "/api/v1/clients/capabilities",
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert!(
        pipette_web_testkit::json_str(&body_json(resp).await?, "error")?.contains("read_clients"),
        "the refusal did not name the permission"
    );
    Ok(())
}

#[tokio::test]
async fn test_the_capability_vocabulary_refuses_a_client_and_an_anonymous_caller()
-> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;

    let resp = authed_get(&state, &key, &client_id, "/api/v1/clients/capabilities").await?;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        unauthed_get(&state, "/api/v1/clients/capabilities")
            .await?
            .status(),
        StatusCode::UNAUTHORIZED
    );
    Ok(())
}

#[tokio::test]
async fn test_a_pending_account_reads_no_capability_vocabulary() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, _client_id) = approved_client(dir.path()).await?;
    let (key, user_id, key_id) = registered_user(&state, &[UserPermission::ReadAnyClients]).await?;
    pipette_web_testkit::set_user_status(&state, &user_id, pipette_plan_types::UserStatus::Pending)
        .await?;

    let resp = authed_user_get(
        &state,
        &key,
        &user_id,
        &key_id,
        "/api/v1/clients/capabilities",
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    Ok(())
}

/// Every clause narrows the same effective capability set that the scheduler tests.
/// Repeated groups stay separate, and a capability query needs `requires` to admit a client.
#[rstest]
#[case::one_tag("tag=lab", &["c-a", "c-b", "c-c"])]
#[case::two_tags("tag=lab&tag=us-east", &["c-a", "c-c"])]
#[case::unknown_tag("tag=unknown", &[])]
#[case::requires("requires=os:ios", &["c-a", "c-c"])]
#[case::repeated_requires("requires=os:ios&requires=runtime:llama_cpp", &["c-a"])]
#[case::whitespace_requires("requires=os:ios%09runtime:llama_cpp", &["c-a"])]
#[case::comma_flag("requires=device:iphone14,2", &["c-a", "c-b", "c-c"])]
#[case::any_of("requires=os:ios&any_of=gpu:a17pro+gpu:a18pro", &["c-a", "c-c"])]
#[case::repeated_any_of("requires=os:ios&any_of=gpu:a17pro+gpu:a18pro&any_of=runtime:llama_cpp", &["c-a"])]
#[case::excludes("requires=os:ios&excludes=gpu:a17pro", &["c-c"])]
#[case::repeated_excludes("requires=os:ios&excludes=gpu:a17pro&excludes=gpu:a18pro", &[])]
#[case::whitespace_excludes("requires=os:ios&excludes=gpu:a17pro+gpu:a18pro", &[])]
#[case::not_all_of_whole("requires=os:ios&not_all_of=os:ios+gpu:a17pro", &["c-c"])]
#[case::not_all_of_partial("requires=os:ios&not_all_of=os:ios+gpu:a19pro", &["c-a", "c-c"])]
#[case::repeated_not_all_of("requires=os:ios&not_all_of=os:ios+gpu:a17pro&not_all_of=os:ios+gpu:a18pro", &[])]
#[case::tag_and_capability("tag=lab&requires=os:android", &["c-b"])]
// A clause reads a client's tags as `tag:` flags, as a job's clauses do.
#[case::requires_a_tag_flag("requires=tag:us-east", &["c-a", "c-c"])]
#[case::excludes_a_tag_flag("requires=tag:lab&excludes=tag:us-east", &["c-b"])]
#[case::status_tag_and_capability("status=pending&tag=lab&requires=os:ios", &["c-c"])]
#[case::any_of_alone("any_of=gpu:a17pro", &[])]
#[case::excludes_alone("excludes=gpu:a17pro", &[])]
#[case::not_all_of_alone("not_all_of=os:ios+gpu:a17pro", &[])]
#[case::empty_scalars("status=&limit=&cursor=", &["c-a", "c-b", "c-c", "c-d"])]
#[case::blank_scalars("status=%20&limit=%205&cursor=%20", &["c-a", "c-b", "c-c", "c-d"])]
#[case::unknown_keys("unknown=value&unknown=another", &["c-a", "c-b", "c-c", "c-d"])]
#[tokio::test]
async fn test_fleet_filters(#[case] query: &str, #[case] expected: &[&str]) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, user_id, key_id) = filter_fleet(dir.path()).await?;
    let resp = authed_user_get(
        &state,
        &key,
        &user_id,
        &key_id,
        &format!("/api/v1/clients?{query}"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK, "{query}");
    let body = body_json(resp).await?;
    assert_eq!(fleet_ids(&body)?, expected, "{query}");
    assert_eq!(body["next_cursor"], Value::Null);
    Ok(())
}

#[rstest]
#[case::profile("c-a", &["device:iphone14,2", "gpu:a17pro", "os:ios"], "runtime:llama_cpp")]
#[case::no_profile("c-d", &[], "runtime:other")]
#[tokio::test]
async fn test_fleet_derived_capabilities(
    #[case] client_id: &str,
    #[case] derived: &[&str],
    #[case] declared: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, user_id, key_id) = filter_fleet(dir.path()).await?;
    for path in [
        "/api/v1/clients".to_string(),
        format!("/api/v1/clients/{client_id}"),
    ] {
        let resp = authed_user_get(&state, &key, &user_id, &key_id, &path).await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await?;
        let client = if path == "/api/v1/clients" {
            row(&body, client_id)?
        } else {
            &body
        };
        assert_eq!(client["derived_capabilities"], json!(derived));
        assert_eq!(client["capabilities"], json!([declared]));
    }
    Ok(())
}

/// Both routes carry the stored record, its key included, but its address only
/// for the device's administrators (`docs/authentication.md`,
/// `client_contact.rs`).
#[tokio::test]
async fn test_fleet_row_carries_record_but_the_address() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, user_id, key_id) = filter_fleet(dir.path()).await?;
    for path in ["/api/v1/clients", "/api/v1/clients/c-a"] {
        let resp = authed_user_get(&state, &key, &user_id, &key_id, path).await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await?;
        let client = if path == "/api/v1/clients" {
            row(&body, "c-a")?
        } else {
            &body
        };
        assert!(client.get("contact_email").is_none(), "{client}");
        assert_eq!(client["public_key"], json!(format!("{:064x}", 0)));
    }
    Ok(())
}

#[tokio::test]
async fn test_fleet_filters_page_to_exhaustion() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, user_id, key_id) = filter_fleet(dir.path()).await?;
    let mut cursor = None;
    for expected in [&["c-a"][..], &["c-c"][..], &[][..]] {
        let cursor_query = cursor
            .as_ref()
            .map_or_else(String::new, |cursor| format!("&cursor={cursor}"));
        let resp = authed_user_get(
            &state,
            &key,
            &user_id,
            &key_id,
            &format!("/api/v1/clients?tag=lab&tag=us-east&requires=os:ios&limit=1{cursor_query}"),
        )
        .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await?;
        assert_eq!(fleet_ids(&body)?, expected);
        cursor = body["next_cursor"].as_str().map(str::to_string);
        assert_eq!(cursor.is_none(), expected.is_empty());
    }
    Ok(())
}
