//! What the app's campaign listings and new-campaign form read beyond the
//! campaign records: each row's plan (`plan=true`), the owners' names
//! (`names=true`), the same flags on one campaign's read, and the create's and
//! the preview's bodies.

use anyhow::Context;
use axum::http::StatusCode;
use futures::TryStreamExt;
use rstest::rstest;
use serde_json::{Value, json};

use pipette_httpapi::state::AppState;
use pipette_plan_types::{CampaignId, ClientId, PlanId, UserPermission};
use pipette_web_testkit::{
    ONE_CELL_PLAN, UserSigner, add_starved_snapshot, approved_client, body_json, create_org,
    created_campaign, json_str, post_campaign, post_plan_document, put_idle_campaign,
    put_loose_plan, registered_user, set_user_org, signed_empty, submit_plan_as, user_address,
};

async fn read(
    state: &AppState,
    signer: &UserSigner,
    uri: &str,
) -> anyhow::Result<(StatusCode, Value)> {
    let resp = signed_empty(state, signer, "GET", uri).await?;
    let status = resp.status();
    Ok((status, body_json(resp).await?))
}

/// [`read`], which must answer `200` with a body the app's type describes.
async fn listed(state: &AppState, signer: &UserSigner, uri: &str) -> anyhow::Result<Value> {
    let (status, body) = read(state, signer, uri).await?;
    anyhow::ensure!(status == StatusCode::OK, "{uri}: {status} {body}");
    crate::api_schema::assert_conforms("NamedCampaignViewPage", &body)?;
    Ok(body)
}

fn row<'a>(body: &'a Value, campaign_id: &str) -> anyhow::Result<&'a Value> {
    body.get("items")
        .and_then(Value::as_array)
        .context("a listing carries an `items` array")?
        .iter()
        .find(|row| row.get("campaign_id").and_then(Value::as_str) == Some(campaign_id))
        .with_context(|| format!("no row for {campaign_id} in {body}"))
}

/// A plan of one job filed by `signer` into a campaign of its own, and that
/// campaign.
async fn running(
    state: &AppState,
    signer: &UserSigner,
    client_id: &ClientId,
    name: &str,
) -> anyhow::Result<(CampaignId, PlanId)> {
    let (key, user_id, key_id) = signer;
    let plan_id = submit_plan_as(state, key, user_id, key_id, client_id, name, 1).await?;
    let campaign_id = state
        .plan_store
        .get_plan(&plan_id)
        .await?
        .and_then(|manifest| manifest.campaign_id)
        .context("the plan names its campaign")?;
    Ok((campaign_id, plan_id))
}

/// Point `campaign_id` at `plan_id`, a state no write produces.
async fn point(state: &AppState, campaign_id: &CampaignId, plan_id: &PlanId) -> anyhow::Result<()> {
    let mut campaign = state
        .campaign_store
        .get_campaign(campaign_id)
        .await?
        .context("the campaign is stored")?;
    campaign.current_plan_id = Some(plan_id.clone());
    state.campaign_store.put_campaign(&campaign).await
}

