//! The campaign group routes, which only the SQL store backs.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroUsize;

use anyhow::Context;
use axum::http::StatusCode;
use chrono::{DateTime, SubsecRound, Utc};
use futures::TryStreamExt;
use httpmock::{Method::POST, Mock, MockServer};
use rstest::rstest;
use serde_json::{Value, json};

use pipette_core::BUILD_VERSION;
use pipette_core::campaign::{Campaign, CampaignOwner, GroupFiling, GroupPolicyState, Refusal};
use pipette_core::organization::Organization;
use pipette_core::plan::{JobExpiry, PlanManifest, SettledOutcome, sample_manifest};
use pipette_core::user_permission::UserPermissionGroup;
use pipette_httpapi::state::AppState;
use pipette_plan_types::{
    AuditAction, AuditOutcome, AuditTarget, CampaignGroupId, CampaignGroupPolicy, CampaignId,
    ExpiresAt, JobId, PlanId, PlanStatus, PolicyKind, PolicyRerun, RerunOutcome, UserPermission,
    UserStatus,
};
use pipette_store::AuditFilter;
use pipette_web_testkit::{
    UserSigner, body_json, created_campaign, json_str, put_idle_campaign_of, registered_user,
    seed_avail, seed_org, seeded_object_state, seeded_sql_state, set_user_org, set_user_status,
    signed_empty, signed_json, with_duckdb,
};

const GROUPS: &str = "/api/v1/campaign-groups";

/// Owns campaigns and reads them.
const OPERATOR: &[UserPermission] = &[UserPermission::SubmitPlans, UserPermission::ReadPlans];

/// Owns campaigns and re-runs them, so it may leave a group's policies on.
const RERUNNER: &[UserPermission] = &[
    UserPermission::SubmitPlans,
    UserPermission::ReadPlans,
    UserPermission::CancelPlans,
    UserPermission::ReadClients,
];

/// A request as `signer`: its status, and its body or `null` when it has none.
async fn send(
    state: &AppState,
    signer: &UserSigner,
    method: &str,
    uri: &str,
    body: Option<&Value>,
) -> anyhow::Result<(StatusCode, Value)> {
    let resp = match body {
        Some(body) => signed_json(state, signer, method, uri, body).await?,
        None => signed_empty(state, signer, method, uri).await?,
    };
    let status = resp.status();
    if status == StatusCode::NO_CONTENT {
        return Ok((status, Value::Null));
    }
    Ok((status, body_json(resp).await?))
}

async fn created_group(
    state: &AppState,
    signer: &UserSigner,
    body: Value,
) -> anyhow::Result<String> {
    let (status, answer) = send(state, signer, "POST", GROUPS, Some(&body)).await?;
    anyhow::ensure!(status == StatusCode::CREATED, "{status} {answer}");
    Ok(json_str(&answer, "group_id")?.to_owned())
}

/// An `OPERATOR` in a fresh `acme` organization.
async fn org_operator(state: &AppState) -> anyhow::Result<(Organization, UserSigner)> {
    let org = seed_org(state, "acme").await?;
    let operator = registered_user(state, OPERATOR).await?;
    set_user_org(state, &operator.1, Some(org.org_id.clone())).await?;
    Ok((org, operator))
}

/// A campaign reader in `org` that reads `campaign_id` alone.
async fn campaign_reader(
    state: &AppState,
    org: &Organization,
    campaign_id: &str,
) -> anyhow::Result<UserSigner> {
    let reader = registered_user(state, &[UserPermission::ReadPlans]).await?;
    set_user_org(state, &reader.1, Some(org.org_id.clone())).await?;
    let mut account = state
        .user_store
        .get_user(&reader.1)
        .await?
        .context("the reader is stored")?;
    account.campaign_scope = Some(BTreeSet::from([CampaignId::try_new(campaign_id)?]));
    state.user_store.put_user(&account).await?;
    Ok(reader)
}

async fn team_group(
    state: &AppState,
    signer: &UserSigner,
    org: &Organization,
) -> anyhow::Result<String> {
    let body = json!({ "name": "team", "owner": org.org_id.as_str() });
    created_group(state, signer, body).await
}

async fn campaign_of(state: &AppState, owner: &CampaignOwner) -> anyhow::Result<String> {
    let campaign = put_idle_campaign_of(state, owner.clone(), "aa-perf").await?;
    Ok(campaign.campaign_id.to_string())
}

/// Set the campaign's owner past the routes, which keeps its group membership.
async fn owner_written(
    state: &AppState,
    campaign_id: &str,
    owner: CampaignOwner,
) -> anyhow::Result<()> {
    let mut campaign = state
        .campaign_store
        .get_campaign(&CampaignId::try_new(campaign_id)?)
        .await?
        .context("the campaign is stored")?;
    campaign.owner = owner;
    state.campaign_store.put_campaign(&campaign).await
}

async fn campaign_record(
    state: &AppState,
    signer: &UserSigner,
    campaign_id: &str,
) -> anyhow::Result<Value> {
    let uri = format!("/api/v1/campaigns/{campaign_id}");
    let (status, record) = send(state, signer, "GET", &uri, None).await?;
    anyhow::ensure!(status == StatusCode::OK, "{status} {record}");
    Ok(record)
}

async fn overview_group(
    state: &AppState,
    signer: &UserSigner,
    campaign_id: &str,
) -> anyhow::Result<Value> {
    let uri = format!("/api/v1/campaigns/{campaign_id}/overview");
    let (status, overview) = send(state, signer, "GET", &uri, None).await?;
    anyhow::ensure!(status == StatusCode::OK, "{status} {overview}");
    crate::api_schema::assert_conforms("NamedCampaignOverview", &overview)?;
    overview
        .get("group")
        .cloned()
        .context("an overview has a group")
}

fn personal(signer: &UserSigner) -> CampaignOwner {
    CampaignOwner::User {
        user_id: signer.1.clone(),
    }
}

async fn add(
    state: &AppState,
    signer: &UserSigner,
    group_id: &str,
    campaign_ids: &[&str],
) -> anyhow::Result<(StatusCode, Value)> {
    let uri = format!("{GROUPS}/{group_id}/members");
    let body = json!({ "campaign_ids": campaign_ids });
    send(state, signer, "POST", &uri, Some(&body)).await
}

async fn added(
    state: &AppState,
    signer: &UserSigner,
    group_id: &str,
    campaign_ids: &[&str],
) -> anyhow::Result<()> {
    let (status, answer) = add(state, signer, group_id, campaign_ids).await?;
    anyhow::ensure!(status == StatusCode::NO_CONTENT, "{status} {answer}");
    Ok(())
}

async fn member_ids(
    state: &AppState,
    signer: &UserSigner,
    group_id: &str,
) -> anyhow::Result<BTreeSet<String>> {
    let (status, group) = send(state, signer, "GET", &format!("{GROUPS}/{group_id}"), None).await?;
    anyhow::ensure!(status == StatusCode::OK, "{status} {group}");
    ids(group.get("members"), "campaign_id")
}

fn ids(rows: Option<&Value>, key: &str) -> anyhow::Result<BTreeSet<String>> {
    rows.and_then(Value::as_array)
        .context("an array")?
        .iter()
        .map(|row| Ok(json_str(row, key)?.to_owned()))
        .collect()
}

#[tokio::test]
async fn a_group_is_created_read_renamed_and_deleted() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(sql) = seeded_sql_state(dir.path()).await? else {
        return Ok(());
    };
    let state = &sql.state;
    let owner = registered_user(state, OPERATOR).await?;

    let body = json!({ "name": "  aa-perf  " });
    crate::api_schema::assert_conforms("CreateCampaignGroupRequest", &body)?;
    let (status, created) = send(state, &owner, "POST", GROUPS, Some(&body)).await?;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let group_id = json_str(&created, "group_id")?.to_owned();
    assert!(group_id.starts_with("group_"), "{group_id}");
    assert_eq!(created["name"], "aa-perf");
    assert_eq!(
        created["owner"],
        json!({ "type": "user", "user_id": owner.1.as_str() })
    );

    let uri = format!("{GROUPS}/{group_id}");
    let (status, read) = send(state, &owner, "GET", &uri, None).await?;
    assert_eq!(status, StatusCode::OK, "{read}");
    crate::api_schema::assert_conforms("CampaignGroupView", &read)?;
    let mut expected = created.clone();
    expected["members"] = json!([]);
    assert_eq!(read, expected);

    let rename = json!({ "name": "aa-perf nightly" });
    let (status, renamed) = send(state, &owner, "PATCH", &uri, Some(&rename)).await?;
    assert_eq!(status, StatusCode::OK, "{renamed}");
    assert_eq!(
        (&renamed["name"], &renamed["changed"]),
        (&json!("aa-perf nightly"), &json!(true))
    );
    let (status, again) = send(state, &owner, "PATCH", &uri, Some(&rename)).await?;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(
        (&again["changed"], &again["updated_at"]),
        (&json!(false), &renamed["updated_at"])
    );

    let (status, answer) = send(state, &owner, "DELETE", &uri, None).await?;
    assert_eq!(status, StatusCode::NO_CONTENT, "{answer}");
    let revive = json!({ "name": "revived" });
    let (status, answer) = send(state, &owner, "PATCH", &uri, Some(&revive)).await?;
    assert_eq!(status, StatusCode::NOT_FOUND, "{answer}");
    let (status, answer) = send(state, &owner, "GET", &uri, None).await?;
    assert_eq!(status, StatusCode::NOT_FOUND, "{answer}");
    Ok(())
}

/// The listing and a read reach the groups of every owner whose campaigns the
/// caller reads: its own and its organization's. A campaign reader reaches
/// none, and nobody creates a group for an owner outside its scope.
#[tokio::test]
async fn the_groups_read_are_the_campaigns_read() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(sql) = seeded_sql_state(dir.path()).await? else {
        return Ok(());
    };
    let state = &sql.state;
    let (org, alice) = org_operator(state).await?;
    let bob = registered_user(state, OPERATOR).await?;
    let reader = registered_user(state, &[UserPermission::ReadPlans]).await?;
    set_user_org(state, &reader.1, Some(org.org_id.clone())).await?;
    let team_owner = CampaignOwner::Org {
        org_id: org.org_id.clone(),
    };
    let listed = campaign_of(state, &team_owner).await?;
    let scoped = campaign_reader(state, &org, &listed).await?;

    let own = created_group(state, &alice, json!({ "name": "mine" })).await?;
    let team = team_group(state, &alice, &org).await?;
    let others = created_group(state, &bob, json!({ "name": "bob's" })).await?;

    futures::stream::iter(
        [
            (&alice, "", vec![&own, &team]),
            (&alice, "?owner=org", vec![&team]),
            (&reader, "", vec![&team]),
            (&scoped, "", vec![]),
            (&bob, "", vec![&others]),
        ]
        .map(anyhow::Ok),
    )
    .try_for_each(|(signer, query, expected)| async move {
        let (status, page) = send(state, signer, "GET", &format!("{GROUPS}{query}"), None).await?;
        assert_eq!(status, StatusCode::OK, "{page}");
        let expected: BTreeSet<String> = expected.into_iter().cloned().collect();
        assert_eq!(ids(page.get("items"), "group_id")?, expected, "{query}");
        anyhow::Ok(())
    })
    .await?;

    let (status, answer) = send(state, &scoped, "GET", &format!("{GROUPS}/{team}"), None).await?;
    assert_eq!(
        (status, answer["error"].as_str()),
        (
            StatusCode::FORBIDDEN,
            Some("a campaign reader reads no campaign group")
        )
    );
    let theirs = json!({ "name": "theirs", "owner": bob.1.as_str() });
    let (status, answer) = send(state, &alice, "POST", GROUPS, Some(&theirs)).await?;
    assert_eq!(status, StatusCode::FORBIDDEN, "{answer}");
    Ok(())
}

#[tokio::test]
async fn a_stranger_reaches_no_route_of_a_group() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(sql) = seeded_sql_state(dir.path()).await? else {
        return Ok(());
    };
    let state = &sql.state;
    let (org, owner) = org_operator(state).await?;
    let stranger = registered_user(state, OPERATOR).await?;
    let own = created_group(state, &owner, json!({ "name": "mine" })).await?;
    let team = team_group(state, &owner, &org).await?;
    let campaign = campaign_of(state, &personal(&stranger)).await?;

    let (name, members) = (
        &json!({ "name": "renamed" }),
        &json!({ "campaign_ids": [campaign] }),
    );
    let refused = "this campaign group belongs to another owner";
    let campaign = campaign.as_str();
    let routes = [own, team].into_iter().flat_map(move |group| {
        let uri = format!("{GROUPS}/{group}");
        [
            ("GET", uri.clone(), None),
            ("PATCH", uri.clone(), Some(name)),
            ("DELETE", uri.clone(), None),
            ("POST", format!("{uri}/members"), Some(members)),
            ("DELETE", format!("{uri}/members/{campaign}"), None),
        ]
    });
    let stranger = &stranger;
    futures::stream::iter(routes.map(anyhow::Ok))
        .try_for_each(|(method, uri, body)| async move {
            let (status, answer) = send(state, stranger, method, &uri, body).await?;
            assert_eq!(
                (status, answer["error"].as_str()),
                (StatusCode::FORBIDDEN, Some(refused)),
                "{method} {uri}"
            );
            anyhow::Ok(())
        })
        .await
}

