//! A plan document whose model carries a HuggingFace token, on each route that
//! files a document: ingestion imports the model into the vault and stores the
//! document with the vault arm in its place, so no stored byte or answer holds
//! the token.

use anyhow::Context;
use axum::http::{Method, StatusCode};
use rstest::rstest;
use serde_json::{Value, json};

use pipette_core::campaign::{Campaign, CampaignOwner};
use pipette_core::types::Mint;
use pipette_httpapi::state::AppState;
use pipette_hub_testkit::{BAD_TOKEN, GOOD_TOKEN, MockHub, OTHER_GOOD_TOKEN};
use pipette_plan_types::{CampaignId, NonEmptyTrimmedString, PlanId, UserPermission};
use pipette_store::drain_vault_resources;
use pipette_web_testkit::{
    FULL_EDITOR, GATED_GGUF_FILE, GatedModel, MLX_700M_VARIANT, ONE_CELL_PLAN, UserSigner,
    assert_no_token_stored, body_json, created_campaign, gated_document, gated_hub,
    gated_macos_variant, gated_variant, json_str, make_state_with_hf_endpoint,
    own_campaign_plans_uri, post_plan_document, put_plan_document, registered_user, seed_org,
    send_plan_document, set_user_org, setup_benchmarks, signed_json, stream_lines,
    stream_plan_document,
};

/// A state that imports from `hub`, and an account that files and revises its
/// own campaigns.
async fn import_state(
    dir: &std::path::Path,
    hub: &MockHub,
) -> anyhow::Result<(AppState, UserSigner)> {
    setup_benchmarks(dir)?;
    let state = make_state_with_hf_endpoint(dir, hub.endpoint.as_str()).await?;
    let signer = registered_user(&state, FULL_EDITOR).await?;
    Ok((state, signer))
}

/// Create a campaign from the gated macOS document. Return the campaign's id
/// and its first plan.
async fn gated_campaign(state: &AppState, signer: &UserSigner) -> anyhow::Result<(String, PlanId)> {
    let (status, created) = file(
        state,
        signer,
        Route::CreateCampaign,
        &[gated_macos_variant(GatedModel::Gguf, GOOD_TOKEN)],
    )
    .await?;
    anyhow::ensure!(status == StatusCode::CREATED, "{created}");
    Ok((
        json_str(&created, "campaign_id")?.to_string(),
        filed_plan(&created)?,
    ))
}

