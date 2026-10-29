//! `POST /api/v1/campaigns/{campaign_id}/plans`: staging a plan into the queue,
//! and the `405` that `POST /api/v1/plans` answers in place of filing. Filing
//! is a user's right, so these tests sign as a user; draining the queue is a
//! device's, in `queue_protocol.rs`.

use std::collections::BTreeSet;
use std::io::Read;

use anyhow::Context;
use axum::http::StatusCode;
use futures::{StreamExt, TryStreamExt};
use http_body_util::BodyExt;
use rstest::rstest;
use serde_json::{Value, json};

use pipette_core::campaign::{Campaign, CampaignOwner};
use pipette_core::vault::VaultOwner;
use pipette_httpapi::state::AppState;
use pipette_hub_testkit::{BAD_TOKEN, GOOD_TOKEN, MockFile, MockHub, OTHER_GOOD_TOKEN};
use pipette_pipeline::vault_admin;
use pipette_plan_types::{
    ClientId, DeviceProfile, NonEmptyTrimmedString, PlanId, PlanStatus, Sha256, UserPermission,
};
use pipette_store::drain_vault_resources;
use pipette_web_testkit::{
    GATED_GGUF_FILE, GatedModel, ONE_CELL_PLAN, UserSigner, VISION_MMPROJ_FILE, VISION_MODEL_BYTES,
    VISION_MODEL_FILE, approved_client, assert_nothing_staged, authed_post, authed_user_get,
    body_json, file_job_bodies, filing_uri, gated_hub, gated_macos_document, job, json_str,
    make_state_with_hf_endpoint, make_state_with_plan_max_jobs, own_campaign_plans_uri,
    post_plan_document, put_idle_campaign, put_idle_campaign_of, raw_request, register_and_approve,
    registered_user, registered_user_with_detail, run_qm, seed_org, seeded_state,
    set_matching_input, set_user_org, setup_benchmarks, setup_scheduler_benchmarks, signed_json,
    stream_lines, stream_plan_document, user_auth_headers,
};

use crate::campaign_listings::as_a_row;

// ── filing outside a campaign (POST /api/v1/plans) ──────────────

/// An account that may file a plan, and sees the full fleet detail a report
/// can carry.
async fn filer(state: &AppState) -> anyhow::Result<UserSigner> {
    registered_user_with_detail(state, true).await
}

/// What `POST /api/v1/plans` answers in place of filing.
const UPGRADE: &str = "This server files a plan only into a campaign. File the plan document with \
                       `POST /api/v1/campaigns` or `POST /api/v1/campaigns/{campaign_id}/plans`.";

/// Who sends the `POST /api/v1/plans` of
/// [`test_a_post_to_the_plan_listing_answers_the_upgrade_405`].
#[derive(Clone, Copy, Debug)]
enum Poster {
    /// No signature at all.
    Unsigned,
    /// A user, with the JSON job bodies of a pipette-plan before the campaign
    /// verbs.
    UserJson,
    /// A user, with a plan document.
    UserDocument,
    /// An approved device.
    Device,
}

/// `POST /api/v1/plans` files nothing, for every caller and every body. It
/// answers `405` with an `Allow` header that lists `GET`. The message names the
/// campaign verbs, so a pipette-plan that predates them prints what to run.
#[rstest]
#[case::unsigned(Poster::Unsigned)]
#[case::user_json(Poster::UserJson)]
#[case::user_document(Poster::UserDocument)]
#[case::device(Poster::Device)]
#[tokio::test]
async fn test_a_post_to_the_plan_listing_answers_the_upgrade_405(
    #[case] poster: Poster,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, key, client_id) = approved_client(dir.path()).await?;
    let jobs = json!({"jobs": [{
        "clients": [client_id.as_str()],
        "spec": pipette_web_testkit::job_spec(),
    }]});
    let uri = "/api/v1/plans";

    let resp = match poster {
        Poster::Unsigned => pipette_web_testkit::unauthed_post(&state, uri, &jobs).await?,
        Poster::UserJson => signed_json(&state, &filer(&state).await?, "POST", uri, &jobs).await?,
        Poster::UserDocument => {
            post_plan_document(&state, &filer(&state).await?, uri, AFM_MLX).await?
        }
        Poster::Device => authed_post(&state, &key, &client_id, uri, &jobs).await?,
    };

    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
    let allow = allowed_methods(&resp)?;
    assert!(allow.contains(&"GET".to_string()), "{allow:?}");
    assert!(!allow.contains(&"POST".to_string()), "{allow:?}");
    assert_eq!(json_str(&body_json(resp).await?, "error")?, UPGRADE);
    assert_nothing_staged(&state).await
}

/// Another method that `/api/v1/plans` does not serve answers the plain `405`
/// of every route. Its `Allow` header does not list `POST` either.
#[tokio::test]
async fn test_another_method_on_the_plan_listing_answers_the_plain_405() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;

    let resp = raw_request(&state, "DELETE", "/api/v1/plans", &[], None, String::new()).await?;

    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
    let allow = allowed_methods(&resp)?;
    assert!(allow.contains(&"GET".to_string()), "{allow:?}");
    assert!(!allow.contains(&"POST".to_string()), "{allow:?}");
    assert_eq!(
        json_str(&body_json(resp).await?, "error")?,
        "method not allowed"
    );
    Ok(())
}

fn allowed_methods(resp: &axum::http::Response<axum::body::Body>) -> anyhow::Result<Vec<String>> {
    let allow = resp
        .headers()
        .get(axum::http::header::ALLOW)
        .context("a 405 carries an Allow header")?
        .to_str()?;
    Ok(allow
        .split(',')
        .map(|method| method.trim().to_string())
        .collect())
}

// ── filing into a campaign (POST /api/v1/campaigns/{campaign_id}/plans) ──

/// Naming the input that merged would tell the filer which of its cells someone
/// else already runs; the count still explains a plan with fewer new jobs.
#[tokio::test]
async fn test_a_filing_reports_a_merge_count_but_not_which_inputs_merged() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, signer) = document_state(dir.path()).await?;

    let first = post_afm_mlx(&state, &signer, "first").await?;
    let second = post_afm_mlx(&state, &signer, "second").await?;

    assert_eq!(first["merged_job_count"], 0);
    assert_eq!(second["job_count"], 3);
    assert_eq!(second["merged_job_count"], 3);
    assert!(
        second.get("merges").is_none(),
        "the HTTP report withholds the per-input merge detail: {second}"
    );
    assert_eq!(
        report_job_ids(&second)?,
        report_job_ids(&first)?,
        "both plans hold the jobs that run the cells"
    );
    assert_eq!(
        state.todo_store.list_avail().await?.len(),
        3,
        "three cells, three queued jobs"
    );
    Ok(())
}

/// A body that matches nobody in today's fleet is not a defect: the excluded
/// client could be replaced tomorrow. The exclusion beats the variant's roster.
#[rstest]
#[case::withheld(false, "withheld")]
#[case::full(true, "full")]
#[tokio::test]
async fn test_an_excluded_client_warns_and_still_stages(
    #[case] full: bool,
    #[case] tier: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_scheduler_benchmarks(dir.path())?;
    let state = seeded_state(dir.path()).await?;
    let (_key, client_id) = register_and_approve(&state).await?;
    // Keep a roster member the exclusion does not name, so the variant is not
    // statically unmatchable — that is a `400`.
    let document = mlx_document(&format!(
        "clients = [\"{client_id}\", \"ev1_nobody\"]\nexclude_clients = [\"{client_id}\"]"
    ));

    let signer = registered_user_with_detail(&state, full).await?;
    let resp = post_into_own_campaign(&state, &signer, "", &document).await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let report = body_json(resp).await?;

    let warnings = report["warnings"]
        .as_array()
        .context("`warnings` is an array")?;
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    let message = json_str(&warnings[0], "message")?;
    assert_eq!(report["fleet_detail"], tier);
    if full {
        assert!(message.contains("excluding clients") && message.contains(client_id.as_str()));
    } else {
        assert_eq!(message, "1 job matches no working client");
        assert!(!report.to_string().contains(client_id.as_str()));
    }

    let plan_id = pipette_plan_types::PlanId::try_new(json_str(&report, "plan_id")?)?;
    let manifest = state
        .plan_store
        .get_plan(&plan_id)
        .await?
        .context("manifest written")?;
    assert_eq!(manifest.status, PlanStatus::PendingClients);

    // Staged regardless, and the clause is stored verbatim for the matcher.
    let job_id = job(json_str(&report["job_ids"], "0")?)?;
    let staged = state
        .todo_store
        .get_avail_by_job(&job_id)
        .await?
        .context("job promoted to avail/")?;
    assert_eq!(staged["exclude_clients"], json!([client_id.as_str()]));
    Ok(())
}

/// The message names the caller's own model, so both tiers read the same text.
#[tokio::test]
async fn test_an_uncatalogued_model_warns_at_both_tiers_and_still_stages() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_scheduler_benchmarks(dir.path())?;
    let state = seeded_state(dir.path()).await?;
    // A table that knows another model only, so the warning is a miss in a
    // table that has rows rather than an empty one.
    state
        .model_params_store
        .put_model_params(&pipette_store_testkit::a_model_params("other-1b", 1000)?)
        .await?;
    let document = mlx_document("requires = [\"os:macos\"]");

    let (state, document) = (&state, document.as_str());
    let messages: Vec<String> =
        futures::stream::iter([(true, "full"), (false, "withheld")].map(anyhow::Ok))
            .and_then(|(full, tier)| async move {
                let signer = registered_user_with_detail(state, full).await?;
                let resp = post_into_own_campaign(state, &signer, "", document).await?;
                assert_eq!(resp.status(), StatusCode::CREATED, "{tier}");
                let report = body_json(resp).await?;
                assert_eq!(report["fleet_detail"], tier);

                let warnings = report["warnings"]
                    .as_array()
                    .context("`warnings` is an array")?;
                let model_warnings: Vec<&Value> = warnings
                    .iter()
                    .filter(|warning| warning["kind"] == "uncatalogued_model")
                    .collect();
                assert_eq!(model_warnings.len(), 1, "{tier}: {warnings:?}");
                assert_eq!(
                    model_warnings[0]["job_ids"],
                    json!([report["job_ids"]["0"]]),
                    "{tier}"
                );
                let message = json_str(model_warnings[0], "message")?.to_string();
                assert!(
                    message.contains("a model with no parameter-count entry")
                        && message.contains("LFM2.5-350M-MLX-4bit"),
                    "{tier}: {message}"
                );

                let job_id = job(json_str(&report["job_ids"], "0")?)?;
                assert!(
                    state.todo_store.get_avail_by_job(&job_id).await?.is_some(),
                    "{tier}: the job is in avail/"
                );
                anyhow::Ok(message)
            })
            .try_collect()
            .await?;
    assert_eq!(messages[0], messages[1], "the tiers read the same message");
    Ok(())
}

