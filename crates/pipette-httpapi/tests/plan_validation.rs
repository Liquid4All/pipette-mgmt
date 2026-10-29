//! `POST /api/v1/plans/validate`: a plan document checked as filing checks it,
//! answered with a verdict, and nothing filed.

use std::num::NonZeroUsize;

use anyhow::Context;
use axum::http::StatusCode;
use futures::TryStreamExt;
use rstest::rstest;
use serde_json::{Value, json};

use pipette_httpapi::state::AppState;
use pipette_plan_types::{UserPermission, UserStatus};
use pipette_store_testkit::an_org_benchmark;
use pipette_web_testkit::{
    UserSigner, assert_nothing_staged, auth_headers, body_json, json_str,
    make_state_with_plan_body_limit, make_state_with_plan_max_jobs, post_plan_document,
    raw_request, register_and_approve, registered_user, seed_org, seeded_state, set_user_org,
    set_user_status, setup_benchmarks, setup_scheduler_benchmarks, upload_gguf_text,
    user_auth_headers,
};

const URI: &str = "/api/v1/plans/validate";

/// The upstream example, which expands to three jobs.
const AFM_MLX: &str = include_str!("../../../examples/plans/scheduler/afm-mlx.toml");

/// The id [`an_org_benchmark`] carries.
const PRIVATE: &str = "prefill_throughput_acme_4096";

/// A resource id no record holds.
const NO_SUCH_RESOURCE: &str = "vault_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e";

/// The verdict the route answers, which must be `200`.
async fn validate(state: &AppState, signer: &UserSigner, text: &str) -> anyhow::Result<Value> {
    let resp = post_plan_document(state, signer, URI, text).await?;
    anyhow::ensure!(
        resp.status() == StatusCode::OK,
        "validating answered {}",
        resp.status()
    );
    let verdict = body_json(resp).await?;
    crate::api_schema::assert_conforms("PlanValidation", &verdict)?;
    Ok(verdict)
}

fn errors(verdict: &Value) -> anyhow::Result<Vec<&Value>> {
    Ok(verdict
        .pointer("/issues")
        .and_then(Value::as_array)
        .context("a verdict lists its issues")?
        .iter()
        .filter(|issue| issue.pointer("/severity") == Some(&json!("error")))
        .collect())
}

fn error_message(verdict: &Value) -> anyhow::Result<String> {
    let errors = errors(verdict)?;
    let [error] = errors.as_slice() else {
        anyhow::bail!("not one error: {verdict}");
    };
    Ok(json_str(error, "message")?.to_string())
}

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

