//! `/api/v1/model-params`: one editable row of parameter counts per model.

use axum::http::StatusCode;
use rstest::rstest;
use serde_json::{Value, json};

use pipette_core::user_permission::UserPermissionGroup;
use pipette_httpapi::state::AppState;
use pipette_web_testkit::*;

const LIST: &str = "/api/v1/model-params";

async fn admin() -> anyhow::Result<(tempfile::TempDir, AppState, UserSigner)> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let admin = registered_user(&state, UserPermissionGroup::ServerAdmin.permissions()).await?;
    Ok((dir, state, admin))
}

async fn send(
    state: &AppState,
    signer: &UserSigner,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> anyhow::Result<(StatusCode, Value)> {
    let resp = match body {
        Some(body) => signed_json(state, signer, method, uri, &body).await?,
        None => signed_empty(state, signer, method, uri).await?,
    };
    let status = resp.status();
    let body = if status == StatusCode::NO_CONTENT {
        Value::Null
    } else {
        body_json(resp).await?
    };
    Ok((status, body))
}

/// An add keys the row by the name lookup normalizes; an edit and a delete
/// address that key.
#[tokio::test]
async fn an_administrator_adds_edits_and_deletes_a_row() -> anyhow::Result<()> {
    let (_dir, state, admin) = admin().await?;

    let (status, created) = send(
        &state,
        &admin,
        "POST",
        LIST,
        Some(json!({
            "model_key": "LiquidAI/AcmeLM-4.5B-Text-GGUF",
            "total_millions": 4512,
            "note": " safetensors header at 93f895af ",
        })),
    )
    .await?;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    crate::api_schema::assert_conforms("ModelParams", &created)?;
    assert_eq!(created["model_key"], "acmelm-4.5b-text");
    assert_eq!(created["active_millions"], 4512, "dense by default");
    assert_eq!(created["note"], "safetensors header at 93f895af", "trimmed");
    assert_eq!(created["updated_by"], admin.1.as_str());

    let (status, again) = send(
        &state,
        &admin,
        "POST",
        LIST,
        Some(json!({"model_key": "AcmeLM-4.5B-Text", "total_millions": 1})),
    )
    .await?;
    assert_eq!(status, StatusCode::CONFLICT, "{again}");

    let row = format!("{LIST}/acmelm-4.5b-text");
    let (status, edited) = send(
        &state,
        &admin,
        "PUT",
        &row,
        Some(json!({"total_millions": 8340, "active_millions": 1500})),
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "{edited}");
    crate::api_schema::assert_conforms("ChangedModelParams", &edited)?;
    let (edited, changed) = changed_record(edited)?;
    assert!(changed, "{edited}");
    assert_eq!(
        (&edited["total_millions"], &edited["active_millions"]),
        (&json!(8340), &json!(1500))
    );
    assert_eq!(edited["note"], Value::Null, "the row is replaced whole");

    let (status, listed) = send(&state, &admin, "GET", LIST, None).await?;
    assert_eq!(status, StatusCode::OK);
    crate::api_schema::assert_conforms("ModelParamsPage", &listed)?;
    assert_eq!(
        listed,
        json!({ "items": [edited], "next_cursor": null, "unreadable": 0 })
    );

    let (status, _) = send(&state, &admin, "DELETE", &row, None).await?;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, listed) = send(&state, &admin, "GET", LIST, None).await?;
    assert_eq!(listed["items"], json!([]));
    Ok(())
}

/// Answered whole, so `unreadable` counts every bad row in the keyspace.
#[tokio::test]
async fn the_listing_counts_an_unreadable_row_and_lists_the_rest() -> anyhow::Result<()> {
    let (dir, state, admin) = admin().await?;
    let (status, created) = send(
        &state,
        &admin,
        "POST",
        LIST,
        Some(json!({"model_key": "acmelm", "total_millions": 4512})),
    )
    .await?;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let rows = dir.path().join("model-params");
    std::fs::create_dir_all(&rows)?;
    std::fs::write(rows.join("broken.json"), b"{")?;

    let (status, listed) = send(&state, &admin, "GET", LIST, None).await?;
    assert_eq!(status, StatusCode::OK);
    crate::api_schema::assert_conforms("ModelParamsPage", &listed)?;
    assert_eq!(
        listed,
        json!({ "items": [created], "next_cursor": null, "unreadable": 1 })
    );
    Ok(())
}