/// A Linux variant whose GGUF model carries the good token.
fn linux() -> String {
    gated_variant(GatedModel::Gguf, GOOD_TOKEN, r#"["os:linux"]"#, "linux-x64")
}

/// A route that files a plan document.
#[derive(Debug, Clone, Copy)]
enum Route {
    /// `POST /api/v1/campaigns`.
    CreateCampaign,
    /// `POST /api/v1/campaigns/{campaign_id}/plans`.
    FileIntoCampaign,
    /// `PUT /api/v1/campaigns/{campaign_id}/document`, on a campaign that runs
    /// `ONE_CELL_PLAN`.
    ReviseDocument,
    /// `POST /api/v1/campaigns/{campaign_id}/variants`, which adds the variants to a
    /// campaign that runs `ONE_CELL_PLAN`.
    AddVariants,
}

/// How a test sends a document through a [`Route`].
#[derive(Debug, Clone, Copy, Default)]
struct Ask {
    /// Add `dry_run=true` to the query.
    dry_run: bool,
    /// Ask for the progress stream with `Accept: application/x-ndjson`.
    stream: bool,
}

/// Ask for the progress stream, with no dry run.
const STREAM: Ask = Ask {
    dry_run: false,
    stream: true,
};

/// Send `variants` through `route` as `signer`, as `ask` says, and give the
/// response.
async fn send(
    state: &AppState,
    signer: &UserSigner,
    route: Route,
    variants: &[String],
    ask: Ask,
) -> anyhow::Result<axum::http::Response<axum::body::Body>> {
    let document = gated_document(variants);
    let (method, uri, body) = match route {
        Route::CreateCampaign => ("POST", "/api/v1/campaigns?name=Gated".to_string(), document),
        Route::FileIntoCampaign => {
            let uri = own_campaign_plans_uri(state, &signer.1).await?;
            ("POST", uri, document)
        }
        Route::ReviseDocument => {
            let (campaign_id, first) =
                created_campaign(state, signer, &[("name", "Gated")]).await?;
            let uri = format!("/api/v1/campaigns/{campaign_id}/document?base_plan_id={first}");
            ("PUT", uri, document)
        }
        Route::AddVariants => {
            let (campaign_id, first) =
                created_campaign(state, signer, &[("name", "Gated")]).await?;
            let uri = format!("/api/v1/campaigns/{campaign_id}/variants?base_plan_id={first}");
            ("POST", uri, variants.concat())
        }
    };
    let uri = match (ask.dry_run, uri.contains('?')) {
        (false, _) => uri,
        (true, true) => format!("{uri}&dry_run=true"),
        (true, false) => format!("{uri}?dry_run=true"),
    };
    send_plan_document(state, signer, method, &uri, &body, ask.stream).await
}

/// File `variants` through `route` as `signer`, and give the status and the
/// answer.
async fn file(
    state: &AppState,
    signer: &UserSigner,
    route: Route,
    variants: &[String],
) -> anyhow::Result<(StatusCode, Value)> {
    let response = send(state, signer, route, variants, Ask::default()).await?;
    let status = response.status();
    Ok((status, body_json(response).await?))
}

/// The plan that `answer` reports. A campaign route nests it under `plan`.
fn filed_plan(answer: &Value) -> anyhow::Result<PlanId> {
    let plan = answer.get("plan").unwrap_or(answer);
    Ok(PlanId::try_new(json_str(plan, "plan_id")?)?)
}

async fn stored_document(state: &AppState, plan_id: &PlanId) -> anyhow::Result<String> {
    Ok(state
        .plan_store
        .get_plan_source(plan_id)
        .await?
        .context("the plan source was stored")?
        .text()
        .to_string())
}

async fn record_count(state: &AppState) -> anyhow::Result<usize> {
    Ok(drain_vault_resources(state.vault_store.as_ref())
        .await?
        .items
        .len())
}

/// A document with a gated model files on each route. The stored document
/// names the vault arm, and no stored byte and no answer holds the token.
#[rstest]
#[case::create_campaign(Route::CreateCampaign, GatedModel::Gguf, "vault_url")]
#[case::file_into_campaign(Route::FileIntoCampaign, GatedModel::Mlx, "vault_archive")]
#[case::revise_document(Route::ReviseDocument, GatedModel::Gguf, "vault_url")]
#[case::add_variants(Route::AddVariants, GatedModel::Gguf, "vault_url")]
#[case::create_campaign_speculative(
    Route::CreateCampaign,
    GatedModel::Speculative,
    "vault_archive"
)]
#[case::file_into_campaign_speculative(
    Route::FileIntoCampaign,
    GatedModel::Speculative,
    "vault_archive"
)]
#[case::revise_document_speculative(
    Route::ReviseDocument,
    GatedModel::Speculative,
    "vault_archive"
)]
#[case::add_variants_speculative(Route::AddVariants, GatedModel::Speculative, "vault_archive")]
#[tokio::test]
async fn test_a_gated_document_files_with_its_vault_arm(
    #[case] route: Route,
    #[case] model: GatedModel,
    #[case] source: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = gated_hub().await?;
    let (state, signer) = import_state(dir.path(), &hub).await?;

    let (status, answer) = file(
        &state,
        &signer,
        route,
        &[gated_macos_variant(model, GOOD_TOKEN)],
    )
    .await?;
    assert_eq!(status, StatusCode::CREATED, "{answer}");
    assert!(!answer.to_string().contains("SECRET"), "{answer}");

    let document = stored_document(&state, &filed_plan(&answer)?).await?;
    assert!(
        document.contains(&format!("source = \"{source}\"")),
        "{document}"
    );
    assert!(!document.contains("auth_token"), "{document}");
    assert_no_token_stored(dir.path())
}

/// A dry run of a gated document echoes no token, in its bodies or in the
/// document it would file.
#[rstest]
#[case::create_campaign(Method::POST, "/api/v1/campaigns?name=Gated&dry_run=true")]
#[case::revise_document(
    Method::PUT,
    "/api/v1/campaigns/{campaign_id}/document?base_plan_id={first}&dry_run=true"
)]
#[tokio::test]
async fn test_a_gated_dry_run_echoes_no_token(
    #[case] method: Method,
    #[case] uri: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = gated_hub().await?;
    let (state, signer) = import_state(dir.path(), &hub).await?;
    let (campaign_id, first) = created_campaign(&state, &signer, &[("name", "Head")]).await?;
    let uri = uri
        .replace("{campaign_id}", &campaign_id)
        .replace("{first}", first.as_str());
    let document = gated_document(&[gated_macos_variant(GatedModel::Gguf, GOOD_TOKEN)]);

    let response = match method {
        Method::PUT => put_plan_document(&state, &signer, &uri, &document).await?,
        Method::POST => post_plan_document(&state, &signer, &uri, &document).await?,
        other => anyhow::bail!("no dry run case files with {other}"),
    };
    assert_eq!(response.status(), StatusCode::OK);
    let answer = body_json(response).await?.to_string();
    assert!(!answer.contains("SECRET"), "{answer}");
    assert!(!answer.contains("auth_token"), "{answer}");
    Ok(())
}