/// A model parameter table that cannot be read degrades to an empty one, so the plan
/// is filed with no warning rather than refused with a `500`.
#[tokio::test]
async fn test_an_unreadable_model_table_files_the_plan_without_a_warning() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (mut state, signer) = document_state(dir.path()).await?;

    // A readable table that lacks the model warns, so the store is the one read.
    state
        .model_params_store
        .put_model_params(&pipette_store_testkit::a_model_params("other-1b", 1000)?)
        .await?;
    let readable = mlx_document("requires = [\"os:macos\"]");
    let resp = post_into_own_campaign(&state, &signer, "", &readable).await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let warned = body_json(resp).await?;
    assert!(
        warned["warnings"]
            .as_array()
            .is_some_and(|warnings| warnings.iter().any(|w| w["kind"] == "uncatalogued_model")),
        "{warned}"
    );

    // A distinct cell, so the job is fresh rather than merged into the first.
    state.model_params_store =
        std::sync::Arc::new(pipette_store_testkit::ModelParamsStoreUnimplemented);
    let unreadable = mlx_document("requires = [\"os:macos\"]\nexcludes = [\"os_version:15.0\"]");
    let resp = post_into_own_campaign(&state, &signer, "", &unreadable).await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let report = body_json(resp).await?;
    assert_eq!(report["merged_job_count"], 0, "{report}");
    assert!(
        report["warnings"]
            .as_array()
            .is_some_and(|warnings| warnings.iter().all(|w| w["kind"] != "uncatalogued_model")),
        "{report}"
    );

    let job_id = job(json_str(&report["job_ids"], "0")?)?;
    assert!(
        state.todo_store.get_avail_by_job(&job_id).await?.is_some(),
        "the job is in avail/"
    );
    Ok(())
}

/// An empty `note` query parameter means no note, because an HTTP client may
/// send an empty parameter as its default. An empty `--note` flag in
/// `campaigns ingest` is an error, because an operator who types the flag makes
/// a mistake.
#[rstest]
#[case::trimmed(Some("  smoke  "), Some("smoke"))]
#[case::absent(None, None)]
#[case::empty(Some(""), None)]
#[case::whitespace_only(Some("   "), None)]
#[tokio::test]
async fn test_a_note_is_trimmed_and_empty_means_absent(
    #[case] submitted: Option<&str>,
    #[case] expected: Option<&str>,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, signer) = document_state(dir.path()).await?;

    let resp = post_named(&state, &signer, submitted).await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let report = body_json(resp).await?;

    match expected {
        Some(name) => assert_eq!(report["note"], name),
        None => assert!(
            report.get("note").is_none(),
            "a plan without a note omits the field: {report}"
        ),
    }
    Ok(())
}

/// Every plan listing reads the note back, so it needs a bound of its own;
/// 4096 multi-byte characters pass.
#[rstest]
#[case::at_the_cap("a".repeat(4096), StatusCode::CREATED)]
#[case::multibyte_at_the_cap("é".repeat(4096), StatusCode::CREATED)]
#[case::multibyte_over_the_cap("é".repeat(4097), StatusCode::BAD_REQUEST)]
#[case::at_the_cap_after_trimming(format!("  {}  ", "a".repeat(4096)), StatusCode::CREATED)]
#[tokio::test]
async fn test_a_note_is_bounded_in_characters_after_the_trim(
    #[case] name: String,
    #[case] expected: StatusCode,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, signer) = document_state(dir.path()).await?;

    let resp = post_named(&state, &signer, Some(&name)).await?;

    assert_eq!(resp.status(), expected);
    if expected == StatusCode::BAD_REQUEST {
        assert_eq!(
            json_str(&body_json(resp).await?, "error")?,
            "note is longer than 4096 characters"
        );
        assert_nothing_staged(&state).await?;
    }
    Ok(())
}

/// File `AFM_MLX` into a new campaign of `signer`'s own, with `note` in
/// the query when it is `Some`.
async fn post_named(
    state: &AppState,
    signer: &UserSigner,
    note: Option<&str>,
) -> anyhow::Result<axum::http::Response<axum::body::Body>> {
    let campaign_id = put_idle_campaign(state, &signer.1, "Named").await?;
    let uri = match note {
        Some(name) => filing_uri(campaign_id.as_str(), name)?,
        None => format!("/api/v1/campaigns/{campaign_id}/plans"),
    };
    post_plan_document(state, signer, &uri, AFM_MLX).await
}

// ── plan documents (application/toml) ─────────────────────────

/// The upstream example, byte for byte. It names the two benchmarks that
/// `setup_scheduler_benchmarks` writes, and it expands to three cells.
const AFM_MLX: &str = include_str!("../../../examples/plans/scheduler/afm-mlx.toml");

/// A secret that no response or stored object may echo.
const SECRET: &str = "hf_supersecret";

/// A one-variant macOS MLX document. `lines` sets the variant's eligibility.
fn mlx_document(lines: &str) -> String {
    format!(
        r#"benchmarks = ["decode_throughput_512_100"]

[[variants]]
{lines}
models   = [{{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "LFM2.5-350M-MLX-4bit" }}]
runtimes = [{{ type = "mlx_macos_pipette", version = "0.31.3", flavor = "macos-arm64", source = {{ type = "pip_requirements_text", contents = "mlx-lm==0.31.3\n" }} }}]
"#
    )
}

/// A one-variant iOS Apple Foundation document. The capability rules inject
/// the supported-iPhone family into it as an `any_of`.
const AFM_IOS: &str = r#"benchmarks = ["decode_throughput_512_100"]

[[variants]]
requires = ["os:ios"]
models   = [{ type = "apple_foundation_text" }]
runtimes = [{ type = "apple_foundation" }]
"#;

/// A state with the benchmarks of the scheduler examples, and an account that
/// files and reads its own plans. The account is at the full tier, so it reads
/// their documents too.
async fn document_state(
    dir: &std::path::Path,
) -> anyhow::Result<(AppState, pipette_web_testkit::UserSigner)> {
    setup_scheduler_benchmarks(dir)?;
    let state = seeded_state(dir).await?;
    let signer = filer(&state).await?;
    Ok((state, signer))
}

/// `query` is empty or starts with `?`.
async fn post_into_own_campaign(
    state: &AppState,
    signer: &pipette_web_testkit::UserSigner,
    query: &str,
    document: &str,
) -> anyhow::Result<axum::http::Response<axum::body::Body>> {
    let campaign_id = put_idle_campaign(state, &signer.1, "Documents").await?;
    post_plan_document(
        state,
        signer,
        &format!("/api/v1/campaigns/{campaign_id}/plans{query}"),
        document,
    )
    .await
}

/// The objects in the two plan-source keyspaces, which must stay empty after a
/// refused or dry-run document.
fn stored_sources(dir: &std::path::Path) -> anyhow::Result<usize> {
    ["plan-sources", "plan-sources-pending"]
        .iter()
        .map(|keyspace| Ok(std::fs::read_dir(dir.join(keyspace))?.count()))
        .sum()
}

/// The `job_id` values of a `201` report, in index order.
fn report_job_ids(report: &Value) -> anyhow::Result<Vec<String>> {
    let jobs = report["job_ids"].as_object().context("a `job_ids` map")?;
    (0..jobs.len())
        .map(|index| Ok(json_str(&report["job_ids"], &index.to_string())?.to_string()))
        .collect()
}

/// The upstream example stages as it is: the server expands it to its three
/// cells and stores the document beside the manifest.
#[tokio::test]
async fn test_a_plan_document_stages_its_cells() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, signer) = document_state(dir.path()).await?;

    let resp = post_into_own_campaign(&state, &signer, "?note=e2e", AFM_MLX).await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let report = body_json(resp).await?;
    assert_eq!(report["job_count"], 3);
    assert_eq!(report["note"], "e2e");
    assert_eq!(state.todo_store.list_avail().await?.len(), 3);

    let plan_id = pipette_plan_types::PlanId::try_new(json_str(&report, "plan_id")?)?;
    let manifest = state
        .plan_store
        .get_plan(&plan_id)
        .await?
        .context("the manifest was written")?;
    assert_eq!(manifest.submitted_by, Some(signer.1.clone()));
    assert!(
        manifest.source_sha256.is_some(),
        "the manifest names no source"
    );
    pipette_store_testkit::assert_each_job_has_its_source_and_link(
        state.plan_store.as_ref(),
        state.todo_store.as_ref(),
        &plan_id,
        &manifest.job_ids,
    )
    .await?;
    Ok(())
}