#[rstest]
#[case::zero_total("POST", "", json!({"model_key": "acmelm", "total_millions": 0}), StatusCode::BAD_REQUEST)]
#[case::zero_active(
    "POST",
    "",
    json!({"model_key": "acmelm", "total_millions": 100, "active_millions": 0}),
    StatusCode::BAD_REQUEST
)]
#[case::active_above_total(
    "POST",
    "",
    json!({"model_key": "acmelm", "total_millions": 100, "active_millions": 101}),
    StatusCode::BAD_REQUEST
)]
#[case::note_too_long(
    "POST",
    "",
    json!({"model_key": "acmelm", "total_millions": 100, "note": "n".repeat(501)}),
    StatusCode::BAD_REQUEST
)]
#[case::key_outside_the_charset(
    "POST",
    "",
    json!({"model_key": "LFM 3", "total_millions": 100}),
    StatusCode::BAD_REQUEST
)]
#[case::both_model_key_names(
    "POST",
    "",
    json!({"model_key": "acmelm", "model": "acmelm", "total_millions": 100}),
    StatusCode::BAD_REQUEST
)]
#[case::edit_without_a_row(
    "PUT",
    "/acmelm-4.5b-text",
    json!({"total_millions": 1}),
    StatusCode::NOT_FOUND
)]
// The body's counts are checked before the row is looked up.
#[case::zero_total_without_a_row(
    "PUT",
    "/acmelm-4.5b-text",
    json!({"total_millions": 0}),
    StatusCode::BAD_REQUEST
)]
#[case::edit_under_an_uppercase_key(
    "PUT",
    "/ACMELM",
    json!({"total_millions": 1}),
    StatusCode::BAD_REQUEST
)]
#[tokio::test]
async fn a_write_is_refused(
    #[case] method: &str,
    #[case] row: &str,
    #[case] body: Value,
    #[case] refusal: StatusCode,
) -> anyhow::Result<()> {
    let (_dir, state, admin) = admin().await?;
    let uri = format!("{LIST}{row}");
    let (status, answer) = send(&state, &admin, method, &uri, Some(body)).await?;
    assert_eq!(status, refusal, "{answer}");
    Ok(())
}

/// A create reads `model_key` under its former name `model` too.
#[tokio::test]
async fn a_create_reads_the_former_model_name() -> anyhow::Result<()> {
    let (_dir, state, admin) = admin().await?;
    let body = json!({"model": "AcmeLM-4.5B-Text", "total_millions": 100});
    let (status, created) = send(&state, &admin, "POST", LIST, Some(body)).await?;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["model_key"], "acmelm-4.5b-text");
    Ok(())
}

/// A blank note reads as none, as when the form leaves it out.
#[tokio::test]
async fn a_blank_note_is_none() -> anyhow::Result<()> {
    let (_dir, state, admin) = admin().await?;
    let body = json!({"model_key": "acmelm", "total_millions": 100, "note": "  "});
    let (status, created) = send(&state, &admin, "POST", LIST, Some(body)).await?;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["note"], Value::Null);
    Ok(())
}

/// An edit to the values already stored writes nothing: the row keeps the
/// author and time of the edit that set them.
#[tokio::test]
async fn an_edit_to_the_stored_values_keeps_its_author() -> anyhow::Result<()> {
    let (_dir, state, admin) = admin().await?;
    let other = registered_user(&state, UserPermissionGroup::ServerAdmin.permissions()).await?;
    let (status, created) = send(
        &state,
        &admin,
        "POST",
        LIST,
        Some(json!({"model_key": "acmelm", "total_millions": 100, "note": "header"})),
    )
    .await?;
    assert_eq!(status, StatusCode::CREATED, "{created}");

    let row = format!("{LIST}/acmelm");
    let same = json!({"total_millions": 100, "active_millions": 100, "note": " header "});
    let (status, kept) = send(&state, &other, "PUT", &row, Some(same)).await?;
    assert_eq!(status, StatusCode::OK, "{kept}");
    assert_eq!(changed_record(kept)?, (created.clone(), false));

    let (status, edited) = send(
        &state,
        &other,
        "PUT",
        &row,
        Some(json!({"total_millions": 100, "note": "header"})),
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "{edited}");
    assert_eq!(
        changed_record(edited)?,
        (created, false),
        "an absent active count is the dense default"
    );

    let (status, edited) = send(
        &state,
        &other,
        "PUT",
        &row,
        Some(json!({"total_millions": 100})),
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "{edited}");
    assert_eq!(edited["changed"], true, "{edited}");
    assert_eq!(edited["note"], Value::Null);
    assert_eq!(edited["updated_by"], other.1.as_str());
    Ok(())
}
