//! A plan's note and its revision change, which only the SQL store keeps: set
//! at filing, read back by every plan read, and the note changed afterwards
//! with `PATCH /api/v1/plans/{plan_id}`.

use anyhow::Context;
use axum::http::StatusCode;
use futures::TryStreamExt;
use serde_json::{Value, json};

use pipette_httpapi::state::AppState;
use pipette_plan_types::{CampaignId, PlanId, UserPermission};
use pipette_web_testkit::{
    FULL_EDITOR, MLX_700M_VARIANT, UserSigner, body_json, created_campaign, json_str,
    post_plan_document, put_loose_plan, register_and_approve, registered_user, seed_org,
    seeded_sql_state, set_user_org, signed_empty, signed_json,
};

/// `PATCH /api/v1/plans/{plan_id}` with `body` as `signer`: the status and the answer.
async fn patch(
    state: &AppState,
    signer: &UserSigner,
    plan_id: &PlanId,
    body: &Value,
) -> anyhow::Result<(StatusCode, Value)> {
    let resp = signed_json(
        state,
        signer,
        "PATCH",
        &format!("/api/v1/plans/{plan_id}"),
        body,
    )
    .await?;
    let status = resp.status();
    Ok((status, body_json(resp).await?))
}

/// The plan's note as its read answers it, `null` when it has none.
async fn read_note(
    state: &AppState,
    signer: &UserSigner,
    plan_id: &PlanId,
) -> anyhow::Result<Value> {
    let body = read(state, signer, &format!("/api/v1/plans/{plan_id}")).await?;
    Ok(body.get("note").cloned().unwrap_or(Value::Null))
}

/// The plan id, note and `changed` of a `PATCH` answer, which is the whole plan.
fn noted(answer: &Value) -> Value {
    json!({
        "plan_id": answer.get("plan_id"),
        "note": answer.get("note"),
        "changed": answer.get("changed"),
    })
}

/// The test checks these behaviors for the owner of a plan:
/// - The server trims the note.
/// - The server keeps the line breaks inside the note.
/// - A blank note clears the note.
/// - The same note again, or no `note`, changes nothing.
#[tokio::test]
async fn test_the_owner_sets_and_clears_a_note() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(sql) = seeded_sql_state(dir.path()).await? else {
        return Ok(());
    };
    let state = &sql.state;
    let owner = registered_user(state, FULL_EDITOR).await?;
    let (_, plan_id) =
        created_campaign(state, &owner, &[("name", "Nightly"), ("note", "first")]).await?;
    assert_eq!(read_note(state, &owner, &plan_id).await?, "first");

    let body = json!({"note": "  Thermals on bos-17pro-1,\nsee the second run  "});
    crate::api_schema::assert_conforms("UpdatePlanRequest", &body)?;
    let (status, answer) = patch(state, &owner, &plan_id, &body).await?;
    assert_eq!(status, StatusCode::OK, "{answer}");
    crate::api_schema::assert_conforms("ChangedPlanView", &answer)?;
    let note = "Thermals on bos-17pro-1,\nsee the second run";
    assert_eq!(
        noted(&answer),
        json!({"plan_id": plan_id.as_str(), "note": note, "changed": true})
    );
    assert_eq!(read_note(state, &owner, &plan_id).await?, note);

    for unchanged in [json!({"note": note}), json!({}), json!({"note": null})] {
        let (status, answer) = patch(state, &owner, &plan_id, &unchanged).await?;
        assert_eq!(status, StatusCode::OK, "{answer}");
        assert_eq!(
            noted(&answer),
            json!({"plan_id": plan_id.as_str(), "note": note, "changed": false})
        );
    }

    let (status, answer) = patch(state, &owner, &plan_id, &json!({"note": "  "})).await?;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(
        noted(&answer),
        json!({"plan_id": plan_id.as_str(), "note": null, "changed": true})
    );
    assert_eq!(read_note(state, &owner, &plan_id).await?, Value::Null);
    Ok(())
}

/// The server rejects a note over the limit, counted in characters. It also
/// rejects a note with a NUL character and a body with an unknown field. In each
/// case the stored note does not change.
#[tokio::test]
async fn test_a_long_note_or_another_field_is_refused() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(sql) = seeded_sql_state(dir.path()).await? else {
        return Ok(());
    };
    let state = &sql.state;
    let owner = registered_user(state, FULL_EDITOR).await?;
    let (_, plan_id) =
        created_campaign(state, &owner, &[("name", "Nightly"), ("note", "first")]).await?;

    let (status, _) = patch(state, &owner, &plan_id, &json!({"note": "🙂".repeat(4096)})).await?;
    assert_eq!(status, StatusCode::OK);
    for refused in [
        json!({"note": "n".repeat(4097)}),
        json!({"note": "a\u{0}b"}),
        json!({"status": "cancelled"}),
    ] {
        let (status, answer) = patch(state, &owner, &plan_id, &refused).await?;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{answer}");
    }
    assert_eq!(read_note(state, &owner, &plan_id).await?, "🙂".repeat(4096));
    Ok(())
}

