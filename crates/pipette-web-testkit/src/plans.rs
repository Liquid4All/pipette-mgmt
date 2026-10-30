//! Filing a plan the way a caller does, so a test reads back what the route
//! wrote rather than what a fixture invented.

use anyhow::Context;
use axum::http::StatusCode;
use chrono::Utc;
use ed25519_dalek::SigningKey;
use http_body_util::BodyExt;
use serde_json::{Value, json};

use pipette_core::campaign::{Campaign, CampaignOwner};
use pipette_core::types::Mint;
use pipette_httpapi::state::AppState;
use pipette_pipeline::campaign_admin;
use pipette_pipeline::plan_document::index_labels;
use pipette_pipeline::plan_ingestion::{
    CampaignMembership, IngestContext, ModelCheck, PlanSubmission, ingest_jobs,
};
use pipette_pipeline::vault_check::VaultScope;
use pipette_plan_types::{CampaignId, ClientId, NonEmptyTrimmedString, PlanId, UserId, UserKeyId};

use crate::queue::job_spec_with_model_path;
use crate::{UserSigner, body_json, json_str, raw_request, user_auth_headers};

/// A submittable job body pinned to `client_id`, for [`file_job_bodies`].
///
/// `cell` must differ per job: ingestion dedups on a digest of the whole
/// `spec`, so identical specs merge into a single job.
pub fn submittable_job(client_id: &ClientId, cell: &str) -> Value {
    let spec = job_spec_with_model_path(&format!("LFM2-700M-Q4_0-{cell}.gguf"));
    json!({
        "clients": [client_id.as_str()],
        "spec": spec,
    })
}