/// A dry run refuses a document as a real ingest of it does, with the same
/// status and the same answer. The dry run downloads nothing. The Hub refuses
/// `token_without_access` in the import step. The expansion of the document
/// refuses `token_outside_a_model`, before the import step.
#[rstest]
#[case::token_without_access(gated_document(&[gated_variant(
    GatedModel::Gguf,
    BAD_TOKEN,
    r#"["os:macos"]"#,
    "macos-arm64",
)]))]
#[case::token_outside_a_model(format!("# auth_token = \"{GOOD_TOKEN}\"\n{ONE_CELL_PLAN}"))]
#[tokio::test]
async fn test_a_dry_run_refuses_as_the_ingest_does(#[case] document: String) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = gated_hub().await?;
    let (state, signer) = import_state(dir.path(), &hub).await?;
    let uri = own_campaign_plans_uri(&state, &signer.1).await?;

    let dry_run =
        post_plan_document(&state, &signer, &format!("{uri}?dry_run=true"), &document).await?;
    let dry_run = (dry_run.status(), body_json(dry_run).await?);
    assert_eq!(hub.downloads(), 0);
    let filed = post_plan_document(&state, &signer, &uri, &document).await?;
    let filed = (filed.status(), body_json(filed).await?);

    assert_eq!(dry_run.0, StatusCode::BAD_REQUEST, "{}", dry_run.1);
    assert_eq!(dry_run, filed);
    assert!(!dry_run.1.to_string().contains("SECRET"), "{}", dry_run.1);
    assert_eq!(hub.downloads(), 0);
    Ok(())
}

/// A dry run reports a fetch for a gated model with its byte count, downloading
/// nothing; two tokens for one model give one entry. After an ingest, it
/// reports a reuse. The bodies keep the HuggingFace form in both answers.
#[tokio::test]
async fn test_a_dry_run_reports_a_fetch_then_a_reuse() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = gated_hub().await?;
    let (state, signer) = import_state(dir.path(), &hub).await?;
    let document = gated_document(&[
        gated_macos_variant(GatedModel::Gguf, GOOD_TOKEN),
        gated_variant(
            GatedModel::Gguf,
            OTHER_GOOD_TOKEN,
            r#"["os:linux"]"#,
            "linux-x64",
        ),
    ]);
    let dry_run_uri = format!(
        "{}?dry_run=true",
        own_campaign_plans_uri(&state, &signer.1).await?
    );
    let dry_run = async || -> anyhow::Result<Value> {
        let response = post_plan_document(&state, &signer, &dry_run_uri, &document).await?;
        assert_eq!(response.status(), StatusCode::OK);
        let answer = body_json(response).await?;
        assert!(!answer.to_string().contains("SECRET"), "{answer}");
        assert_eq!(
            answer["bodies"][0]["spec"]["model"]["source"],
            "huggingface"
        );
        let imports = answer["imports"]
            .as_array()
            .context("the answer lists its imports")?;
        anyhow::ensure!(imports.len() == 1, "{answer}");
        Ok(imports[0].clone())
    };

    let fetch = dry_run().await?;
    assert_eq!(fetch["action"], "fetch", "{fetch}");
    assert_eq!(fetch["bytes"], b"GGUF weights".len(), "{fetch}");
    assert_eq!(hub.downloads(), 0);
    assert_eq!(record_count(&state).await?, 0);

    let (status, answer) = file(
        &state,
        &signer,
        Route::FileIntoCampaign,
        &[gated_macos_variant(GatedModel::Gguf, GOOD_TOKEN)],
    )
    .await?;
    assert_eq!(status, StatusCode::CREATED, "{answer}");
    let downloads = hub.downloads();

    let reuse = dry_run().await?;
    assert_eq!(reuse["action"], "reuse", "{reuse}");
    assert_eq!(reuse["hf_ref"], fetch["hf_ref"]);
    let records = drain_vault_resources(state.vault_store.as_ref())
        .await?
        .items;
    let resource_ids: Vec<&str> = records.iter().map(|r| r.resource_id.as_str()).collect();
    assert_eq!(reuse["resource_ids"], json!(resource_ids), "{reuse}");
    assert_eq!(hub.downloads(), downloads);
    Ok(())
}