/// Each member is its campaign's record. One a racing hand-off left under an
/// owner the reader does not reach is left out.
#[tokio::test]
async fn a_read_lists_the_members_the_reader_reads() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(sql) = seeded_sql_state(dir.path()).await? else {
        return Ok(());
    };
    let state = &sql.state;
    let owner = registered_user(state, OPERATOR).await?;
    let group = created_group(state, &owner, json!({ "name": "aa-perf" })).await?;
    let (planned, _) = created_campaign(state, &owner, &[("name", "aa-perf s26")]).await?;
    let edit = json!({ "labels": { "device": "s26" }, "state": "archived" });
    let uri = format!("/api/v1/campaigns/{planned}");
    let (status, answer) = send(state, &owner, "PATCH", &uri, Some(&edit)).await?;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let idle = campaign_of(state, &personal(&owner)).await?;
    added(state, &owner, &group, &[&idle, &planned]).await?;

    let (status, read) = send(state, &owner, "GET", &format!("{GROUPS}/{group}"), None).await?;
    assert_eq!(status, StatusCode::OK, "{read}");
    let mut expected = [
        campaign_record(state, &owner, &planned).await?,
        campaign_record(state, &owner, &idle).await?,
    ];
    expected.sort_by_key(|member| member["campaign_id"].to_string());
    assert_eq!(read["members"], json!(expected));

    let stranger = registered_user(state, OPERATOR).await?;
    owner_written(state, &idle, personal(&stranger)).await?;
    assert_eq!(
        member_ids(state, &owner, &group).await?,
        BTreeSet::from([planned])
    );
    Ok(())
}

/// Handing a campaign over takes it out of its group, free to join one of its
/// new owner's.
#[tokio::test]
async fn a_hand_off_takes_a_campaign_out_of_its_group() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(sql) = seeded_sql_state(dir.path()).await? else {
        return Ok(());
    };
    let state = &sql.state;
    let (org, owner) = org_operator(state).await?;
    let mine = created_group(state, &owner, json!({ "name": "mine" })).await?;
    let (kept, handed) = (
        campaign_of(state, &personal(&owner)).await?,
        campaign_of(state, &personal(&owner)).await?,
    );
    added(state, &owner, &mine, &[&kept, &handed]).await?;

    let to_team = json!({ "owner": { "type": "org", "org_id": org.org_id.as_str() } });
    let uri = format!("/api/v1/campaigns/{handed}/owner");
    let (status, answer) = send(state, &owner, "PUT", &uri, Some(&to_team)).await?;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(
        member_ids(state, &owner, &mine).await?,
        BTreeSet::from([kept])
    );

    let team = team_group(state, &owner, &org).await?;
    added(state, &owner, &team, &[&handed]).await?;
    assert_eq!(
        member_ids(state, &owner, &team).await?,
        BTreeSet::from([handed])
    );
    Ok(())
}

/// A hand-off to the owner the campaign already has keeps it in a group of that
/// owner but takes it out of a group of another owner, so a retry finishes a
/// removal that failed. The audit log records that removal, naming the group.
#[tokio::test]
async fn a_repeated_hand_off_takes_a_campaign_out_of_its_group() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(sql) = seeded_sql_state(dir.path()).await? else {
        return Ok(());
    };
    let state = &sql.state;
    let owner = registered_user(state, OPERATOR).await?;
    let taker = registered_user(state, OPERATOR).await?;
    let group = created_group(state, &owner, json!({ "name": "aa-perf" })).await?;
    let campaign = campaign_of(state, &personal(&owner)).await?;
    added(state, &owner, &group, &[&campaign]).await?;
    let uri = format!("/api/v1/campaigns/{campaign}/owner");
    let to_owner = json!({ "owner": { "type": "user", "user_id": owner.1.as_str() } });
    let (status, answer) = send(state, &owner, "PUT", &uri, Some(&to_owner)).await?;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(
        member_ids(state, &owner, &group).await?,
        BTreeSet::from([campaign.clone()])
    );
    owner_written(state, &campaign, personal(&taker)).await?;

    let to_taker = json!({ "owner": { "type": "user", "user_id": taker.1.as_str() } });
    let (status, answer) = send(state, &taker, "PUT", &uri, Some(&to_taker)).await?;
    assert_eq!(
        (status, &answer["changed"]),
        (StatusCode::OK, &json!(false)),
        "{answer}"
    );
    let groups = state
        .campaign_group_store
        .as_deref()
        .context("the SQL store keeps groups")?;
    let campaign_id = CampaignId::try_new(campaign.as_str())?;
    assert_eq!(groups.group_of(&campaign_id).await?, None);

    // A success that changes nothing writes no event, so this one records the removal.
    let left = AuditTarget::CampaignGroup {
        group_id: CampaignGroupId::try_new(group.as_str())?,
    };
    let hand_offs: Vec<(AuditOutcome, bool)> = state
        .audit_store
        .as_deref()
        .context("the SQL store keeps the audit log")?
        .list_events(
            &AuditFilter::default(),
            None,
            NonZeroUsize::new(100).context("a page size")?,
        )
        .await?
        .items
        .into_iter()
        .filter(|event| {
            matches!(&event.action, AuditAction::Http { method, path, .. }
                if method == "PUT" && *path == uri)
        })
        .map(|event| (event.outcome, event.targets.contains(&left)))
        .collect();
    assert_eq!(hand_offs, [(AuditOutcome::Succeeded, true)]);
    Ok(())
}

/// A deleted group's campaigns are kept and join another group. Removing a
/// member leaves the others, and removing it again is a no-op.
#[tokio::test]
async fn deleting_a_group_or_a_member_keeps_the_campaigns() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(sql) = seeded_sql_state(dir.path()).await? else {
        return Ok(());
    };
    let state = &sql.state;
    let owner = registered_user(state, OPERATOR).await?;
    let first = created_group(state, &owner, json!({ "name": "first" })).await?;
    let second = created_group(state, &owner, json!({ "name": "second" })).await?;
    let (kept, removed) = (
        campaign_of(state, &personal(&owner)).await?,
        campaign_of(state, &personal(&owner)).await?,
    );
    added(state, &owner, &first, &[&kept, &removed]).await?;

    let (signer, leave) = (&owner, &format!("{GROUPS}/{first}/members/{removed}"));
    futures::stream::iter((0..2).map(anyhow::Ok))
        .try_for_each(|_| async move {
            let (status, answer) = send(state, signer, "DELETE", leave, None).await?;
            assert_eq!(status, StatusCode::NO_CONTENT, "{answer}");
            anyhow::Ok(())
        })
        .await?;
    assert_eq!(
        member_ids(state, &owner, &first).await?,
        BTreeSet::from([kept.clone()])
    );

    let (status, answer) = add(state, &owner, &second, &[&kept]).await?;
    assert_eq!(status, StatusCode::CONFLICT, "{answer}");
    let (status, answer) =
        send(state, &owner, "DELETE", &format!("{GROUPS}/{first}"), None).await?;
    assert_eq!(status, StatusCode::NO_CONTENT, "{answer}");
    added(state, &owner, &second, &[&kept, &removed]).await?;
    assert_eq!(
        member_ids(state, &owner, &second).await?,
        BTreeSet::from([kept.clone(), removed])
    );
    let uri = format!("/api/v1/campaigns/{kept}");
    let (status, answer) = send(state, &owner, "GET", &uri, None).await?;
    assert_eq!(status, StatusCode::OK, "{answer}");
    Ok(())
}

/// One refused campaign adds none. An organization's member fills its group
/// and does not rename it, which takes `manage_orgs`.
#[tokio::test]
async fn adding_members_is_all_or_nothing() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(sql) = seeded_sql_state(dir.path()).await? else {
        return Ok(());
    };
    let state = &sql.state;
    let (org, member) = org_operator(state).await?;
    let stranger = registered_user(state, OPERATOR).await?;
    let group = team_group(state, &member, &org).await?;
    let other = team_group(state, &member, &org).await?;
    let owner = CampaignOwner::Org {
        org_id: org.org_id.clone(),
    };
    let (a, b, taken) = (
        campaign_of(state, &owner).await?,
        campaign_of(state, &owner).await?,
        campaign_of(state, &owner).await?,
    );
    let mine = campaign_of(state, &personal(&member)).await?;
    let strangers = campaign_of(state, &personal(&stranger)).await?;
    added(state, &member, &other, &[&taken]).await?;

    let unknown = "campaign_0195c3e2a4f37b8c".to_owned();
    let (signer, group_id) = (&member, group.as_str());
    futures::stream::iter(
        [
            (
                [&a, &mine],
                StatusCode::CONFLICT,
                format!(
                    "campaign {mine} belongs to {}, not to the group's owner {}",
                    member.1, org.org_id
                ),
            ),
            (
                [&b, &taken],
                StatusCode::CONFLICT,
                format!("campaign {taken} is in campaign group {other}"),
            ),
            (
                [&a, &strangers],
                StatusCode::FORBIDDEN,
                "this campaign belongs to another owner".to_owned(),
            ),
            (
                [&a, &unknown],
                StatusCode::NOT_FOUND,
                format!("no campaign {unknown}"),
            ),
        ]
        .map(anyhow::Ok),
    )
    .try_for_each(|(campaigns, status, error)| async move {
        let campaigns = campaigns.map(String::as_str);
        let (got, answer) = add(state, signer, group_id, &campaigns).await?;
        assert_eq!(
            (got, answer["error"].as_str()),
            (status, Some(error.as_str())),
            "{campaigns:?}"
        );
        anyhow::Ok(())
    })
    .await?;
    assert_eq!(member_ids(state, &member, &group).await?, BTreeSet::new());

    added(state, &member, &group, &[&a, &b, &a]).await?;
    assert_eq!(
        member_ids(state, &member, &group).await?,
        BTreeSet::from([a, b])
    );

    let rename = json!({ "name": "renamed" });
    let uri = format!("{GROUPS}/{group}");
    let (status, answer) = send(state, &member, "PATCH", &uri, Some(&rename)).await?;
    assert_eq!(
        (status, answer["error"].as_str()),
        (
            StatusCode::FORBIDDEN,
            Some("administering your organization's campaigns needs manage_orgs")
        )
    );
    Ok(())
}

/// An organization's administrator renames and deletes its groups, and a
/// disabled member's as it winds the member down. Another organization's
/// administrator does neither.
#[tokio::test]
async fn an_org_admin_administers_its_groups_and_a_disabled_member_s() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(sql) = seeded_sql_state(dir.path()).await? else {
        return Ok(());
    };
    let state = &sql.state;
    let (org, member) = org_operator(state).await?;
    let rival = seed_org(state, "rival").await?;
    let admin = registered_user(state, &[UserPermission::ManageOrgs]).await?;
    let rival_admin = registered_user(state, &[UserPermission::ManageOrgs]).await?;
    set_user_org(state, &admin.1, Some(org.org_id.clone())).await?;
    set_user_org(state, &rival_admin.1, Some(rival.org_id.clone())).await?;
    let team = format!("{GROUPS}/{}", team_group(state, &member, &org).await?);
    let own = json!({ "name": "mine" });
    let own = format!("{GROUPS}/{}", created_group(state, &member, own).await?);
    let rename = &json!({ "name": "renamed" });

    let (status, answer) = send(state, &admin, "PATCH", &own, Some(rename)).await?;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "an active member's: {answer}"
    );
    set_user_status(state, &member.1, UserStatus::Disabled).await?;

    futures::stream::iter(
        [
            (&rival_admin, "PATCH", &own, StatusCode::FORBIDDEN),
            (&rival_admin, "DELETE", &own, StatusCode::FORBIDDEN),
            (&admin, "PATCH", &team, StatusCode::OK),
            (&admin, "PATCH", &own, StatusCode::OK),
            (&admin, "DELETE", &team, StatusCode::NO_CONTENT),
            (&admin, "DELETE", &own, StatusCode::NO_CONTENT),
        ]
        .map(anyhow::Ok),
    )
    .try_for_each(|(signer, method, uri, expected)| async move {
        let body = (method == "PATCH").then_some(rename);
        let (status, answer) = send(state, signer, method, uri, body).await?;
        assert_eq!(status, expected, "{method} {uri}: {answer}");
        anyhow::Ok(())
    })
    .await
}

