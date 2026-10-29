//! The plan read surface: `GET /api/v1/plans`, `GET /api/v1/plans/{plan_id}`,
//! `GET /api/v1/plans/{plan_id}/jobs`, and who may read a written plan back. A
//! plan belongs to its campaign's owner, or to its `submitted_by` outside every
//! campaign.

use anyhow::Context;
use axum::http::StatusCode;
use chrono::{Duration, SubsecRound, Utc};
use futures::TryStreamExt;
use rstest::rstest;
use serde_json::{Value, json};

use pipette_core::plan::{ExpiredJob, sample_manifest};
use pipette_core::todo_filename::{leased_key, parse_avail_filename};
use pipette_core::types::Mint;
use pipette_httpapi::state::AppState;
use pipette_plan_types::{
    CampaignId, ClientId, ExpiresAt, JobId, PlanId, PlanStatus, RuntimeOutput, UserId,
    UserPermission,
};
use pipette_web_testkit::{
    UserSigner, add_starved_snapshot, approved_client, authed_get, authed_user_get, body_json,
    grant_plan_user_detail, grant_user, plan_ids, put_loose_plan, registered_user,
    registered_user_with_detail, submit_plan_as, unauthed_get,
};

// ── fixtures ────────────────────────────────────────────────────────────────

/// Write a plan outside every campaign, then clear its `submitted_by`. The
/// result stands for a plan that a client filed. No route files such a plan.
async fn put_unattributed_plan(state: &AppState, client_id: &ClientId) -> anyhow::Result<PlanId> {
    let (key, user_id, key_id) = registered_user(state, &[UserPermission::SubmitPlans]).await?;
    let plan_id =
        put_loose_plan(state, &key, &user_id, &key_id, client_id, "device-filed", 1).await?;
    let mut manifest = state
        .plan_store
        .get_plan(&plan_id)
        .await?
        .context("manifest written")?;
    manifest.submitted_by = None;
    state.plan_store.put_plan(&manifest).await?;
    Ok(plan_id)
}

/// File a one-job plan as `signer`, into its own campaign or, if `loose`,
/// outside every campaign.
async fn file_as(
    state: &AppState,
    signer: &UserSigner,
    client_id: &ClientId,
    note: &str,
    loose: bool,
) -> anyhow::Result<PlanId> {
    let (key, user_id, key_id) = signer;
    if loose {
        put_loose_plan(state, key, user_id, key_id, client_id, note, 1).await
    } else {
        submit_plan_as(state, key, user_id, key_id, client_id, note, 1).await
    }
}

/// The job ids a plan's manifest lists, which the jobs route pages over.
async fn manifest_job_ids(state: &AppState, plan_id: &PlanId) -> anyhow::Result<Vec<JobId>> {
    Ok(state
        .plan_store
        .get_plan(plan_id)
        .await?
        .context("manifest written")?
        .job_ids)
}

fn job_row<'a>(body: &'a Value, job_id: &JobId) -> anyhow::Result<&'a Value> {
    body["jobs"]
        .as_array()
        .context("a jobs page carries a `jobs` array")?
        .iter()
        .find(|row| row["job_id"] == json!(job_id.as_str()))
        .with_context(|| format!("the page carries {job_id}"))
}

// ── GET /api/v1/plans: scope ───────────────────────────────────────────────────────

/// `read_plans` alone sees the plans the account filed. The two plans differ
/// only in who filed them, so the listing's contents are the scope.
#[rstest]
#[case::in_a_campaign(false)]
#[case::loose(true)]
#[tokio::test]
async fn test_list_plans_scopes_to_the_callers_own(#[case] loose: bool) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let mine_signer = registered_user(&state, &[UserPermission::SubmitPlans]).await?;
    let theirs_signer = registered_user(&state, &[UserPermission::SubmitPlans]).await?;

    let mine = file_as(&state, &mine_signer, &client_id, "mine", loose).await?;
    let theirs = file_as(&state, &theirs_signer, &client_id, "theirs", loose).await?;

    let (mine_key, mine_id, mine_key_id) = &mine_signer;
    let resp = authed_user_get(&state, mine_key, mine_id, mine_key_id, "/api/v1/plans").await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;

    assert_eq!(plan_ids(&body)?, vec![mine.to_string()]);
    assert!(
        !plan_ids(&body)?.contains(&theirs.to_string()),
        "an own-scoped listing carried another account's plan"
    );
    Ok(())
}

/// The second plan records no `submitted_by`, so it belongs to no account and
/// only this grant reaches it.
#[rstest]
#[case::by_default("/api/v1/plans")]
#[case::scope_all("/api/v1/plans?scope=all")]
#[tokio::test]
async fn test_list_plans_any_sees_every_plan(#[case] uri: &str) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _client_key, client_id) = approved_client(dir.path()).await?;
    let (mine_key, mine_id, mine_key_id) =
        registered_user(&state, &[UserPermission::SubmitPlans]).await?;
    let (reader_key, reader_id, reader_key_id) =
        registered_user(&state, &[UserPermission::ReadAnyPlans]).await?;

    let theirs = submit_plan_as(
        &state,
        &mine_key,
        &mine_id,
        &mine_key_id,
        &client_id,
        "mine",
        1,
    )
    .await?;
    let unattributed = put_unattributed_plan(&state, &client_id).await?;

    let resp = authed_user_get(&state, &reader_key, &reader_id, &reader_key_id, uri).await?;
    assert_eq!(resp.status(), StatusCode::OK, "{uri}");
    let body = body_json(resp).await?;
    let ids = plan_ids(&body)?;
    assert_eq!(ids.len(), 2, "{uri}: {body}");
    assert!(
        ids.contains(&theirs.to_string()),
        "{uri} dropped another account's plan"
    );
    assert!(
        ids.contains(&unattributed.to_string()),
        "{uri} dropped the plan nobody filed"
    );
    Ok(())
}

/// "The plans the caller submitted" is not satisfied by an absent submitter.
#[tokio::test]
async fn test_own_scope_hides_a_plan_with_no_submitter() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _client_key, client_id) = approved_client(dir.path()).await?;
    let (reader_key, reader_id, reader_key_id) =
        registered_user(&state, &[UserPermission::ReadPlans]).await?;

    put_unattributed_plan(&state, &client_id).await?;

    let resp = authed_user_get(
        &state,
        &reader_key,
        &reader_id,
        &reader_key_id,
        "/api/v1/plans",
    )
    .await?;
    let body = body_json(resp).await?;
    assert!(plan_ids(&body)?.is_empty());
    Ok(())
}

/// `read_any_plans` narrows to its own plans by asking for `scope=mine`, so the
/// widest grant is not a listing a holder is stuck with.
#[tokio::test]
async fn test_scope_mine_narrows_a_holder_of_read_any() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let (other_key, other_id, other_key_id) =
        registered_user(&state, &[UserPermission::SubmitPlans]).await?;
    let (key, user_id, key_id) = registered_user(
        &state,
        &[UserPermission::SubmitPlans, UserPermission::ReadAnyPlans],
    )
    .await?;

    submit_plan_as(&state, &key, &user_id, &key_id, &client_id, "mine", 1).await?;
    submit_plan_as(
        &state,
        &other_key,
        &other_id,
        &other_key_id,
        &client_id,
        "theirs",
        1,
    )
    .await?;

    let resp = authed_user_get(&state, &key, &user_id, &key_id, "/api/v1/plans").await?;
    assert_eq!(plan_ids(&body_json(resp).await?)?.len(), 2);

    let resp = authed_user_get(&state, &key, &user_id, &key_id, "/api/v1/plans?scope=mine").await?;
    let body = body_json(resp).await?;
    assert_eq!(plan_ids(&body)?.len(), 1);
    Ok(())
}

/// Asking for a scope wider than the grant is a `403` naming what is missing,
/// not a silently narrowed listing: a caller that asked for the fleet's plans
/// must not read a subset as the answer.
#[tokio::test]
async fn test_scope_all_without_read_any_is_refused() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, _client_id) = approved_client(dir.path()).await?;
    let (key, user_id, key_id) = registered_user(&state, &[UserPermission::ReadPlans]).await?;

    let resp = authed_user_get(&state, &key, &user_id, &key_id, "/api/v1/plans?scope=all").await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let body = body_json(resp).await?;
    assert!(
        pipette_web_testkit::json_str(&body, "error")?.contains("read_any_plans"),
        "the refusal did not name the permission: {body}"
    );
    Ok(())
}

#[tokio::test]
async fn test_list_plans_refuses_an_account_with_no_read_grant() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, _client_id) = approved_client(dir.path()).await?;
    let (key, user_id, key_id) = registered_user(&state, &[]).await?;

    let resp = authed_user_get(&state, &key, &user_id, &key_id, "/api/v1/plans").await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    Ok(())
}

/// `read_plans` is grantable to users alone; the refusal is a `401` because the
/// route parses `X-Client-Id` as a user id.
#[tokio::test]
async fn test_list_plans_refuses_a_client() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;
    let resp = authed_get(&state, &key, &client_id, "/api/v1/plans").await?;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    Ok(())
}

/// An unauthenticated read is refused: only the catalog is public.
#[tokio::test]
async fn test_list_plans_refuses_an_unauthenticated_caller() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, _client_id) = approved_client(dir.path()).await?;
    let resp = unauthed_get(&state, "/api/v1/plans").await?;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    Ok(())
}

// ── GET /api/v1/plans: filtering and paging ────────────────────────────────────────

/// `status` narrows the listing, and an unrecognized value is a `400` rather
/// than an unfiltered listing that answers a question nobody asked.
#[tokio::test]
async fn test_list_plans_filters_on_status() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let (key, user_id, key_id) = registered_user(
        &state,
        &[UserPermission::SubmitPlans, UserPermission::ReadPlans],
    )
    .await?;
    submit_plan_as(&state, &key, &user_id, &key_id, &client_id, "p", 1).await?;

    let resp = authed_user_get(
        &state,
        &key,
        &user_id,
        &key_id,
        "/api/v1/plans?status=active",
    )
    .await?;
    assert_eq!(plan_ids(&body_json(resp).await?)?.len(), 1);

    let resp = authed_user_get(
        &state,
        &key,
        &user_id,
        &key_id,
        "/api/v1/plans?status=complete",
    )
    .await?;
    assert!(plan_ids(&body_json(resp).await?)?.is_empty());

    let resp = authed_user_get(
        &state,
        &key,
        &user_id,
        &key_id,
        "/api/v1/plans?status=nonsense",
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    Ok(())
}