/// The plan of another account reads as absent. A reader without `submit_plans`
/// gets a refusal at the gate. The plan page offers the edit to the owner only.
#[tokio::test]
async fn test_only_who_may_file_changes_a_note() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(sql) = seeded_sql_state(dir.path()).await? else {
        return Ok(());
    };
    let state = &sql.state;
    let owner = registered_user(state, FULL_EDITOR).await?;
    let stranger = registered_user(state, FULL_EDITOR).await?;
    let reader = registered_user(state, &[UserPermission::ReadAnyPlans]).await?;
    let (_, plan_id) =
        created_campaign(state, &owner, &[("name", "Nightly"), ("note", "first")]).await?;

    let body = json!({"note": "taken"});
    let (status, _) = patch(state, &stranger, &plan_id, &body).await?;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = patch(state, &reader, &plan_id, &body).await?;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(read_note(state, &owner, &plan_id).await?, "first");

    for (signer, may) in [(&owner, true), (&reader, false)] {
        assert_eq!(may_edit_note(state, signer, &plan_id).await?, may);
    }
    Ok(())
}

/// An organization's campaign takes a note from each member who files, and
/// from a server admin; a filer outside it who reads every plan is refused.
#[tokio::test]
async fn test_a_team_s_plan_takes_a_note_from_its_members() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(sql) = seeded_sql_state(dir.path()).await? else {
        return Ok(());
    };
    let state = &sql.state;
    let acme = seed_org(state, "acme").await?;
    let author = registered_user(
        state,
        &[FULL_EDITOR, &[UserPermission::ManageOrgs]].concat(),
    )
    .await?;
    let member = registered_user(state, FULL_EDITOR).await?;
    for signer in [&author, &member] {
        set_user_org(state, &signer.1, Some(acme.org_id.clone())).await?;
    }
    let reads_all = [UserPermission::SubmitPlans, UserPermission::ReadAnyPlans];
    let outsider = registered_user(state, &reads_all).await?;
    let admin = registered_user(
        state,
        &[&reads_all[..], &[UserPermission::ManageAnyOrgs]].concat(),
    )
    .await?;
    let params = [("name", "Team"), ("owner", acme.org_id.as_str())];
    let (_, plan_id) = created_campaign(state, &author, &params).await?;

    assert_note_edits(
        state,
        &plan_id,
        &[
            (&member, true, "by the member"),
            (&admin, true, "by the admin"),
            (&outsider, false, "by the outsider"),
        ],
    )
    .await?;
    assert_eq!(read_note(state, &author, &plan_id).await?, "by the admin");
    Ok(())
}

/// The plan page's `may_edit_note` for `signer`.
async fn may_edit_note(
    state: &AppState,
    signer: &UserSigner,
    plan_id: &PlanId,
) -> anyhow::Result<Value> {
    let uri = format!("/api/v1/plans/{plan_id}/jobs?results=true");
    let body = read(state, signer, &uri).await?;
    Ok(body.get("may_edit_note").cloned().unwrap_or(Value::Null))
}

/// For each `(signer, may, note)` in order: the plan page offers `signer` the
/// edit if `may`, and a `PATCH` of `note` answers `200` if `may`, else `403`.
async fn assert_note_edits(
    state: &AppState,
    plan_id: &PlanId,
    cases: &[(&UserSigner, bool, &str)],
) -> anyhow::Result<()> {
    for &(signer, may, note) in cases {
        assert_eq!(may_edit_note(state, signer, plan_id).await?, may);
        let (status, answer) = patch(state, signer, plan_id, &json!({"note": note})).await?;
        let expected = if may {
            StatusCode::OK
        } else {
            StatusCode::FORBIDDEN
        };
        assert_eq!(status, expected, "{answer}");
    }
    Ok(())
}

/// `GET uri` as `signer`, which must answer `200`.
async fn read(state: &AppState, signer: &UserSigner, uri: &str) -> anyhow::Result<Value> {
    let resp = signed_empty(state, signer, "GET", uri).await?;
    anyhow::ensure!(resp.status() == StatusCode::OK, "{uri}: {}", resp.status());
    body_json(resp).await
}