/// Every kind's defaults, as a group that stores none answers them.
fn default_policies(group_id: &str) -> Value {
    let policy = |kind: &str, max_reruns: u32, settings: Value| {
        json!({
            "group_id": group_id,
            "kind": kind,
            "enabled": false,
            "max_reruns": max_reruns,
            "settings": settings,
            "enabled_by": null,
            "enabled_at": null,
        })
    };
    let spread =
        json!({ "percent": 5.0, "short_repetition_ms": 1000.0, "short_repetition_percent": 10.0 });
    let failed = json!({ "codes": { "runtime_error": 2, "device_error": 2, "unspecified": 2 } });
    json!([
        policy("swap", 5, json!({})),
        policy("spread", 3, spread),
        policy("failed", 0, failed),
        policy("ttft_over_e2e", 3, json!({})),
        policy("model_params", 0, json!({})),
        policy("smaller_failed", 0, json!({})),
    ])
}

async fn policies_read(
    state: &AppState,
    signer: &UserSigner,
    group_id: &str,
) -> anyhow::Result<Value> {
    let uri = format!("{GROUPS}/{group_id}/policies");
    let (status, read) = send(state, signer, "GET", &uri, None).await?;
    anyhow::ensure!(status == StatusCode::OK, "{status} {read}");
    crate::api_schema::assert_conforms("CampaignGroupPolicies", &read)?;
    Ok(read)
}

async fn policies(state: &AppState, signer: &UserSigner, group_id: &str) -> anyhow::Result<Value> {
    policies_read(state, signer, group_id)
        .await?
        .get("policies")
        .cloned()
        .context("an answer with policies")
}

async fn policy_written(
    state: &AppState,
    signer: &UserSigner,
    group_id: &str,
    kind: &str,
    body: &Value,
) -> anyhow::Result<()> {
    let uri = format!("{GROUPS}/{group_id}/policies/{kind}");
    let (status, answer) = send(state, signer, "PUT", &uri, Some(body)).await?;
    anyhow::ensure!(status == StatusCode::OK, "{uri}: {status} {answer}");
    Ok(())
}

/// `record` as a write answers it.
fn answered(record: &Value, changed: bool) -> anyhow::Result<Value> {
    let mut answer = record.clone();
    answer
        .as_object_mut()
        .context("a record is an object")?
        .insert("changed".into(), changed.into());
    Ok(answer)
}

fn policy_body(enabled: bool, max_reruns: i64, settings: Value, base: &Value) -> Value {
    json!({
        "enabled": enabled,
        "max_reruns": max_reruns,
        "settings": settings,
        "base": {
            "enabled": base["enabled"],
            "max_reruns": base["max_reruns"],
            "settings": base["settings"],
        },
    })
}

/// A failed policy's body keeping `base`'s codes.
fn kept_body(enabled: bool, base: &Value) -> Value {
    policy_body(enabled, 0, base["settings"].clone(), base)
}

async fn org_user(
    state: &AppState,
    org: &Organization,
    permissions: &[UserPermission],
) -> anyhow::Result<UserSigner> {
    let user = registered_user(state, permissions).await?;
    set_user_org(state, &user.1, Some(org.org_id.clone())).await?;
    Ok(user)
}

/// A group answers every kind's defaults until a write; one other than a Turn
/// off replaces the policy whole. Turning a policy on records who did it and
/// when, a write that keeps it on keeps both, and turning it off clears them. A
/// write asking for the policy the group already has writes nothing.
#[tokio::test]
async fn a_policy_write_replaces_the_policy() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(sql) = seeded_sql_state(dir.path()).await? else {
        return Ok(());
    };
    let state = &sql.state;
    let owner = registered_user(state, RERUNNER).await?;
    let server = registered_user(state, UserPermissionGroup::ServerAdmin.permissions()).await?;
    let group = created_group(state, &owner, json!({ "name": "aa-perf" })).await?;
    let mut expected = default_policies(&group);
    assert_eq!(policies(state, &owner, &group).await?, expected);
    let uri = format!("{GROUPS}/{group}/policies/swap");
    let defaults = policy_body(false, 5, json!({}), &expected[0]);
    let (status, kept) = send(state, &owner, "PUT", &uri, Some(&defaults)).await?;
    assert_eq!(status, StatusCode::OK, "{kept}");
    assert_eq!(kept, answered(&expected[0], false)?);
    // A swap policy marks a flagged cell failed, so it acts with no re-run.
    let at_once = policy_body(false, 0, json!({}), &expected[0]);
    let (status, marked) = send(state, &owner, "PUT", &uri, Some(&at_once)).await?;
    assert_eq!(status, StatusCode::OK, "{marked}");
    expected[0]["max_reruns"] = json!(0);
    assert_eq!(marked, answered(&expected[0], true)?);

    let uri = format!("{GROUPS}/{group}/policies/spread");
    let tighten = policy_body(true, 1, json!({ "percent": 7.5 }), &expected[1]);
    crate::api_schema::assert_conforms("SetCampaignGroupPolicyRequest", &tighten)?;
    let (status, tightened) = send(state, &owner, "PUT", &uri, Some(&tighten)).await?;
    assert_eq!(status, StatusCode::OK, "{tightened}");
    crate::api_schema::assert_conforms("ChangedCampaignGroupPolicy", &tightened)?;
    let spread = &mut expected[1];
    spread["enabled"] = json!(true);
    spread["max_reruns"] = json!(1);
    spread["settings"] = json!({ "percent": 7.5 });
    spread["enabled_by"] = json!(owner.1.as_str());
    assert!(tightened["enabled_at"].is_string(), "{tightened}");
    spread["enabled_at"] = tightened["enabled_at"].clone();
    assert_eq!(tightened, answered(spread, true)?);
    // Another writer keeping it on leaves who turned it on, and when.
    let keep = policy_body(true, 1, json!({ "percent": 7.5 }), spread);
    let (status, again) = send(state, &server, "PUT", &uri, Some(&keep)).await?;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again, answered(spread, false)?);

    // A short repetition's time and percent are kept as sent.
    let short =
        json!({ "percent": 7.5, "short_repetition_ms": 1000.0, "short_repetition_percent": 10.0 });
    let raise = policy_body(true, 15, short.clone(), spread);
    crate::api_schema::assert_conforms("SetCampaignGroupPolicyRequest", &raise)?;
    let (status, raised) = send(state, &server, "PUT", &uri, Some(&raise)).await?;
    assert_eq!(status, StatusCode::OK, "{raised}");
    spread["max_reruns"] = json!(15);
    spread["settings"] = short.clone();
    assert_eq!(raised, answered(spread, true)?);

    let off = policy_body(false, 15, json!({ "percent": 7.5 }), spread);
    let (status, turned_off) = send(state, &server, "PUT", &uri, Some(&off)).await?;
    assert_eq!(status, StatusCode::OK, "{turned_off}");
    spread["enabled"] = json!(false);
    spread["enabled_by"] = Value::Null;
    spread["enabled_at"] = Value::Null;
    assert_eq!(turned_off, answered(spread, true)?);
    let raise = policy_body(true, 15, short, spread);
    let (status, turned_on) = send(state, &server, "PUT", &uri, Some(&raise)).await?;
    assert_eq!(status, StatusCode::OK, "{turned_on}");
    spread["enabled"] = json!(true);
    spread["enabled_by"] = json!(server.1.as_str());
    assert!(turned_on["enabled_at"].is_string(), "{turned_on}");
    spread["enabled_at"] = turned_on["enabled_at"].clone();
    assert_eq!(turned_on, answered(spread, true)?);

    // Failed's codes and their re-runs are kept as sent, any failure code among them.
    let uri = format!("{GROUPS}/{group}/policies/failed");
    let codes = json!({ "codes": { "harness_error": 3, "out_of_memory": 1 } });
    let pick = policy_body(false, 0, codes.clone(), &expected[2]);
    crate::api_schema::assert_conforms("SetCampaignGroupPolicyRequest", &pick)?;
    let (status, picked) = send(state, &owner, "PUT", &uri, Some(&pick)).await?;
    assert_eq!(status, StatusCode::OK, "{picked}");
    expected[2]["settings"] = codes;
    assert_eq!(picked, answered(&expected[2], true)?);
    assert_eq!(policies(state, &owner, &group).await?, expected);
    Ok(())
}

/// An unknown kind or field, a field left out (`base` among them), settings
/// the kind does not take, a malformed field, an outcome or a re-run count
/// outside the kind's range is a `400` that writes nothing, and a group that
/// is not there is a `404`.
#[tokio::test]
async fn a_policy_write_refuses_what_its_kind_does_not_take() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(sql) = seeded_sql_state(dir.path()).await? else {
        return Ok(());
    };
    let state = &sql.state;
    let owner = registered_user(state, RERUNNER).await?;
    let group = created_group(state, &owner, json!({ "name": "aa-perf" })).await?;
    let percent = Some("a spread policy takes settings.percent, a finite number above 0");
    let codes = Some(
        "a failed policy takes settings.codes alone, at least one failure code, each with its \
         re-runs, 1 to 15",
    );
    let missing = "group_0195c3e2a4f37b8c";
    let bad = StatusCode::BAD_REQUEST;
    let (signer, group_id) = (&owner, group.as_str());
    let base = default_policies(group_id)[0].clone();
    let on = |settings: Value| policy_body(true, 3, settings, &base);
    let at = |max_reruns: i64| policy_body(true, max_reruns, json!({}), &base);
    let failed = |settings: Value| policy_body(true, 0, settings, &base);
    // What becomes of a cell still flagged is the check's own.
    let mut outcome = on(json!({}));
    outcome
        .as_object_mut()
        .context("a body is an object")?
        .insert("on_limit".into(), "fail".into());
    futures::stream::iter(
        [
            (group_id, "latency", on(json!({})), bad, None),
            (
                group_id,
                "spread",
                on(json!({ "percent": 0 })),
                bad,
                percent,
            ),
            (
                group_id,
                "spread",
                on(json!({ "percent": -2.5 })),
                bad,
                percent,
            ),
            (group_id, "spread", on(json!({})), bad, percent),
            (
                group_id,
                "spread",
                on(json!({ "percent": 5, "short_repetition_ms": 1000 })),
                bad,
                Some(
                    "a spread policy takes settings.short_repetition_ms and \
                     settings.short_repetition_percent together, each a finite number above 0",
                ),
            ),
            (
                group_id,
                "swap",
                on(json!({ "percent": 5 })),
                bad,
                Some("a swap policy takes no settings"),
            ),
            (group_id, "failed", failed(json!({})), bad, codes),
            (
                group_id,
                "failed",
                failed(json!({ "codes": {} })),
                bad,
                codes,
            ),
            (
                group_id,
                "failed",
                failed(json!({ "codes": { "device_error": 2, "harness_error": 0 } })),
                bad,
                codes,
            ),
            (
                group_id,
                "failed",
                failed(json!({ "codes": { "device_error": 16 } })),
                bad,
                codes,
            ),
            (
                group_id,
                "failed",
                failed(json!({ "codes": { "device_error": 2 }, "percent": 5 })),
                bad,
                codes,
            ),
            // Only the existing failure codes, each with its re-runs.
            (
                group_id,
                "failed",
                failed(json!({ "codes": { "not_a_code": 1 } })),
                bad,
                None,
            ),
            (
                group_id,
                "failed",
                failed(json!({ "codes": ["device_error"] })),
                bad,
                None,
            ),
            (
                group_id,
                "swap",
                on(json!({ "codes": { "device_error": 2 } })),
                bad,
                Some("a swap policy takes no settings.codes, only a failed policy does"),
            ),
            (group_id, "failed", on(json!({ "limit": 1 })), bad, None),
            (
                group_id,
                "swap",
                json!({ "enable": true, "max_reruns": 3, "settings": {} }),
                bad,
                None,
            ),
            (
                group_id,
                "swap",
                json!({ "max_reruns": 3, "settings": {} }),
                bad,
                None,
            ),
            (group_id, "failed", outcome, bad, None),
            (
                group_id,
                "swap",
                json!({ "enabled": false, "max_reruns": 5, "settings": {} }),
                bad,
                None,
            ),
            (group_id, "failed", at(-1), bad, None),
            (
                group_id,
                "failed",
                policy_body(
                    true,
                    2,
                    default_policies(group_id)[2]["settings"].clone(),
                    &base,
                ),
                bad,
                Some("max_reruns is 2: a failed policy takes 0 re-runs"),
            ),
            (
                group_id,
                "spread",
                at(16),
                bad,
                Some("max_reruns is 16: a spread policy takes 1 to 15 re-runs"),
            ),
            (
                group_id,
                "ttft_over_e2e",
                at(0),
                bad,
                Some("max_reruns is 0: a ttft_over_e2e policy takes 1 to 15 re-runs"),
            ),
            (
                missing,
                "swap",
                on(json!({})),
                StatusCode::NOT_FOUND,
                Some("no campaign group group_0195c3e2a4f37b8c"),
            ),
        ]
        .map(anyhow::Ok),
    )
    .try_for_each(|(group_id, kind, body, status, error)| async move {
        let uri = format!("{GROUPS}/{group_id}/policies/{kind}");
        let (got, answer) = send(state, signer, "PUT", &uri, Some(&body)).await?;
        assert_eq!(got, status, "{uri} {body}: {answer}");
        if let Some(error) = error {
            assert_eq!(answer["error"].as_str(), Some(error), "{uri} {body}");
        }
        anyhow::Ok(())
    })
    .await?;
    assert_eq!(
        policies(state, &owner, &group).await?,
        default_policies(&group)
    );
    let uri = format!("{GROUPS}/{missing}/policies");
    let (status, answer) = send(state, &owner, "GET", &uri, None).await?;
    assert_eq!(status, StatusCode::NOT_FOUND, "{answer}");
    Ok(())
}