/// `submitted_by` narrows inside the scope rather than widening or refusing it,
/// so naming an account the scope does not reach is an empty page, not a `403`,
/// and a value that is not a user id is a `400`. So is the former name
/// `submitter`, rather than an unfiltered listing.
#[tokio::test]
async fn test_list_plans_filters_on_submitter_within_the_scope() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let (a_key, a_id, a_key_id) = registered_user(&state, &[UserPermission::SubmitPlans]).await?;
    // `submit_plans` implies `read_plans` alone, so `b` reads at an own scope.
    let (b_key, b_id, b_key_id) = registered_user(&state, &[UserPermission::SubmitPlans]).await?;
    let (any_key, any_id, any_key_id) =
        registered_user(&state, &[UserPermission::ReadAnyPlans]).await?;

    let a_plan = submit_plan_as(&state, &a_key, &a_id, &a_key_id, &client_id, "a", 1).await?;
    let b_plan = submit_plan_as(&state, &b_key, &b_id, &b_key_id, &client_id, "b", 1).await?;
    put_unattributed_plan(&state, &client_id).await?;
    let named_a = format!("/api/v1/plans?submitted_by={a_id}");

    // The widest scope narrows to one account, and the plan nobody filed
    // matches no value.
    let resp = authed_user_get(&state, &any_key, &any_id, &any_key_id, &named_a).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(plan_ids(&body_json(resp).await?)?, vec![a_plan.to_string()]);

    // An own scope naming another account reads nothing, and is not refused.
    let resp = authed_user_get(&state, &b_key, &b_id, &b_key_id, &named_a).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert!(
        plan_ids(&body)?.is_empty(),
        "an own scope read another account's plan through the filter: {body}"
    );
    // Naming itself, the same caller reads its own plan, so the empty page
    // above is the filter's answer and not the scope's.
    let resp = authed_user_get(
        &state,
        &b_key,
        &b_id,
        &b_key_id,
        &format!("/api/v1/plans?submitted_by={b_id}"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(plan_ids(&body_json(resp).await?)?, vec![b_plan.to_string()]);

    let resp = authed_user_get(
        &state,
        &any_key,
        &any_id,
        &any_key_id,
        "/api/v1/plans?submitted_by=ev1_x",
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await?;
    let error = pipette_web_testkit::json_str(&body, "error")?;
    assert!(
        error.contains("submitted_by") && error.contains("must start with `user_`"),
        "the refusal did not name the parameter and the prefix it lacks: {body}"
    );

    let resp = authed_user_get(
        &state,
        &any_key,
        &any_id,
        &any_key_id,
        &format!("/api/v1/plans?submitter={a_id}"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await?;
    let error = pipette_web_testkit::json_str(&body, "error")?;
    assert!(
        error.contains("unknown field `submitter`"),
        "the refusal did not name the unknown parameter: {body}"
    );
    Ok(())
}

/// Paging returns a cursor while more plans remain, and a cursor past the end
/// produces an empty page rather than an error.
#[tokio::test]
async fn test_list_plans_pages() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let (key, user_id, key_id) = registered_user(
        &state,
        &[UserPermission::SubmitPlans, UserPermission::ReadPlans],
    )
    .await?;
    for i in 0..3 {
        submit_plan_as(
            &state,
            &key,
            &user_id,
            &key_id,
            &client_id,
            &format!("plan-{i}"),
            1,
        )
        .await?;
    }

    let resp = authed_user_get(&state, &key, &user_id, &key_id, "/api/v1/plans?limit=2").await?;
    let first = body_json(resp).await?;
    assert_eq!(plan_ids(&first)?.len(), 2);
    let cursor = first["next_cursor"]
        .as_str()
        .context("the first page carries a continuation cursor")?;

    let resp = authed_user_get(
        &state,
        &key,
        &user_id,
        &key_id,
        &format!("/api/v1/plans?limit=2&cursor={cursor}"),
    )
    .await?;
    let second = body_json(resp).await?;
    assert_eq!(plan_ids(&second)?.len(), 1);
    assert!(second["next_cursor"].is_null());
    // The two pages partition the listing: no plan appears on both.
    for id in plan_ids(&second)? {
        assert!(
            !plan_ids(&first)?.contains(&id),
            "{id} appeared on two pages"
        );
    }

    let resp = authed_user_get(
        &state,
        &key,
        &user_id,
        &key_id,
        "/api/v1/plans?limit=2&cursor=zzzz",
    )
    .await?;
    assert!(plan_ids(&body_json(resp).await?)?.is_empty());
    Ok(())
}

/// A scoped listing over a store holding mostly plans outside the scope still
/// serves full pages, in either order, and its cursors resume without skipping
/// one of the caller's plans.
#[rstest]
#[case::oldest("")]
#[case::newest("&order=newest")]
#[tokio::test]
async fn test_a_scoped_listing_pages_past_the_plans_outside_it(
    #[case] order: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, _client_id) = approved_client(dir.path()).await?;
    let (key, user_id, key_id) = registered_user(&state, &[UserPermission::ReadPlans]).await?;
    let other = UserId::mint();
    // A campaign that no longer exists, so its plans are nobody's.
    let gone = CampaignId::mint();
    let mut expected = Vec::new();
    for n in 0..40 {
        let plan_id = PlanId::try_new(format!("plan-{n:03}"))?;
        let (campaign, submitter) = match n % 5 {
            0 => (None, Some(&user_id)),
            1 | 4 => (None, Some(&other)),
            2 => (Some(&gone), Some(&user_id)),
            _ => (None, None),
        };
        let mut manifest = sample_manifest(plan_id.clone(), PlanStatus::Active, false)?;
        manifest.campaign_id = campaign.cloned();
        manifest.submitted_by = submitter.cloned();
        state.plan_store.put_plan(&manifest).await?;
        if n % 5 == 0 {
            expected.push(plan_id.to_string());
        }
    }
    if !order.is_empty() {
        expected.reverse();
    }

    let mut listed = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let after = cursor.map(|c| format!("&cursor={c}")).unwrap_or_default();
        let uri = format!("/api/v1/plans?limit=3{order}{after}");
        let resp = authed_user_get(&state, &key, &user_id, &key_id, &uri).await?;
        assert_eq!(resp.status(), StatusCode::OK, "{uri}");
        let page = body_json(resp).await?;
        let ids = plan_ids(&page)?;
        cursor = page["next_cursor"].as_str().map(str::to_owned);
        if let Some(cursor) = &cursor {
            assert_eq!(ids.len(), 3, "{uri}: a page with a cursor is full");
            assert_eq!(
                ids.last(),
                Some(cursor),
                "{uri}: the cursor is its last row"
            );
        }
        listed.extend(ids);
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(listed, expected);
    Ok(())
}

// ── GET /api/v1/plans: what a row carries ──────────────────────────────────────────

/// A `plans/` object that is not a manifest is counted in `unreadable` and the
/// readable plans still list: one bad object must neither fail the listing nor
/// read as "no such plan".
#[tokio::test]
async fn test_list_plans_counts_an_unreadable_manifest_and_lists_the_rest() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let (key, user_id, key_id) = registered_user(
        &state,
        &[UserPermission::SubmitPlans, UserPermission::ReadPlans],
    )
    .await?;
    let kept = submit_plan_as(&state, &key, &user_id, &key_id, &client_id, "kept", 1).await?;
    let garbled = submit_plan_as(&state, &key, &user_id, &key_id, &client_id, "garbled", 1).await?;
    // Bad JSON rather than a directory: an IO failure fails the listing.
    let path = dir.path().join("plans").join(format!("{garbled}.json"));
    anyhow::ensure!(path.is_file(), "{} exists", path.display());
    std::fs::write(&path, b"not json")?;

    let resp = authed_user_get(&state, &key, &user_id, &key_id, "/api/v1/plans").await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["unreadable"], json!(1), "{body}");
    assert_eq!(plan_ids(&body)?, vec![kept.to_string()]);
    Ok(())
}

/// A listing row reports a pending cancel marker, as the detail does, and only
/// on the plan it names: a row that dropped it would show a cancelled plan as
/// untouched until the maintenance pass latches it.
#[tokio::test]
async fn test_list_plans_reports_a_pending_cancel_on_its_row() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let (key, user_id, key_id) = registered_user(
        &state,
        &[UserPermission::SubmitPlans, UserPermission::ReadPlans],
    )
    .await?;
    let doomed = submit_plan_as(&state, &key, &user_id, &key_id, &client_id, "doomed", 1).await?;
    let spared = submit_plan_as(&state, &key, &user_id, &key_id, &client_id, "spared", 1).await?;
    state.plan_store.write_cancel_marker(&doomed).await?;

    let resp = authed_user_get(&state, &key, &user_id, &key_id, "/api/v1/plans").await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    let items = body["items"]
        .as_array()
        .context("a listing carries `items`")?;
    let row = |plan_id: &PlanId| {
        items
            .iter()
            .find(|row| row["plan_id"] == json!(plan_id.as_ref()))
            .with_context(|| format!("the listing carries {plan_id}"))
    };

    let doomed_row = row(&doomed)?;
    assert_eq!(doomed_row["cancel_requested"], json!(true));
    // The marker only *requests* teardown; the status is still the manifest's.
    assert_eq!(doomed_row["status"], json!("active"));
    assert_eq!(row(&spared)?["cancel_requested"], json!(false));
    Ok(())
}

// ── GET /api/v1/plans/{plan_id} ─────────────────────────────────────────────