/// A one-job document whose model is the inline TOML table `model`.
fn model_document(benchmark: &str, model: &str) -> String {
    format!(
        r#"benchmarks = ["{benchmark}"]

[[variants]]
clients  = ["ev1_anyone"]
models   = [{model}]
runtimes = [{{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", repository_version = "b5000", flavor = "macos-arm64" }}]
"#
    )
}

const PUBLIC_MODEL: &str = r#"{ type = "gguf_text", source = "huggingface", org = "LiquidAI", repo_name = "LFM2-700M-GGUF", path = "LFM2-700M-Q4_0.gguf" }"#;

/// A model fetched from the vault resource `resource_id` on behalf of
/// `principal`.
fn vault_model(resource_id: &str, principal: &str) -> String {
    format!(
        "{{ type = \"gguf_text\", source = \"vault_url\", url = \"/vault/resources/{resource_id}/content\", principal_id = \"{principal}\" }}"
    )
}

/// The objects in the two plan-source keyspaces.
fn stored_sources(dir: &std::path::Path) -> anyhow::Result<usize> {
    ["plan-sources", "plan-sources-pending"]
        .iter()
        .map(|keyspace| Ok(std::fs::read_dir(dir.join(keyspace))?.count()))
        .sum()
}

/// A document that expands answers its job count and no issue, carries no job
/// body, and leaves the store as it was.
#[tokio::test]
async fn a_valid_document_answers_its_job_count_and_files_nothing() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_scheduler_benchmarks(dir.path())?;
    let state = seeded_state(dir.path()).await?;
    let signer = registered_user(&state, &[UserPermission::SubmitPlans]).await?;

    let verdict = validate(&state, &signer, AFM_MLX).await?;

    assert_eq!(
        verdict,
        json!({ "job_count": 3, "gated_model_count": 0, "issues": [] })
    );
    assert_nothing_staged(&state).await?;
    assert_eq!(stored_sources(dir.path())?, 0);
    assert_eq!(
        pipette_store_testkit::local_fs_job_record_count(dir.path()),
        0
    );
    Ok(())
}

/// A document that is not TOML answers `200` with one error, placed at a line
/// and a column, and no job count.
#[tokio::test]
async fn invalid_toml_answers_a_placed_error() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let signer = registered_user(&state, &[UserPermission::SubmitPlans]).await?;

    let verdict = validate(
        &state,
        &signer,
        &mlx_document("requires = [\"os:macos\" \"x\"]"),
    )
    .await?;

    let errors = errors(&verdict)?;
    let [error] = errors.as_slice() else {
        anyhow::bail!("not one error: {verdict}");
    };
    assert_eq!(
        (
            verdict.pointer("/job_count"),
            verdict.pointer("/gated_model_count"),
            error.pointer("/line"),
            error.pointer("/column").map(Value::is_u64),
        ),
        (
            Some(&Value::Null),
            Some(&Value::Null),
            Some(&json!(4)),
            Some(true)
        )
    );
    Ok(())
}

/// Signed headers for a `POST` to [`URI`] by a new account holding
/// `submit_plans`.
async fn submitter_headers(state: &AppState) -> anyhow::Result<Vec<(&'static str, String)>> {
    let (key, user_id, key_id) = registered_user(state, &[UserPermission::SubmitPlans]).await?;
    Ok(user_auth_headers(&key, &user_id, &key_id, "POST", URI)?)
}

/// The route's ceiling in the gate tests, so a body over it costs little.
const TEST_PLAN_BODY_LIMIT: usize = 256 * 1024;

/// Who sends what in a refused request.
#[derive(Debug, Clone, Copy)]
enum Refused {
    Unsigned,
    Device,
    PendingUser,
    /// An account without `submit_plans`, sending a body over the ceiling.
    UngrantedUser,
    Json,
    NotUtf8,
}

/// A caller who cannot file is refused before the body is read, an oversized one
/// included; then the body must be UTF-8 TOML.
#[rstest]
#[case::unsigned(
    Refused::Unsigned,
    StatusCode::UNAUTHORIZED,
    "missing X-Client-Id header"
)]
#[case::device(
    Refused::Device,
    StatusCode::FORBIDDEN,
    "a device cannot check a plan; sign the request as a user account"
)]
#[case::pending_user(
    Refused::PendingUser,
    StatusCode::FORBIDDEN,
    "user is pending an operator's approval"
)]
#[case::ungranted_user(
    Refused::UngrantedUser,
    StatusCode::FORBIDDEN,
    "user lacks the submit_plans permission"
)]
#[case::json(
    Refused::Json,
    StatusCode::UNSUPPORTED_MEDIA_TYPE,
    "send the plan as an application/toml plan document"
)]
#[case::not_utf8(Refused::NotUtf8, StatusCode::BAD_REQUEST, "UTF-8")]
#[tokio::test]
async fn the_gate_refuses_before_the_check(
    #[case] refused: Refused,
    #[case] status: StatusCode,
    #[case] error: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let limit = NonZeroUsize::new(TEST_PLAN_BODY_LIMIT).context("limit is non-zero")?;
    let state = make_state_with_plan_body_limit(dir.path(), limit).await?;
    let toml = Some("application/toml");

    let resp = match refused {
        Refused::Unsigned => raw_request(&state, "POST", URI, &[], toml, AFM_MLX).await?,
        Refused::Device => {
            let (key, client_id) = register_and_approve(&state).await?;
            let headers = auth_headers(&key, &client_id, "POST", URI)?;
            raw_request(&state, "POST", URI, &headers, toml, AFM_MLX).await?
        }
        Refused::PendingUser => {
            let signer = registered_user(&state, &[UserPermission::SubmitPlans]).await?;
            set_user_status(&state, &signer.1, UserStatus::Pending).await?;
            post_plan_document(&state, &signer, URI, AFM_MLX).await?
        }
        Refused::UngrantedUser => {
            let signer = registered_user(&state, &[]).await?;
            let oversized = format!("# {}\n", "x".repeat(TEST_PLAN_BODY_LIMIT));
            post_plan_document(&state, &signer, URI, &oversized).await?
        }
        Refused::Json => {
            let headers = submitter_headers(&state).await?;
            let json = Some("application/json");
            raw_request(&state, "POST", URI, &headers, json, AFM_MLX).await?
        }
        Refused::NotUtf8 => {
            let headers = submitter_headers(&state).await?;
            raw_request(&state, "POST", URI, &headers, toml, vec![0xff_u8, 0xfe]).await?
        }
    };

    assert_eq!(resp.status(), status, "{refused:?}");
    let message = json_str(&body_json(resp).await?, "error")?.to_string();
    assert!(message.contains(error), "{refused:?}: {message}");
    assert_nothing_staged(&state).await?;
    Ok(())
}