/// The note and the change of a revision appear on these surfaces:
/// - the plan
/// - the history of its campaign
/// - the bases that a revision reads
/// - the plan listing
/// - the campaign's row with `plan=true`, listed and read alone
/// - the plans listed for one job of the plan
#[tokio::test]
async fn test_every_read_answers_the_note_and_the_change() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(sql) = seeded_sql_state(dir.path()).await? else {
        return Ok(());
    };
    let state = &sql.state;
    let owner = registered_user(state, FULL_EDITOR).await?;
    let (campaign_id, first) =
        created_campaign(state, &owner, &[("name", "Nightly"), ("note", "first")]).await?;
    let uri = format!("/api/v1/campaigns/{campaign_id}/variants?base_plan_id={first}&note=rev-1");
    let resp = post_plan_document(state, &owner, &uri, MLX_700M_VARIANT).await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let filed = body_json(resp).await?;
    let revision = json_str(&filed["plan"], "plan_id")?.to_string();
    let change = json!({"type": "variants", "removed_variant_count": 0, "added_variant_count": 1});
    assert_eq!(filed["plan"]["change"], change);
    let noted = |plan: &Value| (plan.get("note").cloned(), plan.get("change").cloned());
    let revised = (Some(json!("rev-1")), Some(change));

    let campaign = format!("/api/v1/campaigns/{campaign_id}");
    let one_plan = [
        (format!("/api/v1/plans/{revision}"), ""),
        (format!("{campaign}/variants"), "/base"),
        (format!("{campaign}/document"), "/base"),
        (format!("{campaign}?plan=true"), "/plan"),
    ];
    let (signer, expected) = (&owner, &revised);
    futures::stream::iter(one_plan.map(anyhow::Ok))
        .try_for_each(|(uri, at)| async move {
            let body = read(state, signer, &uri).await?;
            let plan = body
                .pointer(at)
                .with_context(|| format!("{uri}: no {at}: {body}"))?;
            assert_eq!(&noted(plan), expected, "{uri}");
            anyhow::Ok(())
        })
        .await?;

    let overview = read(
        state,
        &owner,
        &format!("/api/v1/campaigns/{campaign_id}/overview"),
    )
    .await?;
    let history: Vec<_> = overview["history"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|row| noted(&row["plan"]))
        .collect();
    assert_eq!(history, [revised.clone(), (Some(json!("first")), None)]);

    let listed = read(
        state,
        &owner,
        &format!("/api/v1/plans?campaign_id={campaign_id}"),
    )
    .await?;
    let mut listed: Vec<_> = listed["items"]
        .as_array()
        .into_iter()
        .flatten()
        .map(noted)
        .collect();
    listed.sort_by_key(|(note, _)| note.as_ref().map(Value::to_string));
    assert_eq!(listed, [(Some(json!("first")), None), revised.clone()]);

    let rows = read(state, &owner, "/api/v1/campaigns?plan=true").await?;
    let row = rows["items"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|row| row.get("campaign_id") == Some(&json!(campaign_id)))
        .context("the campaign is listed")?;
    assert_eq!(noted(&row["plan"]), revised, "{row}");

    let jobs = read(state, &owner, &format!("/api/v1/plans/{first}/jobs")).await?;
    let job_id = json_str(&jobs["jobs"][0], "job_id")?;
    let by_job = read(state, &owner, &format!("/api/v1/jobs/{job_id}/plans")).await?;
    let notes: Vec<_> = by_job["plans"]
        .as_array()
        .into_iter()
        .flatten()
        .map(noted)
        .collect();
    assert_eq!(notes, [(Some(json!("first")), None), revised], "{by_job}");
    Ok(())
}

/// Outside a campaign, the filer changes its own plan's note and a server admin
/// any; another filer who reads every plan may not.
#[tokio::test]
async fn test_a_loose_plan_takes_a_note_from_its_filer() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(sql) = seeded_sql_state(dir.path()).await? else {
        return Ok(());
    };
    let state = &sql.state;
    let (_, client_id) = register_and_approve(state).await?;
    let filer = registered_user(state, FULL_EDITOR).await?;
    let reads_all = [UserPermission::SubmitPlans, UserPermission::ReadAnyPlans];
    let other = registered_user(state, &reads_all).await?;
    let admin = registered_user(
        state,
        &[&reads_all[..], &[UserPermission::ManageAnyOrgs]].concat(),
    )
    .await?;
    let (key, user_id, key_id) = &filer;
    let plan_id = put_loose_plan(state, key, user_id, key_id, &client_id, "loose", 1).await?;

    assert_note_edits(
        state,
        &plan_id,
        &[
            (&filer, true, "by the filer"),
            (&other, false, "by another filer"),
            (&admin, true, "by the admin"),
        ],
    )
    .await?;
    assert_eq!(read_note(state, &filer, &plan_id).await?, "by the admin");
    Ok(())
}

/// A plan whose campaign record was deleted keeps naming the campaign. Its note
/// is then only for `manage_any_orgs`: the campaign's author, who still reads
/// every plan, is refused.
#[tokio::test]
async fn test_a_deleted_campaign_s_plan_takes_a_note_from_an_admin_alone() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(sql) = seeded_sql_state(dir.path()).await? else {
        return Ok(());
    };
    let state = &sql.state;
    let reads_all = [FULL_EDITOR, &[UserPermission::ReadAnyPlans]].concat();
    let author = registered_user(state, &reads_all).await?;
    let admin = registered_user(
        state,
        &[&reads_all[..], &[UserPermission::ManageAnyOrgs]].concat(),
    )
    .await?;
    let (campaign_id, plan_id) = created_campaign(state, &author, &[("name", "Gone")]).await?;
    state
        .campaign_store
        .delete_campaign(&CampaignId::try_new(campaign_id)?)
        .await?;

    assert_note_edits(
        state,
        &plan_id,
        &[
            (&author, false, "by the author"),
            (&admin, true, "by the admin"),
        ],
    )
    .await?;
    assert_eq!(read_note(state, &admin, &plan_id).await?, "by the admin");
    Ok(())
}