/// Each negative clause, and an `any_of` the capability rules inject, reaches
/// the queued body; the upstream examples use none of them.
#[rstest]
#[case::excludes(
    mlx_document("requires = [\"os:macos\"]\nexcludes = [\"os_version:15.0\"]"),
    "excludes"
)]
#[case::not_all_of(
    mlx_document(
        "requires = [\"os:macos\"]\nnot_all_of = [[\"chip:applem1\", \"runtime:mlx:0.31.3\"]]"
    ),
    "not_all_of"
)]
#[case::exclude_clients(
    mlx_document("requires = [\"os:macos\"]\nexclude_clients = [\"ev1_4a1b2c3d4e5f6a7b\"]"),
    "exclude_clients"
)]
#[case::injected_any_of(AFM_IOS.to_string(), "any_of")]
// A roster-only variant takes no injection, so its versions are the one group.
#[case::roster_only_client_versions(
    mlx_document("clients = [\"ev1_4a1b2c3d4e5f6a7b\"]\nclient_versions = [\"1.0\"]"),
    "any_of"
)]
#[tokio::test]
async fn test_a_document_clause_stages(
    #[case] document: String,
    #[case] clause: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, signer) = document_state(dir.path()).await?;

    let resp = post_into_own_campaign(&state, &signer, "", &document).await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let report = body_json(resp).await?;
    let job_id = job(json_str(&report["job_ids"], "0")?)?;
    let body = state
        .todo_store
        .get_avail_by_job(&job_id)
        .await?
        .context("the job is in avail/")?;
    let staged = body[clause].as_array().context("the clause is an array")?;
    assert!(!staged.is_empty(), "{clause} is empty in {body}");
    Ok(())
}

/// A model parameter table that records each call, so a test can tell whether a route read it.
struct RecordedTable(std::sync::Arc<pipette_store_testkit::UnexpectedCalls>);

#[async_trait::async_trait]
impl pipette_store::ModelParamsStore for RecordedTable {
    async fn get_model_params(
        &self,
        _key: &pipette_plan_types::ModelKey,
    ) -> anyhow::Result<Option<pipette_core::model_params::ModelParams>> {
        Err(self.0.record("get_model_params"))
    }

    async fn put_model_params(
        &self,
        _params: &pipette_core::model_params::ModelParams,
    ) -> anyhow::Result<()> {
        Err(self.0.record("put_model_params"))
    }

    async fn list_model_params(
        &self,
        _after: Option<&pipette_plan_types::ModelKey>,
        _limit: std::num::NonZeroUsize,
    ) -> anyhow::Result<pipette_store::ModelParamsPage> {
        Err(self.0.record("list_model_params"))
    }

    async fn delete_model_params(&self, _key: &pipette_plan_types::ModelKey) -> anyhow::Result<()> {
        Err(self.0.record("delete_model_params"))
    }
}

/// A dry run reads no model parameter table: coverage is a warning only filing reports.
#[tokio::test]
async fn test_a_dry_run_reads_no_model_table() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (mut state, signer) = document_state(dir.path()).await?;
    let reads = std::sync::Arc::new(pipette_store_testkit::UnexpectedCalls::default());
    state.model_params_store = std::sync::Arc::new(RecordedTable(std::sync::Arc::clone(&reads)));

    let resp = post_into_own_campaign(&state, &signer, "?dry_run=true", AFM_MLX).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let report = body_json(resp).await?;
    assert_eq!(
        report
            .pointer("/bodies")
            .and_then(Value::as_array)
            .map(Vec::len),
        Some(3),
        "{report}"
    );
    reads.none()?;

    // Filing does read it, so the recording table is the one in use.
    let resp = post_into_own_campaign(&state, &signer, "", AFM_MLX).await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    anyhow::ensure!(
        reads.none().is_err(),
        "filing read no model parameter table"
    );
    Ok(())
}

/// A document that the server refuses answers `400` in the error envelope, and
/// nothing reaches the queue or the source store. No message echoes a token.
#[rstest]
#[case::bad_toml("benchmarks = [".to_string(), "not a valid scheduler plan")]
#[case::capability_rule(
    mlx_document("requires = [\"os:macos\"]\nnot_all_of = [[\"chip:applem1\", \"chip:applem2\"]]"),
    "capability rule"
)]
#[case::dead_path_without_a_roster(
    mlx_document("requires = [\"os:macos\"]\nexcludes = [\"os:macos\"]"),
    "`excludes` names"
)]
#[case::client_version_not_canonical(
    mlx_document("requires = [\"os:macos\"]\nclient_versions = [\"2026.10.02-ABC\"]"),
    "`client_versions` value \"2026.10.02-ABC\""
)]
#[case::every_client_version_excluded(
    mlx_document(
        "requires = [\"os:macos\"]\nclient_versions = [\"1.0\"]\nexcludes = [\"client_version:1.0\"]"
    ),
    "covers every member of the `any_of` group [\"client_version:1.0\"]"
)]
#[case::unknown_benchmark(
    mlx_document("requires = [\"os:macos\"]")
        .replace("decode_throughput_512_100", "no_such_benchmark"),
    "rejected job \"0\" (index 0): spec.benchmark no_such_benchmark"
)]
#[case::hf_token_env(
    format!(
        r#"benchmarks = ["prefill_throughput_256"]

[[variants]]
requires = ["os:macos"]
models   = [{{ type = "gguf_text", source = "huggingface", org = "o", repo_name = "r", path = "a.gguf" }}]
runtimes = [{{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b9050", flavor = "macos-arm64" }}]
runtime_flags = [{{ benchmark_type = "prefill_throughput", runtime_type = "llamacpp_cli_stock_tools", model_type = "gguf_text", envs = ["HF_TOKEN={SECRET}"] }}]
"#
    ),
    "sets HF_TOKEN to a value"
)]
#[tokio::test]
async fn test_a_refused_document_stages_nothing(
    #[case] document: String,
    #[case] needle: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, signer) = document_state(dir.path()).await?;

    let resp = post_into_own_campaign(&state, &signer, "", &document).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let error = json_str(&body_json(resp).await?, "error")?.to_string();
    assert!(error.contains(needle), "{needle:?} not in {error}");
    assert!(
        !error.contains(SECRET),
        "the message echoes the token: {error}"
    );
    assert_nothing_staged(&state).await?;
    assert_eq!(stored_sources(dir.path())?, 0);
    assert_eq!(
        pipette_store_testkit::local_fs_job_record_count(dir.path()),
        0
    );
    Ok(())
}

/// A malformed document's TOML diagnostic is reported once, after the
/// planner's own sentence.
#[tokio::test]
async fn test_a_toml_diagnostic_is_reported_once() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, signer) = document_state(dir.path()).await?;

    let resp = post_into_own_campaign(&state, &signer, "", "benchmarks = [").await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let error = json_str(&body_json(resp).await?, "error")?.to_string();
    assert!(
        error.starts_with("the document is not a valid scheduler plan: TOML parse error"),
        "{error}"
    );
    assert_eq!(error.matches("TOML parse error").count(), 1, "{error}");
    Ok(())
}

/// A dead capability path with a live roster stages, as does a roster that
/// ignores its `client_versions`, warning in `plan_warnings` at both tiers. The
/// warning quotes the document, so the plan read serves it at the full tier
/// only.
#[rstest]
#[case::dead_capability_path(
    "requires = [\"os:macos\"]\nclients = [\"ev1_live\"]\nexcludes = [\"os:macos\"]",
    "`excludes` names"
)]
#[case::roster_beside_client_versions(
    "requires = [\"os:macos\"]\nclients = [\"ev1_live\"]\nclient_versions = [\"1.0\"]",
    "`client_versions` narrows only"
)]
#[tokio::test]
async fn test_plan_warnings_reach_the_submitter_and_the_full_tier(
    #[case] lines: &str,
    #[case] needle: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_scheduler_benchmarks(dir.path())?;
    let state = seeded_state(dir.path()).await?;
    let document = mlx_document(lines);

    let mut seen = Vec::new();
    for (full, tier) in [(true, "full"), (false, "withheld")] {
        let signer = registered_user_with_detail(&state, full).await?;
        let resp = post_into_own_campaign(&state, &signer, "", &document).await?;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let report = body_json(resp).await?;
        assert_eq!(report["fleet_detail"], tier);

        let (key, user_id, key_id) = &signer;
        let uri = format!("/api/v1/plans/{}", json_str(&report, "plan_id")?);
        let resp = authed_user_get(&state, key, user_id, key_id, &uri).await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let status = body_json(resp).await?;
        crate::api_schema::assert_conforms("PlanView", &status)?;
        if full {
            assert_eq!(status["plan_warnings"], report["plan_warnings"], "{tier}");
        } else {
            assert_eq!(status["plan_warnings"], json!([]), "{tier}");
        }
        seen.push(report["plan_warnings"].clone());
    }
    let warnings = seen[0].as_array().context("a `plan_warnings` array")?;
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0]
            .as_str()
            .is_some_and(|warning| warning.contains("variant 0") && warning.contains(needle)),
        "{warnings:?}"
    );
    assert_eq!(seen[0], seen[1]);
    Ok(())
}

/// An approved Mac reporting `capabilities`, on a chip the MLX rules admit.
async fn mac_reporting(
    state: &AppState,
    capabilities: &[&str],
) -> anyhow::Result<(ed25519_dalek::SigningKey, ClientId)> {
    let (key, client_id) = register_and_approve(state).await?;
    let profile = DeviceProfile {
        device_os_name: Some(NonEmptyTrimmedString::try_new("macOS")?),
        device_chip_model: Some(NonEmptyTrimmedString::try_new("Apple M4")?),
        ..Default::default()
    };
    set_matching_input(state, &client_id, profile, capabilities, &[]).await?;
    Ok((key, client_id))
}