/// `owner` validates `document(hidden)`, and `outsider` is refused it with the
/// message for `absent`, an id no record holds.
async fn assert_hidden_reads_as_absent(
    state: &AppState,
    owner: &UserSigner,
    outsider: &UserSigner,
    document: impl Fn(&str) -> String,
    hidden: &str,
    absent: &str,
) -> anyhow::Result<()> {
    let own = validate(state, owner, &document(hidden)).await?;
    assert_eq!(errors(&own)?.len(), 0, "{own}");

    let refused = validate(state, outsider, &document(hidden)).await?;
    let unknown = validate(state, outsider, &document(absent)).await?;
    assert_eq!(
        error_message(&refused)?,
        error_message(&unknown)?.replace(absent, hidden)
    );
    Ok(())
}

/// Another organization's private benchmark reads as one no catalog holds,
/// "did you mean" included, while a member's own validates.
#[tokio::test]
async fn another_organization_s_private_benchmark_reads_as_unknown() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let acme = seed_org(&state, "acme").await?.org_id;
    let labs = seed_org(&state, "labs").await?.org_id;
    state
        .catalog_store
        .put_org_benchmark(&acme, &an_org_benchmark()?)
        .await?;
    let member = registered_user(&state, &[UserPermission::SubmitPlans]).await?;
    set_user_org(&state, &member.1, Some(acme)).await?;
    let outsider = registered_user(&state, &[UserPermission::SubmitPlans]).await?;
    set_user_org(&state, &outsider.1, Some(labs)).await?;

    assert_hidden_reads_as_absent(
        &state,
        &member,
        &outsider,
        |id| model_document(id, PUBLIC_MODEL),
        PRIVATE,
        "prefill_throughput_nobody_keeps",
    )
    .await
}

/// A vault resource outside the caller's reach reads as one that does not
/// exist, while its owner's validates.
#[tokio::test]
async fn a_vault_resource_out_of_reach_reads_as_unknown() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = seeded_state(dir.path()).await?;
    let owner = registered_user(&state, &[UserPermission::SubmitPlans]).await?;
    let resource_id =
        upload_gguf_text(&state, &owner, None, pipette_web_testkit::VAULT_GGUF).await?;
    let outsider = registered_user(&state, &[UserPermission::SubmitPlans]).await?;
    let principal = owner.1.to_string();

    assert_hidden_reads_as_absent(
        &state,
        &owner,
        &outsider,
        |id| model_document("eval_test", &vault_model(id, &principal)),
        &resource_id,
        NO_SUCH_RESOURCE,
    )
    .await
}

/// The job ceiling of the parity cases: the "over the ceiling" document
/// expands past it, and every other one fits.
const PARITY_MAX_JOBS: usize = 3;

/// A token value in a variant's environment.
const HF_TOKEN_ENV: &str = r#"benchmarks = ["prefill_throughput_256"]