/// A dry run into an organization's campaign resolves the import for the
/// organization. The filer holds the records of an earlier import of the
/// model, and the organization holds none, so the dry run reports a fetch.
#[tokio::test]
async fn test_a_dry_run_resolves_for_the_campaign_owner() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = gated_hub().await?;
    let (state, _signer) = import_state(dir.path(), &hub).await?;
    let operator = registered_user(
        &state,
        &[
            UserPermission::SubmitPlans,
            UserPermission::SubmitAnyPlans,
            UserPermission::ManageAnyOrgs,
        ],
    )
    .await?;
    let (status, answer) = file(
        &state,
        &operator,
        Route::FileIntoCampaign,
        &[gated_macos_variant(GatedModel::Gguf, GOOD_TOKEN)],
    )
    .await?;
    assert_eq!(status, StatusCode::CREATED, "{answer}");
    let org = seed_org(&state, "acme").await?;
    let now = chrono::Utc::now();
    let campaign = Campaign {
        campaign_id: CampaignId::mint(),
        name: NonEmptyTrimmedString::try_new("Gated")?,
        owner: CampaignOwner::Org { org_id: org.org_id },
        current_plan_id: None,
        labels: Default::default(),
        state: Default::default(),
        created_at: now,
        updated_at: now,
    };
    state.campaign_store.put_campaign(&campaign).await?;

    let uri = format!(
        "/api/v1/campaigns/{}/plans?dry_run=true",
        campaign.campaign_id
    );
    let document = gated_document(&[gated_macos_variant(GatedModel::Gguf, GOOD_TOKEN)]);
    let response = post_plan_document(&state, &operator, &uri, &document).await?;
    assert_eq!(response.status(), StatusCode::OK);
    let answer = body_json(response).await?;
    assert_eq!(answer["imports"][0]["action"], "fetch", "{answer}");
    Ok(())
}

/// A document revision keeps the head's gated model and adds a variant. A dry
/// run of it counts the gated job as carried over, as filing the revision
/// does.
#[tokio::test]
async fn test_a_revision_dry_run_carries_the_gated_job() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = gated_hub().await?;
    let (state, signer) = import_state(dir.path(), &hub).await?;
    let (campaign_id, head) = gated_campaign(&state, &signer).await?;
    let revise = format!("/api/v1/campaigns/{campaign_id}/document?base_plan_id={head}");
    let document = gated_document(&[gated_macos_variant(GatedModel::Gguf, GOOD_TOKEN), linux()]);

    let response = put_plan_document(
        &state,
        &signer,
        &format!("{revise}&dry_run=true"),
        &document,
    )
    .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let preview = body_json(response).await?;
    assert_eq!(preview["carried_job_count"], 1, "{preview}");
    assert_eq!(preview["new_job_count"], 1, "{preview}");
    assert_eq!(preview["dropped_job_ids"], json!([]), "{preview}");
    assert_eq!(preview["imports"][0]["action"], "reuse", "{preview}");

    let response = put_plan_document(&state, &signer, &revise, &document).await?;
    assert_eq!(response.status(), StatusCode::CREATED);
    let revised = body_json(response).await?;
    assert_eq!(
        revised["carried_job_count"], preview["carried_job_count"],
        "{revised}"
    );
    assert_eq!(revised["dropped_job_count"], 0, "{revised}");
    let filed = filed_plan(&revised)?;
    let held = state
        .plan_store
        .get_plan(&filed)
        .await?
        .context("the revision exists")?
        .job_ids;
    let carried = state
        .plan_store
        .get_plan(&head)
        .await?
        .context("the head exists")?
        .job_ids;
    assert!(
        carried.iter().all(|job_id| held.contains(job_id)),
        "{held:?} does not carry {carried:?}"
    );
    Ok(())
}

/// A re-run and a variant edit of a gated campaign expand the stored document
/// again. Each files the vault arm, carries over the unchanged jobs, and makes
/// no call to the Hub.
#[tokio::test]
async fn test_a_gated_campaign_revises_without_the_hub() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = gated_hub().await?;
    let (state, signer) = import_state(dir.path(), &hub).await?;
    let (campaign_id, first) = gated_campaign(&state, &signer).await?;
    let manifest = state
        .plan_store
        .get_plan(&first)
        .await?
        .context("the first plan exists")?;
    let gated = manifest.job_ids.first().context("one job")?.clone();
    pipette_store_testkit::finish_job(
        state.submission_store.as_ref(),
        state.todo_store.as_ref(),
        &gated,
    )
    .await?;
    let calls = (hub.revision_requests(), hub.downloads());

    let response = signed_json(
        &state,
        &signer,
        "POST",
        &format!("/api/v1/campaigns/{campaign_id}/rerun"),
        &json!({"job_ids": [gated.as_str()]}),
    )
    .await?;
    assert_eq!(response.status(), StatusCode::CREATED);
    let rerun = filed_plan(&body_json(response).await?)?;
    let rerun_job = state
        .plan_store
        .get_plan(&rerun)
        .await?
        .context("the re-run plan exists")?
        .job_ids
        .first()
        .context("the re-run holds one job")?
        .clone();
    let model = state
        .plan_store
        .get_job_source(&rerun_job)
        .await?
        .context("the re-run job source was written")?["spec"]["model"]
        .clone();
    assert_eq!(model["source"], "vault_url", "{model}");

    let response = post_plan_document(
        &state,
        &signer,
        &format!("/api/v1/campaigns/{campaign_id}/variants?base_plan_id={rerun}"),
        MLX_700M_VARIANT,
    )
    .await?;
    assert_eq!(response.status(), StatusCode::CREATED);
    let edited = filed_plan(&body_json(response).await?)?;
    let held = state
        .plan_store
        .get_plan(&edited)
        .await?
        .context("the edited plan exists")?
        .job_ids;
    assert!(
        held.contains(&rerun_job),
        "{held:?} does not carry {rerun_job}"
    );

    assert_eq!((hub.revision_requests(), hub.downloads()), calls);
    assert_no_token_stored(dir.path())
}