/// A document's `client_versions` holds from filing to the claim: only
/// the client reporting a listed build is indexed, and its claim names it.
#[tokio::test]
async fn test_a_pinned_document_is_claimed_at_a_listed_build() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, signer) = document_state(dir.path()).await?;
    let (listed_key, listed) =
        mac_reporting(&state, &["client_version:2.0", "client_version:3.0"]).await?;
    let (other_key, other) = mac_reporting(&state, &["client_version:3.0"]).await?;

    let document = mlx_document("requires = [\"os:macos\"]\nclient_versions = [\"1.0\", \"2.0\"]");
    let resp = post_into_own_campaign(&state, &signer, "", &document).await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let report = body_json(resp).await?;
    run_qm(&state).await?;

    let resp = authed_post(&state, &other_key, &other, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let resp = authed_post(&state, &listed_key, &listed, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let claim = body_json(resp).await?;
    assert_eq!(claim["job_id"], report["job_ids"]["0"]);
    assert_eq!(claim["client_versions"], json!(["2.0"]));
    Ok(())
}

/// On a real submission and on a dry run; nothing is staged and no source
/// written.
#[rstest]
#[case::submit("")]
#[case::dry_run("?dry_run=true")]
#[tokio::test]
async fn test_a_document_with_a_past_expires_at_is_refused(
    #[case] query: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, signer) = document_state(dir.path()).await?;
    let document = format!(
        "expires_at = \"2020-01-01T00:00:00Z\"\n{}",
        mlx_document("requires = [\"os:macos\"]")
    );

    let resp = post_into_own_campaign(&state, &signer, query, &document).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let error = json_str(&body_json(resp).await?, "error")?.to_string();
    assert!(
        error.contains("at or before the time of ingestion"),
        "{error}"
    );
    assert_nothing_staged(&state).await?;
    assert_eq!(stored_sources(dir.path())?, 0);
    assert_eq!(
        pipette_store_testkit::local_fs_job_record_count(dir.path()),
        0
    );
    Ok(())
}

/// On a real submission and a dry run alike, so a dry run never accepts a note
/// the real run refuses.
#[rstest]
#[case::submit("?note=")]
#[case::dry_run("?dry_run=true&note=")]
#[tokio::test]
async fn test_a_document_note_over_the_cap_is_refused(#[case] prefix: &str) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, signer) = document_state(dir.path()).await?;

    let query = format!("{prefix}{}", "a".repeat(4097));
    let resp = post_into_own_campaign(&state, &signer, &query, AFM_MLX).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        json_str(&body_json(resp).await?, "error")?,
        "note is longer than 4096 characters"
    );
    assert_nothing_staged(&state).await?;
    assert_eq!(stored_sources(dir.path())?, 0);
    assert_eq!(
        pipette_store_testkit::local_fs_job_record_count(dir.path()),
        0
    );
    Ok(())
}

/// With the text a plan of job bodies gets, before any body is built.
#[tokio::test]
async fn test_a_document_over_the_job_ceiling_is_refused() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_scheduler_benchmarks(dir.path())?;
    setup_benchmarks(dir.path())?;
    let limit = std::num::NonZeroUsize::new(2).context("limit is non-zero")?;
    let state = make_state_with_plan_max_jobs(dir.path(), limit).await?;
    let signer = registered_user(&state, &[UserPermission::SubmitPlans]).await?;

    let resp = post_into_own_campaign(&state, &signer, "", AFM_MLX).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        json_str(&body_json(resp).await?, "error")?,
        "plan declares 3 jobs, over the ceiling of 2 (`plan_max_jobs`)"
    );
    assert_nothing_staged(&state).await?;
    assert_eq!(stored_sources(dir.path())?, 0);
    assert_eq!(
        pipette_store_testkit::local_fs_job_record_count(dir.path()),
        0
    );
    Ok(())
}

/// Before the body is read. Unknown keys are refused, so a misspelt `dry_run`
/// or a leftover `submitter` cannot file for real.
#[rstest]
#[case::dry_run_not_a_bool("?dry_run=maybe")]
#[case::misspelt_dry_run("?dryrun=true")]
#[case::submitter("?submitter=user_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e")]
#[tokio::test]
async fn test_a_malformed_document_query_is_refused(#[case] query: &str) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, signer) = document_state(dir.path()).await?;

    let resp = post_into_own_campaign(&state, &signer, query, AFM_MLX).await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let error = json_str(&body_json(resp).await?, "error")?.to_string();
    assert!(!error.is_empty(), "the envelope carries an error message");
    assert_nothing_staged(&state).await?;
    assert_eq!(stored_sources(dir.path())?, 0);
    assert_eq!(
        pipette_store_testkit::local_fs_job_record_count(dir.path()),
        0
    );
    Ok(())
}

/// Nothing is staged, and no source is written.
#[tokio::test]
async fn test_a_document_that_is_not_utf8_is_refused() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, (key, user_id, key_id)) = document_state(dir.path()).await?;
    let uri = own_campaign_plans_uri(&state, &user_id).await?;

    let headers = pipette_web_testkit::user_auth_headers(&key, &user_id, &key_id, "POST", &uri)?;
    let resp = raw_request(
        &state,
        "POST",
        &uri,
        &headers,
        Some("application/toml"),
        vec![0xffu8, 0xfe, b'x'],
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let error = json_str(&body_json(resp).await?, "error")?.to_string();
    // The cause, not axum's wording: a lossy decode would fail as a plan-parse 400.
    assert!(error.contains("UTF-8"), "{error}");
    assert_nothing_staged(&state).await?;
    assert_eq!(stored_sources(dir.path())?, 0);
    assert_eq!(
        pipette_store_testkit::local_fs_job_record_count(dir.path()),
        0
    );
    Ok(())
}

/// A dry run answers with the bodies the document expands to and stages
/// nothing: no manifest, no job, no source.
#[tokio::test]
async fn test_a_dry_run_stages_nothing() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, signer) = document_state(dir.path()).await?;

    let resp = post_into_own_campaign(&state, &signer, "?dry_run=true&note=dry", AFM_MLX).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let report = body_json(resp).await?;
    assert_eq!(report["bodies"].as_array().map(Vec::len), Some(3));
    assert_eq!(report["plan_warnings"], json!([]));
    assert_nothing_staged(&state).await?;
    assert_eq!(stored_sources(dir.path())?, 0);
    assert_eq!(
        pipette_store_testkit::local_fs_job_record_count(dir.path()),
        0
    );
    Ok(())
}

/// `GET /api/v1/plans/{plan_id}/document` answers with the document byte for
/// byte.
#[tokio::test]
async fn test_a_plan_document_round_trips() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, signer) = document_state(dir.path()).await?;

    let resp = post_into_own_campaign(&state, &signer, "", AFM_MLX).await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let plan_id = json_str(&body_json(resp).await?, "plan_id")?.to_string();

    let (key, user_id, key_id) = &signer;
    let resp = authed_user_get(
        &state,
        key,
        user_id,
        key_id,
        &format!("/api/v1/plans/{plan_id}/document"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get(axum::http::header::CONTENT_TYPE)
            .map(|value| value.to_str())
            .transpose()?,
        Some("application/toml; charset=utf-8")
    );
    let bytes = resp.into_body().collect().await?.to_bytes();
    assert_eq!(bytes.as_ref(), AFM_MLX.as_bytes());
    Ok(())
}

/// A plan filed as job bodies has no source, at both tiers, and another
/// account's plan reads as absent, exactly as its status does.
#[tokio::test]
async fn test_a_plan_source_answers_404() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    setup_scheduler_benchmarks(dir.path())?;
    let owner = filer(&state).await?;
    let withheld_owner = registered_user_with_detail(&state, false).await?;
    // Holds no `read_clients`. A plan outside the scope answers `404` at the
    // withheld tier too.
    let stranger = registered_user(&state, &[UserPermission::ReadPlans]).await?;

    let job_body_plans: Vec<String> = {
        let (state, client_id) = (&state, &client_id);
        futures::stream::iter([&owner, &withheld_owner])
            .then(|signer| async move {
                let campaign_id = put_idle_campaign(state, &signer.1, "Job bodies").await?;
                let plan_id =
                    file_job_bodies(state, signer, campaign_id.as_str(), client_id, "bodies")
                        .await?;
                anyhow::Ok(plan_id.to_string())
            })
            .try_collect()
            .await?
    };

    let resp = post_into_own_campaign(&state, &owner, "", AFM_IOS).await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let document_plan = json_str(&body_json(resp).await?, "plan_id")?.to_string();

    for (signer, plan_id, expected) in [
        (&owner, &job_body_plans[0], "plan has no source document"),
        (
            &withheld_owner,
            &job_body_plans[1],
            "plan has no source document",
        ),
        (&stranger, &document_plan, "plan not found"),
    ] {
        let (key, user_id, key_id) = signer;
        let resp = authed_user_get(
            &state,
            key,
            user_id,
            key_id,
            &format!("/api/v1/plans/{plan_id}/document"),
        )
        .await?;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_eq!(json_str(&body_json(resp).await?, "error")?, expected);
    }
    Ok(())
}

/// Who reads a team campaign's plan document in
/// [`test_a_plan_document_is_gated_on_fleet_detail`].
#[derive(Clone, Copy, Debug)]
enum DocumentReader {
    /// The account that filed the plan, at the full tier.
    FullFiler,
    /// The account that filed the plan, at the withheld tier.
    WithheldFiler,
    /// A teammate who reads the plan through the team's campaign, at the
    /// withheld tier.
    WithheldTeammate,
}