/// A Turn on, or Save changes on a policy that is on or off, lands only on the
/// policy its `base` read: a stale `base` is a `409` that writes nothing, also
/// for Save changes on a policy read as off that was turned on since.
#[tokio::test]
async fn a_write_lands_only_on_the_policy_its_base_read() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(sql) = seeded_sql_state(dir.path()).await? else {
        return Ok(());
    };
    let state = &sql.state;
    let owner = registered_user(state, RERUNNER).await?;
    let group = created_group(state, &owner, json!({ "name": "aa-perf" })).await?;
    let defaults = default_policies(&group);
    let (swap, failed) = (&defaults[0], &defaults[2]);
    let saved = policy_body(false, 4, json!({}), swap);
    policy_written(state, &owner, &group, "swap", &saved).await?;
    let turned_on = kept_body(true, failed);
    policy_written(state, &owner, &group, "failed", &turned_on).await?;
    let read = policies(state, &owner, &group).await?;

    let (signer, group_id) = (&owner, group.as_str());
    futures::stream::iter(
        [
            ("swap", policy_body(true, 4, json!({}), swap)),
            ("swap", policy_body(false, 3, json!({}), swap)),
            ("failed", kept_body(true, failed)),
            ("failed", kept_body(false, failed)),
        ]
        .map(anyhow::Ok),
    )
    .try_for_each(|(kind, body)| async move {
        let uri = format!("{GROUPS}/{group_id}/policies/{kind}");
        let (status, answer) = send(state, signer, "PUT", &uri, Some(&body)).await?;
        let stale = format!("the {kind} policy changed since it was read; read it again");
        assert_eq!(
            (status, answer["error"].as_str()),
            (StatusCode::CONFLICT, Some(stale.as_str())),
            "{body}"
        );
        anyhow::Ok(())
    })
    .await?;
    assert_eq!(policies(state, &owner, &group).await?, read);
    Ok(())
}

/// A Turn off, `enabled: false` over a `base` that is on, reads no other value
/// it is sent: it turns the stored policy off with its own re-runs and
/// settings, over a stale `base`, and over a spread policy stored with 0
/// re-runs, which no write takes now, and leaves a policy found off as it is.
#[tokio::test]
async fn a_turn_off_keeps_the_stored_values() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(sql) = seeded_sql_state(dir.path()).await? else {
        return Ok(());
    };
    let state = &sql.state;
    let owner = registered_user(state, RERUNNER).await?;
    let group = created_group(state, &owner, json!({ "name": "aa-perf" })).await?;
    let defaults = &default_policies(&group)[1];
    let on = policy_body(true, 1, json!({ "percent": 7.5 }), defaults);
    policy_written(state, &owner, &group, "spread", &on).await?;
    let mut spread = policies(state, &owner, &group).await?[1].clone();
    let uri = format!("{GROUPS}/{group}/policies/spread");
    // The defaults turned on as a stale `base`, a re-run count out of range,
    // and settings spread does not take.
    let mut stale = defaults.clone();
    stale["enabled"] = json!(true);
    let off = policy_body(false, 99, json!({}), &stale);
    let (status, answer) = send(state, &owner, "PUT", &uri, Some(&off)).await?;
    assert_eq!(status, StatusCode::OK, "{answer}");
    spread["enabled"] = json!(false);
    spread["enabled_by"] = Value::Null;
    spread["enabled_at"] = Value::Null;
    assert_eq!(answer, answered(&spread, true)?);

    let store = state
        .campaign_group_store
        .as_deref()
        .context("the SQL store keeps groups")?;
    let group_id = CampaignGroupId::try_new(group.as_str())?;
    let stored = PolicyKind::Spread.policy(&group_id, &store.list_policies(&group_id).await?);
    let unbudgeted = CampaignGroupPolicy {
        enabled: true,
        max_reruns: 0,
        enabled_by: Some(owner.1.clone()),
        enabled_at: Some(Utc::now()),
        ..stored.clone()
    };
    anyhow::ensure!(store.replace_policy(&stored, &unbudgeted).await?);
    let (status, answer) = send(state, &owner, "PUT", &uri, Some(&off)).await?;
    assert_eq!(status, StatusCode::OK, "{answer}");
    spread["max_reruns"] = json!(0);
    assert_eq!(answer, answered(&spread, true)?);
    let (status, answer) = send(state, &owner, "PUT", &uri, Some(&off)).await?;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer, answered(&spread, false)?);
    assert_eq!(policies(state, &owner, &group).await?[1], spread);
    Ok(())
}

/// A write that leaves a policy on takes the grants a re-run of the owner's
/// campaigns takes, the `_any_` pair from outside the owner's organization,
/// and a refusal names the one it lacks. Save changes on a policy that is off,
/// and a Turn off, take the route's gate alone.
#[tokio::test]
async fn leaving_a_policy_on_takes_a_rerun_s_grants() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(sql) = seeded_sql_state(dir.path()).await? else {
        return Ok(());
    };
    let state = &sql.state;
    let org = seed_org(state, "acme").await?;
    let rerunner = org_user(state, &org, RERUNNER).await?;
    let without = |grant: UserPermission| -> Vec<UserPermission> {
        RERUNNER
            .iter()
            .copied()
            .filter(|held| *held != grant)
            .collect()
    };
    let no_clients = org_user(state, &org, &without(UserPermission::ReadClients)).await?;
    let no_cancel = org_user(state, &org, &without(UserPermission::CancelPlans)).await?;
    let filer = org_user(state, &org, &[UserPermission::SubmitPlans]).await?;
    let no_cancel_any = registered_user(
        state,
        &[
            UserPermission::ManageAnyOrgs,
            UserPermission::SubmitPlans,
            UserPermission::ReadAnyPlans,
            UserPermission::ReadClients,
        ],
    )
    .await?;
    let team = team_group(state, &rerunner, &org).await?;
    let defaults = default_policies(&team);
    let (swap, failed) = (&defaults[0], &defaults[2]);
    let failed_on = kept_body(true, failed);
    policy_written(state, &rerunner, &team, "failed", &failed_on).await?;

    let lacks = |grant: &str| Some(format!("user lacks the {grant} permission"));
    let (ok, refused) = (StatusCode::OK, StatusCode::FORBIDDEN);
    let turn_on = policy_body(true, 5, json!({}), swap);
    futures::stream::iter(
        [
            (
                &no_clients,
                "swap",
                &turn_on,
                refused,
                lacks("read_clients"),
            ),
            (&no_cancel, "swap", &turn_on, refused, lacks("cancel_plans")),
            (
                &no_cancel_any,
                "swap",
                &turn_on,
                refused,
                lacks("cancel_any_plans"),
            ),
            (
                &no_cancel,
                "failed",
                &kept_body(true, &failed_on),
                refused,
                lacks("cancel_plans"),
            ),
            (
                &filer,
                "swap",
                &policy_body(false, 3, json!({}), swap),
                ok,
                None,
            ),
            (
                &filer,
                "failed",
                &policy_body(false, 2, json!({}), &failed_on),
                ok,
                None,
            ),
        ]
        .map(anyhow::Ok),
    )
    .try_for_each(|(signer, kind, body, status, error)| {
        let team = &team;
        async move {
            let uri = format!("{GROUPS}/{team}/policies/{kind}");
            let (got, answer) = send(state, signer, "PUT", &uri, Some(body)).await?;
            assert_eq!(
                (got, answer["error"].as_str()),
                (status, error.as_deref()),
                "{kind} {body}"
            );
            anyhow::Ok(())
        }
    })
    .await?;
    let mut expected = defaults.clone();
    expected[0]["max_reruns"] = json!(3);
    assert_eq!(policies(state, &rerunner, &team).await?, expected);
    Ok(())
}

/// While a policy of the group is on, the next queue-maintenance run re-runs
/// and marks a new member, so adding one takes a re-run's grants, as leaving a
/// policy on does. With every policy off, the route's gate alone does.
#[tokio::test]
async fn adding_a_member_while_a_policy_is_on_takes_a_rerun_s_grants() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(sql) = seeded_sql_state(dir.path()).await? else {
        return Ok(());
    };
    let state = &sql.state;
    let org = seed_org(state, "acme").await?;
    let rerunner = org_user(state, &org, RERUNNER).await?;
    let without = |grant: UserPermission| -> Vec<UserPermission> {
        RERUNNER
            .iter()
            .copied()
            .filter(|held| *held != grant)
            .collect()
    };
    let no_clients = org_user(state, &org, &without(UserPermission::ReadClients)).await?;
    let no_cancel = org_user(state, &org, &without(UserPermission::CancelPlans)).await?;
    let filer = org_user(state, &org, &[UserPermission::SubmitPlans]).await?;
    let team = team_group(state, &rerunner, &org).await?;
    let owner = CampaignOwner::Org {
        org_id: org.org_id.clone(),
    };
    let (off, refused, on) = (
        campaign_of(state, &owner).await?,
        campaign_of(state, &owner).await?,
        campaign_of(state, &owner).await?,
    );
    added(state, &filer, &team, &[&off]).await?;

    let failed_on = kept_body(true, &default_policies(&team)[2]);
    policy_written(state, &rerunner, &team, "failed", &failed_on).await?;
    let lacks = |grant: &str| format!("user lacks the {grant} permission");
    futures::stream::iter(
        [
            (&no_cancel, lacks("cancel_plans")),
            (&no_clients, lacks("read_clients")),
            // `submit_plans` holds `read_plans`.
            (&filer, lacks("cancel_plans")),
        ]
        .map(anyhow::Ok),
    )
    .try_for_each(|(signer, error)| {
        let (team, refused) = (&team, &refused);
        async move {
            let (status, answer) = add(state, signer, team, &[refused]).await?;
            assert_eq!(
                (status, answer["error"].as_str()),
                (StatusCode::FORBIDDEN, Some(error.as_str()))
            );
            anyhow::Ok(())
        }
    })
    .await?;
    added(state, &rerunner, &team, &[&on]).await?;
    assert_eq!(
        member_ids(state, &rerunner, &team).await?,
        BTreeSet::from([off, on])
    );
    Ok(())
}

/// The model_params policy re-runs and marks nothing, so turning it on, and
/// adding a member while it alone is on, take the route's gate alone. It takes
/// no re-run and no setting.
#[tokio::test]
async fn the_model_params_policy_takes_no_rerun_grant() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(sql) = seeded_sql_state(dir.path()).await? else {
        return Ok(());
    };
    let state = &sql.state;
    let org = seed_org(state, "acme").await?;
    let filer = org_user(state, &org, &[UserPermission::SubmitPlans]).await?;
    let team = team_group(state, &filer, &org).await?;
    let defaults = default_policies(&team);
    let model_params = &defaults[4];
    let uri = format!("{GROUPS}/{team}/policies/model_params");
    futures::stream::iter(
        [
            (
                policy_body(true, 1, json!({}), model_params),
                "max_reruns is 1: a model_params policy takes 0 re-runs",
            ),
            (
                policy_body(true, 0, json!({ "percent": 5 }), model_params),
                "a model_params policy takes no settings",
            ),
        ]
        .map(anyhow::Ok),
    )
    .try_for_each(|(body, error)| {
        let (filer, uri) = (&filer, &uri);
        async move {
            let (status, answer) = send(state, filer, "PUT", uri, Some(&body)).await?;
            assert_eq!(
                (status, answer["error"].as_str()),
                (StatusCode::BAD_REQUEST, Some(error)),
                "{body}"
            );
            anyhow::Ok(())
        }
    })
    .await?;

    let turn_on = policy_body(true, 0, json!({}), model_params);
    policy_written(state, &filer, &team, "model_params", &turn_on).await?;
    let owner = CampaignOwner::Org {
        org_id: org.org_id.clone(),
    };
    let campaign = campaign_of(state, &owner).await?;
    added(state, &filer, &team, &[&campaign]).await?;
    let mut expected = defaults.clone();
    let read = policies(state, &filer, &team).await?;
    expected[4]["enabled"] = json!(true);
    expected[4]["enabled_by"] = json!(filer.1.as_str());
    expected[4]["enabled_at"] = read[4]["enabled_at"].clone();
    assert_eq!(read, expected);
    assert_eq!(
        member_ids(state, &filer, &team).await?,
        BTreeSet::from([campaign])
    );
    Ok(())
}