[[variants]]
requires = ["os:macos"]
models   = [{ type = "gguf_text", source = "huggingface", org = "o", repo_name = "r", path = "a.gguf" }]
runtimes = [{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b9050", flavor = "macos-arm64" }]
runtime_flags = [{ benchmark_type = "prefill_throughput", runtime_type = "llamacpp_cli_stock_tools", model_type = "gguf_text", envs = ["HF_TOKEN=hf_SECRET"] }]
"#;

/// Four cells, one past [`PARITY_MAX_JOBS`].
const OVER_THE_CEILING: &str = r#"benchmarks = ["prefill_throughput_256", "max_memory_usage_256"]

[[variants]]
requires = ["os:macos"]
models   = [
  { type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "LFM2.5-350M-MLX-4bit" },
  { type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "LFM2-700M-MLX-4bit" },
]
runtimes = [{ type = "mlx_macos_pipette", version = "0.31.3", flavor = "macos-arm64", source = { type = "pip_requirements_text", contents = "mlx-lm==0.31.3\n" } }]
"#;

/// A document is an error exactly when a dry run of it is a `400`, and the
/// error's message is part of the `400`'s.
#[rstest]
#[case::valid(mlx_document("requires = [\"os:macos\"]"), false)]
#[case::bad_toml("benchmarks = [".to_string(), true)]
#[case::unknown_key(mlx_document("requires = [\"os:macos\"]\nmodelz = 1"), true)]
#[case::missing_variants("benchmarks = [\"decode_throughput_512_100\"]\n".to_string(), true)]
#[case::no_eligibility(mlx_document(""), true)]
#[case::capability_rule(
    mlx_document("requires = [\"os:macos\"]\nnot_all_of = [[\"chip:applem1\", \"chip:applem2\"]]"),
    true
)]
#[case::hf_token_env(HF_TOKEN_ENV.to_string(), true)]
#[case::token_in_a_comment(
    format!("# auth_token = \"hf_SECRET\"\n{}", mlx_document("requires = [\"os:macos\"]")),
    true
)]
#[case::over_the_ceiling(OVER_THE_CEILING.to_string(), true)]
#[case::unknown_benchmark(
    mlx_document("requires = [\"os:macos\"]")
        .replace("decode_throughput_512_100", "decode_throughput_512_10"),
    true
)]
#[case::unknown_vault_resource(
    model_document(
        "eval_test",
        &vault_model(NO_SUCH_RESOURCE, "user_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e")
    ),
    true
)]
#[tokio::test]
async fn a_verdict_agrees_with_a_dry_run(
    #[case] document: String,
    #[case] refused: bool,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_scheduler_benchmarks(dir.path())?;
    setup_benchmarks(dir.path())?;
    let limit = NonZeroUsize::new(PARITY_MAX_JOBS).context("limit is non-zero")?;
    let state = make_state_with_plan_max_jobs(dir.path(), limit).await?;
    let signer = registered_user(&state, &[UserPermission::SubmitPlans]).await?;

    let verdict = validate(&state, &signer, &document).await?;
    let dry_run = "/api/v1/campaigns?name=parity&dry_run=true";
    let dry = post_plan_document(&state, &signer, dry_run, &document).await?;
    let dry_status = dry.status();
    let dry_body = body_json(dry).await?;

    let errors = errors(&verdict)?;
    assert_eq!(errors.is_empty(), !refused, "{verdict}");
    match errors.as_slice() {
        [] => assert_eq!(dry_status, StatusCode::OK, "{dry_body}"),
        [error] => {
            assert_eq!(dry_status, StatusCode::BAD_REQUEST, "{verdict}");
            let refusal = json_str(&dry_body, "error")?;
            let message = json_str(error, "message")?;
            assert!(refusal.contains(message), "{message:?} not in {refusal:?}");
        }
        more => anyhow::bail!("more than one error: {more:?}"),
    }
    Ok(())
}

/// Every example plan the app offers validates against the example catalog.
#[tokio::test]
async fn every_app_example_validates() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let catalog = dir.path().join("benchmarks");
    std::fs::create_dir_all(&catalog)?;
    std::fs::read_dir(root.join("examples/benchmarks"))?.try_for_each(
        |entry| -> anyhow::Result<()> {
            let path = entry?.path();
            if path.extension().is_some_and(|ext| ext == "toml") {
                std::fs::copy(
                    &path,
                    catalog.join(path.file_name().context("a file name")?),
                )?;
            }
            Ok(())
        },
    )?;
    let state = pipette_web_testkit::make_state(dir.path()).await?;
    let signer = registered_user(&state, &[UserPermission::SubmitPlans]).await?;

    let mut examples: Vec<std::path::PathBuf> =
        std::fs::read_dir(root.join("ui/src/lib/plan-examples"))?
            .map(|entry| Ok(entry?.path()))
            .collect::<anyhow::Result<_>>()?;
    examples.retain(|path| path.extension().is_some_and(|ext| ext == "toml"));
    examples.sort();
    anyhow::ensure!(!examples.is_empty(), "no example plans");

    let (state, signer) = (&state, &signer);
    futures::stream::iter(examples.into_iter().map(Ok::<_, anyhow::Error>))
        .try_for_each(|path| async move {
            let verdict = validate(state, signer, &std::fs::read_to_string(&path)?).await?;
            let issues = verdict.pointer("/issues").and_then(Value::as_array);
            let jobs = verdict.pointer("/job_count").and_then(Value::as_u64);
            assert!(
                issues.is_some_and(Vec::is_empty) && jobs.is_some_and(|jobs| jobs > 0),
                "{}: {verdict}",
                path.display()
            );
            Ok(())
        })
        .await
}