/// `plan_id`'s plan read cut to a row's `plan`: the keys the two share, and the
/// snapshot's `counts`.
pub(crate) async fn as_a_row(
    state: &AppState,
    signer: &UserSigner,
    plan_id: &PlanId,
) -> anyhow::Result<Value> {
    let uri = format!("/api/v1/plans/{plan_id}");
    let (status, read) = read(state, signer, &uri).await?;
    anyhow::ensure!(status == StatusCode::OK, "{uri}: {status} {read}");
    let mut row: serde_json::Map<String, Value> = [
        "plan_id",
        "note",
        "change",
        "status",
        "created_at",
        "submitted_by",
        "campaign_id",
        "cancel_requested",
    ]
    .into_iter()
    .filter_map(|key| Some((key.to_string(), read.get(key)?.clone())))
    .collect();
    let counts = read.pointer("/progress_snapshot/counts").cloned();
    row.insert("counts".into(), counts.unwrap_or(Value::Null));
    Ok(Value::Object(row))
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

// ── GET /api/v1/campaigns?plan=true ────────────────────────────────────────────────

/// Asked, a row carries its campaign's plan as the plan read draws it, with its
/// snapshot's counts. A campaign naming no plan, or one whose manifest does not
/// read, has none. Unasked, the body is the bare listing the CLI reads.
#[rstest]
#[case::oldest("")]
#[case::newest("&order=newest")]
#[tokio::test]
async fn test_each_row_carries_its_plan_when_asked(#[case] order: &str) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let reader = registered_user(
        &state,
        &[
            UserPermission::SubmitPlans,
            UserPermission::ReadPlans,
            UserPermission::CancelPlans,
            UserPermission::ReadClients,
        ],
    )
    .await?;
    let (campaign_id, head) = running(&state, &reader, &client_id, "running").await?;
    let idle = put_idle_campaign(&state, &reader.1, "Idle").await?;
    add_starved_snapshot(&state, &head).await?;
    let resp = signed_empty(
        &state,
        &reader,
        "POST",
        &format!("/api/v1/plans/{head}/cancel"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    let body = listed(
        &state,
        &reader,
        &format!("/api/v1/campaigns?plan=true{order}"),
    )
    .await?;
    let plan = row(&body, campaign_id.as_str())?
        .get("plan")
        .context("the row carries its plan")?;
    // The snapshot's counts, not its starved groups, though the reader holds
    // `read_clients`.
    assert_eq!(
        fields(plan)?,
        [
            "campaign_id",
            "cancel_requested",
            "counts",
            "created_at",
            "plan_id",
            "status",
            "submitted_by"
        ]
    );
    assert_eq!(plan["cancel_requested"], json!(true));
    assert!(plan["status"].is_string(), "{plan}");
    assert_eq!(plan["counts"]["total"], json!(1));
    assert_eq!(plan, &as_a_row(&state, &reader, &head).await?);
    assert!(row(&body, idle.as_str())?.get("plan").is_none(), "{body}");
    assert!(body.get("names").is_none(), "{body}");

    let body = listed(&state, &reader, &format!("/api/v1/campaigns?{order}")).await?;
    assert_eq!(fields(&body)?, ["items", "next_cursor", "unreadable"]);
    assert_eq!(
        fields(row(&body, campaign_id.as_str())?)?,
        [
            "campaign_id",
            "created_at",
            "current_plan_id",
            "name",
            "owner",
            "updated_at"
        ]
    );
    assert_eq!(
        fields(row(&body, idle.as_str())?)?,
        ["campaign_id", "created_at", "name", "owner", "updated_at"]
    );

    std::fs::write(
        dir.path().join("plans").join(format!("{head}.json")),
        "not a plan manifest",
    )?;
    let body = listed(
        &state,
        &reader,
        &format!("/api/v1/campaigns?plan=true{order}"),
    )
    .await?;
    let unread = row(&body, campaign_id.as_str())?;
    assert_eq!(unread["current_plan_id"], json!(head.as_str()));
    assert!(unread.get("plan").is_none(), "{unread}");
    Ok(())
}

/// Where the plan a reader's campaign points at was filed.
#[derive(Clone, Copy)]
enum Filed {
    /// Outside every campaign, by another account.
    Loose,
    /// Into another account's campaign.
    Stranger,
    /// Into another of the reader's campaigns.
    Own,
}

/// A campaign's pointer is a claim, not a proof: a row carries its plan only
/// where the reader's plan scope admits it, judged by the campaign the plan
/// itself names. A page holding the pointing campaign alone still reaches a
/// plan another of the reader's campaigns filed.
#[rstest]
#[case::own_scope_loose_oldest(UserPermission::ReadPlans, Filed::Loose, "", false)]
#[case::own_scope_loose_newest(UserPermission::ReadPlans, Filed::Loose, "&order=newest", false)]
#[case::own_scope_stranger_oldest(UserPermission::ReadPlans, Filed::Stranger, "", false)]
#[case::own_scope_stranger_newest(
    UserPermission::ReadPlans,
    Filed::Stranger,
    "&order=newest",
    false
)]
#[case::every_plan_loose_oldest(UserPermission::ReadAnyPlans, Filed::Loose, "", true)]
#[case::every_plan_loose_newest(UserPermission::ReadAnyPlans, Filed::Loose, "&order=newest", true)]
#[case::every_plan_stranger_oldest(UserPermission::ReadAnyPlans, Filed::Stranger, "", true)]
#[case::every_plan_stranger_newest(
    UserPermission::ReadAnyPlans,
    Filed::Stranger,
    "&order=newest",
    true
)]
#[case::own_campaign_oldest(UserPermission::ReadPlans, Filed::Own, "", true)]
#[case::own_campaign_newest(UserPermission::ReadPlans, Filed::Own, "&order=newest", true)]
#[tokio::test]
async fn test_a_row_keeps_the_readers_plan_scope(
    #[case] grant: UserPermission,
    #[case] filed: Filed,
    #[case] order: &str,
    #[case] drawn: bool,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let reader = registered_user(&state, &[grant, UserPermission::SubmitPlans]).await?;
    let stranger = registered_user(&state, &[UserPermission::SubmitPlans]).await?;
    let (filer, plan_id) = match filed {
        Filed::Loose => {
            let (key, user_id, key_id) = &stranger;
            let loose =
                put_loose_plan(&state, key, user_id, key_id, &client_id, "foreign", 1).await?;
            (None, loose)
        }
        Filed::Stranger => {
            let (filer, plan_id) = running(&state, &stranger, &client_id, "foreign").await?;
            (Some(filer), plan_id)
        }
        Filed::Own => {
            let (filer, plan_id) = running(&state, &reader, &client_id, "filer").await?;
            (Some(filer), plan_id)
        }
    };
    let pointer = put_idle_campaign(&state, &reader.1, "Pointer").await?;
    point(&state, &pointer, &plan_id).await?;

    // A page holding the pointer alone: the newest row, or the rows past the
    // filer's campaign, which is older and, for `read_any_plans`, listed.
    let uri = match (order, filer) {
        ("", Some(filer)) => format!("/api/v1/campaigns?plan=true&cursor={filer}"),
        ("", None) => "/api/v1/campaigns?plan=true".to_string(),
        (order, _) => format!("/api/v1/campaigns?plan=true&limit=1{order}"),
    };
    let body = listed(&state, &reader, &uri).await?;
    assert_eq!(body["items"].as_array().map(Vec::len), Some(1), "{body}");
    let pointing = row(&body, pointer.as_str())?;
    assert_eq!(pointing["current_plan_id"], json!(plan_id.as_str()));
    assert_eq!(pointing.get("plan").is_some(), drawn, "{pointing}");
    Ok(())
}

