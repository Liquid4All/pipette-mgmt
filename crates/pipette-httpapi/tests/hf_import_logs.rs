//! The log lines of a gated model's import: a token reaches none, on success or
//! refusal. Its own test target, since it asserts on the log stream
//! (`pipette_web_testkit::logs`).

use axum::http::StatusCode;
use futures::{StreamExt, TryStreamExt};
use rstest::rstest;

use pipette_hub_testkit::{BAD_TOKEN, GOOD_TOKEN};
use pipette_plan_types::UserPermission;
use pipette_web_testkit::{
    FULL_EDITOR, GatedModel, capture_logs, created_campaign, gated_hub, gated_macos_document,
    make_state_with_hf_endpoint, own_campaign_plans_uri, post_plan_document, put_plan_document,
    registered_user, setup_benchmarks,
};

fn assert_no_token_logged(logs: &str) {
    assert!(
        !logs.contains("auth_token"),
        "a token key is logged: {logs}"
    );
    assert!(!logs.contains("SECRET"), "a token is logged: {logs}");
}

/// A route that files a gated plan document.
#[derive(Debug, Clone, Copy)]
enum Route {
    /// `POST /api/v1/campaigns`.
    CreateCampaign,
    /// `POST /api/v1/campaigns/{campaign_id}/plans`.
    FileIntoCampaign,
    /// `PUT /api/v1/campaigns/{campaign_id}/document`.
    ReviseDocument,
}

/// Each route logs that the store kept the gated document, and neither the
/// token nor its key.
#[rstest]
#[case::create_campaign(Route::CreateCampaign)]
#[case::file_into_campaign(Route::FileIntoCampaign)]
#[case::revise_document(Route::ReviseDocument)]
fn a_gated_document_logs_no_token(#[case] route: Route) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (status, logs) = capture_logs(|runtime| {
        runtime.block_on(async {
            let hub = gated_hub().await?;
            setup_benchmarks(dir.path())?;
            let state = make_state_with_hf_endpoint(dir.path(), hub.endpoint.as_str()).await?;
            let signer = registered_user(&state, FULL_EDITOR).await?;
            let document = gated_macos_document(GatedModel::Gguf, GOOD_TOKEN);
            let response = match route {
                Route::FileIntoCampaign => {
                    let uri = own_campaign_plans_uri(&state, &signer.1).await?;
                    post_plan_document(&state, &signer, &uri, &document).await?
                }
                Route::CreateCampaign => {
                    post_plan_document(&state, &signer, "/api/v1/campaigns?name=Gated", &document)
                        .await?
                }
                Route::ReviseDocument => {
                    let (campaign_id, first) =
                        created_campaign(&state, &signer, &[("name", "Gated")]).await?;
                    let uri =
                        format!("/api/v1/campaigns/{campaign_id}/document?base_plan_id={first}");
                    put_plan_document(&state, &signer, &uri, &document).await?
                }
            };
            anyhow::Ok(response.status())
        })
    })?;
    assert_eq!(status?, StatusCode::CREATED, "{logs}");
    assert!(
        logs.contains("plan document stored with vault arms"),
        "{logs}"
    );
    assert_no_token_logged(&logs);
    Ok(())
}

/// The import logs its start and its records, and no token or key, nor does a
/// refusal.
#[test]
fn an_import_logs_no_token() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (statuses, logs) = capture_logs(|runtime| {
        runtime.block_on(async {
            let hub = gated_hub().await?;
            setup_benchmarks(dir.path())?;
            let state = make_state_with_hf_endpoint(dir.path(), hub.endpoint.as_str()).await?;
            let signer = registered_user(&state, &[UserPermission::SubmitPlans]).await?;
            let uri = own_campaign_plans_uri(&state, &signer.1).await?;
            let (state, signer, uri) = (&state, &signer, &uri);
            let statuses: Vec<StatusCode> = futures::stream::iter([
                (GatedModel::Gguf, GOOD_TOKEN),
                (GatedModel::Mlx, BAD_TOKEN),
            ])
            .then(|(model, token)| async move {
                let document = gated_macos_document(model, token);
                let response = post_plan_document(state, signer, uri, &document).await?;
                anyhow::Ok(response.status())
            })
            .try_collect()
            .await?;
            anyhow::Ok(statuses)
        })
    })?;
    assert_eq!(
        statuses?,
        [StatusCode::CREATED, StatusCode::BAD_REQUEST],
        "{logs}"
    );

    let (imported, refused) = logs
        .split_once("hf import refused by hub")
        .ok_or_else(|| anyhow::anyhow!("no refusal is logged: {logs}"))?;
    let missing = ["hf import step started", "hf import stored record"]
        .into_iter()
        .find(|line| !imported.contains(line));
    assert!(missing.is_none(), "`{missing:?}` is not logged: {logs}");
    assert_no_token_logged(&logs);
    assert!(refused.contains("rejected a submitted plan"), "{refused}");
    Ok(())
}