/// The plan document names requirement sets, so only the full tier reads it. Below
/// the full tier, `GET /api/v1/plans/{plan_id}/document` answers `403`, for the filer
/// too. `GET /api/v1/jobs/{job_id}/plans` drops the eligibility fields of the job.
#[rstest]
#[case::full_filer(DocumentReader::FullFiler)]
#[case::withheld_filer(DocumentReader::WithheldFiler)]
#[case::withheld_teammate(DocumentReader::WithheldTeammate)]
#[tokio::test]
async fn test_a_plan_document_is_gated_on_fleet_detail(
    #[case] who: DocumentReader,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let org = pipette_web_testkit::seed_org(&state, "acme").await?;
    let filer_grants = [UserPermission::SubmitPlans, UserPermission::ManageOrgs];
    let full = [&filer_grants[..], &[UserPermission::ReadClients]].concat();
    let filer = match who {
        DocumentReader::FullFiler => registered_user(&state, &full).await?,
        DocumentReader::WithheldFiler | DocumentReader::WithheldTeammate => {
            registered_user(&state, &filer_grants).await?
        }
    };
    pipette_web_testkit::set_user_org(&state, &filer.1, Some(org.org_id.clone())).await?;
    let (_, plan_id) = pipette_web_testkit::created_campaign_from(
        &state,
        &filer,
        &[("name", "Team nightly"), ("owner", org.org_id.as_str())],
        pipette_web_testkit::ONE_CLIENT_PLAN,
    )
    .await?;
    let reader = match who {
        DocumentReader::WithheldTeammate => {
            let teammate = registered_user(&state, &[UserPermission::ReadPlans]).await?;
            pipette_web_testkit::set_user_org(&state, &teammate.1, Some(org.org_id.clone()))
                .await?;
            teammate
        }
        DocumentReader::FullFiler | DocumentReader::WithheldFiler => filer,
    };
    let is_full = matches!(who, DocumentReader::FullFiler);

    let (key, user_id, key_id) = &reader;
    let resp = authed_user_get(
        &state,
        key,
        user_id,
        key_id,
        &format!("/api/v1/plans/{plan_id}/document"),
    )
    .await?;
    if is_full {
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = resp.into_body().collect().await?.to_bytes();
        assert_eq!(
            bytes.as_ref(),
            pipette_web_testkit::ONE_CLIENT_PLAN.as_bytes()
        );
    } else {
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let error = body_json(resp).await?.to_string();
        assert!(error.contains("read_clients"), "{error}");
        assert!(
            !error.contains(pipette_web_testkit::NAMED_CLIENT_ID),
            "{error}"
        );
    }

    let job_id = state
        .plan_store
        .get_plan(&plan_id)
        .await?
        .context("the plan is filed")?
        .job_ids
        .first()
        .cloned()
        .context("the plan holds a job")?;
    let (status, view) = plans_by_job(&state, &reader, job_id.as_str()).await?;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(view["body"]["job_id"], job_id.as_str());
    if is_full {
        assert_eq!(view["fleet_detail"], "full");
        assert_eq!(view["body"]["requires"], json!(["os:macos"]));
        assert_eq!(
            view["body"]["clients"],
            json!([pipette_web_testkit::NAMED_CLIENT_ID])
        );
    } else {
        assert_eq!(view["fleet_detail"], "withheld");
        assert!(
            !view
                .to_string()
                .contains(pipette_web_testkit::NAMED_CLIENT_ID),
            "{view}"
        );
        let body = view["body"].as_object().context("a body object")?;
        let mut keys: Vec<&str> = body.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["expires_at", "job_id", "spec"], "{view}");
    }
    Ok(())
}

/// A source that the GC collected before the manifest answers `404`. A stored
/// source whose digest differs from the manifest's answers `500`, and the route
/// does not serve it.
#[rstest]
#[case::collected(None, StatusCode::NOT_FOUND)]
#[case::replaced(Some("benchmarks = []\n"), StatusCode::INTERNAL_SERVER_ERROR)]
#[tokio::test]
async fn test_a_plan_source_that_the_manifest_does_not_match(
    #[case] stored: Option<&str>,
    #[case] status: StatusCode,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, signer) = document_state(dir.path()).await?;

    let resp = post_into_own_campaign(&state, &signer, "", AFM_MLX).await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let plan_id = json_str(&body_json(resp).await?, "plan_id")?.to_string();
    let id = pipette_plan_types::PlanId::try_new(&plan_id)?;
    match stored {
        None => state.plan_store.delete_plan_source(&id).await?,
        Some(text) => {
            // A plan source is write-once, so the replacement needs the
            // original gone first.
            state.plan_store.delete_plan_source(&id).await?;
            let source = pipette_core::plan::PlanSource::new(text.to_string())?;
            state.plan_store.put_plan_source(&id, &source).await?;
        }
    }

    let (key, user_id, key_id) = &signer;
    let uri = format!("/api/v1/plans/{plan_id}/document");
    let resp = authed_user_get(&state, key, user_id, key_id, &uri).await?;
    assert_eq!(resp.status(), status);
    let error = json_str(&body_json(resp).await?, "error")?.to_string();
    if stored.is_none() {
        assert_eq!(error, "plan source was collected");
    }
    Ok(())
}

// ── plans by job (GET /api/v1/jobs/{job_id}/plans) ─────────────────

/// `GET /api/v1/jobs/{job_id}/plans` as `signer`: the status and the JSON body.
async fn plans_by_job(
    state: &AppState,
    signer: &pipette_web_testkit::UserSigner,
    job_id: &str,
) -> anyhow::Result<(StatusCode, Value)> {
    let (key, user_id, key_id) = signer;
    let uri = format!("/api/v1/jobs/{job_id}/plans");
    let resp = authed_user_get(state, key, user_id, key_id, &uri).await?;
    let status = resp.status();
    Ok((status, body_json(resp).await?))
}

/// File `AFM_MLX` as `signer` with `note`, into a new campaign of
/// `signer`'s own, and answer with the `201` report.
async fn post_afm_mlx(
    state: &AppState,
    signer: &pipette_web_testkit::UserSigner,
    note: &str,
) -> anyhow::Result<Value> {
    let campaign_id = put_idle_campaign(state, &signer.1, note).await?;
    let uri = filing_uri(campaign_id.as_str(), note)?;
    let resp = post_plan_document(state, signer, &uri, AFM_MLX).await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    body_json(resp).await
}

fn listed_plan_ids(view: &Value) -> anyhow::Result<Vec<String>> {
    view["plans"]
        .as_array()
        .context("a `plans` array")?
        .iter()
        .map(|plan| Ok(json_str(plan, "plan_id")?.to_string()))
        .collect()
}

/// A job that two of the caller's plans share lists both, oldest first, with
/// the body that the queue holds for the job. Each plan is drawn as the plan
/// read answers it, cancel marker included.
#[tokio::test]
async fn test_a_shared_job_lists_its_plans_oldest_first() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, signer) = document_state(dir.path()).await?;
    let first = post_afm_mlx(&state, &signer, "a").await?;
    let second = post_afm_mlx(&state, &signer, "b").await?;
    assert_eq!(second["merged_job_count"], 3);
    let first_id = PlanId::try_new(json_str(&first, "plan_id")?)?;
    let cancelled = PlanId::try_new(json_str(&second, "plan_id")?)?;
    state.plan_store.write_cancel_marker(&cancelled).await?;
    let expected = json!([
        as_a_row(&state, &signer, &first_id).await?,
        as_a_row(&state, &signer, &cancelled).await?,
    ]);

    let (state, signer, first, second, expected) = (&state, &signer, &first, &second, &expected);
    futures::stream::iter(report_job_ids(first)?.into_iter().map(anyhow::Ok))
        .try_for_each(|job_id| async move {
            let (status, view) = plans_by_job(state, signer, &job_id).await?;
            assert_eq!(status, StatusCode::OK, "{view}");
            assert_eq!(
                listed_plan_ids(&view)?,
                vec![
                    json_str(first, "plan_id")?.to_string(),
                    json_str(second, "plan_id")?.to_string(),
                ]
            );
            assert!(view["plans"][0]["created_at"].is_string(), "{view}");
            assert_eq!(view["plans"][0]["cancel_requested"], false, "{view}");
            assert_eq!(view["plans"][1]["cancel_requested"], true, "{view}");
            assert_eq!(&view["plans"], expected, "{view}");
            let queued = state
                .todo_store
                .get_avail_by_job(&job(&job_id)?)
                .await?
                .context("the job is in avail/")?;
            assert_eq!(view["body"], queued);
            anyhow::Ok(())
        })
        .await
}

/// The body still answers after the job finishes and leaves `todo/`.
#[tokio::test]
async fn test_a_job_body_outlives_the_queue() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, signer) = document_state(dir.path()).await?;
    let report = post_afm_mlx(&state, &signer, "e2e").await?;
    let job_id = job(json_str(&report["job_ids"], "0")?)?;
    let queued = state
        .todo_store
        .get_avail_by_job(&job_id)
        .await?
        .context("the job is in avail/")?;

    pipette_store_testkit::finish_job(
        state.submission_store.as_ref(),
        state.todo_store.as_ref(),
        &job_id,
    )
    .await?;
    assert_eq!(state.todo_store.get_avail_by_job(&job_id).await?, None);

    let (status, view) = plans_by_job(&state, &signer, job_id.as_str()).await?;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(view["body"], queued);
    Ok(())
}

/// A job that the caller's plan shares with the plan of another user lists
/// only the plan of the caller.
#[tokio::test]
async fn test_a_cross_tenant_job_lists_only_the_callers_plan() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, owner) = document_state(dir.path()).await?;
    let stranger = registered_user(
        &state,
        &[UserPermission::SubmitPlans, UserPermission::ReadPlans],
    )
    .await?;
    let owner_plan = post_afm_mlx(&state, &owner, "owner").await?;
    let stranger_plan = post_afm_mlx(&state, &stranger, "stranger").await?;
    assert_eq!(stranger_plan["merged_job_count"], 3);

    for job_id in report_job_ids(&owner_plan)? {
        for (signer, report) in [(&owner, &owner_plan), (&stranger, &stranger_plan)] {
            let (status, view) = plans_by_job(&state, signer, &job_id).await?;
            assert_eq!(status, StatusCode::OK, "{view}");
            assert_eq!(
                listed_plan_ids(&view)?,
                vec![json_str(report, "plan_id")?.to_string()]
            );
        }
    }
    Ok(())
}

/// `read_any_plans` widens the route across tenants: a holder reads every plan
/// that links a shared job, oldest first, with the job's body.
#[tokio::test]
async fn test_a_read_any_plans_holder_lists_every_tenants_plan() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, owner) = document_state(dir.path()).await?;
    let stranger = registered_user(
        &state,
        &[UserPermission::SubmitPlans, UserPermission::ReadPlans],
    )
    .await?;
    // Holds no `read_clients`, so it reads at the withheld tier.
    let reader = registered_user(&state, &[UserPermission::ReadAnyPlans]).await?;
    let owner_plan = post_afm_mlx(&state, &owner, "owner").await?;
    let stranger_plan = post_afm_mlx(&state, &stranger, "stranger").await?;
    assert_eq!(stranger_plan["merged_job_count"], 3);
    let expected = vec![
        json_str(&owner_plan, "plan_id")?.to_string(),
        json_str(&stranger_plan, "plan_id")?.to_string(),
    ];

    for job_id in report_job_ids(&owner_plan)? {
        let (status, view) = plans_by_job(&state, &reader, &job_id).await?;
        assert_eq!(status, StatusCode::OK, "{view}");
        assert_eq!(listed_plan_ids(&view)?, expected);
        assert_eq!(view["body"]["job_id"], job_id.as_str());
        assert_eq!(view["fleet_detail"], "withheld");
    }
    Ok(())
}