/// The listing is opened by `manage_orgs` too, which reads the team's
/// campaigns and not their plans, so its rows carry none, nor does one
/// campaign's read.
#[rstest]
#[case::oldest("")]
#[case::newest("&order=newest")]
#[tokio::test]
async fn test_a_reader_without_read_plans_gets_no_plan(#[case] order: &str) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, _client_id) = approved_client(dir.path()).await?;
    let admin = registered_user(
        &state,
        &[
            UserPermission::ManageAnyOrgs,
            UserPermission::SubmitAnyPlans,
            UserPermission::ReadAnyPlans,
        ],
    )
    .await?;
    let acme = create_org(&state, &admin, "acme").await?;
    let (team, _) = created_campaign(
        &state,
        &admin,
        &[("name", "Team"), ("owner", acme.as_str())],
    )
    .await?;
    let org_admin = registered_user(&state, &[UserPermission::ManageOrgs]).await?;
    set_user_org(&state, &org_admin.1, Some(acme.parse()?)).await?;

    let body = listed(
        &state,
        &org_admin,
        &format!("/api/v1/campaigns?plan=true{order}"),
    )
    .await?;
    let drawn = row(&body, &team)?;
    assert!(drawn["current_plan_id"].is_string(), "{drawn}");
    assert!(drawn.get("plan").is_none(), "{drawn}");

    let one = one_campaign(
        &state,
        &org_admin,
        &format!("/api/v1/campaigns/{team}?plan=true"),
    )
    .await?;
    assert_eq!(&one, drawn);
    Ok(())
}