/// Reading a group's policies takes reading its campaigns, and writing one
/// takes filing into them, as adding a member does: its organization's members
/// do both, its administrator only reads, and a stranger or a campaign reader
/// does neither.
#[tokio::test]
async fn the_policies_take_the_group_s_rights() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(sql) = seeded_sql_state(dir.path()).await? else {
        return Ok(());
    };
    let state = &sql.state;
    let (org, member) = org_operator(state).await?;
    let teammate = registered_user(state, OPERATOR).await?;
    let admin = registered_user(state, &[UserPermission::ManageOrgs]).await?;
    let reader = registered_user(state, &[UserPermission::ReadPlans]).await?;
    set_user_org(state, &teammate.1, Some(org.org_id.clone())).await?;
    set_user_org(state, &admin.1, Some(org.org_id.clone())).await?;
    set_user_org(state, &reader.1, Some(org.org_id.clone())).await?;
    let stranger = registered_user(state, OPERATOR).await?;
    let server = registered_user(state, UserPermissionGroup::ServerAdmin.permissions()).await?;
    let team_owner = CampaignOwner::Org {
        org_id: org.org_id.clone(),
    };
    let listed = CampaignId::try_new(campaign_of(state, &team_owner).await?)?;
    let mut account = state
        .user_store
        .get_user(&reader.1)
        .await?
        .context("the reader is stored")?;
    account.campaign_scope = Some(BTreeSet::from([listed]));
    state.user_store.put_user(&account).await?;
    let team = team_group(state, &member, &org).await?;
    let own = created_group(state, &member, json!({ "name": "mine" })).await?;

    let another = Some("this campaign group belongs to another owner");
    let unfiled = Some("user lacks the submit_plans permission");
    let (ok, refused) = (StatusCode::OK, StatusCode::FORBIDDEN);
    // Save changes on a policy that is off, which takes the route's gate alone.
    let save = &kept_body(false, &default_policies(&team)[2]);
    futures::stream::iter(
        [
            (&member, &own, ok, ok, None, None),
            (&member, &team, ok, ok, None, None),
            (&teammate, &team, ok, ok, None, None),
            (&teammate, &own, refused, refused, another, another),
            (&admin, &team, ok, refused, None, unfiled),
            (&server, &own, ok, ok, None, None),
            (&stranger, &team, refused, refused, another, another),
            (
                &reader,
                &team,
                refused,
                refused,
                Some("a campaign reader reads no campaign group"),
                Some("a campaign reader holds read_plans alone, not submit_plans"),
            ),
        ]
        .map(anyhow::Ok),
    )
    .try_for_each(
        |(signer, group, read, write, read_error, write_error)| async move {
            let uri = format!("{GROUPS}/{group}/policies");
            let (status, answer) = send(state, signer, "GET", &uri, None).await?;
            assert_eq!(status, read, "GET {uri}: {answer}");
            assert_eq!(answer["error"].as_str(), read_error, "GET {uri}");
            let uri = format!("{uri}/failed");
            let (status, answer) = send(state, signer, "PUT", &uri, Some(save)).await?;
            assert_eq!(status, write, "PUT {uri}: {answer}");
            assert_eq!(answer["error"].as_str(), write_error, "PUT {uri}");
            anyhow::Ok(())
        },
    )
    .await
}

/// A policy write is in the group owner's log, naming the owner as the group
/// writes do, though the writer is in no organization.
#[tokio::test]
async fn a_policy_write_is_in_the_group_owner_s_log() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(sql) = seeded_sql_state(dir.path()).await? else {
        return Ok(());
    };
    let state = &sql.state;
    let (org, member) = org_operator(state).await?;
    let team = team_group(state, &member, &org).await?;
    let server = registered_user(state, UserPermissionGroup::ServerAdmin.permissions()).await?;
    let uri = format!("{GROUPS}/{team}/policies/swap");
    let enable = policy_body(true, 5, json!({}), &default_policies(&team)[0]);
    let (status, answer) = send(state, &server, "PUT", &uri, Some(&enable)).await?;
    assert_eq!(status, StatusCode::OK, "{answer}");

    let owner = AuditTarget::Org {
        org_id: org.org_id.clone(),
    };
    let in_log = AuditFilter {
        org_id: Some(org.org_id.clone()),
        ..AuditFilter::default()
    };
    let writes: Vec<(AuditOutcome, bool)> = state
        .audit_store
        .as_deref()
        .context("the SQL store keeps the audit log")?
        .list_events(
            &in_log,
            None,
            NonZeroUsize::new(100).context("a page size")?,
        )
        .await?
        .items
        .into_iter()
        .filter(|event| {
            matches!(&event.action, AuditAction::Http { method, path, .. }
                if method == "PUT" && *path == uri)
        })
        .map(|event| (event.outcome, event.targets.contains(&owner)))
        .collect();
    assert_eq!(writes, [(AuditOutcome::Succeeded, true)]);
    Ok(())
}

/// The method, route and status of each event in `org`'s log that names
/// `group_id`, newest first, as `reader` reads them.
async fn group_history(
    state: &AppState,
    reader: &UserSigner,
    org: &Organization,
    group_id: &str,
) -> anyhow::Result<Vec<String>> {
    let uri = format!("/api/v1/orgs/{}/audit?target={group_id}", org.org_id);
    let (status, page) = send(state, reader, "GET", &uri, None).await?;
    anyhow::ensure!(status == StatusCode::OK, "{status} {page}");
    page.get("items")
        .and_then(Value::as_array)
        .context("a page of events")?
        .iter()
        .map(|event| {
            let action = event.get("action").context("an action")?;
            let (method, route) = (json_str(action, "method")?, json_str(action, "route")?);
            let status = action.get("status").context("a status")?;
            Ok(format!("{method} {route} {status}"))
        })
        .collect()
}

/// Each write to a group names it, as does a delete of a member campaign, one
/// refused while a plan runs too, so the owner's log narrowed to the group
/// reads its history, newest first, for the organization's administrator.
#[tokio::test]
async fn the_owner_s_log_narrowed_to_a_group_reads_its_writes() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(sql) = seeded_sql_state(dir.path()).await? else {
        return Ok(());
    };
    let state = &sql.state;
    let (org, member) = org_operator(state).await?;
    let admin = org_user(state, &org, &[UserPermission::ManageOrgs]).await?;
    let team = team_group(state, &member, &org).await?;
    let owner = CampaignOwner::Org {
        org_id: org.org_id.clone(),
    };
    let campaign = campaign_of(state, &owner).await?;
    let in_org = [("name", "aa-perf nightly"), ("owner", org.org_id.as_str())];
    let (running, _) = created_campaign(state, &member, &in_org).await?;
    added(state, &member, &team, &[&campaign, &running]).await?;
    let turn_on = policy_body(true, 0, json!({}), &default_policies(&team)[4]);
    policy_written(state, &member, &team, "model_params", &turn_on).await?;
    let uri = format!("/api/v1/campaigns/{running}");
    let (status, answer) = send(state, &admin, "DELETE", &uri, None).await?;
    assert_eq!(status, StatusCode::CONFLICT, "{answer}");
    let uri = format!("/api/v1/campaigns/{campaign}");
    let (status, answer) = send(state, &admin, "DELETE", &uri, None).await?;
    assert_eq!(status, StatusCode::NO_CONTENT, "{answer}");
    let uri = format!("{GROUPS}/{team}");
    let rename = json!({ "name": "team nightly" });
    let (status, answer) = send(state, &admin, "PATCH", &uri, Some(&rename)).await?;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let (status, answer) = send(state, &admin, "DELETE", &uri, None).await?;
    assert_eq!(status, StatusCode::NO_CONTENT, "{answer}");

    assert_eq!(
        group_history(state, &admin, &org, &team).await?,
        [
            "DELETE /api/v1/campaign-groups/{group_id} 204",
            "PATCH /api/v1/campaign-groups/{group_id} 200",
            "DELETE /api/v1/campaigns/{campaign_id} 204",
            "DELETE /api/v1/campaigns/{campaign_id} 409",
            "PUT /api/v1/campaign-groups/{group_id}/policies/{kind} 200",
            "POST /api/v1/campaign-groups/{group_id}/members 204",
            "POST /api/v1/campaign-groups 201",
        ]
    );
    Ok(())
}

/// A campaign of another owner, left in a group by a hand-off whose removal
/// failed, leaves on a retry and on a delete. Only the group puts those events
/// in its owner's log, where they read under the group.
#[tokio::test]
async fn a_member_of_another_owner_leaving_is_in_the_group_owner_s_log() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(sql) = seeded_sql_state(dir.path()).await? else {
        return Ok(());
    };
    let state = &sql.state;
    let (org, member) = org_operator(state).await?;
    let admin = org_user(state, &org, &[UserPermission::ManageOrgs]).await?;
    let taker = registered_user(state, OPERATOR).await?;
    let team = team_group(state, &member, &org).await?;
    let owner = CampaignOwner::Org {
        org_id: org.org_id.clone(),
    };
    let (handed, deleted) = (
        campaign_of(state, &owner).await?,
        campaign_of(state, &owner).await?,
    );
    added(state, &member, &team, &[&handed, &deleted]).await?;
    owner_written(state, &handed, personal(&taker)).await?;
    owner_written(state, &deleted, personal(&taker)).await?;

    let to_taker = json!({ "owner": { "type": "user", "user_id": taker.1.as_str() } });
    let uri = format!("/api/v1/campaigns/{handed}/owner");
    let (status, answer) = send(state, &taker, "PUT", &uri, Some(&to_taker)).await?;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let uri = format!("/api/v1/campaigns/{deleted}");
    let (status, answer) = send(state, &taker, "DELETE", &uri, None).await?;
    assert_eq!(status, StatusCode::NO_CONTENT, "{answer}");

    assert_eq!(
        group_history(state, &admin, &org, &team).await?,
        [
            "DELETE /api/v1/campaigns/{campaign_id} 204",
            "PUT /api/v1/campaigns/{campaign_id}/owner 200",
            "POST /api/v1/campaign-groups/{group_id}/members 204",
            "POST /api/v1/campaign-groups 201",
        ]
    );
    Ok(())
}

/// A member's overview names its group, the record its create answered. It is
/// `null` for a campaign in no group, for a campaign reader, and once the group
/// is deleted.
#[tokio::test]
async fn an_overview_names_the_campaign_s_group() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(sql) = seeded_sql_state(dir.path()).await? else {
        return Ok(());
    };
    let state = &sql.state;
    let (org, owner) = org_operator(state).await?;
    let admin = registered_user(state, &[UserPermission::ManageOrgs]).await?;
    set_user_org(state, &admin.1, Some(org.org_id.clone())).await?;
    let team = CampaignOwner::Org {
        org_id: org.org_id.clone(),
    };
    let (member, loose) = (
        campaign_of(state, &team).await?,
        campaign_of(state, &team).await?,
    );
    let body = json!({ "name": "team", "owner": org.org_id.as_str() });
    let (status, group) = send(state, &owner, "POST", GROUPS, Some(&body)).await?;
    assert_eq!(status, StatusCode::CREATED, "{group}");
    let group_id = json_str(&group, "group_id")?;
    added(state, &owner, group_id, &[&member]).await?;
    let reader = campaign_reader(state, &org, &member).await?;

    assert_eq!(overview_group(state, &owner, &member).await?, group);
    assert_eq!(overview_group(state, &owner, &loose).await?, Value::Null);
    assert_eq!(overview_group(state, &reader, &member).await?, Value::Null);

    let uri = format!("{GROUPS}/{group_id}");
    let (status, answer) = send(state, &admin, "DELETE", &uri, None).await?;
    assert_eq!(status, StatusCode::NO_CONTENT, "{answer}");
    assert_eq!(overview_group(state, &owner, &member).await?, Value::Null);
    Ok(())
}