/// A job in only the plan of another user, and a job with no link (one minted
/// before job-plan links existed), answer the same `404`.
#[tokio::test]
async fn test_a_foreign_job_and_an_unlinked_job_answer_the_same_404() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, owner) = document_state(dir.path()).await?;
    let stranger = registered_user(&state, &[UserPermission::ReadPlans]).await?;
    let report = post_afm_mlx(&state, &owner, "owner").await?;
    let plan_id = pipette_plan_types::PlanId::try_new(json_str(&report, "plan_id")?)?;
    let job_ids = report_job_ids(&report)?;

    // The state of a job minted before job-plan links existed: a body in
    // `todo/`, and no link or job source.
    let unlinked = job(&job_ids[1])?;
    pipette_store_testkit::local_fs_remove_job_plan_link(dir.path(), &unlinked, &plan_id)?;
    pipette_store_testkit::local_fs_remove_job_source(dir.path(), &unlinked)?;
    assert!(
        state
            .todo_store
            .get_avail_by_job(&unlinked)
            .await?
            .is_some()
    );

    let (foreign_status, foreign) = plans_by_job(&state, &stranger, &job_ids[0]).await?;
    let (unlinked_status, unlinked) = plans_by_job(&state, &owner, unlinked.as_str()).await?;
    assert_eq!(foreign_status, StatusCode::NOT_FOUND);
    assert_eq!(unlinked_status, StatusCode::NOT_FOUND);
    assert_eq!(
        json_str(&foreign, "error")?,
        "no readable plan holds this job"
    );
    assert_eq!(foreign, unlinked);
    Ok(())
}

#[tokio::test]
async fn test_a_link_whose_manifest_is_gone_is_skipped() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, signer) = document_state(dir.path()).await?;
    let gone = post_afm_mlx(&state, &signer, "gone").await?;
    let kept = post_afm_mlx(&state, &signer, "kept").await?;
    pipette_store_testkit::local_fs_remove_plan(
        dir.path(),
        &pipette_plan_types::PlanId::try_new(json_str(&gone, "plan_id")?)?,
    )?;

    let (status, view) = plans_by_job(&state, &signer, json_str(&gone["job_ids"], "0")?).await?;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(
        listed_plan_ids(&view)?,
        vec![json_str(&kept, "plan_id")?.to_string()]
    );
    Ok(())
}

/// Which stored object a test writes over with bytes that are not JSON.
enum Garbled {
    /// The sole linked manifest cannot establish that the caller owns the job.
    Manifest,
    /// The job source. A `null` body would hide the corruption.
    JobSource,
}

/// An unreadable sole manifest answers `404`; an unreadable job source still
/// answers `500` after a readable manifest establishes the caller's scope.
#[rstest]
#[case::manifest(Garbled::Manifest, StatusCode::NOT_FOUND)]
#[case::job_source(Garbled::JobSource, StatusCode::INTERNAL_SERVER_ERROR)]
#[tokio::test]
async fn test_a_stored_object_that_is_not_json_uses_scope_aware_status(
    #[case] garbled: Garbled,
    #[case] expected: StatusCode,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, signer) = document_state(dir.path()).await?;
    let report = post_afm_mlx(&state, &signer, "e2e").await?;
    let job_id = json_str(&report["job_ids"], "0")?;
    let path = match garbled {
        Garbled::Manifest => dir
            .path()
            .join("plans")
            .join(format!("{}.json", json_str(&report, "plan_id")?)),
        Garbled::JobSource => dir
            .path()
            .join("job-sources")
            .join(format!("{job_id}.json")),
    };
    assert!(path.is_file(), "{} exists", path.display());
    std::fs::write(&path, b"not json")?;

    let (status, view) = plans_by_job(&state, &signer, job_id).await?;
    assert_eq!(status, expected, "{view}");
    Ok(())
}

/// A broken foreign manifest must not reveal that a linked job exists.
#[tokio::test]
async fn test_an_unreadable_foreign_manifest_answers_the_same_404_as_a_miss() -> anyhow::Result<()>
{
    let dir = tempfile::tempdir()?;
    let (state, owner) = document_state(dir.path()).await?;
    let stranger = registered_user(&state, &[UserPermission::ReadPlans]).await?;
    let report = post_afm_mlx(&state, &owner, "owner").await?;
    let job_id = json_str(&report["job_ids"], "0")?;
    let manifest_path = dir
        .path()
        .join("plans")
        .join(format!("{}.json", json_str(&report, "plan_id")?));
    std::fs::write(&manifest_path, b"not json")?;

    let (status, body) = plans_by_job(&state, &stranger, job_id).await?;
    let (miss_status, miss_body) = plans_by_job(&state, &stranger, "job-no-such-job").await?;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(status, miss_status);
    assert_eq!(body, miss_body);
    Ok(())
}

/// Once a readable plan establishes scope, a broken linked manifest remains
/// a server error rather than silently dropping a potentially relevant plan.
#[tokio::test]
async fn test_an_unreadable_link_with_a_readable_plan_answers_500() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, signer) = document_state(dir.path()).await?;
    let broken = post_afm_mlx(&state, &signer, "broken").await?;
    let kept = post_afm_mlx(&state, &signer, "kept").await?;
    assert_eq!(kept["merged_job_count"], 3);
    let manifest_path = dir
        .path()
        .join("plans")
        .join(format!("{}.json", json_str(&broken, "plan_id")?));
    std::fs::write(&manifest_path, b"not json")?;

    let (status, view) = plans_by_job(&state, &signer, json_str(&broken["job_ids"], "0")?).await?;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{view}");
    Ok(())
}

/// The route drops every `auth_token` a stored job source holds, and a plan
/// filed with no note lists no `note`.
#[tokio::test]
async fn test_a_legacy_job_source_is_served_without_its_tokens() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, signer) = document_state(dir.path()).await?;
    let resp = post_into_own_campaign(&state, &signer, "", AFM_MLX).await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let job_id = job(json_str(&body_json(resp).await?["job_ids"], "0")?)?;
    let mut legacy = state
        .plan_store
        .get_job_source(&job_id)
        .await?
        .context("the job source was written")?;
    legacy["spec"]["model"]["auth_token"] = json!("hf_SECRET");
    std::fs::write(
        dir.path()
            .join("job-sources")
            .join(format!("{job_id}.json")),
        serde_json::to_vec(&legacy)?,
    )?;

    let (status, view) = plans_by_job(&state, &signer, job_id.as_str()).await?;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert!(view["plans"][0].get("note").is_none(), "{view}");
    assert_eq!(view["body"]["job_id"], json!(job_id.as_str()));
    let text = serde_json::to_string(&view)?;
    for secret in ["auth_token", "hf_SECRET"] {
        assert!(!text.contains(secret), "{secret} leaked into {text}");
    }
    Ok(())
}

/// An old source can carry an extra field that a second plan never submitted.
/// The route serves only fields that the old merge key made them share.
#[tokio::test]
async fn test_a_legacy_job_source_excludes_unkeyed_fields() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, owner) = document_state(dir.path()).await?;
    let stranger = registered_user(
        &state,
        &[UserPermission::SubmitPlans, UserPermission::ReadPlans],
    )
    .await?;
    let first = post_afm_mlx(&state, &owner, "owner").await?;
    let second = post_afm_mlx(&state, &stranger, "stranger").await?;
    assert_eq!(second["merged_job_count"], 3);
    let job_id = job(json_str(&first["job_ids"], "0")?)?;
    let mut source = state
        .plan_store
        .get_job_source(&job_id)
        .await?
        .context("the job source was written")?;
    source["private_note"] = json!("owner only");
    std::fs::write(
        dir.path()
            .join("job-sources")
            .join(format!("{job_id}.json")),
        serde_json::to_vec(&source)?,
    )?;

    let (status, view) = plans_by_job(&state, &stranger, job_id.as_str()).await?;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(
        listed_plan_ids(&view)?,
        vec![json_str(&second, "plan_id")?.to_string()]
    );
    assert_eq!(view["body"]["job_id"], job_id.as_str());
    assert_eq!(view["body"]["spec"], source["spec"]);
    assert!(view["body"].get("private_note").is_none(), "{view}");
    Ok(())
}

/// A link whose job source someone deleted from storage by hand answers `200`
/// with `body: null`.
#[tokio::test]
async fn test_a_missing_job_source_answers_a_null_body() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, signer) = document_state(dir.path()).await?;
    let report = post_afm_mlx(&state, &signer, "e2e").await?;
    let job_id = job(json_str(&report["job_ids"], "0")?)?;
    pipette_store_testkit::local_fs_remove_job_source(dir.path(), &job_id)?;

    let (status, view) = plans_by_job(&state, &signer, job_id.as_str()).await?;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(
        listed_plan_ids(&view)?,
        vec![json_str(&report, "plan_id")?.to_string()]
    );
    assert_eq!(view.get("body"), Some(&Value::Null));
    Ok(())
}

/// The no-grant case grants `cancel_plans`, since `submit_plans` implies
/// `read_plans`.
#[rstest]
#[case::no_read_grant(UserPermission::CancelPlans, "job-0", StatusCode::FORBIDDEN)]
#[case::bad_job_id(UserPermission::ReadPlans, "not%20a%20job", StatusCode::BAD_REQUEST)]
#[tokio::test]
async fn test_plans_by_job_gates(
    #[case] permission: UserPermission,
    #[case] job_id: &str,
    #[case] expected: StatusCode,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, _client_id) = approved_client(dir.path()).await?;
    let signer = registered_user(&state, &[permission]).await?;

    let (status, view) = plans_by_job(&state, &signer, job_id).await?;
    assert_eq!(status, expected, "{view}");
    Ok(())
}