// ── GET /api/v1/campaigns?names=true ───────────────────────────────────────────────

/// `names` names exactly the owners of the rows served: an organization by
/// handle, an account by address, nobody for a filter outside the reach.
#[tokio::test]
async fn test_names_name_the_owners_served() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, _client_id) = approved_client(dir.path()).await?;
    let admin = registered_user(
        &state,
        &[
            UserPermission::ManageAnyOrgs,
            UserPermission::SubmitAnyPlans,
            UserPermission::ReadAnyPlans,
        ],
    )
    .await?;
    let acme = create_org(&state, &admin, "acme").await?;
    let rival = create_org(&state, &admin, "rival").await?;
    let member = registered_user(&state, &[UserPermission::SubmitPlans]).await?;
    set_user_org(&state, &member.1, Some(acme.parse()?)).await?;
    let stranger = registered_user(&state, &[UserPermission::SubmitPlans]).await?;
    created_campaign(&state, &member, &[("name", "Mine")]).await?;
    created_campaign(
        &state,
        &member,
        &[("name", "Team"), ("owner", acme.as_str())],
    )
    .await?;
    created_campaign(&state, &stranger, &[("name", "Theirs")]).await?;
    let member_address = user_address(&state, &member.1).await?;

    let body = listed(&state, &admin, "/api/v1/campaigns?names=true&order=newest").await?;
    assert_eq!(
        body["names"],
        json!({
            member.1.as_str(): member_address,
            stranger.1.as_str(): user_address(&state, &stranger.1).await?,
            acme.as_str(): "acme",
        })
    );

    let body = listed(&state, &member, "/api/v1/campaigns?names=true&owner=mine").await?;
    assert_eq!(body["names"], json!({ member.1.as_str(): member_address }));
    let body = listed(
        &state,
        &member,
        &format!("/api/v1/campaigns?names=true&org_id={rival}"),
    )
    .await?;
    assert_eq!(body["names"], json!({}));
    let body = listed(&state, &member, "/api/v1/campaigns").await?;
    assert!(body.get("names").is_none(), "{body}");
    Ok(())
}

// ── GET /api/v1/campaigns?plan_id= ─────────────────────────────────────────────────