/// A second plan of the same owner names the gated model with the same token
/// on other devices and another runtime. It resolves the model on the Hub,
/// downloads nothing and writes no record.
#[tokio::test]
async fn test_a_similar_plan_reuses_the_import() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = gated_hub().await?;
    let (state, signer) = import_state(dir.path(), &hub).await?;
    let (status, answer) = file(
        &state,
        &signer,
        Route::FileIntoCampaign,
        &[gated_macos_variant(GatedModel::Gguf, GOOD_TOKEN)],
    )
    .await?;
    assert_eq!(status, StatusCode::CREATED, "{answer}");
    let (revisions, downloads, records) = (
        hub.revision_requests(),
        hub.downloads(),
        record_count(&state).await?,
    );

    let (status, answer) = file(&state, &signer, Route::CreateCampaign, &[linux()]).await?;
    assert_eq!(status, StatusCode::CREATED, "{answer}");

    assert!(hub.revision_requests() > revisions);
    assert_eq!(hub.downloads(), downloads);
    assert_eq!(record_count(&state).await?, records);
    Ok(())
}

/// A document that names the gated model in two variants imports it once.
#[tokio::test]
async fn test_one_model_in_two_variants_imports_once() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = gated_hub().await?;
    let (state, signer) = import_state(dir.path(), &hub).await?;
    let (status, answer) = file(
        &state,
        &signer,
        Route::FileIntoCampaign,
        &[gated_macos_variant(GatedModel::Gguf, GOOD_TOKEN), linux()],
    )
    .await?;
    assert_eq!(status, StatusCode::CREATED, "{answer}");
    assert_eq!(hub.downloads_of(GATED_GGUF_FILE), 1);
    assert_eq!(record_count(&state).await?, 1);
    let document = stored_document(&state, &filed_plan(&answer)?).await?;
    assert_eq!(document.matches("vault_url").count(), 2, "{document}");
    Ok(())
}

/// Matched on the digest of the text as sent: a retry off the replaced plan and
/// a second send off the new head each answer `changed: false` (a dry run
/// `unchanged`), file nothing, and make no call to the Hub.
#[tokio::test]
async fn test_a_gated_document_sent_again_changes_nothing() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = gated_hub().await?;
    let (state, signer) = import_state(dir.path(), &hub).await?;
    let (campaign_id, first) = created_campaign(&state, &signer, &[("name", "Gated")]).await?;
    let document = gated_document(&[gated_macos_variant(GatedModel::Gguf, GOOD_TOKEN)]);
    let revise =
        |base: &str| format!("/api/v1/campaigns/{campaign_id}/document?base_plan_id={base}");

    let response = put_plan_document(&state, &signer, &revise(first.as_str()), &document).await?;
    assert_eq!(response.status(), StatusCode::CREATED);
    let head = filed_plan(&body_json(response).await?)?;
    let calls = (hub.revision_requests(), hub.downloads());

    for base in [first.as_str(), head.as_str()] {
        let response = put_plan_document(&state, &signer, &revise(base), &document).await?;
        assert_eq!(response.status(), StatusCode::OK, "base {base}");
        let answer = body_json(response).await?;
        assert_eq!(answer["changed"], false, "{answer}");
    }
    let dry_run = put_plan_document(
        &state,
        &signer,
        &format!("{}&dry_run=true", revise(head.as_str())),
        &document,
    )
    .await?;
    assert_eq!(dry_run.status(), StatusCode::OK);
    let answer = body_json(dry_run).await?;
    assert_eq!(answer["unchanged"], true, "{answer}");
    assert_eq!(answer["imports"], json!([]), "{answer}");
    assert!(!answer.to_string().contains("SECRET"), "{answer}");
    let campaign = state
        .campaign_store
        .get_campaign(&campaign_id.parse()?)
        .await?
        .context("the campaign exists")?;
    assert_eq!(campaign.current_plan_id.as_ref(), Some(&head));
    assert_eq!((hub.revision_requests(), hub.downloads()), calls);
    assert_no_token_stored(dir.path())
}