// ── gated models (the import step) ──────────────────────────────

/// A state that imports from `hub`, and an account that files into campaigns
/// of its own.
async fn import_state(
    dir: &std::path::Path,
    hub: &MockHub,
) -> anyhow::Result<(AppState, UserSigner)> {
    setup_benchmarks(dir)?;
    let state = make_state_with_hf_endpoint(dir, hub.endpoint.as_str()).await?;
    let signer = registered_user(
        &state,
        &[UserPermission::SubmitPlans, UserPermission::ReadPlans],
    )
    .await?;
    Ok((state, signer))
}

async fn post_document(
    state: &AppState,
    signer: &UserSigner,
    uri: &str,
    document: &str,
) -> anyhow::Result<(StatusCode, Value)> {
    let response = post_plan_document(state, signer, uri, document).await?;
    let status = response.status();
    Ok((status, body_json(response).await?))
}

async fn record_owners(state: &AppState) -> anyhow::Result<Vec<VaultOwner>> {
    Ok(drain_vault_resources(state.vault_store.as_ref())
        .await?
        .items
        .into_iter()
        .map(|record| record.owner)
        .collect())
}

/// The stored `spec.model` of the first job that `report` names.
async fn stored_model(state: &AppState, report: &Value) -> anyhow::Result<Value> {
    let job_id = job(json_str(&report["job_ids"], "0")?)?;
    let stored = state
        .plan_store
        .get_job_source(&job_id)
        .await?
        .context("the job source was written")?;
    stored
        .pointer("/spec/model")
        .cloned()
        .context("the job source names a model")
}

/// The vault holds at least one record, and `owner` owns every record.
async fn assert_records_owned_by(state: &AppState, owner: &VaultOwner) -> anyhow::Result<()> {
    let owners = record_owners(state).await?;
    anyhow::ensure!(
        !owners.is_empty() && owners.iter().all(|each| each == owner),
        "{owners:?}"
    );
    Ok(())
}

/// The Hub refusing a token is a `400`, rate-limiting a download or a dry run's
/// resolve a `429`, and a failed download a `502`. None stages anything, and
/// none holds the token.
#[rstest]
#[case::refused(
    GatedModel::Gguf,
    BAD_TOKEN,
    "",
    StatusCode::BAD_REQUEST,
    Some("403 Forbidden")
)]
#[case::rate_limited(
    GatedModel::RateLimitedGguf,
    GOOD_TOKEN,
    "",
    StatusCode::TOO_MANY_REQUESTS,
    Some("Retry after 55 seconds")
)]
#[case::rate_limited_dry_run(
    GatedModel::RateLimitedRepo,
    GOOD_TOKEN,
    "?dry_run=true",
    StatusCode::TOO_MANY_REQUESTS,
    Some("Retry after 55 seconds")
)]
#[case::failed(GatedModel::FailingGguf, GOOD_TOKEN, "", StatusCode::BAD_GATEWAY, None)]
#[tokio::test]
async fn test_a_hub_failure_answers_its_status_without_the_token(
    #[case] model: GatedModel,
    #[case] token: &str,
    #[case] query: &str,
    #[case] expected: StatusCode,
    #[case] cause: Option<&str>,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = gated_hub().await?;
    let (state, signer) = import_state(dir.path(), &hub).await?;
    let uri = format!(
        "{}{query}",
        own_campaign_plans_uri(&state, &signer.1).await?
    );

    let (status, answer) =
        post_document(&state, &signer, &uri, &gated_macos_document(model, token)).await?;
    assert_eq!(status, expected, "{answer}");
    assert!(!answer.to_string().contains("SECRET"), "{answer}");
    if let Some(cause) = cause {
        let error = json_str(&answer, "error")?;
        assert!(
            error.contains("(index 0)") && error.contains(cause),
            "{error}"
        );
    }
    assert_nothing_staged(&state).await
}

/// With every import permit held, a plan that downloads a model answers `429`
/// and downloads nothing. A plan with no token, and a plan that only reuses
/// records, take no permit, so they file.
#[tokio::test]
async fn test_an_import_past_the_permits_is_told_to_retry() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = gated_hub().await?;
    let (state, signer) = import_state(dir.path(), &hub).await?;
    let uri = own_campaign_plans_uri(&state, &signer.1).await?;
    let mlx = gated_macos_document(GatedModel::Mlx, GOOD_TOKEN);
    let (status, answer) = post_document(&state, &signer, &uri, &mlx).await?;
    assert_eq!(status, StatusCode::CREATED, "{answer}");
    let downloads = hub.downloads();

    let all = u32::try_from(state.hf_imports.permits.available_permits())?;
    let _busy = state.hf_imports.permits.try_acquire_many(all)?;

    let gguf = gated_macos_document(GatedModel::Gguf, GOOD_TOKEN);
    let (status, answer) = post_document(&state, &signer, &uri, &gguf).await?;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{answer}");
    assert_eq!(hub.downloads(), downloads);

    let (status, answer) = post_document(&state, &signer, &uri, &mlx).await?;
    assert_eq!(status, StatusCode::CREATED, "{answer}");
    assert_eq!(hub.downloads(), downloads);

    let (status, answer) = post_document(&state, &signer, &uri, ONE_CELL_PLAN).await?;
    assert_eq!(status, StatusCode::CREATED, "{answer}");
    Ok(())
}

/// An organization's campaign owns what a plan filed into it imports, also
/// when the filer holds no `manage_orgs`. The organization's own admin files it
/// too: the records fall inside its vault reach, so the check of the rewritten
/// bodies passes.
#[rstest]
#[case::server_admin(
    &[UserPermission::SubmitAnyPlans, UserPermission::ManageAnyOrgs],
    false
)]
#[case::org_admin(&[UserPermission::SubmitPlans, UserPermission::ManageOrgs], true)]
#[tokio::test]
async fn test_an_organization_campaign_owns_its_imports(
    #[case] grants: &[UserPermission],
    #[case] member: bool,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = gated_hub().await?;
    let (state, _signer) = import_state(dir.path(), &hub).await?;
    let org = seed_org(&state, "acme").await?;
    let operator = registered_user(&state, grants).await?;
    if member {
        set_user_org(&state, &operator.1, Some(org.org_id.clone())).await?;
    }
    let owner = CampaignOwner::Org {
        org_id: org.org_id.clone(),
    };
    let campaign = put_idle_campaign_of(&state, owner, "Gated").await?;

    let uri = format!("/api/v1/campaigns/{}/plans", campaign.campaign_id);
    let document = gated_macos_document(GatedModel::Mlx, GOOD_TOKEN);
    let (status, report) = post_document(&state, &operator, &uri, &document).await?;
    assert_eq!(status, StatusCode::CREATED, "{report}");

    let model = stored_model(&state, &report).await?;
    assert_eq!(model["principal_id"], org.org_id.as_str(), "{model}");
    let acme = VaultOwner::Org {
        org_id: org.org_id.clone(),
    };
    assert_records_owned_by(&state, &acme).await
}

/// A campaign that changes its owner during the import refuses the plan with
/// `409`, and stays on its plan.
#[tokio::test]
async fn test_a_campaign_handed_off_during_an_import_refuses_the_plan() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = gated_hub().await?;
    let (state, _signer) = import_state(dir.path(), &hub).await?;
    let org = seed_org(&state, "acme").await?;
    let operator = registered_user(
        &state,
        &[
            UserPermission::SubmitAnyPlans,
            UserPermission::ManageAnyOrgs,
        ],
    )
    .await?;
    let owner = CampaignOwner::Org {
        org_id: org.org_id.clone(),
    };
    let campaign = put_idle_campaign_of(&state, owner, "Gated").await?;
    let handed = Campaign {
        owner: CampaignOwner::User {
            user_id: operator.1.clone(),
        },
        ..campaign.clone()
    };

    let uri = format!("/api/v1/campaigns/{}/plans", campaign.campaign_id);
    let document = gated_macos_document(GatedModel::Mlx, GOOD_TOKEN);
    let held = hub.hold().await;
    let hand_off = async {
        hub.arrived().await;
        state.campaign_store.put_campaign(&handed).await?;
        drop(held);
        anyhow::Ok(())
    };
    let ((status, answer), ()) =
        tokio::try_join!(post_document(&state, &operator, &uri, &document), hand_off)?;
    assert_eq!(status, StatusCode::CONFLICT, "{answer}");

    let stored = state
        .campaign_store
        .get_campaign(&campaign.campaign_id)
        .await?
        .context("the campaign exists")?;
    assert_eq!(stored.current_plan_id, None);
    assert_nothing_staged(&state).await?;
    Ok(())
}

/// Two documents that name one commit with different tokens rewrite to the same
/// vault records, so they merge onto one job.
#[tokio::test]
async fn test_two_tokens_for_one_commit_merge_onto_one_job() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = gated_hub().await?;
    let (state, signer) = import_state(dir.path(), &hub).await?;
    let uri = own_campaign_plans_uri(&state, &signer.1).await?;

    let document = gated_macos_document(GatedModel::Gguf, GOOD_TOKEN);
    let (status, first) = post_document(&state, &signer, &uri, &document).await?;
    assert_eq!(status, StatusCode::CREATED, "{first}");
    let document = gated_macos_document(GatedModel::Gguf, OTHER_GOOD_TOKEN);
    let (status, second) = post_document(&state, &signer, &uri, &document).await?;
    assert_eq!(status, StatusCode::CREATED, "{second}");

    assert_eq!(second["merged_job_count"], 1, "{second}");
    assert_eq!(
        json_str(&second["job_ids"], "0")?,
        json_str(&first["job_ids"], "0")?
    );
    Ok(())
}