fn listed_ids(body: &Value) -> Vec<&str> {
    body.get("items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|row| row.get("campaign_id").and_then(Value::as_str))
        .collect()
}

/// `plan_id` finds the campaign a plan was filed into, through a superseded
/// plan as well as the head, inside the reach and the owner filter; any other
/// plan is an empty page, and a malformed id, a `cursor` or a bad `limit` a `400`.
#[tokio::test]
async fn test_plan_id_finds_its_campaign_through_any_revision() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let reader = registered_user(
        &state,
        &[UserPermission::SubmitPlans, UserPermission::ReadPlans],
    )
    .await?;
    let stranger = registered_user(&state, &[UserPermission::SubmitPlans]).await?;
    let (campaign_id, first) = created_campaign(&state, &reader, &[("name", "Nightly")]).await?;
    created_campaign(&state, &reader, &[("name", "Other")]).await?;
    let resp = post_plan_document(
        &state,
        &reader,
        &format!("/api/v1/campaigns/{campaign_id}/plans"),
        ONE_CELL_PLAN,
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let head = state
        .campaign_store
        .get_campaign(&campaign_id.parse()?)
        .await?
        .and_then(|campaign| campaign.current_plan_id)
        .context("the revision is the head")?;
    assert_ne!(head, first);
    let (_, theirs) = created_campaign(&state, &stranger, &[("name", "Theirs")]).await?;
    let (key, user_id, key_id) = &reader;
    let loose = put_loose_plan(&state, key, user_id, key_id, &client_id, "loose", 1).await?;

    let uris = [&first, &head].into_iter().flat_map(|plan_id| {
        ["", "&plan=true&names=true&order=newest", "&owner=mine"]
            .map(|extra| format!("/api/v1/campaigns?plan_id={plan_id}{extra}"))
    });
    let (state, reader, campaign) = (&state, &reader, campaign_id.as_str());
    futures::stream::iter(uris.map(anyhow::Ok))
        .try_for_each(|uri| async move {
            let body = listed(state, reader, &uri).await?;
            assert_eq!(listed_ids(&body), [campaign], "{uri}");
            assert_eq!(body["next_cursor"], Value::Null);
            anyhow::Ok(())
        })
        .await?;
    let body = listed(
        state,
        reader,
        &format!("/api/v1/campaigns?plan=true&plan_id={head}"),
    )
    .await?;
    assert_eq!(
        row(&body, &campaign_id)?["current_plan_id"],
        json!(head.as_str())
    );
    assert!(row(&body, &campaign_id)?.get("plan").is_some(), "{body}");

    futures::stream::iter(
        [
            format!("plan_id={theirs}"),
            format!("plan_id={loose}"),
            format!("plan_id={first}&owner=org"),
        ]
        .map(anyhow::Ok),
    )
    .try_for_each(|query| async move {
        let body = listed(state, reader, &format!("/api/v1/campaigns?{query}")).await?;
        assert!(listed_ids(&body).is_empty(), "{query}: {body}");
        anyhow::Ok(())
    })
    .await?;
    futures::stream::iter(
        [
            "plan_id=not%20a%20plan".to_string(),
            format!("plan_id={first}&cursor={campaign_id}"),
            format!("plan_id={first}&limit=0"),
        ]
        .map(anyhow::Ok),
    )
    .try_for_each(|query| async move {
        let (status, body) = read(state, reader, &format!("/api/v1/campaigns?{query}")).await?;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {body}");
        anyhow::Ok(())
    })
    .await?;
    Ok(())
}

// ── GET /api/v1/campaigns/{campaign_id} ───────────────────────────────────────────

/// [`read`] of one campaign, which must answer `200` with a body the app's
/// type describes.
async fn one_campaign(state: &AppState, signer: &UserSigner, uri: &str) -> anyhow::Result<Value> {
    let (status, body) = read(state, signer, uri).await?;
    anyhow::ensure!(status == StatusCode::OK, "{uri}: {status} {body}");
    crate::api_schema::assert_conforms("NamedCampaignView", &body)?;
    Ok(body)
}

/// `campaign_id`'s stored record as its wire body.
async fn record(state: &AppState, campaign_id: &CampaignId) -> anyhow::Result<Value> {
    let campaign = state
        .campaign_store
        .get_campaign(campaign_id)
        .await?
        .context("the campaign is stored")?;
    Ok(serde_json::to_value(pipette_plan_types::Campaign::from(
        campaign,
    ))?)
}

/// One campaign's read takes the listing's flags and answers the listing's
/// row: `plan=true` adds `plan`, `names=true` the owner's name. Unasked, the
/// body is the record.
#[tokio::test]
async fn test_the_campaign_read_takes_the_listing_s_flags() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let reader = registered_user(
        &state,
        &[UserPermission::SubmitPlans, UserPermission::ReadPlans],
    )
    .await?;
    let (campaign_id, head) = running(&state, &reader, &client_id, "running").await?;
    let idle = put_idle_campaign(&state, &reader.1, "Idle").await?;
    let names = json!({ reader.1.as_str(): user_address(&state, &reader.1).await? });
    let uri = format!("/api/v1/campaigns/{campaign_id}");
    let stored = record(&state, &campaign_id).await?;

    let (state, reader, base, expected) = (&state, &reader, uri.as_str(), &stored);
    futures::stream::iter(["", "?names=&plan=", "?names=false&plan=false"].map(anyhow::Ok))
        .try_for_each(|unasked| async move {
            let body = one_campaign(state, reader, &format!("{base}{unasked}")).await?;
            assert_eq!(&body, expected, "{unasked}");
            anyhow::Ok(())
        })
        .await?;

    let listing = listed(state, reader, "/api/v1/campaigns?plan=true").await?;
    let body = one_campaign(state, reader, &format!("{uri}?plan=true")).await?;
    assert_eq!(&body, row(&listing, campaign_id.as_str())?);
    assert_eq!(body["plan"], as_a_row(state, reader, &head).await?);

    let body = one_campaign(state, reader, &format!("{uri}?names=true")).await?;
    let mut named = stored.clone();
    named["names"] = names.clone();
    assert_eq!(body, named);

    // A campaign that runs nothing has no plan to add.
    let body = one_campaign(
        state,
        reader,
        &format!("/api/v1/campaigns/{idle}?plan=true&names=true"),
    )
    .await?;
    let mut named = record(state, &idle).await?;
    named["names"] = names;
    assert_eq!(body, named);
    Ok(())
}