/// A document revision whose campaign changes its owner during the import is
/// refused with `409`. The campaign stays on its plan, so the records of the
/// old owner never back the new owner's jobs. It goes to the filer's own
/// organization, which the filer still files into; an owner the filer cannot
/// file for is a `403`.
#[tokio::test]
async fn test_a_revision_refuses_an_owner_change_during_the_import() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = gated_hub().await?;
    let (state, signer) = import_state(dir.path(), &hub).await?;
    let acme = seed_org(&state, "acme").await?.org_id;
    set_user_org(&state, &signer.1, Some(acme.clone())).await?;
    let (campaign_id, first) = created_campaign(&state, &signer, &[("name", "Gated")]).await?;
    let campaign_id: pipette_plan_types::CampaignId = campaign_id.parse()?;
    let campaign = state
        .campaign_store
        .get_campaign(&campaign_id)
        .await?
        .context("the campaign exists")?;
    let handed = pipette_core::campaign::Campaign {
        owner: pipette_core::campaign::CampaignOwner::Org { org_id: acme },
        ..campaign
    };
    let uri = format!("/api/v1/campaigns/{campaign_id}/document?base_plan_id={first}");
    let document = gated_document(&[gated_macos_variant(GatedModel::Gguf, GOOD_TOKEN)]);

    let held = hub.hold().await;
    let hand_off = async {
        hub.arrived().await;
        state.campaign_store.put_campaign(&handed).await?;
        drop(held);
        anyhow::Ok(())
    };
    let (response, ()) = tokio::try_join!(
        put_plan_document(&state, &signer, &uri, &document),
        hand_off
    )?;
    assert_eq!(response.status(), StatusCode::CONFLICT);

    let stored = state
        .campaign_store
        .get_campaign(&campaign_id)
        .await?
        .context("the campaign exists")?;
    assert_eq!(stored.current_plan_id.as_ref(), Some(&first));
    Ok(())
}

/// A campaign route a member files a gated document through.
#[derive(Debug, Clone, Copy)]
enum TeamFiling {
    /// `POST /api/v1/campaigns/{campaign_id}/plans`.
    Plans,
    /// `PUT /api/v1/campaigns/{campaign_id}/document`.
    Document,
}

/// A member who leaves the organization during the import files nothing into
/// its campaign: the filer's authority is checked again after the import, as
/// the owner and the head are.
#[rstest]
#[case::plans(TeamFiling::Plans)]
#[case::document(TeamFiling::Document)]
#[tokio::test]
async fn test_a_member_who_leaves_during_the_import_files_nothing(
    #[case] route: TeamFiling,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = gated_hub().await?;
    let (state, author) = import_state(dir.path(), &hub).await?;
    let acme = seed_org(&state, "acme").await?.org_id;
    let member = registered_user(&state, FULL_EDITOR).await?;
    for signer in [&author, &member] {
        set_user_org(&state, &signer.1, Some(acme.clone())).await?;
    }
    let params = [("name", "Team"), ("owner", acme.as_str())];
    let (campaign_id, first) = created_campaign(&state, &author, &params).await?;
    let document = gated_document(&[gated_macos_variant(GatedModel::Gguf, GOOD_TOKEN)]);
    let filing = async {
        match route {
            TeamFiling::Plans => {
                let uri = format!("/api/v1/campaigns/{campaign_id}/plans");
                post_plan_document(&state, &member, &uri, &document).await
            }
            TeamFiling::Document => {
                let uri = format!("/api/v1/campaigns/{campaign_id}/document?base_plan_id={first}");
                put_plan_document(&state, &member, &uri, &document).await
            }
        }
    };

    let held = hub.hold().await;
    let leave = async {
        hub.arrived().await;
        set_user_org(&state, &member.1, None).await?;
        drop(held);
        anyhow::Ok(())
    };
    let (response, ()) = tokio::try_join!(filing, leave)?;
    assert_eq!(response.status(), StatusCode::FORBIDDEN, "{route:?}");

    let campaign_id: CampaignId = campaign_id.parse()?;
    let stored = state
        .campaign_store
        .get_campaign(&campaign_id)
        .await?
        .context("the campaign exists")?;
    assert_eq!(stored.current_plan_id.as_ref(), Some(&first));
    Ok(())
}