/// Reads a group and files into it, and sees the device failures the failed
/// check counts.
const PREVIEWER: &[UserPermission] = &[
    UserPermission::SubmitPlans,
    UserPermission::ReadPlans,
    UserPermission::ReadClients,
];

/// The columns of [`result_row`].
const RESULT_COLUMNS: [&str; 9] = [
    "job_id",
    "benchmark_id",
    "benchmark_type",
    "metric",
    "value",
    "value_stddev",
    "unit",
    "failed",
    "observation_max_swap_bytes",
];

/// A decode result of 100 tokens/sec with `stddev`, whose run swapped
/// `swap_bytes`.
fn result_row(job_id: &str, stddev: f64, swap_bytes: u64) -> Value {
    json!([
        job_id,
        "decode_throughput_512_100",
        "decode_throughput",
        "decode",
        100.0,
        stddev,
        "tokens/sec",
        false,
        swap_bytes
    ])
}

/// The columns of [`model_row`].
const MODEL_COLUMNS: [&str; 6] = [
    "job_id",
    "benchmark_id",
    "benchmark_type",
    "metric",
    "unit",
    "model_descriptor",
];

/// A decode result of `job_id` whose run recorded `model`, with no counts.
fn model_row(job_id: &str, model: &str) -> Value {
    json!([
        job_id,
        "decode_throughput_512_100",
        "decode_throughput",
        "decode",
        "tokens/sec",
        model
    ])
}

/// pipette-duckdb answering `rows` to a query that names every one of `jobs`.
async fn answering<'a>(duckdb: &'a MockServer, jobs: &[&str], rows: Vec<Value>) -> Mock<'a> {
    answering_in(duckdb, &RESULT_COLUMNS, jobs, rows).await
}

/// [`answering`] with rows of `columns`.
async fn answering_in<'a>(
    duckdb: &'a MockServer,
    columns: &[&str],
    jobs: &[&str],
    rows: Vec<Value>,
) -> Mock<'a> {
    let named: Vec<String> = jobs.iter().map(|job| format!("'{job}'")).collect();
    duckdb
        .mock_async(|when, then| {
            named
                .into_iter()
                .fold(when.method(POST).path("/api/query"), |when, named| {
                    when.body_includes(named)
                });
            then.status(200)
                .json_body(json!({ "columns": columns, "rows": rows }));
        })
        .await
}

/// Run `campaign` on a plan of `status` whose `jobs` are each answered, the
/// earliest due at `earliest`.
async fn running(
    state: &AppState,
    campaign: &mut Campaign,
    status: PlanStatus,
    jobs: &[&str],
    earliest: &str,
) -> anyhow::Result<PlanId> {
    let plan_id = PlanId::from_uuid(uuid::Uuid::now_v7());
    let job_ids = jobs
        .iter()
        .map(|job| JobId::try_new(*job))
        .collect::<Result<Vec<JobId>, _>>()?;
    let plan = PlanManifest {
        campaign_id: Some(campaign.campaign_id.clone()),
        content_keys: job_ids
            .iter()
            .map(|job_id| (job_id.clone(), "c".repeat(64)))
            .collect(),
        rerun_of: BTreeMap::new(),
        settled_jobs: job_ids
            .iter()
            .map(|job_id| (job_id.clone(), SettledOutcome::Finished))
            .collect(),
        expired_jobs: BTreeMap::new(),
        warnings: Vec::new(),
        job_expiry: Some(JobExpiry {
            earliest: ExpiresAt::At(earliest.parse()?),
            latest: ExpiresAt::Never,
        }),
        job_ids,
        ..sample_manifest(plan_id.clone(), status, false)?
    };
    state.plan_store.put_plan(&plan).await?;
    campaign.current_plan_id = Some(plan_id.clone());
    state.campaign_store.put_campaign(campaign).await?;
    Ok(plan_id)
}

/// A check's counts, with none kept.
fn check_counts(kind: &str, on: bool, flagged: u32, rerun: u32, marked: u32) -> Value {
    json!({
        "kind": kind,
        "on": on,
        "flagged": flagged,
        "rerun": rerun,
        "marked": marked,
        "kept": 0,
    })
}

/// A member's preview that read no job, while swap and spread are on.
fn unread_member(campaign: &Campaign, state: &str) -> Value {
    let check = |kind: &str, on: bool| check_counts(kind, on, 0, 0, 0);
    json!({
        "campaign_id": campaign.campaign_id.as_str(),
        "plan_id": campaign.current_plan_id.as_ref().map(PlanId::as_str),
        "state": state,
        "answered_job_count": 0,
        "waiting_job_count": 0,
        "earliest_deadline": null,
        "deadline_passed": false,
        "checks": [
            check("swap", true),
            check("spread", true),
            check("failed", false),
            check("ttft_over_e2e", false),
            check("model_params", false),
            check("smaller_failed", false),
        ],
        "triggered": [],
        "rerun_job_ids": [],
        "marked_job_ids": [],
        "kept_job_count": 0,
        "skipped": {},
        "filing_plan_id": null,
        "unclaimed_job_count": 0,
        "missing_model_params": [],
        "smaller_failed": [],
        "last_filing": null,
    })
}

/// `kept`'s filing as the preview shows it.
fn last_filing(kept: &GroupPolicyState) -> Value {
    kept.filed.as_ref().map_or(Value::Null, |filed| {
        json!({
            "plan_id": filed.plan_id.as_str(),
            "filed_at": filed.filed_at,
            "queued_job_count": filed.queued.len(),
        })
    })
}

/// Each member, in member order, reads as far as its plan lets a policy act.
/// The first pipette-duckdb failure ends the reads: a later member that would
/// read rows is `unreadable` unasked, and one stopped before its rows reads
/// its own state.
#[tokio::test]
async fn the_preview_reads_each_member_s_plan() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(sql) = seeded_sql_state(dir.path()).await? else {
        return Ok(());
    };
    let duckdb = MockServer::start_async().await;
    let state = &with_duckdb(sql.state.clone(), &duckdb.base_url())?;
    let org = seed_org(state, "acme").await?;
    let owner = org_user(state, &org, RERUNNER).await?;
    let group = created_group(state, &owner, json!({ "name": "aa-perf" })).await?;
    let defaults = default_policies(&group);
    let at_once = policy_body(true, 0, json!({}), &defaults[0]);
    policy_written(state, &owner, &group, "swap", &at_once).await?;
    let spread = policy_body(true, 3, json!({ "percent": 5 }), &defaults[1]);
    policy_written(state, &owner, &group, "spread", &spread).await?;
    let mut campaigns = futures::future::try_join_all(
        (0..6).map(|_| put_idle_campaign_of(state, personal(&owner), "aa-perf")),
    )
    .await?;
    // Member order is campaign id order.
    campaigns.sort_by(|a, b| a.campaign_id.cmp(&b.campaign_id));
    let [evaluated, creating, handed_off, lost, cancelled, unasked] = &mut campaigns[..] else {
        anyhow::bail!("six campaigns");
    };
    let (past, future) = ("2026-08-19T17:55:00Z", "2099-01-01T00:00:00Z");
    let read = ["job-swapped", "job-spread", "job-steady", "job-waiting"];
    running(state, evaluated, PlanStatus::Active, &read, future).await?;
    running(state, creating, PlanStatus::Creating, &["job-new"], future).await?;
    running(state, lost, PlanStatus::Active, &["job-lost"], past).await?;
    running(
        state,
        cancelled,
        PlanStatus::Cancelled,
        &["job-stopped"],
        past,
    )
    .await?;
    running(state, unasked, PlanStatus::Active, &["job-unasked"], future).await?;
    let ids: Vec<&str> = [
        &*evaluated,
        &*creating,
        &*handed_off,
        &*lost,
        &*cancelled,
        &*unasked,
    ]
    .map(|campaign| campaign.campaign_id.as_str())
    .to_vec();
    added(state, &owner, &group, &ids).await?;
    owner_written(
        state,
        handed_off.campaign_id.as_str(),
        CampaignOwner::Org {
            org_id: org.org_id.clone(),
        },
    )
    .await?;
    let rows = vec![
        result_row("job-swapped", 1.0, 4096),
        result_row("job-spread", 10.0, 0),
        result_row("job-steady", 1.0, 0),
    ];
    let answered = answering(&duckdb, &read, rows).await;
    let never = answering(&duckdb, &["job-unasked"], Vec::new()).await;
    // A recorded re-run of a job the preview reads.
    let groups = state
        .campaign_group_store
        .as_deref()
        .context("a [database] keeps campaign groups")?;
    let group_id = CampaignGroupId::try_new(group.as_str())?;
    let waiting = a_rerun(&evaluated.campaign_id, "job-steady", Utc::now(), 4096.0)?;
    anyhow::ensure!(
        groups
            .record_reruns(&group_id, std::slice::from_ref(&waiting))
            .await?
            == 1
    );

    let uri = format!("{GROUPS}/{group}/policy-preview");
    let (status, preview) = send(state, &owner, "GET", &uri, None).await?;
    assert_eq!(status, StatusCode::OK, "{preview}");
    crate::api_schema::assert_conforms("CampaignGroupPolicyPreview", &preview)?;
    // It records nothing: the re-run still waits, and nothing it would file
    // is recorded.
    assert_eq!(
        groups
            .waiting_reruns(&group_id, &evaluated.campaign_id)
            .await?,
        [waiting]
    );
    let campaign_ids = ids
        .iter()
        .map(|id| CampaignId::try_new(*id))
        .collect::<Result<Vec<_>, _>>()?;
    let (_, recorded) = groups
        .rerun_results(&group_id, &campaign_ids, 0, None)
        .await?;
    assert_eq!(recorded.len(), 1);
    let mut checked = unread_member(evaluated, "evaluated");
    checked["answered_job_count"] = json!(4);
    checked["waiting_job_count"] = json!(1);
    checked["earliest_deadline"] = json!(future);
    checked["checks"][0] = check_counts("swap", true, 1, 0, 1);
    checked["checks"][1] = check_counts("spread", true, 1, 1, 0);
    checked["triggered"] = json!(["swap", "spread"]);
    checked["rerun_job_ids"] = json!(["job-spread"]);
    checked["marked_job_ids"] = json!(["job-swapped"]);
    checked["kept_job_count"] = json!(1);
    let mut failed = unread_member(lost, "unreadable");
    failed["answered_job_count"] = json!(1);
    failed["earliest_deadline"] = json!(past);
    failed["deadline_passed"] = json!(true);
    let mut after = unread_member(unasked, "unreadable");
    after["answered_job_count"] = json!(1);
    after["earliest_deadline"] = json!(future);
    assert_eq!(
        preview,
        json!({ "members": [
            checked,
            unread_member(creating, "not_ready"),
            unread_member(handed_off, "other_owner"),
            failed,
            unread_member(cancelled, "cancelled"),
            after,
        ] })
    );
    answered.assert_async().await;
    assert_eq!(never.calls_async().await, 0);
    Ok(())
}