/// Two users who file one gated commit import it each for themselves, so
/// their jobs stay apart: neither user can read the other's job source.
#[tokio::test]
async fn test_two_owners_of_one_gated_commit_do_not_merge() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = gated_hub().await?;
    let (state, alice) = import_state(dir.path(), &hub).await?;
    let bob = registered_user(
        &state,
        &[UserPermission::SubmitPlans, UserPermission::ReadPlans],
    )
    .await?;

    let state = &state;
    let job_ids: Vec<String> = futures::stream::iter([&alice, &bob].map(anyhow::Ok))
        .and_then(|signer| async move {
            let uri = own_campaign_plans_uri(state, &signer.1).await?;
            let document = gated_macos_document(GatedModel::Gguf, GOOD_TOKEN);
            let (status, report) = post_document(state, signer, &uri, &document).await?;
            assert_eq!(status, StatusCode::CREATED, "{report}");
            assert_eq!(report["merged_job_count"], 0, "{report}");
            let model = stored_model(state, &report).await?;
            assert_eq!(model["principal_id"], signer.1.as_str(), "{model}");
            anyhow::Ok(json_str(&report["job_ids"], "0")?.to_owned())
        })
        .try_collect()
        .await?;
    assert_ne!(job_ids[0], job_ids[1]);
    Ok(())
}

// ── the progress stream (`Accept: application/x-ndjson`) ────────

/// With the header, a gated import answers `200`. The body has `progress` lines
/// and then one `result` line. The `result` line carries the `201` report. The
/// same document without the header answers the plain `201`.
#[tokio::test]
async fn test_a_streamed_import_ends_with_the_result() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = gated_hub().await?;
    let (state, signer) = import_state(dir.path(), &hub).await?;
    let uri = own_campaign_plans_uri(&state, &signer.1).await?;
    let document = gated_macos_document(GatedModel::Gguf, GOOD_TOKEN);

    let response = stream_plan_document(&state, &signer, "POST", &uri, &document).await?;
    let lines = stream_lines(response).await?;
    let (last, progress) = lines.split_last().context("the stream has a line")?;
    assert!(
        !progress.is_empty() && progress.iter().all(|line| line["event"] == "progress"),
        "{lines:?}"
    );
    assert_eq!(last["event"], "result", "{last}");
    assert_eq!(last["status"], 201, "{last}");
    let model = stored_model(&state, &last["body"]).await?;
    assert_eq!(model["source"], "vault_url", "{model}");

    let (status, report) = post_document(&state, &signer, &uri, &document).await?;
    assert_eq!(status, StatusCode::CREATED, "{report}");
    Ok(())
}

/// A browser asks for gzip. The gzipped stream ends with the `result` line.
#[tokio::test]
async fn test_a_gzipped_stream_ends_with_the_result() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let (key, user_id, key_id) = registered_user(&state, &[UserPermission::SubmitPlans]).await?;
    let uri = "/api/v1/campaigns?name=Gzipped";
    let mut headers = user_auth_headers(&key, &user_id, &key_id, "POST", uri)?;
    headers.push(("Accept", "application/x-ndjson".to_owned()));
    headers.push(("Accept-Encoding", "gzip".to_owned()));

    let response = raw_request(
        &state,
        "POST",
        uri,
        &headers,
        Some("application/toml"),
        ONE_CELL_PLAN,
    )
    .await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-encoding"),
        Some(&axum::http::HeaderValue::from_static("gzip"))
    );
    let zipped = response.into_body().collect().await?.to_bytes();
    let mut unzipped = String::new();
    flate2::read::GzDecoder::new(&zipped[..]).read_to_string(&mut unzipped)?;
    let last: Value =
        serde_json::from_str(unzipped.lines().last().context("the stream has a line")?)?;
    assert_eq!(last["event"], "result", "{last}");
    assert_eq!(last["status"], 201, "{last}");
    Ok(())
}

/// Each `progress` line names the source of its file as `org/repo_name@commit`.
/// The test checks this for each model type. For a speculative model, the
/// lines of the target and of the drafter name different repositories.
#[rstest]
#[case::gguf_text(GatedModel::Gguf, &[("LiquidAI/LFM2-700M-GGUF", GATED_GGUF_FILE)])]
#[case::gguf_vision(
    GatedModel::Vision,
    &[("LiquidAI/LFM2-VL-450M-GGUF", VISION_MODEL_FILE), ("LiquidAI/LFM2-VL-450M-GGUF", VISION_MMPROJ_FILE)]
)]
#[case::mlx_speculative(
    GatedModel::Speculative,
    &[
        ("LiquidAI/LFM2.5-1.2B-MLX-4bit", "config.json"),
        ("LiquidAI/LFM2.5-1.2B-MLX-4bit", "model.safetensors"),
        ("LiquidAI/LFM2.5-350M-MLX-draft", "config.json"),
        ("LiquidAI/LFM2.5-350M-MLX-draft", "model.safetensors"),
    ]
)]
#[tokio::test]
async fn test_a_progress_line_names_its_source_at_its_commit(
    #[case] model: GatedModel,
    #[case] files: &[(&str, &str)],
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = gated_hub().await?;
    let (state, signer) = import_state(dir.path(), &hub).await?;
    let uri = own_campaign_plans_uri(&state, &signer.1).await?;
    let document = gated_macos_document(model, GOOD_TOKEN);

    let response = stream_plan_document(&state, &signer, "POST", &uri, &document).await?;
    let lines = stream_lines(response).await?;
    let field = |line: &Value, name: &str| line[name].as_str().unwrap_or_default().to_owned();
    let seen: BTreeSet<_> = lines
        .iter()
        .filter(|line| line["event"] == "progress")
        .map(|line| (field(line, "hf_ref"), field(line, "file")))
        .collect();
    let expected: BTreeSet<_> = files
        .iter()
        .map(|&(repo, file)| Some((hub.reference(repo)?, file.to_owned())))
        .collect::<Option<_>>()
        .context("the hub holds each repository")?;
    assert_eq!(seen, expected, "{lines:?}");
    Ok(())
}

/// A streamed failure ends with one `error` line. The line holds the status
/// and the message of the plain answer to the same request.
#[rstest]
#[case::refused(BAD_TOKEN, false, StatusCode::BAD_REQUEST)]
#[case::busy(GOOD_TOKEN, true, StatusCode::TOO_MANY_REQUESTS)]
#[tokio::test]
async fn test_a_streamed_failure_ends_with_the_plain_error(
    #[case] token: &'static str,
    #[case] hold_permits: bool,
    #[case] expected: StatusCode,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = gated_hub().await?;
    let (state, signer) = import_state(dir.path(), &hub).await?;
    let uri = own_campaign_plans_uri(&state, &signer.1).await?;
    let document = gated_macos_document(GatedModel::Gguf, token);
    let all = u32::try_from(state.hf_imports.permits.available_permits())?;
    let _held = hold_permits
        .then(|| state.hf_imports.permits.try_acquire_many(all))
        .transpose()?;

    let (status, plain) = post_document(&state, &signer, &uri, &document).await?;
    assert_eq!(status, expected, "{plain}");
    let response = stream_plan_document(&state, &signer, "POST", &uri, &document).await?;
    let lines = stream_lines(response).await?;
    let (last, before) = lines.split_last().context("the stream has a line")?;
    assert_eq!(
        last,
        &json!({"event": "error", "status": status.as_u16(), "error": plain["error"]}),
        "{lines:?}"
    );
    assert!(
        before.iter().all(|line| line["event"] == "progress"),
        "{lines:?}"
    );
    assert_nothing_staged(&state).await
}

/// With the header, a dry run of a document answers `200` with one `result`
/// line. The line carries the status `200` and the dry-run report.
#[tokio::test]
async fn test_a_streamed_dry_run_ends_with_its_report() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, signer) = document_state(dir.path()).await?;
    let uri = format!(
        "{}?dry_run=true",
        own_campaign_plans_uri(&state, &signer.1).await?
    );

    let response = stream_plan_document(&state, &signer, "POST", &uri, AFM_MLX).await?;
    let lines = stream_lines(response).await?;
    let [line] = lines.as_slice() else {
        anyhow::bail!("the stream has one line: {lines:?}");
    };
    assert_eq!(line["event"], "result", "{line}");
    assert_eq!(line["status"], 200, "{line}");
    assert_eq!(
        line["body"]["bodies"].as_array().map(Vec::len),
        Some(3),
        "{line}"
    );
    assert_nothing_staged(&state).await?;
    assert_eq!(stored_sources(dir.path())?, 0);
    Ok(())
}

/// A caller that drops the stream during an import gets no plan. The import
/// keeps each blob that it stored completely. A retry does not download that
/// blob again.
#[tokio::test]
async fn test_a_dropped_stream_files_no_plan_and_keeps_stored_blobs() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let hub = gated_hub().await?;
    let (state, signer) = import_state(dir.path(), &hub).await?;
    let uri = own_campaign_plans_uri(&state, &signer.1).await?;
    let document = gated_macos_document(GatedModel::Vision, GOOD_TOKEN);
    let permits = state.hf_imports.permits.available_permits();

    // The import stores the model file, then waits at the projector file.
    let held = hub.hold_files().await;
    let response = stream_plan_document(&state, &signer, "POST", &uri, &document).await?;
    assert_eq!(response.status(), StatusCode::OK);
    hub.file_arrived().await;
    drop(response);
    // The import gives its permit back when the dropped ingest stops.
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while state.hf_imports.permits.available_permits() < permits {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await?;
    drop(held);

    assert_nothing_staged(&state).await?;
    assert!(record_owners(&state).await?.is_empty());
    let owner = VaultOwner::User {
        user_id: signer.1.clone(),
    };
    let sha256 = Sha256::try_new(MockFile::lfs(VISION_MODEL_FILE, VISION_MODEL_BYTES).sha256())?;
    assert!(
        vault_admin::blob_size(state.vault_store.as_ref(), &owner, &sha256)
            .await?
            .is_some()
    );

    let (status, report) = post_document(&state, &signer, &uri, &document).await?;
    assert_eq!(status, StatusCode::CREATED, "{report}");
    assert_eq!(hub.downloads_of(VISION_MODEL_FILE), 1);
    Ok(())
}