/// A `manage_any_orgs` holder files into another account's campaign, but its
/// vault reach is its own: without `submit_any_plans` the records an import
/// writes for the campaign's owner are outside it, so the filing is refused.
#[tokio::test]
async fn test_a_delegate_imports_for_another_owner_only_with_submit_any_plans() -> anyhow::Result<()>
{
    let dir = tempfile::tempdir()?;
    let hub = gated_hub().await?;
    let (state, author) = import_state(dir.path(), &hub).await?;
    let (campaign_id, first) = created_campaign(&state, &author, &[("name", "Gated")]).await?;
    let uri = format!("/api/v1/campaigns/{campaign_id}/plans");
    let document = gated_document(&[gated_macos_variant(GatedModel::Gguf, GOOD_TOKEN)]);
    let delegate = registered_user(
        &state,
        &[FULL_EDITOR, &[UserPermission::ManageAnyOrgs]].concat(),
    )
    .await?;
    let response = post_plan_document(&state, &delegate, &uri, &document).await?;
    let status = response.status();
    let body = body_json(response).await?;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    let wider = registered_user(
        &state,
        &[
            FULL_EDITOR,
            &[
                UserPermission::ManageAnyOrgs,
                UserPermission::SubmitAnyPlans,
            ],
        ]
        .concat(),
    )
    .await?;
    let response = post_plan_document(&state, &wider, &uri, &document).await?;
    let status = response.status();
    let body = body_json(response).await?;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_ne!(filed_plan(&body)?, first);
    Ok(())
}

/// A document with no model token is stored as it is sent. One that names
/// `auth_token` anyway, here in a comment, is refused with `400`. The answer
/// quotes no token, and nothing is stored.
#[tokio::test]
async fn test_a_token_in_a_comment_is_refused() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = gated_hub().await?;
    let (state, signer) = import_state(dir.path(), &hub).await?;
    let (campaign_id, first) = created_campaign(&state, &signer, &[("name", "Head")]).await?;
    let document = format!("# auth_token = \"{GOOD_TOKEN}\"\n{ONE_CELL_PLAN}");
    let calls = (hub.revision_requests(), hub.downloads());

    let uri = own_campaign_plans_uri(&state, &signer.1).await?;
    let filed = post_plan_document(&state, &signer, &uri, &document).await?;
    let uri = format!("/api/v1/campaigns/{campaign_id}/document?base_plan_id={first}");
    let revised = put_plan_document(&state, &signer, &uri, &document).await?;
    for response in [filed, revised] {
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let answer = body_json(response).await?.to_string();
        assert!(answer.contains("outside a model's token"), "{answer}");
        assert!(!answer.contains("SECRET"), "{answer}");
    }
    assert_eq!((hub.revision_requests(), hub.downloads()), calls);
    assert_no_token_stored(dir.path())
}

// ── the progress stream (`Accept: application/x-ndjson`) ────────

/// With the header, a gated document on each campaign route answers `200`. The
/// body has `progress` lines and then one `result` line. Each `progress` line
/// names the gguf file and its source as `org/repo_name@commit`. The `result`
/// line carries the `201` and the body of the plain answer.
#[rstest]
#[case::create_campaign(Route::CreateCampaign)]
#[case::revise_document(Route::ReviseDocument)]
#[case::add_variants(Route::AddVariants)]
#[tokio::test]
async fn test_a_streamed_campaign_import_ends_with_the_result(
    #[case] route: Route,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = gated_hub().await?;
    let (state, signer) = import_state(dir.path(), &hub).await?;

    let response = send(
        &state,
        &signer,
        route,
        &[gated_macos_variant(GatedModel::Gguf, GOOD_TOKEN)],
        STREAM,
    )
    .await?;
    let lines = stream_lines(response).await?;
    let (last, progress) = lines.split_last().context("the stream has a line")?;
    assert!(
        !progress.is_empty() && progress.iter().all(|line| line["event"] == "progress"),
        "{lines:?}"
    );
    let hf_ref = hub
        .reference("LiquidAI/LFM2-700M-GGUF")
        .context("the hub holds the gguf repository")?;
    assert!(
        progress
            .iter()
            .all(|line| line["hf_ref"] == hf_ref.as_str() && line["file"] == GATED_GGUF_FILE),
        "{lines:?}"
    );
    assert_eq!(last["event"], "result", "{last}");
    assert_eq!(last["status"], 201, "{last}");
    assert!(!format!("{lines:?}").contains("SECRET"), "{lines:?}");
    let document = stored_document(&state, &filed_plan(&last["body"])?).await?;
    assert!(document.contains("source = \"vault_url\""), "{document}");
    assert_no_token_stored(dir.path())
}