/// The listing and one campaign's read refuse an unknown flag value; an empty
/// one reads as absent.
#[rstest]
#[case::bogus_names("names=maybe", StatusCode::BAD_REQUEST)]
#[case::bogus_plan("plan=1", StatusCode::BAD_REQUEST)]
#[case::empty_names("names=", StatusCode::OK)]
#[case::empty_plan("plan=", StatusCode::OK)]
#[tokio::test]
async fn test_the_reads_check_their_flags(
    #[values(false, true)] one: bool,
    #[case] query: &str,
    #[case] status: StatusCode,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, _client_id) = approved_client(dir.path()).await?;
    let reader = registered_user(
        &state,
        &[UserPermission::SubmitPlans, UserPermission::ReadPlans],
    )
    .await?;
    let campaign_id = put_idle_campaign(&state, &reader.1, "Idle").await?;
    let uri = if one {
        format!("/api/v1/campaigns/{campaign_id}?{query}")
    } else {
        format!("/api/v1/campaigns?{query}")
    };
    let (answered, body) = read(&state, &reader, &uri).await?;
    assert_eq!(answered, status, "{uri}: {body}");
    if status == StatusCode::OK {
        assert!(body.get("names").is_none(), "{uri}: {body}");
    }
    Ok(())
}

// ── POST /api/v1/campaigns ─────────────────────────────────────────────────────────

/// The form's two answers conform to the app's types: the preview's report and
/// the created campaign with its first plan.
#[tokio::test]
async fn test_the_create_and_its_preview_conform() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, _client_id) = approved_client(dir.path()).await?;
    let owner = registered_user(&state, &[UserPermission::SubmitPlans]).await?;

    let resp = post_campaign(&state, &owner, &[("name", "Nightly"), ("dry_run", "true")]).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let preview = body_json(resp).await?;
    crate::api_schema::assert_conforms("PlanDryRun", &preview)?;
    assert_eq!(preview["bodies"].as_array().map(Vec::len), Some(1));

    let resp = post_campaign(
        &state,
        &owner,
        &[("name", "Nightly LFM2"), ("note", "nightly-1")],
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let created = body_json(resp).await?;
    crate::api_schema::assert_conforms("CampaignCreated", &created)?;
    assert_eq!(
        json_str(&created["plan"], "plan_id")?,
        json_str(&created, "current_plan_id")?
    );
    Ok(())
}