/// The detail route serves the manifest's client-facing projection, and
/// withholds the two fields that projection drops.
#[tokio::test]
async fn test_get_plan_serves_the_status_view() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let (key, user_id, key_id) = registered_user(
        &state,
        &[UserPermission::SubmitPlans, UserPermission::ReadPlans],
    )
    .await?;
    let plan_id = submit_plan_as(&state, &key, &user_id, &key_id, &client_id, "named", 2).await?;

    let resp = authed_user_get(
        &state,
        &key,
        &user_id,
        &key_id,
        &format!("/api/v1/plans/{plan_id}"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;

    assert_eq!(body["plan_id"], json!(plan_id.as_ref()));
    assert_eq!(body["submitted_by"], json!(user_id.as_str()));
    assert_eq!(body["cancel_requested"], json!(false));
    // `null`, not absent: "not computed yet" is a different answer from a field
    // the schema lacks.
    assert!(body.get("progress_snapshot").is_some());
    assert_eq!(body["progress_snapshot"], Value::Null);
    assert_eq!(body["terminal_at"], Value::Null);
    // The two the projection drops.
    assert!(body.get("job_ids").is_none(), "detail leaked job_ids");
    assert!(
        body.get("expired_jobs").is_none(),
        "detail leaked expired_jobs"
    );
    Ok(())
}

/// The detail carries the frozen `job_expiry` range, and a campaign's revision
/// names its campaign and the plan it supersedes.
#[tokio::test]
async fn test_get_plan_carries_the_expiry_range_and_the_campaign_chain() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let signer = registered_user(
        &state,
        &[UserPermission::SubmitPlans, UserPermission::ReadPlans],
    )
    .await?;
    let (key, user_id, key_id) = &signer;
    let status_of = |plan_id: &PlanId| {
        let uri = format!("/api/v1/plans/{plan_id}");
        let state = state.clone();
        async move {
            let resp = authed_user_get(&state, key, user_id, key_id, &uri).await?;
            anyhow::ensure!(resp.status() == StatusCode::OK, "{uri}: {}", resp.status());
            let body = body_json(resp).await?;
            crate::api_schema::assert_conforms("PlanView", &body)?;
            Ok(body)
        }
    };

    // A plan from before campaigns: an expiry range, and no campaign to name.
    let one_off = put_loose_plan(&state, key, user_id, key_id, &client_id, "one-off", 1).await?;
    let stored = state
        .plan_store
        .get_plan(&one_off)
        .await?
        .context("manifest written")?;
    let body = status_of(&one_off).await?;
    let expiry = stored.job_expiry.context("an expiry range")?;
    assert_eq!(
        body["job_expiry"],
        json!({
            "earliest_expires_at": expiry.earliest.instant(),
            "latest_expires_at": expiry.latest.instant(),
        }),
        "{body}"
    );
    assert!(body.get("campaign_id").is_none(), "{body}");
    assert!(body.get("previous_plan_id").is_none(), "{body}");

    // A campaign's first plan names the campaign and supersedes nothing.
    let (campaign_id, first) =
        pipette_web_testkit::created_campaign(&state, &signer, &[("name", "Nightly")]).await?;
    let body = status_of(&first).await?;
    assert_eq!(body["campaign_id"], json!(campaign_id), "{body}");
    assert!(body.get("previous_plan_id").is_none(), "{body}");

    // Its revision names both.
    let resp = pipette_web_testkit::post_plan_document(
        &state,
        &signer,
        &pipette_web_testkit::filing_uri(&campaign_id, "rev-2")?,
        pipette_web_testkit::ONE_CELL_PLAN,
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let second = PlanId::try_new(pipette_web_testkit::json_str(
        &body_json(resp).await?,
        "plan_id",
    )?)?;
    let body = status_of(&second).await?;
    assert_eq!(body["campaign_id"], json!(campaign_id), "{body}");
    assert_eq!(body["previous_plan_id"], json!(first.as_ref()), "{body}");
    Ok(())
}

/// A plan outside the caller's scope reads as **absent**, not forbidden: a `403`
/// would confirm that the id names a real plan. The plan is another account's
/// through its campaign, or through its `submitted_by` outside every campaign.
#[rstest]
#[case::in_a_campaign(false)]
#[case::loose(true)]
#[tokio::test]
async fn test_get_plan_outside_the_scope_is_a_404(#[case] loose: bool) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let owner = registered_user(&state, &[UserPermission::SubmitPlans]).await?;
    let (key, user_id, key_id) = registered_user(&state, &[UserPermission::ReadPlans]).await?;

    let plan_id = file_as(&state, &owner, &client_id, "theirs", loose).await?;

    let resp = authed_user_get(
        &state,
        &key,
        &user_id,
        &key_id,
        &format!("/api/v1/plans/{plan_id}"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // And a plan that does not exist reads the same way, so the two are
    // indistinguishable to a caller.
    let absent = PlanId::from_uuid(uuid::Uuid::now_v7());
    let resp = authed_user_get(
        &state,
        &key,
        &user_id,
        &key_id,
        &format!("/api/v1/plans/{absent}"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    Ok(())
}

/// A cancel marker shows on the detail before the maintenance pass latches it,
/// which is the window that would otherwise look like a dropped cancel.
#[tokio::test]
async fn test_get_plan_surfaces_a_pending_cancel() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let (key, user_id, key_id) = registered_user(
        &state,
        &[UserPermission::SubmitPlans, UserPermission::ReadPlans],
    )
    .await?;
    let plan_id = submit_plan_as(&state, &key, &user_id, &key_id, &client_id, "doomed", 1).await?;
    state.plan_store.write_cancel_marker(&plan_id).await?;

    let resp = authed_user_get(
        &state,
        &key,
        &user_id,
        &key_id,
        &format!("/api/v1/plans/{plan_id}"),
    )
    .await?;
    let body = body_json(resp).await?;
    assert_eq!(body["cancel_requested"], json!(true));
    // The marker only *requests* teardown; the status is still what the
    // manifest holds.
    assert_eq!(body["status"], json!("active"));
    Ok(())
}

// ── GET /api/v1/plans/{plan_id}/jobs ──────────────────────────────────────────

/// A freshly staged plan's jobs are `available`, carry the deadline the
/// `avail/` key encodes, and hold no lease.
#[tokio::test]
async fn test_list_jobs_reports_queued_jobs_as_available() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let (key, user_id, key_id) = registered_user(
        &state,
        &[UserPermission::SubmitPlans, UserPermission::ReadPlans],
    )
    .await?;
    let plan_id = submit_plan_as(&state, &key, &user_id, &key_id, &client_id, "p", 2).await?;

    let resp = authed_user_get(
        &state,
        &key,
        &user_id,
        &key_id,
        &format!("/api/v1/plans/{plan_id}/jobs"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;

    assert_eq!(body["plan_id"], json!(plan_id.as_ref()));
    assert_eq!(body["total"], json!(2));
    for job in body["jobs"].as_array().context("a `jobs` array")? {
        assert_eq!(job["state"], json!("available"));
        assert_eq!(job["queue"]["lease"], Value::Null);
        assert_eq!(job["plan"]["lapsed"], Value::Null);
        assert!(
            !job["queue"]["expires_at"].is_null(),
            "a queued job carries its deadline"
        );
    }
    Ok(())
}

/// One test rather than five: the states are decided by one ordered chain in
/// the handler, so only seeding all five against one manifest holds the order.
#[rstest]
#[case::withheld(None, "withheld")]
#[case::full(Some(UserPermission::ReadClients), "full")]
#[case::read_any_clients(Some(UserPermission::ReadAnyClients), "full")]
#[tokio::test]
async fn test_list_jobs_reports_every_lifecycle_state(
    #[case] permission: Option<UserPermission>,
    #[case] tier: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let (key, user_id, key_id) = registered_user(
        &state,
        &[UserPermission::SubmitPlans, UserPermission::ReadPlans],
    )
    .await?;
    grant_plan_user_detail(&state, &user_id, permission).await?;
    let plan_id = submit_plan_as(&state, &key, &user_id, &key_id, &client_id, "p", 5).await?;
    let jobs = manifest_job_ids(&state, &plan_id).await?;
    let (available, running, finished, failed, cancelled) =
        (&jobs[0], &jobs[1], &jobs[2], &jobs[3], &jobs[4]);

    // `running`: rename its `avail/` entry into a live lease, as `claim` does.
    // The deadline comes off the key rather than being assumed: ingestion
    // stamps a default expiry, and the rename is addressed by it.
    let lease_expiry = Utc::now() + Duration::hours(1);
    let avail_key = state
        .todo_store
        .avail_key_for_job(running)
        .await?
        .context("the job is queued")?;
    let (_, expires_at) = parse_avail_filename(&avail_key)?;
    state
        .todo_store
        .claim_job(running, expires_at, &client_id, lease_expiry)
        .await?;

    // `finished` / `failed`: out of the queue, holding a record of each kind.
    for (job_id, message_type) in [(finished, "success"), (failed, "failure")] {
        state.todo_store.delete_avail_by_job(job_id).await?;
        state
            .submission_store
            .write_incoming(
                job_id,
                &json!({"job_id": job_id.as_str(), "message_type": message_type}),
            )
            .await?;
    }

    // `cancelled`: out of the queue and holding no record, which is what
    // teardown leaves.
    state.todo_store.delete_avail_by_job(cancelled).await?;

    let resp = authed_user_get(
        &state,
        &key,
        &user_id,
        &key_id,
        &format!("/api/v1/plans/{plan_id}/jobs"),
    )
    .await?;
    let body = body_json(resp).await?;
    crate::api_schema::assert_conforms("NamedPlanJobs", &body)?;

    assert_eq!(job_row(&body, available)?["state"], json!("available"));
    assert_eq!(job_row(&body, finished)?["state"], json!("finished"));
    assert_eq!(job_row(&body, failed)?["state"], json!("failed"));
    assert_eq!(job_row(&body, cancelled)?["state"], json!("cancelled"));

    let leased = job_row(&body, running)?;
    assert_eq!(leased["state"], json!("running"));
    assert_eq!(body["fleet_detail"], tier);
    // The lease times the job at either tier; only the full tier names its
    // holder.
    let lease = &leased["queue"]["lease"];
    assert!(lease["expires_at"].is_string());
    if tier == "full" {
        assert_eq!(lease["client_id"], json!(client_id.as_str()));
    } else {
        assert_eq!(lease["client_id"], json!(null));
    }

    // Every state names the spec the job was filed with, at either tier: off the
    // queued body while it is queued, off its stored source once it left.
    for job_id in &jobs {
        let planned = &job_row(&body, job_id)?["planned"];
        assert_eq!(planned["benchmark"], json!("eval_test"), "{body}");
        assert_eq!(planned["model"]["repo_name"], json!("LFM2-700M-GGUF"));
        assert_eq!(
            planned["runtime"]["type"],
            json!("llamacpp_cli_stock_tools")
        );
        assert_eq!(planned["runtime_flags"], Value::Null, "{body}");
    }
    assert_eq!(job_row(&body, cancelled)?["queue"], Value::Null, "{body}");
    assert_eq!(job_row(&body, finished)?["queue"], Value::Null, "{body}");
    Ok(())
}

/// A job that left the queue with no stored source names no request, and a
/// source that does not parse, or whose spec this build does not read, costs
/// its row the request rather than the page.
#[tokio::test]
async fn test_a_job_without_a_readable_source_names_no_request() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let (key, user_id, key_id) = registered_user(
        &state,
        &[UserPermission::SubmitPlans, UserPermission::ReadPlans],
    )
    .await?;
    let plan_id = submit_plan_as(&state, &key, &user_id, &key_id, &client_id, "p", 3).await?;
    let jobs = manifest_job_ids(&state, &plan_id).await?;
    let (missing, garbled, unread) = (&jobs[0], &jobs[1], &jobs[2]);
    for job_id in [missing, garbled, unread] {
        state.todo_store.delete_avail_by_job(job_id).await?;
    }
    pipette_store_testkit::local_fs_remove_job_source(dir.path(), missing)?;
    let source = |job_id: &JobId| {
        dir.path()
            .join("job-sources")
            .join(format!("{job_id}.json"))
    };
    std::fs::write(source(garbled), b"not json")?;
    std::fs::write(
        source(unread),
        serde_json::to_vec(
            &json!({"spec": {"benchmark": "eval_test", "model": {"type": "unheard_of"}}}),
        )?,
    )?;

    let resp = authed_user_get(
        &state,
        &key,
        &user_id,
        &key_id,
        &format!("/api/v1/plans/{plan_id}/jobs"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    crate::api_schema::assert_conforms("NamedPlanJobs", &body)?;
    for job_id in [missing, garbled, unread] {
        let row = job_row(&body, job_id)?;
        assert_eq!(row["state"], json!("cancelled"), "{body}");
        assert_eq!(row["planned"], Value::Null, "{body}");
    }
    Ok(())
}

/// Write a lease on `job_id` held by `client` until `expiry`, as
/// [`pipette_web_testkit::seed_leased`] does for a live one.
fn seed_lease(
    dir: &std::path::Path,
    job_id: &JobId,
    client: &str,
    expiry: chrono::DateTime<Utc>,
    body: &Value,
) -> anyhow::Result<()> {
    let client = ClientId::try_new(client)?;
    let path =
        dir.join("todo")
            .join("leased")
            .join(leased_key(job_id, ExpiresAt::Never, &client, expiry));
    std::fs::create_dir_all(path.parent().context("a lease path has a parent")?)?;
    std::fs::write(path, serde_json::to_vec(body)?)?;
    Ok(())
}

/// A lease past its expiry still reads `running`, flagged `lapsed`, until the
/// maintenance pass recycles it; beside a live lease for the same job, the live
/// one answers.
#[tokio::test]
async fn test_a_lapsed_lease_reads_running_until_a_live_one_answers() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let (key, user_id, key_id) = registered_user(
        &state,
        &[UserPermission::SubmitPlans, UserPermission::ReadPlans],
    )
    .await?;
    let plan_id = submit_plan_as(&state, &key, &user_id, &key_id, &client_id, "p", 1).await?;
    let jobs = manifest_job_ids(&state, &plan_id).await?;
    let job_id = &jobs[0];
    let job_body = state
        .todo_store
        .get_avail_by_job(job_id)
        .await?
        .context("the job is queued")?;
    state.todo_store.delete_avail_by_job(job_id).await?;
    // Lapsed leases under fixed names on both sides of the live one: the listing
    // is unsorted, so a lone lapsed lease would catch a seen-order bug by luck.
    let lapsed = Utc::now() - Duration::minutes(1);
    for client in [
        "ev1_a", "ev1_b", "ev1_c", "ev1_d", "ev1_w", "ev1_x", "ev1_y", "ev1_z",
    ] {
        seed_lease(dir.path(), job_id, client, lapsed, &job_body)?;
    }
    let uri = format!("/api/v1/plans/{plan_id}/jobs");

    let resp = authed_user_get(&state, &key, &user_id, &key_id, &uri).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    crate::api_schema::assert_conforms("NamedPlanJobs", &body)?;
    let row = job_row(&body, job_id)?;
    assert_eq!(row["state"], json!("running"), "{body}");
    let expiry = pipette_web_testkit::json_str(&row["queue"]["lease"], "expires_at")?;
    assert!(
        chrono::DateTime::parse_from_rfc3339(expiry)? < Utc::now(),
        "a lapsed lease reported a future expiry: {expiry}"
    );

    // A live lease among the lapsed ones: the job is held, not stranded.
    seed_lease(
        dir.path(),
        job_id,
        "ev1_m",
        Utc::now() + Duration::hours(1),
        &job_body,
    )?;
    let resp = authed_user_get(&state, &key, &user_id, &key_id, &uri).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    let row = job_row(&body, job_id)?;
    assert_eq!(row["state"], json!("running"), "{body}");
    let expiry = pipette_web_testkit::json_str(&row["queue"]["lease"], "expires_at")?;
    assert!(
        chrono::DateTime::parse_from_rfc3339(expiry)? > Utc::now(),
        "a lapsed lease answered over the live one: {expiry}"
    );
    Ok(())
}

/// An `avail/` entry past its deadline still reads `available`, with the
/// deadline its key encodes, so a reader sees it is no longer claimable before
/// the expiry sweep runs.
#[tokio::test]
async fn test_a_queued_job_past_its_deadline_reads_available_and_expired() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let (key, user_id, key_id) = registered_user(
        &state,
        &[UserPermission::SubmitPlans, UserPermission::ReadPlans],
    )
    .await?;
    let plan_id = submit_plan_as(&state, &key, &user_id, &key_id, &client_id, "p", 1).await?;
    let jobs = manifest_job_ids(&state, &plan_id).await?;
    let job_id = &jobs[0];
    // Ingestion refuses a past deadline, so the entry is re-planted under one.
    let job_body = state
        .todo_store
        .get_avail_by_job(job_id)
        .await?
        .context("the job is queued")?;
    state.todo_store.delete_avail_by_job(job_id).await?;
    // Whole seconds, as the `avail/` key keeps it.
    let deadline = ExpiresAt::At((Utc::now() - Duration::hours(1)).trunc_subsecs(0));
    pipette_web_testkit::seed_avail(dir.path(), job_id.as_str(), deadline, &job_body)?;

    let resp = authed_user_get(
        &state,
        &key,
        &user_id,
        &key_id,
        &format!("/api/v1/plans/{plan_id}/jobs"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    crate::api_schema::assert_conforms("NamedPlanJobs", &body)?;
    let row = job_row(&body, job_id)?;
    assert_eq!(row["state"], json!("available"), "{body}");
    assert_eq!(
        row["queue"],
        json!({"expires_at": deadline.instant(), "lease": null}),
        "{body}"
    );
    Ok(())
}

/// A lapse is **this plan's** outcome for the job whatever else is true of it:
/// the job is still queued and still claimable for another member, and this
/// plan reads it `failed`.
#[tokio::test]
async fn test_a_lapsed_job_reads_failed_while_still_queued() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let (key, user_id, key_id) = registered_user(
        &state,
        &[UserPermission::SubmitPlans, UserPermission::ReadPlans],
    )
    .await?;
    let plan_id = submit_plan_as(&state, &key, &user_id, &key_id, &client_id, "p", 1).await?;
    let jobs = manifest_job_ids(&state, &plan_id).await?;
    let job_id = &jobs[0];

    let lapse = ExpiredJob {
        horizon: (Utc::now() - Duration::hours(2)).trunc_subsecs(0),
        recorded_at: Utc::now(),
    };
    let mut manifest = state
        .plan_store
        .get_plan(&plan_id)
        .await?
        .context("manifest written")?;
    manifest.expired_jobs.insert(job_id.clone(), lapse.clone());
    state.plan_store.put_plan(&manifest).await?;

    let resp = authed_user_get(
        &state,
        &key,
        &user_id,
        &key_id,
        &format!("/api/v1/plans/{plan_id}/jobs"),
    )
    .await?;
    let body = body_json(resp).await?;
    crate::api_schema::assert_conforms("NamedPlanJobs", &body)?;
    let row = job_row(&body, job_id)?;

    assert_eq!(row["state"], json!("failed"));
    // Surfaced, with the stored `horizon` as `expires_at`.
    assert_eq!(
        row["plan"]["lapsed"],
        json!({"expires_at": lapse.horizon, "recorded_at": lapse.recorded_at}),
        "{body}"
    );
    // Still in `avail/`, which is what makes the precedence visible.
    assert!(!row["queue"]["expires_at"].is_null());
    // A lapse settles the state, not what the job was filed to run.
    assert_eq!(
        row["planned"]["model"]["repo_name"],
        json!("LFM2-700M-GGUF"),
        "{body}"
    );
    Ok(())
}

/// The jobs are left in `avail/` here, which a complete plan never sees, so a
/// page that still listed the tree would read them `available`. A lapsed job
/// still has the listings place it.
#[tokio::test]
async fn test_a_complete_plan_lists_the_queue_only_for_a_lapsed_job() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let (key, user_id, key_id) = registered_user(
        &state,
        &[UserPermission::SubmitPlans, UserPermission::ReadPlans],
    )
    .await?;
    let plan_id = submit_plan_as(&state, &key, &user_id, &key_id, &client_id, "p", 1).await?;
    let jobs = manifest_job_ids(&state, &plan_id).await?;
    let job_id = &jobs[0];
    let mut manifest = state
        .plan_store
        .get_plan(&plan_id)
        .await?
        .context("manifest written")?;
    manifest.status = PlanStatus::Complete;
    state.plan_store.put_plan(&manifest).await?;
    let path = format!("/api/v1/plans/{plan_id}/jobs");

    let resp = authed_user_get(&state, &key, &user_id, &key_id, &path).await?;
    let body = body_json(resp).await?;
    let row = job_row(&body, job_id)?;
    assert_ne!(row["state"], json!("available"), "{body}");
    assert!(row["queue"].is_null(), "{body}");

    manifest.expired_jobs.insert(
        job_id.clone(),
        ExpiredJob {
            horizon: Utc::now() - Duration::hours(2),
            recorded_at: Utc::now(),
        },
    );
    state.plan_store.put_plan(&manifest).await?;
    let resp = authed_user_get(&state, &key, &user_id, &key_id, &path).await?;
    let body = body_json(resp).await?;
    let row = job_row(&body, job_id)?;
    assert_eq!(row["state"], json!("failed"), "{body}");
    assert!(
        !row["queue"].is_null(),
        "the lapsed job is still placed: {body}"
    );
    Ok(())
}

/// The jobs page is scoped exactly as the plan is: a caller that cannot read the
/// plan cannot read its membership list either.
#[tokio::test]
async fn test_list_jobs_outside_the_scope_is_a_404() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let (owner_key, owner_id, owner_key_id) =
        registered_user(&state, &[UserPermission::SubmitPlans]).await?;
    let (key, user_id, key_id) = registered_user(&state, &[UserPermission::ReadPlans]).await?;
    let plan_id = submit_plan_as(
        &state,
        &owner_key,
        &owner_id,
        &owner_key_id,
        &client_id,
        "theirs",
        1,
    )
    .await?;

    let resp = authed_user_get(
        &state,
        &key,
        &user_id,
        &key_id,
        &format!("/api/v1/plans/{plan_id}/jobs"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    Ok(())
}

/// The jobs page pages over the manifest's own `job_ids` order, and reports the
/// full membership count beside the page.
#[tokio::test]
async fn test_list_jobs_pages_in_manifest_order() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let (key, user_id, key_id) = registered_user(
        &state,
        &[UserPermission::SubmitPlans, UserPermission::ReadPlans],
    )
    .await?;
    let plan_id = submit_plan_as(&state, &key, &user_id, &key_id, &client_id, "p", 3).await?;
    let jobs = manifest_job_ids(&state, &plan_id).await?;

    let resp = authed_user_get(
        &state,
        &key,
        &user_id,
        &key_id,
        &format!("/api/v1/plans/{plan_id}/jobs?limit=2&offset=1"),
    )
    .await?;
    let body = body_json(resp).await?;
    assert_eq!(body["total"], json!(3));
    assert_eq!(body["offset"], json!(1));
    let page: Vec<String> = body["jobs"]
        .as_array()
        .context("a `jobs` array")?
        .iter()
        .map(|row| Ok(pipette_web_testkit::json_str(row, "job_id")?.to_string()))
        .collect::<anyhow::Result<_>>()?;
    assert_eq!(
        page,
        vec![jobs[1].as_str().to_string(), jobs[2].as_str().to_string()]
    );
    Ok(())
}

/// The page size defaults to 100, a larger one is clamped to 500 rather than
/// refused, and `0` asks for the count alone; the body echoes the size it used.
#[rstest]
#[case::by_default("", 100, 3)]
#[case::clamped("?limit=9999", 500, 3)]
#[case::count_only("?limit=0", 0, 0)]
#[tokio::test]
async fn test_list_jobs_defaults_and_clamps_the_page_size(
    #[case] query: &str,
    #[case] limit: usize,
    #[case] listed: usize,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let (key, user_id, key_id) = registered_user(
        &state,
        &[UserPermission::SubmitPlans, UserPermission::ReadPlans],
    )
    .await?;
    let plan_id = submit_plan_as(&state, &key, &user_id, &key_id, &client_id, "p", 3).await?;

    let uri = format!("/api/v1/plans/{plan_id}/jobs{query}");
    let resp = authed_user_get(&state, &key, &user_id, &key_id, &uri).await?;
    assert_eq!(resp.status(), StatusCode::OK, "{uri}");
    let body = body_json(resp).await?;
    assert_eq!(body["limit"], json!(limit), "{uri}: {body}");
    assert_eq!(body["total"], json!(3), "{uri}: {body}");
    assert_eq!(body["offset"], json!(0), "{uri}: {body}");
    assert_eq!(
        body["jobs"].as_array().map(Vec::len),
        Some(listed),
        "{uri}: {body}"
    );
    Ok(())
}

/// From its plan cell, never from what a client reported. The caller holds
/// `read_clients`, so a withheld tier does not explain a missing client id.
#[tokio::test]
async fn test_a_job_row_names_its_plan_cell_in_every_state() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let (key, user_id, key_id) = registered_user(
        &state,
        &[
            UserPermission::SubmitPlans,
            UserPermission::ReadPlans,
            UserPermission::ReadClients,
        ],
    )
    .await?;
    let plan_id = submit_plan_as(
        &state,
        &key,
        &user_id,
        &key_id,
        &client_id,
        "descriptors",
        2,
    )
    .await?;
    let manifest = state
        .plan_store
        .get_plan(&plan_id)
        .await?
        .context("the plan was just filed")?;

    // Queued: read off the staged `spec`, with the keys sorted.
    let body = body_json(
        authed_user_get(
            &state,
            &key,
            &user_id,
            &key_id,
            &format!("/api/v1/plans/{plan_id}/jobs"),
        )
        .await?,
    )
    .await?;
    let queued = &body["jobs"][0];
    assert_eq!(queued["state"], json!("available"));
    let planned = queued["planned"].clone();
    assert_eq!(planned["benchmark"], json!("eval_test"));
    assert_eq!(queued["queue"]["lease"], json!(null));
    // The spec's typed descriptors.
    assert_eq!(planned["model"]["org"], json!("LiquidAI"));
    assert_eq!(planned["model"]["type"], json!("gguf_text"));
    assert_eq!(planned["runtime"]["flavor"], json!("macos-arm64"));

    // Finished: the record settles the state and nothing else.
    let done = manifest.job_ids.first().context("the plan holds a job")?;
    state.todo_store.delete_avail_by_job(done).await?;
    state
        .submission_store
        .write_incoming(
            done,
            &json!({
                "job_id": done.as_str(),
                "message_type": "success",
                "client_id": client_id.as_str(),
                "benchmark_id": "eval_test",
                "model_descriptor": r#"{"org":"acme","path":"reported.gguf","repo_name":"r","source":"huggingface","type":"gguf_text"}"#,
                "runtime_name": "llamacpp",
                "runtime_version": "b5001",
            }),
        )
        .await?;

    let body = body_json(
        authed_user_get(
            &state,
            &key,
            &user_id,
            &key_id,
            &format!("/api/v1/plans/{plan_id}/jobs"),
        )
        .await?,
    )
    .await?;
    crate::api_schema::assert_conforms("NamedPlanJobs", &body)?;
    let row = job_row(&body, done)?;
    assert_eq!(row["state"], json!("finished"));
    assert_eq!(row["planned"]["model"]["org"], json!("LiquidAI"), "{body}");
    assert_eq!(row["queue"], json!(null));

    // Teardown leaves neither a queue entry nor a record; the plan cell still
    // says what the job was filed to run.
    let retired = manifest.job_ids.get(1).context("the plan holds two jobs")?;
    state.todo_store.delete_avail_by_job(retired).await?;
    let body = body_json(
        authed_user_get(
            &state,
            &key,
            &user_id,
            &key_id,
            &format!("/api/v1/plans/{plan_id}/jobs"),
        )
        .await?,
    )
    .await?;
    let row = job_row(&body, retired)?;
    assert_eq!(row["state"], json!("cancelled"));
    assert_eq!(row["planned"]["benchmark"], planned["benchmark"], "{body}");
    assert_eq!(
        row["planned"]["model"]["repo_name"],
        planned["model"]["repo_name"]
    );
    assert_eq!(row["queue"], json!(null));
    Ok(())
}

/// Holds every `find_job` open to count how many run at once. Any other call
/// lands in `unexpected`, so a page that grows a second read fails here.
struct CountingFindJob {
    inner: std::sync::Arc<dyn pipette_store::SubmissionStore>,
    unexpected: std::sync::Arc<pipette_store_testkit::UnexpectedCalls>,
    in_flight: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    peak: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl pipette_store::SubmissionStore for CountingFindJob {
    async fn find_job(
        &self,
        job_id: &JobId,
    ) -> anyhow::Result<Option<pipette_store::SubmissionRecord>> {
        use std::sync::atomic::Ordering::SeqCst;
        let now = self.in_flight.fetch_add(1, SeqCst) + 1;
        self.peak.fetch_max(now, SeqCst);
        // Every call parks here, so a serial loop can never have two in flight
        // and a fan-out always does. The yield is what makes that so without a
        // wall-clock assertion: nothing here depends on how long a sleep takes.
        tokio::task::yield_now().await;
        let found = self.inner.find_job(job_id).await;
        self.in_flight.fetch_sub(1, SeqCst);
        found
    }

    async fn write_incoming(&self, _: &JobId, _: &Value) -> anyhow::Result<()> {
        Err(self.unexpected.record("write_incoming"))
    }
    async fn write_incoming_new(
        &self,
        _: &JobId,
        _: &Value,
    ) -> anyhow::Result<pipette_store::CreateOutcome> {
        Err(self.unexpected.record("write_incoming_new"))
    }
    async fn write_processed(&self, _: &JobId, _: &Value) -> anyhow::Result<()> {
        Err(self.unexpected.record("write_processed"))
    }
    async fn get_submission(
        &self,
        _: &JobId,
    ) -> anyhow::Result<Option<pipette_store::SubmissionRecord>> {
        Err(self.unexpected.record("get_submission"))
    }
    async fn list_incoming_after(
        &self,
        _: Option<&JobId>,
        _: std::num::NonZeroUsize,
    ) -> anyhow::Result<Vec<JobId>> {
        Err(self.unexpected.record("list_incoming_after"))
    }
    async fn mark_processed(&self, _: &JobId) -> anyhow::Result<()> {
        Err(self.unexpected.record("mark_processed"))
    }
    async fn delete_incoming(&self, _: &JobId) -> anyhow::Result<()> {
        Err(self.unexpected.record("delete_incoming"))
    }
    async fn find_in_score_queue(
        &self,
        _: &JobId,
    ) -> anyhow::Result<Option<pipette_store::SubmissionRecord>> {
        Err(self.unexpected.record("find_in_score_queue"))
    }
    async fn has_record(&self, _: &JobId) -> anyhow::Result<bool> {
        Err(self.unexpected.record("has_record"))
    }
    async fn find_outcomes(
        &self,
        _: &[JobId],
    ) -> anyhow::Result<std::collections::HashMap<JobId, pipette_core::plan::SettledOutcome>> {
        Err(self.unexpected.record("find_outcomes"))
    }
    async fn enqueue(
        &self,
        _: pipette_store::ScoreQueueStage,
        _: &JobId,
        _: &Value,
    ) -> anyhow::Result<()> {
        Err(self.unexpected.record("enqueue"))
    }
    async fn list_queue_after(
        &self,
        _: pipette_store::ScoreQueueStage,
        _: Option<&JobId>,
        _: std::num::NonZeroUsize,
    ) -> anyhow::Result<Vec<JobId>> {
        Err(self.unexpected.record("list_queue_after"))
    }
    async fn read_queue(
        &self,
        _: pipette_store::ScoreQueueStage,
        _: &JobId,
    ) -> anyhow::Result<Option<Value>> {
        Err(self.unexpected.record("read_queue"))
    }
    async fn dequeue(&self, _: pipette_store::ScoreQueueStage, _: &JobId) -> anyhow::Result<()> {
        Err(self.unexpected.record("dequeue"))
    }
    async fn write_unverified(&self, _: &ClientId, _: &JobId, _: &Value) -> anyhow::Result<()> {
        Err(self.unexpected.record("write_unverified"))
    }
    async fn list_unverified_client(&self, _: &ClientId) -> anyhow::Result<Vec<(JobId, Value)>> {
        Err(self.unexpected.record("list_unverified_client"))
    }
    async fn delete_unverified(&self, _: &ClientId, _: &JobId) -> anyhow::Result<()> {
        Err(self.unexpected.record("delete_unverified"))
    }
    async fn delete_unverified_client(&self, _: &ClientId, _: bool) -> anyhow::Result<usize> {
        Err(self.unexpected.record("delete_unverified_client"))
    }
    async fn prune_unverified(
        &self,
        _: std::time::Duration,
        _: bool,
    ) -> anyhow::Result<pipette_store::PruneSummary> {
        Err(self.unexpected.record("prune_unverified"))
    }
    async fn write_runtime_output(
        &self,
        _: &JobId,
        _: &ClientId,
        _: &RuntimeOutput,
    ) -> anyhow::Result<()> {
        Err(self.unexpected.record("write_runtime_output"))
    }
    async fn list_runtime_output(
        &self,
        _: &JobId,
    ) -> anyhow::Result<Vec<(ClientId, RuntimeOutput)>> {
        Err(self.unexpected.record("list_runtime_output"))
    }
}

/// A job out of both queues needs up to three probes for its record, so a
/// serial page of a hundred was the console's slowest screen. Asserted on the
/// peak in flight, not a duration, so a busy machine cannot fail it.
#[tokio::test]
async fn test_a_jobs_page_reads_its_records_concurrently() -> anyhow::Result<()> {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

    const JOBS: usize = 8;

    let dir = tempfile::tempdir()?;
    let (mut state, _key, client_id) = approved_client(dir.path()).await?;
    let (key, user_id, key_id) = registered_user(
        &state,
        &[UserPermission::SubmitPlans, UserPermission::ReadPlans],
    )
    .await?;

    let plan_id = submit_plan_as(
        &state, &key, &user_id, &key_id, &client_id, "parallel", JOBS,
    )
    .await?;
    let manifest = state
        .plan_store
        .get_plan(&plan_id)
        .await?
        .context("the plan was just filed")?;

    // Out of both queues and holding a record, so every row on the page needs a
    // `find_job`. That is the case the fan-out exists for.
    for job_id in &manifest.job_ids {
        state.todo_store.delete_avail_by_job(job_id).await?;
        state
            .submission_store
            .write_incoming(
                job_id,
                &json!({"job_id": job_id.as_str(), "message_type": "success"}),
            )
            .await?;
    }

    let in_flight = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let unexpected = Arc::new(pipette_store_testkit::UnexpectedCalls::default());
    state.submission_store = Arc::new(CountingFindJob {
        inner: state.submission_store.clone(),
        unexpected: unexpected.clone(),
        in_flight: in_flight.clone(),
        peak: peak.clone(),
    });

    let resp = authed_user_get(
        &state,
        &key,
        &user_id,
        &key_id,
        &format!("/api/v1/plans/{plan_id}/jobs"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    unexpected.none()?;
    let body = body_json(resp).await?;

    // Every row was classified from its record, and they overlapped.
    assert_eq!(body["jobs"].as_array().map(|rows| rows.len()), Some(JOBS));
    assert_eq!(
        peak.load(SeqCst),
        JOBS,
        "the page's record reads ran one at a time"
    );
    assert_eq!(in_flight.load(SeqCst), 0, "every read was accounted for");
    Ok(())
}

// ── POST /api/v1/plans/{plan_id}/cancel ────────────────────────────────────────────

/// `cancel_plans` cancels the caller's own plan: the marker lands, and the
/// status is unchanged, because `queue-maintenance` owns the latch and has not
/// run. The answer says what the call did and holds no plan.
#[tokio::test]
async fn test_cancel_writes_the_marker_and_leaves_the_status() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let (key, user_id, key_id) = registered_user(
        &state,
        &[UserPermission::SubmitPlans, UserPermission::CancelPlans],
    )
    .await?;
    let plan_id = submit_plan_as(&state, &key, &user_id, &key_id, &client_id, "doomed", 1).await?;

    let resp = pipette_web_testkit::authed_user_post(
        &state,
        &key,
        &user_id,
        &key_id,
        &format!("/api/v1/plans/{plan_id}/cancel"),
        &json!({}),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let body = body_json(resp).await?;

    assert_eq!(body["outcome"], json!("requested"));
    let mut fields: Vec<&str> = body
        .as_object()
        .context("the answer is an object")?
        .keys()
        .map(String::as_str)
        .collect();
    fields.sort_unstable();
    assert_eq!(fields, ["message", "outcome"]);
    crate::api_schema::assert_conforms("PlanCancelRequested", &body)?;
    assert!(state.plan_store.has_cancel_marker(&plan_id).await?);
    // The marker only *requests* teardown; the latch is the maintenance pass's.
    let plan = state
        .plan_store
        .get_plan(&plan_id)
        .await?
        .context("the plan is still there")?;
    assert_eq!(plan.status, PlanStatus::Active);
    Ok(())
}

#[tokio::test]
async fn test_cancel_over_http_is_torn_down_by_queue_maintenance() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let (key, user_id, key_id) = registered_user(
        &state,
        &[UserPermission::SubmitPlans, UserPermission::CancelPlans],
    )
    .await?;
    let plan_id = submit_plan_as(&state, &key, &user_id, &key_id, &client_id, "doomed", 2).await?;
    let job_ids = manifest_job_ids(&state, &plan_id).await?;

    let resp = pipette_web_testkit::authed_user_post(
        &state,
        &key,
        &user_id,
        &key_id,
        &format!("/api/v1/plans/{plan_id}/cancel"),
        &json!({}),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    pipette_web_testkit::run_qm(&state).await?;

    let manifest = state
        .plan_store
        .get_plan(&plan_id)
        .await?
        .context("manifest written")?;
    assert_eq!(
        manifest.status,
        PlanStatus::Cancelling,
        "the pass that retires the jobs latches the plan"
    );
    let state = &state;
    futures::stream::iter(job_ids.iter().map(Ok::<_, anyhow::Error>))
        .try_for_each(|job_id| async move {
            assert!(
                state.todo_store.get_avail_by_job(job_id).await?.is_none(),
                "{job_id} is still claimable after the teardown pass"
            );
            Ok(())
        })
        .await?;
    Ok(())
}

/// Re-cancelling is a no-op rather than an error: the marker is a plain
/// idempotent write.
#[tokio::test]
async fn test_cancel_is_idempotent() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let (key, user_id, key_id) = registered_user(
        &state,
        &[UserPermission::SubmitPlans, UserPermission::CancelPlans],
    )
    .await?;
    let plan_id = submit_plan_as(&state, &key, &user_id, &key_id, &client_id, "doomed", 1).await?;

    for _ in 0..2 {
        let resp = pipette_web_testkit::authed_user_post(
            &state,
            &key,
            &user_id,
            &key_id,
            &format!("/api/v1/plans/{plan_id}/cancel"),
            &json!({}),
        )
        .await?;
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        assert_eq!(body_json(resp).await?["outcome"], json!("requested"));
    }
    Ok(())
}

/// A latched status controls maintenance directly, so another cancel marker
/// would duplicate work.
#[rstest]
#[case::cancelling(PlanStatus::Cancelling, "already_cancelling")]
#[case::cancelled(PlanStatus::Cancelled, "already_cancelled")]
#[tokio::test]
async fn test_cancel_a_latched_plan_reports_the_latch(
    #[case] status: PlanStatus,
    #[case] expected_outcome: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let (key, user_id, key_id) = registered_user(
        &state,
        &[UserPermission::SubmitPlans, UserPermission::CancelPlans],
    )
    .await?;
    let plan_id = submit_plan_as(&state, &key, &user_id, &key_id, &client_id, "latched", 1).await?;

    let mut manifest = state
        .plan_store
        .get_plan(&plan_id)
        .await?
        .context("manifest written")?;
    manifest.status = status;
    state.plan_store.put_plan(&manifest).await?;

    let resp = pipette_web_testkit::authed_user_post(
        &state,
        &key,
        &user_id,
        &key_id,
        &format!("/api/v1/plans/{plan_id}/cancel"),
        &json!({}),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let body = body_json(resp).await?;
    assert_eq!(body["outcome"], json!(expected_outcome));
    crate::api_schema::assert_conforms("PlanCancelRequested", &body)?;
    assert!(
        !state.plan_store.has_cancel_marker(&plan_id).await?,
        "a latched plan takes no second marker"
    );
    Ok(())
}

/// Cancelling another account's plan takes `cancel_any_plans`, and the refusal
/// names it.
#[tokio::test]
async fn test_cancel_another_accounts_plan_needs_the_any_grant() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let (owner_key, owner_id, owner_key_id) =
        registered_user(&state, &[UserPermission::SubmitPlans]).await?;
    let (key, user_id, key_id) = registered_user(&state, &[UserPermission::CancelPlans]).await?;
    let plan_id = submit_plan_as(
        &state,
        &owner_key,
        &owner_id,
        &owner_key_id,
        &client_id,
        "theirs",
        1,
    )
    .await?;

    let resp = pipette_web_testkit::authed_user_post(
        &state,
        &key,
        &user_id,
        &key_id,
        &format!("/api/v1/plans/{plan_id}/cancel"),
        &json!({}),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert!(
        pipette_web_testkit::json_str(&body_json(resp).await?, "error")?
            .contains("cancel_any_plans"),
        "the refusal did not name the permission"
    );
    assert!(!state.plan_store.has_cancel_marker(&plan_id).await?);

    // The wider grant carries it.
    grant_user(&state, &user_id, &[UserPermission::CancelAnyPlans]).await?;
    let resp = pipette_web_testkit::authed_user_post(
        &state,
        &key,
        &user_id,
        &key_id,
        &format!("/api/v1/plans/{plan_id}/cancel"),
        &json!({}),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    assert!(state.plan_store.has_cancel_marker(&plan_id).await?);
    Ok(())
}

/// Scoped callers receive the same response for absent and foreign plans to
/// prevent plan-ID enumeration. Global callers can receive `404` because they
/// can already discover every plan.
#[rstest]
#[case::scoped(&[UserPermission::CancelPlans], StatusCode::FORBIDDEN)]
#[case::global(&[UserPermission::CancelAnyPlans], StatusCode::NOT_FOUND)]
#[tokio::test]
async fn test_cancel_an_absent_plan_answers_by_scope(
    #[case] grants: &[UserPermission],
    #[case] expected: StatusCode,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, _client_id) = approved_client(dir.path()).await?;
    let (key, user_id, key_id) = registered_user(&state, grants).await?;

    let absent = PlanId::from_uuid(uuid::Uuid::now_v7());
    let resp = pipette_web_testkit::authed_user_post(
        &state,
        &key,
        &user_id,
        &key_id,
        &format!("/api/v1/plans/{absent}/cancel"),
        &json!({}),
    )
    .await?;
    assert_eq!(resp.status(), expected);
    Ok(())
}

/// Cancellation is destructive and requires a separate grant, even for the
/// submitter.
#[tokio::test]
async fn test_cancel_own_plan_needs_more_than_submit_plans() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let (key, user_id, key_id) = registered_user(&state, &[UserPermission::SubmitPlans]).await?;
    let plan_id = submit_plan_as(&state, &key, &user_id, &key_id, &client_id, "mine", 1).await?;

    let resp = pipette_web_testkit::authed_user_post(
        &state,
        &key,
        &user_id,
        &key_id,
        &format!("/api/v1/plans/{plan_id}/cancel"),
        &json!({}),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert!(
        pipette_web_testkit::json_str(&body_json(resp).await?, "error")?.contains("cancel_plans"),
        "the refusal did not name the permission"
    );
    assert!(!state.plan_store.has_cancel_marker(&plan_id).await?);
    Ok(())
}

/// The extractor rejects client credentials before it checks permissions.
#[tokio::test]
async fn test_cancel_refuses_a_client() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;

    let absent = PlanId::from_uuid(uuid::Uuid::now_v7());
    let resp = pipette_web_testkit::authed_post(
        &state,
        &key,
        &client_id,
        &format!("/api/v1/plans/{absent}/cancel"),
        &json!({}),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    Ok(())
}

/// So it learns nothing about whether the plan exists. Only `cancel_any_plans`
/// implies `cancel_plans`; `read_plans` does not.
#[tokio::test]
async fn test_cancel_without_a_grant_is_refused_before_the_lookup() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, _client_id) = approved_client(dir.path()).await?;
    let (key, user_id, key_id) = registered_user(
        &state,
        &[UserPermission::ReadAnyPlans, UserPermission::SubmitPlans],
    )
    .await?;

    let absent = PlanId::from_uuid(uuid::Uuid::now_v7());
    let resp = pipette_web_testkit::authed_user_post(
        &state,
        &key,
        &user_id,
        &key_id,
        &format!("/api/v1/plans/{absent}/cancel"),
        &json!({}),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert!(
        pipette_web_testkit::json_str(&body_json(resp).await?, "error")?.contains("cancel_plans"),
        "the refusal did not name the permission"
    );
    Ok(())
}

/// A `complete` plan is a `409`: it is right there and the caller may read it,
/// but every job reached a terminal state, so no marker could tear anything
/// down.
#[tokio::test]
async fn test_cancel_a_complete_plan_is_a_conflict() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let (key, user_id, key_id) = registered_user(
        &state,
        &[UserPermission::SubmitPlans, UserPermission::CancelPlans],
    )
    .await?;
    let plan_id = submit_plan_as(&state, &key, &user_id, &key_id, &client_id, "done", 1).await?;

    let mut manifest = state
        .plan_store
        .get_plan(&plan_id)
        .await?
        .context("manifest written")?;
    manifest.status = PlanStatus::Complete;
    state.plan_store.put_plan(&manifest).await?;

    let resp = pipette_web_testkit::authed_user_post(
        &state,
        &key,
        &user_id,
        &key_id,
        &format!("/api/v1/plans/{plan_id}/cancel"),
        &json!({}),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert!(!state.plan_store.has_cancel_marker(&plan_id).await?);
    Ok(())
}

// ── GET /api/v1/plans/{plan_id}/explain ────────────────────────────────────────────

/// The report reaches a scheduling state and a headline, and the caller's **own** counts
/// are never gated: a submitter must be able to ask why its plan is stuck
/// whatever fleet rights it holds.
#[tokio::test]
async fn test_explain_withholds_the_fleet_without_read_clients() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let (key, user_id, key_id) = registered_user(
        &state,
        &[UserPermission::SubmitPlans, UserPermission::ReadPlans],
    )
    .await?;
    let plan_id = submit_plan_as(&state, &key, &user_id, &key_id, &client_id, "p", 1).await?;

    let resp = authed_user_get(
        &state,
        &key,
        &user_id,
        &key_id,
        &format!("/api/v1/plans/{plan_id}/explain"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;

    assert_eq!(body["plan_id"], json!(plan_id.as_ref()));
    assert!(!body["headline"].as_str().unwrap_or_default().is_empty());
    assert!(
        !body["scheduling_state"]
            .as_str()
            .unwrap_or_default()
            .is_empty()
    );
    // The caller's own jobs, always present.
    assert_eq!(body["census"]["total"], json!(1));
    // The fleet, never present at this tier.
    assert_eq!(body["fleet_detail"], json!("withheld"));
    assert!(
        body.get("fleet").is_none(),
        "the fleet standing reached a caller holding no read_clients: {body}"
    );
    Ok(())
}

/// `read_clients` opens the fleet standing, which is the whole difference
/// between the two tiers.
#[tokio::test]
async fn test_explain_names_the_fleet_with_read_clients() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let (key, user_id, key_id) = registered_user(
        &state,
        &[
            UserPermission::SubmitPlans,
            UserPermission::ReadPlans,
            UserPermission::ReadClients,
        ],
    )
    .await?;
    let plan_id = submit_plan_as(&state, &key, &user_id, &key_id, &client_id, "p", 1).await?;

    let resp = authed_user_get(
        &state,
        &key,
        &user_id,
        &key_id,
        &format!("/api/v1/plans/{plan_id}/explain"),
    )
    .await?;
    let body = body_json(resp).await?;
    assert_eq!(body["fleet_detail"], json!("full"));
    // One approved client registered by the fixture.
    assert_eq!(body["fleet"]["registered"], json!(1));
    assert_eq!(body["fleet"]["approved"], json!(1));
    Ok(())
}

#[tokio::test]
async fn test_explain_outside_the_scope_is_a_404() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let (owner_key, owner_id, owner_key_id) =
        registered_user(&state, &[UserPermission::SubmitPlans]).await?;
    let (key, user_id, key_id) = registered_user(&state, &[UserPermission::ReadPlans]).await?;
    let plan_id = submit_plan_as(
        &state,
        &owner_key,
        &owner_id,
        &owner_key_id,
        &client_id,
        "theirs",
        1,
    )
    .await?;

    let resp = authed_user_get(
        &state,
        &key,
        &user_id,
        &key_id,
        &format!("/api/v1/plans/{plan_id}/explain"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    Ok(())
}

// ── plan-addressed reads: refused before the lookup ─────────────────────────

/// An account holding no plan read grant is refused `403` before the manifest
/// is read, so another account's plan and an absent id answer alike. A fleet or
/// cancel grant opens none of these routes.
#[rstest]
#[tokio::test]
async fn test_a_plan_read_without_a_read_grant_is_refused_before_the_lookup(
    #[values("", "/jobs", "/explain", "/document", "/results.csv")] suffix: &str,
    #[values(&[], &[UserPermission::ReadClients], &[UserPermission::CancelPlans])]
    grants: &[UserPermission],
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let (owner_key, owner_id, owner_key_id) =
        registered_user(&state, &[UserPermission::SubmitPlans]).await?;
    let (key, user_id, key_id) = registered_user(&state, grants).await?;
    let filed = submit_plan_as(
        &state,
        &owner_key,
        &owner_id,
        &owner_key_id,
        &client_id,
        "theirs",
        1,
    )
    .await?;
    let absent = PlanId::from_uuid(uuid::Uuid::now_v7());

    for plan_id in [&filed, &absent] {
        let uri = format!("/api/v1/plans/{plan_id}{suffix}");
        let resp = authed_user_get(&state, &key, &user_id, &key_id, &uri).await?;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{uri}");
        assert_eq!(
            body_json(resp).await?,
            json!({"error": "user lacks the read_plans permission"}),
            "{uri}"
        );
    }
    Ok(())
}

/// A `plan_id` outside `[A-Za-z0-9-]` is a `400` naming the character, so a
/// segment the caller chose never becomes a storage key.
#[rstest]
#[case::jobs("GET", "/api/v1/plans/bad.id/jobs")]
#[case::explain("GET", "/api/v1/plans/bad.id/explain")]
#[case::status("GET", "/api/v1/plans/bad.id")]
#[case::cancel("POST", "/api/v1/plans/bad.id/cancel")]
#[case::document("GET", "/api/v1/plans/bad.id/document")]
#[case::variants("GET", "/api/v1/plans/bad.id/variants")]
#[case::export("GET", "/api/v1/plans/bad.id/results.csv")]
#[tokio::test]
async fn test_a_malformed_plan_id_is_refused_at_the_boundary(
    #[case] method: &str,
    #[case] uri: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, _client_id) = approved_client(dir.path()).await?;
    let signer = registered_user(
        &state,
        &[UserPermission::ReadPlans, UserPermission::CancelPlans],
    )
    .await?;

    let resp = pipette_web_testkit::signed_empty(&state, &signer, method, uri).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{method} {uri}");
    assert_eq!(
        body_json(resp).await?,
        json!({"error": "plan_id contains invalid character '.'; allowed: [A-Za-z0-9-]"}),
        "{method} {uri}"
    );
    Ok(())
}

// ── status is checked before permissions ────────────────────────────────────

/// Seeded with the grant, so the refusal has to be about its standing.
/// `GET /api/v1/users/me` is the one user route that admits it.
#[rstest]
#[case::list("/api/v1/plans")]
#[case::status("/api/v1/plans/plan-0")]
#[case::document("/api/v1/plans/plan-0/document")]
#[case::variants("/api/v1/plans/plan-0/variants")]
#[case::jobs("/api/v1/plans/plan-0/jobs")]
#[case::explain("/api/v1/plans/plan-0/explain")]
#[case::by_job("/api/v1/jobs/job-0/plans")]
#[case::export("/api/v1/plans/plan-0/results.csv")]
#[tokio::test]
async fn test_a_pending_account_reads_no_plans(#[case] path: &str) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, _client_id) = approved_client(dir.path()).await?;
    let (key, user_id, key_id) = registered_user(&state, &[UserPermission::ReadAnyPlans]).await?;
    pipette_web_testkit::set_user_status(&state, &user_id, pipette_plan_types::UserStatus::Pending)
        .await?;

    let resp = authed_user_get(&state, &key, &user_id, &key_id, path).await?;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "{path} admitted a pending account"
    );
    assert_eq!(
        body_json(resp).await?,
        json!({"error": "user is pending an operator's approval"}),
        "{path} refused a pending account for another reason"
    );
    Ok(())
}

#[tokio::test]
async fn test_a_pending_account_cannot_cancel_plans() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, _client_id) = approved_client(dir.path()).await?;
    let (key, user_id, key_id) = registered_user(&state, &[UserPermission::CancelAnyPlans]).await?;
    pipette_web_testkit::set_user_status(&state, &user_id, pipette_plan_types::UserStatus::Pending)
        .await?;

    let resp = pipette_web_testkit::authed_user_post(
        &state,
        &key,
        &user_id,
        &key_id,
        "/api/v1/plans/plan-0/cancel",
        &json!({}),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    Ok(())
}

#[rstest]
#[case::withheld(None, "withheld")]
#[case::full(Some(UserPermission::ReadClients), "full")]
#[case::read_any_clients(Some(UserPermission::ReadAnyClients), "full")]
#[tokio::test]
async fn test_status_listing_and_explain_project_fleet_detail(
    #[case] permission: Option<UserPermission>,
    #[case] tier: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let (key, user_id, key_id) = registered_user(
        &state,
        &[
            UserPermission::SubmitPlans,
            UserPermission::ReadPlans,
            UserPermission::CancelPlans,
        ],
    )
    .await?;
    grant_plan_user_detail(&state, &user_id, permission).await?;
    let plan_id = submit_plan_as(&state, &key, &user_id, &key_id, &client_id, "tier", 2).await?;
    let full_group = add_starved_snapshot(&state, &plan_id).await?;
    let stored = state
        .plan_store
        .get_plan(&plan_id)
        .await?
        .context("plan exists")?;

    for route in [format!("/api/v1/plans/{plan_id}"), "/api/v1/plans".into()] {
        let resp = authed_user_get(&state, &key, &user_id, &key_id, &route).await?;
        assert!(resp.status().is_success(), "{route}: {}", resp.status());
        let body = body_json(resp).await?;
        let def = match route.as_str() {
            "/api/v1/plans" => "PlanViewPage",
            _ => "PlanView",
        };
        crate::api_schema::assert_conforms(def, &body)?;
        let view = if route == "/api/v1/plans" {
            &body["items"][0]
        } else {
            &body
        };
        assert_eq!(view["fleet_detail"], tier, "{route}");
        assert_eq!(
            view["progress_snapshot"]["counts"],
            json!(
                stored
                    .progress_snapshot
                    .as_ref()
                    .context("snapshot")?
                    .counts
            )
        );
        assert_eq!(view["warnings"][0]["job_ids"], json!(stored.job_ids));
        let group = &view["progress_snapshot"]["starved_groups"][0];
        if tier == "full" {
            assert_eq!(group, &full_group);
            assert_eq!(view["warnings"], json!(stored.warnings));
        } else {
            assert_eq!(
                view["warnings"][0]["message"],
                "2 jobs match no working client"
            );
            assert_eq!(group, &json!({"job_ids": stored.job_ids}));
            assert!(!view.to_string().contains("secret"));
        }
    }
    // Remove the cancellation marker so the explanation reports on the jobs
    // rather than the teardown.
    state.plan_store.delete_cancel_marker(&plan_id).await?;
    let resp = authed_user_get(
        &state,
        &key,
        &user_id,
        &key_id,
        &format!("/api/v1/plans/{plan_id}/explain"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["fleet_detail"], tier);
    // No `queue-maintenance` pass has run, so gate 5 answers before starvation.
    // The groups below hold under any scheduling state; this names the one
    // they run under.
    assert_eq!(body["scheduling_state"], "not_indexed", "{body}");
    assert_eq!(body["groups"][0]["job_count"], 2);
    for field in [
        "description",
        "ladder",
        "live_working_client_count",
        "live_approved_client_count",
        "live_registered_client_count",
        "roster",
    ] {
        assert_eq!(
            body["groups"][0].get(field).is_some(),
            tier == "full",
            "{field}: {body}"
        );
    }
    if tier == "withheld" {
        assert!(body["groups"][0].get("never_matches").is_none());
        assert!(!body.to_string().contains("secret"));
    }
    Ok(())
}

/// At the withheld tier, one fixed sentence naming no group count and no
/// snapshot age.
#[rstest]
#[case::withheld(false)]
#[case::full(true)]
#[tokio::test]
async fn test_explain_projects_a_starved_headline(#[case] full: bool) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, _client_id) = approved_client(dir.path()).await?;
    let (key, user_id, key_id) = registered_user_with_detail(&state, full).await?;
    // Pinned to a client that never registered, so no approved client matches
    // the job and the scan reaches starvation rather than a standing gate.
    let absent = ClientId::try_new("ev1_absent")?;
    let plan_id = submit_plan_as(&state, &key, &user_id, &key_id, &absent, "starved", 1).await?;
    pipette_web_testkit::run_qm(&state).await?;
    let resp = authed_user_get(
        &state,
        &key,
        &user_id,
        &key_id,
        &format!("/api/v1/plans/{plan_id}/explain"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    let withheld = "all 1 claimable job(s) match no working client. Whether the snapshot is current, and whether any group matches a working client now, is withheld: reading it needs read_clients or read_any_clients. A queue-maintenance run clears a group that recovered";
    // The cached set names the job, so the scheduling state reads the snapshot.
    assert_eq!(body["scheduling_state"], "starved", "{body}");
    assert_eq!(body["starved_by_live_scan"], false, "{body}");
    if !full {
        assert_eq!(body["headline"], withheld);
    }

    // Drop the cached set and leave the job indexed, so the live scan reaches
    // the scheduling state instead. That arm reports a stale snapshot and
    // counts the groups, both of which read the live fleet, so the tier decides
    // first and the withheld sentence stays the one above.
    let mut manifest = state
        .plan_store
        .get_plan(&plan_id)
        .await?
        .context("plan exists")?;
    if let Some(snapshot) = manifest.progress_snapshot.as_mut() {
        snapshot.starved.clear();
    }
    state.plan_store.put_plan(&manifest).await?;
    let resp = authed_user_get(
        &state,
        &key,
        &user_id,
        &key_id,
        &format!("/api/v1/plans/{plan_id}/explain"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["scheduling_state"], "starved", "{body}");
    assert_eq!(body["starved_by_live_scan"], true, "{body}");
    if !full {
        assert_eq!(body["headline"], withheld);
    }
    Ok(())
}

#[rstest]
#[case::withheld(false)]
#[case::full(true)]
#[tokio::test]
async fn test_explain_projects_a_blocked_clients_headline(
    #[case] full: bool,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let (key, user_id, key_id) = registered_user_with_detail(&state, full).await?;
    let plan_id = submit_plan_as(&state, &key, &user_id, &key_id, &client_id, "blocked", 1).await?;
    pipette_web_testkit::run_qm(&state).await?;
    let jobs = manifest_job_ids(&state, &plan_id).await?;
    state
        .todo_store
        .write_suspension(&client_id, Utc::now(), &jobs[0])
        .await?;
    let resp = authed_user_get(
        &state,
        &key,
        &user_id,
        &key_id,
        &format!("/api/v1/plans/{plan_id}/explain"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["scheduling_state"], "clients_blocked");
    if full {
        assert_eq!(
            body["headline"],
            "no client can take a claimable job: 1 suspended"
        );
    } else {
        assert_eq!(
            body["headline"],
            "no client can take a claimable job. Which fleet gate blocks them is withheld: reading it needs read_clients or read_any_clients"
        );
    }
    Ok(())
}