/// A streamed failure on each campaign route ends with one `error` line. The
/// line holds the status and the message of the plain answer to the same
/// request.
#[rstest]
#[case::create_campaign(Route::CreateCampaign)]
#[case::revise_document(Route::ReviseDocument)]
#[case::add_variants(Route::AddVariants)]
#[tokio::test]
async fn test_a_streamed_campaign_failure_ends_with_the_plain_error(
    #[case] route: Route,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = gated_hub().await?;
    let (state, signer) = import_state(dir.path(), &hub).await?;
    let variants = [gated_variant(
        GatedModel::Gguf,
        BAD_TOKEN,
        r#"["os:macos"]"#,
        "macos-arm64",
    )];

    let (status, plain) = file(&state, &signer, route, &variants).await?;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{plain}");
    let lines = stream_lines(send(&state, &signer, route, &variants, STREAM).await?).await?;
    let (last, before) = lines.split_last().context("the stream has a line")?;
    assert_eq!(
        last,
        &json!({"event": "error", "status": 400, "error": plain["error"]}),
        "{lines:?}"
    );
    assert!(
        before.iter().all(|line| line["event"] == "progress"),
        "{lines:?}"
    );
    Ok(())
}

/// With the header, a dry run on each campaign route answers one `result`
/// line. The line carries the status `200` and the dry-run report. The dry
/// run downloads nothing.
#[rstest]
#[case::create_campaign(Route::CreateCampaign)]
#[case::revise_document(Route::ReviseDocument)]
#[case::add_variants(Route::AddVariants)]
#[tokio::test]
async fn test_a_streamed_campaign_dry_run_ends_with_its_report(
    #[case] route: Route,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = gated_hub().await?;
    let (state, signer) = import_state(dir.path(), &hub).await?;
    let ask = Ask {
        dry_run: true,
        stream: true,
    };

    let response = send(
        &state,
        &signer,
        route,
        &[gated_macos_variant(GatedModel::Gguf, GOOD_TOKEN)],
        ask,
    )
    .await?;
    let lines = stream_lines(response).await?;
    let [line] = lines.as_slice() else {
        anyhow::bail!("the stream has one line: {lines:?}");
    };
    assert_eq!(line["event"], "result", "{line}");
    assert_eq!(line["status"], 200, "{line}");
    assert!(line["body"]["bodies"].is_array(), "{line}");
    assert!(!line.to_string().contains("SECRET"), "{line}");
    assert_eq!(hub.downloads(), 0);
    Ok(())
}

/// With the header, the head's own document files nothing. The stream is one
/// `result` line with the status `200` and the campaign unchanged.
#[tokio::test]
async fn test_a_streamed_unchanged_document_ends_with_the_campaign() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = gated_hub().await?;
    let (state, signer) = import_state(dir.path(), &hub).await?;
    let (campaign_id, first) = created_campaign(&state, &signer, &[("name", "Head")]).await?;
    let uri = format!("/api/v1/campaigns/{campaign_id}/document?base_plan_id={first}");

    let response = stream_plan_document(&state, &signer, "PUT", &uri, ONE_CELL_PLAN).await?;
    let lines = stream_lines(response).await?;
    let [line] = lines.as_slice() else {
        anyhow::bail!("the stream has one line: {lines:?}");
    };
    assert_eq!(line["event"], "result", "{line}");
    assert_eq!(line["status"], 200, "{line}");
    assert_eq!(line["body"]["changed"], false, "{line}");
    assert_eq!(line["body"]["campaign_id"], campaign_id, "{line}");
    Ok(())
}

/// With the header, a document revision on a campaign that moved off
/// `base_plan_id` ends with one `error` line. The line holds the `409` and the
/// message of the plain answer, so the console can tell the move apart.
#[tokio::test]
async fn test_a_streamed_revision_of_a_moved_campaign_ends_with_the_conflict() -> anyhow::Result<()>
{
    let dir = tempfile::tempdir()?;
    let hub = gated_hub().await?;
    let (state, signer) = import_state(dir.path(), &hub).await?;
    let (campaign_id, first) = created_campaign(&state, &signer, &[("name", "Moved")]).await?;
    let uri = format!("/api/v1/campaigns/{campaign_id}/document?base_plan_id={first}");
    let moved = put_plan_document(
        &state,
        &signer,
        &uri,
        &gated_document(&[gated_macos_variant(GatedModel::Gguf, GOOD_TOKEN)]),
    )
    .await?;
    assert_eq!(moved.status(), StatusCode::CREATED);
    let stale = gated_document(&[linux()]);

    let plain = put_plan_document(&state, &signer, &uri, &stale).await?;
    assert_eq!(plain.status(), StatusCode::CONFLICT);
    let plain = body_json(plain).await?;
    let response = stream_plan_document(&state, &signer, "PUT", &uri, &stale).await?;
    assert_eq!(response.status(), StatusCode::OK);
    let lines = stream_lines(response).await?;
    assert_eq!(
        lines,
        [json!({"event": "error", "status": 409, "error": plain["error"]})],
        "{lines:?}"
    );
    Ok(())
}