/// A plan document of `job_count` jobs pinned to `client_id`, with no
/// `requires`, so the profile-less [`crate::approved_client`] matches it.
///
/// Pass a fresh `token` (path-safe characters only) on each call: ingestion
/// merges a job into an identical one that another plan staged.
pub fn submittable_document(client_id: &ClientId, token: &str, job_count: usize) -> String {
    let models: Vec<String> = (0..job_count)
        .map(|index| {
            format!(
                "  {{ type = \"gguf_text\", source = \"huggingface\", org = \"LiquidAI\", \
                 repo_name = \"LFM2-700M-GGUF\", path = \"LFM2-700M-Q4_0-{token}-{index}.gguf\" }},"
            )
        })
        .collect();
    format!(
        r#"benchmarks = ["eval_test"]

[[variants]]
clients  = ["{client}"]
runtimes = [{{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", repository_version = "b5000", flavor = "macos-arm64" }}]
models   = [
{models}
]
"#,
        client = client_id.as_str(),
        models = models.join("\n"),
    )
}

pub fn filing_uri(campaign_id: &str, note: &str) -> anyhow::Result<String> {
    path_and_query(
        &format!("/api/v1/campaigns/{campaign_id}/plans"),
        &[("note", note)],
    )
}

/// `path` with `params` as its URL-encoded query string, in the form a signed
/// request carries.
fn path_and_query(path: &str, params: &[(&str, &str)]) -> anyhow::Result<String> {
    let url = reqwest::Url::parse_with_params(&format!("http://localhost{path}"), params)?;
    Ok(match url.query() {
        Some(query) => format!("{}?{query}", url.path()),
        None => url.path().to_string(),
    })
}

/// File a plan as `user` into a fresh campaign that `user` owns, so that every
/// scope check that depends on the campaign sees the caller as its owner. `note`
/// also names the campaign, so `note` must not be blank.
pub async fn submit_plan_as(
    state: &AppState,
    signing_key: &SigningKey,
    user_id: &UserId,
    key_id: &UserKeyId,
    client_id: &ClientId,
    note: &str,
    job_count: usize,
) -> anyhow::Result<PlanId> {
    let campaign_id = put_idle_campaign(state, user_id, note).await?;
    let signer = (signing_key.clone(), user_id.clone(), key_id.clone());
    let resp = post_plan_document(
        state,
        &signer,
        &filing_uri(campaign_id.as_str(), note)?,
        &submittable_document(client_id, campaign_id.as_str(), job_count),
    )
    .await?;
    anyhow::ensure!(
        resp.status() == StatusCode::CREATED,
        "plan submission failed with {}",
        resp.status()
    );
    let report = body_json(resp).await?;
    Ok(PlanId::try_new(json_str(&report, "plan_id")?)?)
}

/// [`submit_plan_as`], then detach the plan from its campaign, which no route
/// does, so access to the plan follows its `submitted_by` alone.
pub async fn put_loose_plan(
    state: &AppState,
    signing_key: &SigningKey,
    user_id: &UserId,
    key_id: &UserKeyId,
    client_id: &ClientId,
    note: &str,
    job_count: usize,
) -> anyhow::Result<PlanId> {
    let plan_id = submit_plan_as(
        state,
        signing_key,
        user_id,
        key_id,
        client_id,
        note,
        job_count,
    )
    .await?;
    let mut manifest = state
        .plan_store
        .get_plan(&plan_id)
        .await?
        .context("the filed plan exists")?;
    let campaign_id = manifest
        .campaign_id
        .take()
        .context("the filed plan names its campaign")?;
    manifest.previous_plan_id = None;
    state.plan_store.put_plan(&manifest).await?;
    state.campaign_store.delete_campaign(&campaign_id).await?;
    Ok(plan_id)
}

/// The `plan_id`s a `GET /api/v1/plans` body carries, in the order it returned them.
pub fn plan_ids(body: &Value) -> anyhow::Result<Vec<String>> {
    body.get("items")
        .and_then(Value::as_array)
        .context("a listing carries an `items` array")?
        .iter()
        .map(|plan| Ok(json_str(plan, "plan_id")?.to_string()))
        .collect()
}

/// Post `document` to `uri` as an `application/toml` plan document, signed as
/// `signer`. `uri` carries the query string, and the signature covers it.
pub async fn post_plan_document(
    state: &AppState,
    signer: &UserSigner,
    uri: &str,
    document: &str,
) -> anyhow::Result<axum::http::Response<axum::body::Body>> {
    send_plan_document(state, signer, "POST", uri, document, false).await
}

/// [`post_plan_document`] as a `PUT`, the method that replaces a campaign's
/// document.
pub async fn put_plan_document(
    state: &AppState,
    signer: &UserSigner,
    uri: &str,
    document: &str,
) -> anyhow::Result<axum::http::Response<axum::body::Body>> {
    send_plan_document(state, signer, "PUT", uri, document, false).await
}

/// [`post_plan_document`] with `method`, asking for the ndjson progress
/// stream. Read the answer with [`stream_lines`].
pub async fn stream_plan_document(
    state: &AppState,
    signer: &UserSigner,
    method: &str,
    uri: &str,
    document: &str,
) -> anyhow::Result<axum::http::Response<axum::body::Body>> {
    send_plan_document(state, signer, method, uri, document, true).await
}

/// Each line of a streamed answer, parsed.
pub async fn stream_lines(
    response: axum::http::Response<axum::body::Body>,
) -> anyhow::Result<Vec<Value>> {
    anyhow::ensure!(response.status() == StatusCode::OK, "{}", response.status());
    let content_type = response.headers().get("content-type").cloned();
    anyhow::ensure!(
        content_type.as_ref().and_then(|value| value.to_str().ok()) == Some("application/x-ndjson"),
        "{content_type:?}"
    );
    let bytes = response.into_body().collect().await?.to_bytes();
    bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| Ok(serde_json::from_slice(line)?))
        .collect()
}

pub async fn send_plan_document(
    state: &AppState,
    signer: &UserSigner,
    method: &str,
    uri: &str,
    document: &str,
    stream: bool,
) -> anyhow::Result<axum::http::Response<axum::body::Body>> {
    let (key, user_id, key_id) = signer;
    let mut headers = user_auth_headers(key, user_id, key_id, method, uri)?;
    if stream {
        headers.push(("Accept", "application/x-ndjson".to_string()));
    }
    raw_request(
        state,
        method,
        uri,
        &headers,
        Some("application/toml"),
        document.to_string(),
    )
    .await
}

/// File a one-job plan into `campaign_id` and return the raw response, for
/// the refusal tests. `cell` goes into the model path, so revisions never merge.
pub async fn file_plan(
    state: &AppState,
    signer: &UserSigner,
    campaign_id: &str,
    client_id: &ClientId,
    cell: &str,
) -> anyhow::Result<axum::http::Response<axum::body::Body>> {
    post_plan_document(
        state,
        signer,
        &filing_uri(campaign_id, cell)?,
        &submittable_document(client_id, cell, 1),
    )
    .await
}

/// [`file_plan`], then check that the plan was filed, and return its `plan_id`.
pub async fn filed_plan(
    state: &AppState,
    signer: &UserSigner,
    campaign_id: &str,
    client_id: &ClientId,
    cell: &str,
) -> anyhow::Result<PlanId> {
    let resp = file_plan(state, signer, campaign_id, client_id, cell).await?;
    anyhow::ensure!(
        resp.status() == StatusCode::CREATED,
        "filing a plan failed with {}",
        resp.status()
    );
    Ok(PlanId::try_new(json_str(
        &body_json(resp).await?,
        "plan_id",
    )?)?)
}

/// File one job into `campaign_id` as a job body with no stored document. No
/// route does that, so this calls ingestion and advances the campaign itself.
///
/// It runs none of the route's gates: no campaign authority check, a vault
/// scope over every resource, no `uncatalogued_model` warning, no model import.
pub async fn file_job_bodies(
    state: &AppState,
    signer: &UserSigner,
    campaign_id: &str,
    client_id: &ClientId,
    cell: &str,
) -> anyhow::Result<PlanId> {
    let campaign_id = CampaignId::try_new(campaign_id)?;
    let campaign = state
        .campaign_store
        .get_campaign(&campaign_id)
        .await?
        .context("the campaign to file into exists")?;
    let catalog = state.catalog_cache.get().await?;
    let ctx = IngestContext {
        plans: state.plan_store.as_ref(),
        todo: state.todo_store.as_ref(),
        clients: state.client_store.as_ref(),
        tags: state.tag_store.as_ref(),
        submissions: state.submission_store.as_ref(),
        orgs: state.org_store.as_ref(),
        vault: state.vault_store.as_ref(),
        vault_scope: &VaultScope::Every,
        import: None,
        catalog: &catalog,
        models: ModelCheck::none(),
        max_jobs: state.config.plan_max_jobs,
        creating_max_age: std::time::Duration::from_secs(
            state.config.plan_creating_max_age_secs.get(),
        ),
    };
    let report = ingest_jobs(
        &ctx,
        PlanSubmission {
            note: Some(NonEmptyTrimmedString::try_new(cell)?),
            change: None,
            submitted_by: Some(signer.1.clone()),
            membership: CampaignMembership::of(&campaign),
            jobs: index_labels(vec![submittable_job(client_id, cell)]),
            source: None,
            plan_warnings: Vec::new(),
        },
    )
    .await?;
    campaign_admin::advance(
        state.campaign_store.as_ref(),
        state.plan_store.as_ref(),
        &campaign_id,
        report.plan_id.clone(),
    )
    .await?;
    Ok(report.plan_id)
}

/// A plan document of one cell, naming a benchmark that [`crate::setup_benchmarks`]
/// writes, so a test on [`crate::seeded_state`] or [`crate::approved_client`]
/// can create a campaign without a catalog of its own.
pub const ONE_CELL_PLAN: &str = r#"
benchmarks = ["prefill_throughput_256"]

[[variants]]
requires = ["os:macos"]
models   = [{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "LFM2.5-350M-MLX-4bit" }]
runtimes = [{ type = "mlx_macos_pipette", version = "0.31.3", flavor = "macos-arm64", source = { type = "pip_requirements_text", contents = "mlx-lm==0.31.3\n" } }]
"#;

/// The client id that [`ONE_CLIENT_PLAN`] names. No test registers it.
pub const NAMED_CLIENT_ID: &str = "ev1_named";

/// [`ONE_CELL_PLAN`] with its variant bound to [`NAMED_CLIENT_ID`], so a test
/// can check that a surface withholds the client ids a document names.
pub const ONE_CLIENT_PLAN: &str = r#"
benchmarks = ["prefill_throughput_256"]

[[variants]]
requires = ["os:macos"]
clients  = ["ev1_named"]
models   = [{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "LFM2.5-350M-MLX-4bit" }]
runtimes = [{ type = "mlx_macos_pipette", version = "0.31.3", flavor = "macos-arm64", source = { type = "pip_requirements_text", contents = "mlx-lm==0.31.3\n" } }]
"#;

/// A variant edit of [`ONE_CELL_PLAN`] that appends one more cell, on a model
/// the plan does not run.
pub const MLX_700M_VARIANT: &str = r#"
[[variants]]
requires = ["os:macos"]
models   = [{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "LFM2-700M-MLX-4bit" }]
runtimes = [{ type = "mlx_macos_pipette", version = "0.31.3", flavor = "macos-arm64", source = { type = "pip_requirements_text", contents = "mlx-lm==0.31.3\n" } }]
"#;

/// Post [`ONE_CELL_PLAN`] to `POST /api/v1/campaigns` as `signer`, with `params` as the
/// query string, and return the raw response.
pub async fn post_campaign(
    state: &AppState,
    signer: &UserSigner,
    params: &[(&str, &str)],
) -> anyhow::Result<axum::http::Response<axum::body::Body>> {
    post_campaign_document(state, signer, params, ONE_CELL_PLAN).await
}

/// [`post_campaign`] with `document` in place of [`ONE_CELL_PLAN`].
pub async fn post_campaign_document(
    state: &AppState,
    signer: &UserSigner,
    params: &[(&str, &str)],
    document: &str,
) -> anyhow::Result<axum::http::Response<axum::body::Body>> {
    let uri = path_and_query("/api/v1/campaigns", params)?;
    post_plan_document(state, signer, &uri, document).await
}

/// [`post_campaign`], asserting it created the campaign, and returning the
/// campaign's id and its first plan's.
pub async fn created_campaign(
    state: &AppState,
    signer: &UserSigner,
    params: &[(&str, &str)],
) -> anyhow::Result<(String, PlanId)> {
    created_campaign_from(state, signer, params, ONE_CELL_PLAN).await
}

/// [`created_campaign`] with `document` in place of [`ONE_CELL_PLAN`].
pub async fn created_campaign_from(
    state: &AppState,
    signer: &UserSigner,
    params: &[(&str, &str)],
    document: &str,
) -> anyhow::Result<(String, PlanId)> {
    let resp = post_campaign_document(state, signer, params, document).await?;
    anyhow::ensure!(
        resp.status() == StatusCode::CREATED,
        "campaign creation failed with {}",
        resp.status()
    );
    let body = body_json(resp).await?;
    let plan = body.get("plan").context("the create reports its plan")?;
    Ok((
        json_str(&body, "campaign_id")?.to_string(),
        PlanId::try_new(json_str(plan, "plan_id")?)?,
    ))
}

/// Write a campaign `owner` holds that runs nothing, as legacy records do: no
/// route creates one.
pub async fn put_idle_campaign(
    state: &AppState,
    owner: &UserId,
    name: &str,
) -> anyhow::Result<CampaignId> {
    let owner = CampaignOwner::User {
        user_id: owner.clone(),
    };
    Ok(put_idle_campaign_of(state, owner, name).await?.campaign_id)
}

/// [`put_idle_campaign`] for any `owner`, returning the record.
pub async fn put_idle_campaign_of(
    state: &AppState,
    owner: CampaignOwner,
    name: &str,
) -> anyhow::Result<Campaign> {
    let now = Utc::now();
    let campaign = Campaign {
        campaign_id: CampaignId::mint(),
        name: NonEmptyTrimmedString::try_new(name)?,
        owner,
        current_plan_id: None,
        labels: Default::default(),
        state: Default::default(),
        created_at: now,
        updated_at: now,
    };
    state.campaign_store.put_campaign(&campaign).await?;
    Ok(campaign)
}

/// The plans route of a new, idle campaign that `owner` holds:
/// `POST /api/v1/campaigns/{campaign_id}/plans`.
pub async fn own_campaign_plans_uri(state: &AppState, owner: &UserId) -> anyhow::Result<String> {
    let campaign_id = put_idle_campaign(state, owner, "Idle").await?;
    Ok(format!("/api/v1/campaigns/{campaign_id}/plans"))
}

/// A cached group with all six clauses populated proves that the full wire
/// shape survives and that the withheld tier drops the entire clause set.
pub async fn add_starved_snapshot(state: &AppState, plan_id: &PlanId) -> anyhow::Result<Value> {
    use pipette_core::plan::{ProgressSnapshot, StarvedGroup};
    use pipette_plan_types::Counts;
    use pipette_plan_types::StarvedClauses;
    use pipette_plan_types::Warning;
    use pipette_plan_types::WarningKind;
    let mut manifest = state
        .plan_store
        .get_plan(plan_id)
        .await?
        .context("plan exists")?;
    let group = StarvedGroup {
        clauses: StarvedClauses {
            requires: vec!["os:secret".into()],
            any_of: vec![vec!["arch:secret".into()]],
            clients: vec!["ev1_secret".into()],
            exclude_clients: vec!["ev1_excluded".into()],
            excludes: vec!["gpu:secret".into()],
            not_all_of: vec![vec!["os:excluded".into()]],
        },
        job_ids: manifest.job_ids.clone(),
    };
    let full_group = serde_json::to_value(&group)?;
    manifest.warnings = vec![Warning {
        kind: WarningKind::Fleet,
        // Deliberately inconsistent frozen text: withheld text must be rebuilt.
        message: "999 jobs requiring os:secret match no working client".into(),
        job_ids: group.job_ids.clone(),
    }];
    manifest.progress_snapshot = Some(ProgressSnapshot {
        computed_at: Utc::now(),
        counts: Counts {
            total: group.job_ids.len() as u64,
            available: group.job_ids.len() as u64,
            running: 0,
            finished: 0,
            failed: 0,
            cancelled: 0,
        },
        starved: vec![group],
    });
    state.plan_store.put_plan(&manifest).await?;
    Ok(full_group)
}