/// The preview takes what holds a member from what queue-maintenance kept of
/// it, with the plan and the count, shows its last filing, and reads every
/// answered job, a decided one too, and those of a member that waits or was
/// refused for their rows that lack model params. It writes nothing and
/// cancels nothing.
#[tokio::test]
async fn the_preview_shows_what_holds_a_member() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(sql) = seeded_sql_state(dir.path()).await? else {
        return Ok(());
    };
    let duckdb = MockServer::start_async().await;
    let state = &with_duckdb(sql.state.clone(), &duckdb.base_url())?;
    let org = seed_org(state, "acme").await?;
    let owner = org_user(state, &org, RERUNNER).await?;
    let group = created_group(state, &owner, json!({ "name": "aa-perf" })).await?;
    let swap_on = policy_body(true, 3, json!({}), &default_policies(&group)[0]);
    policy_written(state, &owner, &group, "swap", &swap_on).await?;
    let mut campaigns = futures::future::try_join_all(
        (0..4).map(|_| put_idle_campaign_of(state, personal(&owner), "aa-perf")),
    )
    .await?;
    campaigns.sort_by(|a, b| a.campaign_id.cmp(&b.campaign_id));
    let [waiting, unadvanced, refused, decided] = &mut campaigns[..] else {
        anyhow::bail!("four campaigns");
    };
    let future = "2099-01-01T00:00:00Z";
    let waiting_head = running(state, waiting, PlanStatus::Active, &["job-a"], future).await?;
    running(state, unadvanced, PlanStatus::Active, &["job-b"], future).await?;
    let refused_head = running(state, refused, PlanStatus::Active, &["job-c"], future).await?;
    running(state, decided, PlanStatus::Active, &["job-swapped"], future).await?;
    let ids: Vec<&str> = [&*waiting, &*unadvanced, &*refused, &*decided]
        .map(|campaign| campaign.campaign_id.as_str())
        .to_vec();
    added(state, &owner, &group, &ids).await?;

    // The filing that did not advance still runs beside the head.
    let not_advanced = PlanId::from_uuid(uuid::Uuid::now_v7());
    state
        .plan_store
        .put_plan(&PlanManifest {
            campaign_id: Some(unadvanced.campaign_id.clone()),
            ..sample_manifest(not_advanced.clone(), PlanStatus::Active, false)?
        })
        .await?;
    let queued = seed_avail(
        state.todo_store.as_ref(),
        "job-queued",
        ExpiresAt::Never,
        &json!({ "job_id": "job-queued" }),
    )
    .await?;
    let groups = state
        .campaign_group_store
        .as_deref()
        .context("a [database] keeps campaign groups")?;
    let group_id = CampaignGroupId::try_new(group.as_str())?;
    let acting: Vec<CampaignGroupPolicy> = groups
        .list_policies(&group_id)
        .await?
        .into_iter()
        .filter(|policy| policy.enabled)
        .collect();
    let filing = |plan_id: &PlanId, queued: BTreeSet<JobId>| GroupFiling {
        plan_id: plan_id.clone(),
        queued,
        commit_unknown: false,
        filed_at: Utc::now(),
    };
    let kept = [
        (
            &*waiting,
            GroupPolicyState {
                filed: Some(filing(&waiting_head, BTreeSet::from([queued]))),
                ..GroupPolicyState::default()
            },
        ),
        (
            &*unadvanced,
            GroupPolicyState {
                filed: Some(filing(&not_advanced, BTreeSet::new())),
                ..GroupPolicyState::default()
            },
        ),
        (
            &*refused,
            GroupPolicyState {
                policies: acting.clone(),
                refusal: Some(Refusal {
                    plan_id: refused_head,
                    reason: "the deadline has passed".to_owned(),
                    build: BUILD_VERSION.to_owned(),
                }),
                ..GroupPolicyState::default()
            },
        ),
        (
            &*decided,
            GroupPolicyState {
                policies: acting,
                decided: BTreeSet::from([JobId::try_new("job-swapped")?]),
                ..GroupPolicyState::default()
            },
        ),
    ];
    let written =
        futures::future::try_join_all(kept.iter().map(|(campaign, kept)| {
            groups.set_policy_state(&group_id, &campaign.campaign_id, kept)
        }))
        .await?;
    anyhow::ensure!(written.iter().all(|written| *written), "{written:?}");
    // The held member's swap re-run waits for its rows.
    let rerun = a_rerun(&waiting.campaign_id, "job-a", Utc::now(), 4096.0)?;
    anyhow::ensure!(groups.record_reruns(&group_id, &[rerun]).await? == 1);
    let swapped = answering(
        &duckdb,
        &["job-swapped"],
        vec![result_row("job-swapped", 1.0, 4096)],
    )
    .await;
    let granite =
        r#"{"type":"gguf_text","source":"relative_file","path":"models/granite-4.2-8b-Q4_0.gguf"}"#;
    let waiting_rows = answering_in(
        &duckdb,
        &MODEL_COLUMNS,
        &["job-a"],
        vec![model_row("job-a", granite)],
    )
    .await;
    let refused_rows = answering(&duckdb, &["job-c"], vec![result_row("job-c", 1.0, 0)]).await;

    let uri = format!("{GROUPS}/{group}/policy-preview");
    let (status, preview) = send(state, &owner, "GET", &uri, None).await?;
    assert_eq!(status, StatusCode::OK, "{preview}");
    crate::api_schema::assert_conforms("CampaignGroupPolicyPreview", &preview)?;
    let held = |campaign: &Campaign, state: &str| {
        let mut member = unread_member(campaign, state);
        member["checks"][1]["on"] = json!(false);
        member
    };
    let mut queue_held = held(&*waiting, "waiting_for_devices");
    queue_held["answered_job_count"] = json!(1);
    queue_held["earliest_deadline"] = json!(future);
    queue_held["filing_plan_id"] = json!(waiting_head.as_str());
    queue_held["unclaimed_job_count"] = json!(1);
    queue_held["last_filing"] = last_filing(&kept[0].1);
    queue_held["checks"][4] = json!({
        "kind": "model_params", "on": false, "flagged": 1, "rerun": 0, "marked": 0, "kept": 1,
    });
    queue_held["missing_model_params"] = json!([
        { "key": "granite-4.2-8b", "row_count": 1, "job_count": 1 },
    ]);
    // Swap reads no rows of a held member but has a re-run waiting; model
    // params flags a job but is off.
    queue_held["triggered"] = json!(["swap"]);
    let mut filing_held = held(&*unadvanced, "filing_not_advanced");
    filing_held["filing_plan_id"] = json!(not_advanced.as_str());
    filing_held["last_filing"] = last_filing(&kept[1].1);
    let mut refusal_held = held(&*refused, "refused");
    refusal_held["answered_job_count"] = json!(1);
    refusal_held["earliest_deadline"] = json!(future);
    let mut read_again = held(&*decided, "evaluated");
    read_again["answered_job_count"] = json!(1);
    read_again["earliest_deadline"] = json!(future);
    read_again["checks"][0] = check_counts("swap", true, 1, 1, 0);
    read_again["triggered"] = json!(["swap"]);
    read_again["rerun_job_ids"] = json!(["job-swapped"]);
    assert_eq!(
        preview,
        json!({ "members": [queue_held, filing_held, refusal_held, read_again] })
    );
    swapped.assert_async().await;
    waiting_rows.assert_async().await;
    refused_rows.assert_async().await;

    let after: Vec<(CampaignId, Option<GroupPolicyState>)> = groups
        .policy_states(&group_id)
        .await?
        .into_iter()
        .map(|member| Ok((member.campaign_id, member.policy_state?)))
        .collect::<anyhow::Result<_>>()?;
    let before: Vec<(CampaignId, Option<GroupPolicyState>)> = kept
        .into_iter()
        .map(|(campaign, kept)| (campaign.campaign_id.clone(), Some(kept)))
        .collect();
    assert_eq!(after, before);
    assert!(!state.plan_store.has_cancel_marker(&not_advanced).await?);
    Ok(())
}

/// The preview counts, by the key an entry would take, the rows whose model
/// the model params table resolves to no entry, two quant files of one model
/// under one key, whether the policy is on or not, and shows the member's last
/// filing, which no longer holds it.
#[tokio::test]
async fn the_preview_shows_rows_that_lack_model_params_and_the_last_filing() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(sql) = seeded_sql_state(dir.path()).await? else {
        return Ok(());
    };
    let duckdb = MockServer::start_async().await;
    let state = &with_duckdb(sql.state.clone(), &duckdb.base_url())?;
    let org = seed_org(state, "acme").await?;
    let owner = org_user(state, &org, RERUNNER).await?;
    let group = created_group(state, &owner, json!({ "name": "aa-perf" })).await?;
    let mut campaign = put_idle_campaign_of(state, personal(&owner), "aa-perf").await?;
    let future = "2099-01-01T00:00:00Z";
    let jobs = ["job-q4", "job-q5", "job-filled"];
    let head = running(state, &mut campaign, PlanStatus::Active, &jobs, future).await?;
    added(state, &owner, &group, &[campaign.campaign_id.as_str()]).await?;
    state
        .model_params_store
        .put_model_params(&pipette_store_testkit::a_model_params("lfm2-700m", 742)?)
        .await?;
    let kept = GroupPolicyState {
        filed: Some(GroupFiling {
            plan_id: head.clone(),
            queued: BTreeSet::from([JobId::try_new("job-q4")?]),
            commit_unknown: false,
            filed_at: Utc::now(),
        }),
        ..GroupPolicyState::default()
    };
    let groups = state
        .campaign_group_store
        .as_deref()
        .context("a [database] keeps campaign groups")?;
    let group_id = CampaignGroupId::try_new(group.as_str())?;
    anyhow::ensure!(
        groups
            .set_policy_state(&group_id, &campaign.campaign_id, &kept)
            .await?
    );
    let q4 =
        r#"{"type":"gguf_text","source":"relative_file","path":"models/granite-4.2-8b-Q4_0.gguf"}"#;
    let q5 = r#"{"type":"gguf_text","source":"relative_file","path":"models/granite-4.2-8b-Q5_K_M.gguf"}"#;
    let filled = r#"{"type":"mlx","source":"huggingface","org":"LiquidAI","repo_name":"LFM2-700M-MLX-4bit"}"#;
    let rows = vec![
        model_row("job-q4", q4),
        model_row("job-q4", q4),
        model_row("job-q5", q5),
        model_row("job-filled", filled),
    ];
    answering_in(&duckdb, &MODEL_COLUMNS, &jobs, rows).await;

    let uri = format!("{GROUPS}/{group}/policy-preview");
    let (status, preview) = send(state, &owner, "GET", &uri, None).await?;
    assert_eq!(status, StatusCode::OK, "{preview}");
    crate::api_schema::assert_conforms("CampaignGroupPolicyPreview", &preview)?;
    let mut expected = unread_member(&campaign, "evaluated");
    expected["checks"][0]["on"] = json!(false);
    expected["checks"][1]["on"] = json!(false);
    expected["checks"][4] = json!({
        "kind": "model_params", "on": false, "flagged": 2, "rerun": 0, "marked": 0, "kept": 2,
    });
    expected["answered_job_count"] = json!(3);
    expected["earliest_deadline"] = json!(future);
    expected["kept_job_count"] = json!(3);
    expected["missing_model_params"] = json!([
        { "key": "granite-4.2-8b", "row_count": 3, "job_count": 2 },
    ]);
    expected["last_filing"] = last_filing(&kept);
    assert_eq!(preview, json!({ "members": [expected] }));
    Ok(())
}

/// The preview flags a failed job whose cell ran clean at a larger size, by
/// the jobs' sources and the catalog, whether the policy is on or not; a job
/// with no source has no family. The failed policy, on with a code's re-runs
/// and none at the top level, triggers the member by that code alone.
#[tokio::test]
async fn the_preview_shows_a_failure_below_a_clean_larger_run() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(sql) = seeded_sql_state(dir.path()).await? else {
        return Ok(());
    };
    std::fs::write(
        dir.path().join("benchmarks/prefill_throughput_1024.toml"),
        "benchmark_type = \"prefill_throughput\"\nparameter_prefill_tokens = 1024",
    )?;
    let duckdb = MockServer::start_async().await;
    let state = &with_duckdb(sql.state.clone(), &duckdb.base_url())?;
    let org = seed_org(state, "acme").await?;
    let owner = org_user(state, &org, RERUNNER).await?;
    let group = created_group(state, &owner, json!({ "name": "aa-perf" })).await?;
    // Failed flags the client's out_of_memory, not the expired run.
    let codes = json!({ "codes": { "out_of_memory": 1 } });
    let failed = policy_body(true, 0, codes, &default_policies(&group)[2]);
    policy_written(state, &owner, &group, "failed", &failed).await?;
    let mut campaign = put_idle_campaign_of(state, personal(&owner), "aa-perf").await?;
    let future = "2099-01-01T00:00:00Z";
    let jobs = ["job-small", "job-large", "job-sourceless"];
    running(state, &mut campaign, PlanStatus::Active, &jobs, future).await?;
    added(state, &owner, &group, &[campaign.campaign_id.as_str()]).await?;
    let sources = [
        ("job-small", "prefill_throughput_256"),
        ("job-large", "prefill_throughput_1024"),
    ];
    futures::future::try_join_all(sources.map(|(job_id, benchmark)| async move {
        let body = json!({ "job_id": job_id, "spec": { "benchmark": benchmark } });
        state
            .plan_store
            .put_job_source(&JobId::try_new(job_id)?, &body)
            .await
    }))
    .await?;
    let row = |job_id: &str, benchmark: &str, failure: Option<(&str, &str)>| {
        json!([
            job_id,
            benchmark,
            "prefill_throughput",
            "ttft",
            "ms",
            failure.is_some(),
            failure.map(|(source, _)| source),
            failure.map(|(_, code)| code),
        ])
    };
    let columns = [
        "job_id",
        "benchmark_id",
        "benchmark_type",
        "metric",
        "unit",
        "failed",
        "failure_source",
        "failure_code",
    ];
    let rows = vec![
        row(
            "job-small",
            "prefill_throughput_256",
            Some(("client", "out_of_memory")),
        ),
        row("job-large", "prefill_throughput_1024", None),
        row(
            "job-sourceless",
            "prefill_throughput_256",
            Some(("expired", "unspecified")),
        ),
    ];
    answering_in(&duckdb, &columns, &jobs, rows).await;

    let uri = format!("{GROUPS}/{group}/policy-preview");
    let (status, preview) = send(state, &owner, "GET", &uri, None).await?;
    assert_eq!(status, StatusCode::OK, "{preview}");
    crate::api_schema::assert_conforms("CampaignGroupPolicyPreview", &preview)?;
    assert_eq!(
        preview["members"][0]["checks"][2],
        check_counts("failed", true, 1, 1, 0)
    );
    assert_eq!(preview["members"][0]["triggered"], json!(["failed"]));
    assert_eq!(
        preview["members"][0]["checks"][5],
        json!({
            "kind": "smaller_failed", "on": false, "flagged": 1, "rerun": 0, "marked": 0, "kept": 1,
        })
    );
    assert_eq!(
        preview["members"][0]["smaller_failed"],
        json!([{
            "job_id": "job-small",
            "benchmark_id": "prefill_throughput_256",
            "failure_source": "client",
            "failure_code": "out_of_memory",
            "model_key": null,
            "clean_job_id": "job-large",
            "clean_benchmark_id": "prefill_throughput_1024",
        }])
    );
    Ok(())
}

/// The preview and the re-run results take the plan reads and `read_clients`,
/// and read only the members whose plans the caller reads. The preview also
/// takes a `[duckdb]`.
#[rstest]
#[case::preview(
    "policy-preview",
    "/members/0/state",
    json!("no_plan"),
    json!({ "members": [] }),
    Some("the policy preview is not served: no [duckdb] is configured")
)]
#[case::rerun_results(
    "rerun-results",
    "/total",
    json!(1),
    json!({ "summary": [], "total": 0, "offset": 0, "limit": 100, "rows": [] }),
    None
)]
#[tokio::test]
async fn the_preview_and_rerun_results_take_the_plan_reads_and_read_clients(
    #[case] route: &str,
    #[case] field: &str,
    #[case] member_value: Value,
    #[case] no_member: Value,
    #[case] without_duckdb: Option<&str>,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(sql) = seeded_sql_state(dir.path()).await? else {
        return Ok(());
    };
    let duckdb = MockServer::start_async().await;
    let state = &with_duckdb(sql.state.clone(), &duckdb.base_url())?;
    let (org, member) = org_operator(state).await?;
    let previewer = org_user(state, &org, PREVIEWER).await?;
    let stranger = registered_user(state, PREVIEWER).await?;
    let team = team_group(state, &member, &org).await?;
    let team_owner = CampaignOwner::Org {
        org_id: org.org_id.clone(),
    };
    let idle = campaign_of(state, &team_owner).await?;
    added(state, &member, &team, &[&idle]).await?;
    let groups = state
        .campaign_group_store
        .as_deref()
        .context("a [database] keeps campaign groups")?;
    let group_id = CampaignGroupId::try_new(team.as_str())?;
    let rerun = a_rerun(&CampaignId::try_new(&idle)?, "job-1", Utc::now(), 4096.0)?;
    anyhow::ensure!(
        groups
            .record_reruns(&group_id, std::slice::from_ref(&rerun))
            .await?
            == 1
    );
    let reader = campaign_reader(state, &org, &idle).await?;
    let admin = org_user(
        state,
        &org,
        &[UserPermission::ManageOrgs, UserPermission::ReadClients],
    )
    .await?;
    // Reads every group, but without `read_any_plans` only its own campaigns' plans.
    let server = registered_user(
        state,
        &[
            UserPermission::ManageAnyOrgs,
            UserPermission::ManageOrgs,
            UserPermission::ReadPlans,
            UserPermission::ReadClients,
        ],
    )
    .await?;
    let uri = format!("{GROUPS}/{team}/{route}");

    let (status, answer) = send(state, &previewer, "GET", &uri, None).await?;
    assert_eq!(
        (status, answer.pointer(field)),
        (StatusCode::OK, Some(&member_value)),
        "{answer}"
    );
    let (status, answer) = send(state, &server, "GET", &uri, None).await?;
    assert_eq!((status, answer), (StatusCode::OK, no_member));
    futures::stream::iter(
        [
            (&member, "user lacks the read_clients permission"),
            (
                &reader,
                "a campaign reader holds read_plans alone, not read_clients",
            ),
            (&admin, "user lacks the read_plans permission"),
            (&stranger, "this campaign group belongs to another owner"),
        ]
        .map(anyhow::Ok),
    )
    .try_for_each(|(signer, refused)| {
        let uri = &uri;
        async move {
            let (status, answer) = send(state, signer, "GET", uri, None).await?;
            assert_eq!(
                (status, answer["error"].as_str()),
                (StatusCode::FORBIDDEN, Some(refused))
            );
            anyhow::Ok(())
        }
    })
    .await?;
    let (status, answer) = send(&sql.state, &previewer, "GET", &uri, None).await?;
    let served = without_duckdb.map_or(StatusCode::OK, |_| StatusCode::NOT_FOUND);
    assert_eq!((status, answer["error"].as_str()), (served, without_duckdb));
    Ok(())
}

/// A swap re-run of `job` in `campaign_id`, filed at `filed_at` over a run
/// that swapped `before` bytes, waiting.
fn a_rerun(
    campaign_id: &CampaignId,
    job: &str,
    filed_at: DateTime<Utc>,
    before: f64,
) -> anyhow::Result<PolicyRerun> {
    Ok(PolicyRerun {
        campaign_id: campaign_id.clone(),
        kind: PolicyKind::Swap,
        rerun_job_id: JobId::try_new(job)?,
        rerun_of: JobId::try_new(format!("{job}-before"))?,
        plan_id: PlanId::from_uuid(uuid::Uuid::now_v7()),
        filed_at: filed_at.trunc_subsecs(6),
        attempt: 1,
        benchmark_id: "decode_throughput_512_100".to_owned(),
        model_key: None,
        before,
        threshold: Some(0.0),
        short_repetition_ms: None,
        short_repetition_percent: None,
        codes: None,
        before_code: None,
        after: None,
        after_code: None,
        outcome: None,
        judged_at: None,
    })
}

/// The re-run results answer what queue-maintenance recorded of the members
/// whose plans the caller reads: a summary per policy over all their rows, and
/// the rows newest filing first, a page at a time.
#[tokio::test]
async fn the_rerun_results_sum_up_and_page_the_readable_members_reruns() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(sql) = seeded_sql_state(dir.path()).await? else {
        return Ok(());
    };
    let state = &sql.state;
    let (org, member) = org_operator(state).await?;
    let previewer = org_user(state, &org, PREVIEWER).await?;
    let team = team_group(state, &member, &org).await?;
    let team_owner = CampaignOwner::Org {
        org_id: org.org_id.clone(),
    };
    let (read, unread) = (
        campaign_of(state, &team_owner).await?,
        campaign_of(state, &team_owner).await?,
    );
    added(state, &member, &team, &[&read, &unread]).await?;
    // The member's own campaign now, which the previewer does not read.
    owner_written(state, &unread, personal(&member)).await?;

    let (read, unread) = (CampaignId::try_new(&read)?, CampaignId::try_new(&unread)?);
    let at = Utc::now();
    let minutes = chrono::Duration::minutes;
    let waiting = a_rerun(&read, "job-1", at - minutes(3), 4096.0)?;
    let cleared = a_rerun(&read, "job-2", at - minutes(2), 1024.0)?;
    let flagged = PolicyRerun {
        kind: PolicyKind::Spread,
        threshold: Some(5.0),
        short_repetition_ms: None,
        short_repetition_percent: None,
        ..a_rerun(&read, "job-3", at - minutes(1), 10.0)?
    };
    let theirs = a_rerun(&unread, "job-4", at, 1.0)?;
    let groups = state
        .campaign_group_store
        .as_deref()
        .context("a [database] keeps campaign groups")?;
    let group_id = CampaignGroupId::try_new(team.as_str())?;
    let recorded = [waiting, cleared.clone(), flagged.clone(), theirs];
    anyhow::ensure!(groups.record_reruns(&group_id, &recorded).await? == 4);
    let judged = |rerun: &PolicyRerun, outcome, after| PolicyRerun {
        after: Some(after),
        outcome: Some(outcome),
        judged_at: Some(at.trunc_subsecs(6)),
        ..rerun.clone()
    };
    let outcomes = [
        judged(&cleared, RerunOutcome::Cleared, 0.0),
        judged(&flagged, RerunOutcome::Flagged, 8.0),
    ];
    anyhow::ensure!(groups.judge_reruns(&group_id, &outcomes).await? == 2);

    let uri = format!("{GROUPS}/{team}/rerun-results");
    let (status, page) = send(
        state,
        &previewer,
        "GET",
        &format!("{uri}?limit=2&offset=1"),
        None,
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "{page}");
    crate::api_schema::assert_conforms("CampaignGroupRerunResults", &page)?;
    assert_eq!(
        page["summary"],
        json!([
            {
                "kind": "swap", "reruns": 2, "waiting": 1, "cleared": 1, "flagged": 0,
                "failed": 0, "dropped": 0, "median_before": 1024.0, "median_after": 0.0,
            },
            {
                "kind": "spread", "reruns": 1, "waiting": 0, "cleared": 0, "flagged": 1,
                "failed": 0, "dropped": 0, "median_before": 10.0, "median_after": 8.0,
            },
        ])
    );
    let paged = |page: &Value| -> anyhow::Result<(Value, Value, Value, BTreeSet<String>)> {
        let jobs = ids(page.get("rows"), "rerun_job_id")?;
        Ok((
            page["total"].clone(),
            page["offset"].clone(),
            page["limit"].clone(),
            jobs,
        ))
    };
    assert_eq!(
        paged(&page)?,
        (
            json!(3),
            json!(1),
            json!(2),
            BTreeSet::from(["job-2".to_owned(), "job-1".to_owned()])
        )
    );
    assert_eq!(page["rows"][0]["rerun_job_id"], "job-2");
    let (status, all) = send(state, &previewer, "GET", &format!("{uri}?all=true"), None).await?;
    assert_eq!(status, StatusCode::OK, "{all}");
    assert_eq!(
        paged(&all)?,
        (
            json!(3),
            json!(0),
            json!(3),
            BTreeSet::from(["job-1", "job-2", "job-3"].map(str::to_owned))
        )
    );
    assert_eq!(all["rows"][0]["rerun_job_id"], "job-3");
    Ok(())
}

#[rstest]
#[case::list("GET", "", None)]
#[case::create("POST", "", Some(r#"{ "name": "aa-perf" }"#))]
#[case::read("GET", "/group_0195c3e2a4f37b8c", None)]
#[case::rename("PATCH", "/group_0195c3e2a4f37b8c", Some(r#"{ "name": "aa-perf" }"#))]
#[case::delete("DELETE", "/group_0195c3e2a4f37b8c", None)]
#[case::add(
    "POST",
    "/group_0195c3e2a4f37b8c/members",
    Some(r#"{ "campaign_ids": ["campaign_0195c3e2a4f37b8c"] }"#)
)]
#[case::remove(
    "DELETE",
    "/group_0195c3e2a4f37b8c/members/campaign_0195c3e2a4f37b8c",
    None
)]
#[case::policies("GET", "/group_0195c3e2a4f37b8c/policies", None)]
#[case::set_policy(
    "PUT",
    "/group_0195c3e2a4f37b8c/policies/swap",
    Some(
        r#"{ "enabled": true, "max_reruns": 5, "settings": {},
             "base": { "enabled": false, "max_reruns": 5, "settings": {} } }"#
    )
)]
#[case::policy_preview("GET", "/group_0195c3e2a4f37b8c/policy-preview", None)]
#[case::rerun_results("GET", "/group_0195c3e2a4f37b8c/rerun-results", None)]
#[tokio::test]
async fn every_route_needs_a_database(
    #[case] method: &str,
    #[case] path: &str,
    #[case] body: Option<&str>,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_object_state(dir.path()).await?;
    let previewer = registered_user(&state, PREVIEWER).await?;
    let body: Option<Value> = body.map(serde_json::from_str).transpose()?;
    let uri = format!("{GROUPS}{path}");
    let (status, answer) = send(&state, &previewer, method, &uri, body.as_ref()).await?;
    assert_eq!(
        (status, answer["error"].as_str()),
        (
            StatusCode::NOT_FOUND,
            Some("campaign groups are kept only on a [database]")
        )
    );
    Ok(())
}
