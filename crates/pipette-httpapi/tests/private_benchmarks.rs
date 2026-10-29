//! A benchmark an organization keeps to itself, run as work: the plan that
//! names it, the submissions that report it, and the passes that score and
//! settle it. What reads its *definition* is `benchmarks.rs` and
//! `org_catalog.rs`.

use std::collections::BTreeMap;

use anyhow::Context;
use axum::http::StatusCode;
use chrono::{TimeZone, Utc};
use ed25519_dalek::SigningKey;
use rstest::rstest;
use serde_json::{Value, json};
use tempfile::TempDir;

use pipette_core::client::ClientStatus;
use pipette_core::user_permission::UserPermissionGroup;
use pipette_httpapi::state::AppState;
use pipette_plan_types::{
    BenchmarkDefinition, BenchmarkId, BenchmarkType, ClientId, ExpiresAt, JobId, OrgId, PlanId,
    UserPermission,
};
use pipette_store::JobState;
use pipette_store_testkit::an_org_benchmark;
use pipette_web_testkit::{
    authed_get, authed_post, benchmark_ids, body_json, denied_clients, job, json_str,
    make_state_with_unverified, own_client_by_org, post_plan_document, put_idle_campaign,
    register_and_approve, register_pending, registered_user, run_full_score, run_qm, seed_avail,
    seed_leased, seed_org, set_user_org, setup_benchmarks, signed_empty, signed_json, unauthed_get,
    valid_submission,
};

/// The id [`an_org_benchmark`] carries: a prefill entry, so scoring needs no
/// evals server.
const PRIVATE: &str = "prefill_throughput_acme_4096";
const CLAIMED: &str = "0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e";

/// The public example catalog, `acme` keeping [`PRIVATE`], and `labs`
/// keeping nothing.
struct World {
    dir: TempDir,
    state: AppState,
    acme: OrgId,
    labs: OrgId,
}

async fn world() -> anyhow::Result<World> {
    world_holding(false).await
}

/// [`world`] with `[unverified_submissions] enabled = holding`, so a pending
/// device's results are held rather than refused.
async fn world_holding(holding: bool) -> anyhow::Result<World> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state_with_unverified(dir.path(), holding).await?;
    let acme = seed_org(&state, "acme").await?.org_id;
    let labs = seed_org(&state, "labs").await?.org_id;
    state
        .catalog_store
        .put_org_benchmark(&acme, &an_org_benchmark()?)
        .await?;
    Ok(World {
        dir,
        state,
        acme,
        labs,
    })
}

/// Who a device belongs to, relative to `acme`.
#[derive(Clone, Copy, Debug)]
enum Owner {
    Acme,
    OtherOrg,
    Nobody,
}

impl World {
    fn org(&self, owner: Owner) -> Option<OrgId> {
        match owner {
            Owner::Acme => Some(self.acme.clone()),
            Owner::OtherOrg => Some(self.labs.clone()),
            Owner::Nobody => None,
        }
    }
}

async fn device(world: &World, owner: Owner) -> anyhow::Result<(SigningKey, ClientId)> {
    let (key, client_id) = register_and_approve(&world.state).await?;
    if let Some(org_id) = world.org(owner) {
        own_client_by_org(&world.state, &client_id, &org_id).await?;
    }
    Ok((key, client_id))
}

fn spec(benchmark_id: &str) -> Value {
    json!({
        "benchmark": benchmark_id,
        "model": {"type": "gguf_text", "source": "huggingface", "org": "LiquidAI", "repo_name": "LFM2-700M-GGUF", "path": "LFM2-700M-Q4_0.gguf"},
        "runtime": {"type": "llamacpp_cli_stock_tools", "source": "github_release", "repository_url": "github.com/ggml-org/llama.cpp", "repository_version": "b5000", "flavor": "macos-arm64"},
    })
}

/// A plan document of one job that runs `benchmark_id`.
fn document(benchmark_id: &str) -> String {
    format!(
        r#"benchmarks = ["{benchmark_id}"]

[[variants]]
clients  = ["ev1_anyone"]
models   = [{{ type = "gguf_text", source = "huggingface", org = "LiquidAI", repo_name = "LFM2-700M-GGUF", path = "LFM2-700M-Q4_0.gguf" }}]
runtimes = [{{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", repository_version = "b5000", flavor = "macos-arm64" }}]
"#
    )
}

/// Plant a live lease for `client_id` on a job that runs `benchmark_id`.
fn lease(world: &World, client_id: &ClientId, benchmark_id: &str) -> anyhow::Result<JobId> {
    let body = json!({
        "job_id": CLAIMED,
        "expires_at": "never",
        "clients": [client_id.as_str(), "ev1_member"],
        "spec": spec(benchmark_id),
    });
    seed_leased(world.dir.path(), CLAIMED, client_id, &body)
}

fn failure(retriable: bool) -> Value {
    json!({
        "message_type": "failure",
        "job_id": CLAIMED,
        "benchmark_id": PRIVATE,
        "retriable": retriable,
        "failure_reason": "GET /benchmarks/prefill_throughput_acme_4096: 404",
    })
}

#[derive(Clone, Copy, Debug)]
enum Route {
    Single,
    Batch,
}

/// Submit `body` on `route`: `None` when accepted, else the error it drew. A
/// batch carries it as the one item; every refusal expected here is a `404`.
async fn submit(
    state: &AppState,
    key: &SigningKey,
    client_id: &ClientId,
    route: Route,
    body: Value,
) -> anyhow::Result<Option<String>> {
    match route {
        Route::Single => {
            let resp = authed_post(state, key, client_id, "/benchmarks", &body).await?;
            if resp.status() == StatusCode::ACCEPTED {
                return Ok(None);
            }
            anyhow::ensure!(
                resp.status() == StatusCode::NOT_FOUND,
                "refused with {}",
                resp.status()
            );
            let error = body_json(resp).await?;
            Ok(Some(json_str(&error, "error")?.to_string()))
        }
        Route::Batch => {
            let resp = authed_post(
                state,
                key,
                client_id,
                "/benchmarks/batch",
                &json!({"submissions": [body]}),
            )
            .await?;
            assert_eq!(resp.status(), StatusCode::OK);
            let results = body_json(resp).await?;
            let item = results
                .get("results")
                .and_then(|r| r.get(0))
                .context("one result per item")?;
            Ok(item
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_string))
        }
    }
}

fn not_found() -> Option<String> {
    Some("benchmark not found".into())
}

/// A device of the organization reports a private result, as it would a
/// public one.
#[rstest]
#[tokio::test]
async fn a_member_device_reports_a_private_result(
    #[values(Route::Single, Route::Batch)] route: Route,
) -> anyhow::Result<()> {
    let world = world().await?;
    let (key, client_id) = device(&world, Owner::Acme).await?;

    let refused = submit(
        &world.state,
        &key,
        &client_id,
        route,
        valid_submission(PRIVATE),
    )
    .await?;
    assert_eq!(refused, None);
    Ok(())
}

/// A device outside the organization is refused the way an unknown id is, so
/// the answer says nothing about whether the id exists.
#[rstest]
#[tokio::test]
async fn an_outsider_cannot_report_a_private_result(
    #[values(Route::Single, Route::Batch)] route: Route,
    #[values(Owner::OtherOrg, Owner::Nobody)] owner: Owner,
) -> anyhow::Result<()> {
    let world = world().await?;
    let (key, client_id) = device(&world, owner).await?;

    let refused = submit(
        &world.state,
        &key,
        &client_id,
        route,
        valid_submission(PRIVATE),
    )
    .await?;
    assert_eq!(refused, not_found());
    let unknown = submit(
        &world.state,
        &key,
        &client_id,
        route,
        valid_submission("prefill_throughput_nobody_keeps"),
    )
    .await?;
    assert_eq!(unknown, refused, "a private id reads as an unknown one");
    Ok(())
}

/// A pending device is served no organization's entries, its owner's included:
/// its private result is the unknown-id `404` and is not held, while a public
/// one is held, and approval alone lets the private one through.
#[rstest]
#[tokio::test]
async fn a_pending_member_device_cannot_report_a_private_result(
    #[values(Route::Single, Route::Batch)] route: Route,
) -> anyhow::Result<()> {
    let world = world_holding(true).await?;
    let state = &world.state;
    let (key, client_id) = register_pending(state).await?;
    own_client_by_org(state, &client_id, &world.acme).await?;
    let held = async || {
        state
            .submission_store
            .list_unverified_client(&client_id)
            .await
    };

    let refused = submit(state, &key, &client_id, route, valid_submission(PRIVATE)).await?;
    assert_eq!(refused, not_found());
    let unknown = submit(
        state,
        &key,
        &client_id,
        route,
        valid_submission("prefill_throughput_nobody_keeps"),
    )
    .await?;
    assert_eq!(unknown, refused, "a private id reads as an unknown one");
    assert!(held().await?.is_empty(), "a refused result is not held");

    let public = submit(
        state,
        &key,
        &client_id,
        route,
        valid_submission("prefill_throughput_256"),
    )
    .await?;
    assert_eq!(public, None);
    assert_eq!(held().await?.len(), 1, "a public result is held");

    // The same device approved: the refusal was its standing, not its owner.
    let mut client = state
        .client_store
        .get_client(&client_id)
        .await?
        .context("the device is registered")?;
    client.status = ClientStatus::Approved;
    state.client_store.put_client(&client).await?;
    let approved = submit(state, &key, &client_id, route, valid_submission(PRIVATE)).await?;
    assert_eq!(
        approved, None,
        "an approved member's private result is accepted"
    );
    Ok(())
}

/// An outsider that claimed a private job it was never served hands it back:
/// the retriable failure denies it the job and returns it to `avail/` for the
/// organization's devices, rather than leaving the lease to lapse.
#[rstest]
#[tokio::test]
async fn an_outsider_hands_back_a_claimed_private_job(
    #[values(Route::Single, Route::Batch)] route: Route,
    #[values(Owner::OtherOrg, Owner::Nobody)] owner: Owner,
) -> anyhow::Result<()> {
    let world = world().await?;
    let (key, client_id) = device(&world, owner).await?;
    let job = lease(&world, &client_id, PRIVATE)?;

    let refused = submit(&world.state, &key, &client_id, route, failure(true)).await?;
    assert_eq!(refused, None);
    assert_eq!(denied_clients(&world.state, &job).await?, vec![client_id]);
    assert!(
        world
            .state
            .todo_store
            .get_avail_by_job(&job)
            .await?
            .is_some()
    );
    assert!(world.state.submission_store.find_job(&job).await?.is_none());
    Ok(())
}

/// Only a failure rides the claim to another organization's entry: an
/// outsider's measured result on its own lease is the unknown-id `404`, and the
/// lease stays put. A member's is accepted, so the refusal is not the
/// `job_id`'s.
#[rstest]
#[tokio::test]
async fn an_outsider_cannot_report_a_result_on_its_private_claim(
    #[values(Route::Single, Route::Batch)] route: Route,
    #[values(Owner::Acme, Owner::OtherOrg, Owner::Nobody)] owner: Owner,
) -> anyhow::Result<()> {
    let world = world().await?;
    let (key, client_id) = device(&world, owner).await?;
    let job = lease(&world, &client_id, PRIVATE)?;
    let mut body = valid_submission(PRIVATE);
    body["job_id"] = json!(CLAIMED);

    let refused = submit(&world.state, &key, &client_id, route, body).await?;
    let recorded = world.state.submission_store.find_job(&job).await?;
    if let Owner::Acme = owner {
        assert_eq!(refused, None);
        assert!(recorded.is_some(), "a member's result is recorded");
        return Ok(());
    }
    assert_eq!(refused, not_found());
    assert!(recorded.is_none());
    assert!(denied_clients(&world.state, &job).await?.is_empty());
    let todo = &world.state.todo_store;
    assert!(todo.get_avail_by_job(&job).await?.is_none(), "not recycled");
    assert_eq!(
        todo.list_leased_for_client(&client_id).await?.len(),
        1,
        "the lease is left in place"
    );
    Ok(())
}

/// A terminal failure bound to the claim is the job's result, from any
/// claimant; it is recorded against the private benchmark's type.
#[rstest]
#[tokio::test]
async fn a_claimant_records_a_terminal_failure(
    #[values(Route::Single, Route::Batch)] route: Route,
    #[values(Owner::Acme, Owner::OtherOrg)] owner: Owner,
) -> anyhow::Result<()> {
    let world = world().await?;
    let (key, client_id) = device(&world, owner).await?;
    let job = lease(&world, &client_id, PRIVATE)?;

    let refused = submit(&world.state, &key, &client_id, route, failure(false)).await?;
    assert_eq!(refused, None);
    let record = world
        .state
        .submission_store
        .find_job(&job)
        .await?
        .context("the failure is recorded")?;
    assert_eq!(record.state, JobState::Incoming);
    assert_eq!(record.body["benchmark_type"], "prefill_throughput");
    Ok(())
}

/// `failure` declaring a `benchmark_type` / `benchmark_params` pair that
/// disagrees with [`PRIVATE`]'s parameters.
fn failure_declaring_wrong_params(retriable: bool) -> Value {
    json!({
        "message_type": "failure",
        "job_id": CLAIMED,
        "benchmark_id": PRIVATE,
        "retriable": retriable,
        "failure_reason": "GET /benchmarks/prefill_throughput_acme_4096: 404",
        "benchmark_type": "prefill_throughput",
        "benchmark_params": {"prefill_tokens": 1, "batch_size": 1},
    })
}

/// An outsider's pair is never read, however malformed: a mismatch or a parse
/// refusal would confirm the private parameters.
#[rstest]
#[case::mismatch(json!({"prefill_tokens": 1, "batch_size": 1}))]
#[case::empty(json!({}))]
#[case::not_an_object(json!(5))]
#[case::type_alone(Value::Null)]
#[tokio::test]
async fn an_outsider_declaring_params_learns_nothing(
    #[case] benchmark_params: Value,
    #[values(Route::Single, Route::Batch)] route: Route,
    #[values(Owner::OtherOrg, Owner::Nobody)] owner: Owner,
) -> anyhow::Result<()> {
    let world = world().await?;
    let (key, client_id) = device(&world, owner).await?;
    let job = lease(&world, &client_id, PRIVATE)?;
    let mut failure = failure_declaring_wrong_params(true);
    failure["benchmark_params"] = benchmark_params;

    let refused = submit(&world.state, &key, &client_id, route, failure).await?;
    assert_eq!(refused, None);
    assert_eq!(denied_clients(&world.state, &job).await?, vec![client_id]);
    Ok(())
}

/// A member reads the private entry's pair from `/api/v2` and declares it on its
/// result as-is: the pair the server checks against, so it is accepted.
#[rstest]
#[tokio::test]
async fn a_member_declares_the_pair_v2_serves_it(
    #[values(Route::Single, Route::Batch)] route: Route,
) -> anyhow::Result<()> {
    let world = world().await?;
    let (key, client_id) = device(&world, Owner::Acme).await?;
    let path = format!("/api/v2/benchmarks/{PRIVATE}");
    let entry = body_json(authed_get(&world.state, &key, &client_id, &path).await?).await?;
    assert_eq!(entry["org_id"], world.acme.as_str());
    assert_eq!(entry["benchmark_type"], "prefill_throughput");

    let mut body = valid_submission(PRIVATE);
    body["benchmark_type"] = entry["benchmark_type"].clone();
    body["benchmark_params"] = entry["benchmark_params"].clone();
    let refused = submit(&world.state, &key, &client_id, route, body).await?;
    assert_eq!(refused, None);
    Ok(())
}

/// A member may read the definition, so its declared pair is checked like a
/// public one's.
#[tokio::test]
async fn a_member_declaring_wrong_params_is_refused() -> anyhow::Result<()> {
    let world = world().await?;
    let (key, client_id) = device(&world, Owner::Acme).await?;
    let job = lease(&world, &client_id, PRIVATE)?;

    let refused = submit(
        &world.state,
        &key,
        &client_id,
        Route::Batch,
        failure_declaring_wrong_params(false),
    )
    .await?
    .context("a mismatched pair is refused")?;
    assert!(
        refused.starts_with("benchmark_params do not match "),
        "{refused}"
    );
    assert!(world.state.submission_store.find_job(&job).await?.is_none());
    Ok(())
}

/// Without its own lease on a job that runs the id, an outsider's failure is
/// the unknown-id `404`: not the claim check's `404`, which would say the id
/// exists, and nothing is denied.
#[rstest]
#[case::no_lease(None)]
#[case::a_lease_on_other_work(Some("prefill_throughput_256"))]
#[tokio::test]
async fn an_outsider_without_the_claim_learns_nothing(
    #[case] leased: Option<&str>,
    #[values(Route::Single, Route::Batch)] route: Route,
) -> anyhow::Result<()> {
    let world = world().await?;
    let (key, client_id) = device(&world, Owner::Nobody).await?;
    if let Some(benchmark_id) = leased {
        lease(&world, &client_id, benchmark_id)?;
    }

    let refused = submit(&world.state, &key, &client_id, route, failure(true)).await?;
    assert_eq!(refused, not_found());
    assert!(
        denied_clients(&world.state, &job(CLAIMED)?)
            .await?
            .is_empty()
    );
    Ok(())
}

/// Who may file a plan naming a private id: the organization's own members,
/// and a `submit_any_plans` operator. Anyone else is refused exactly as for an
/// unknown id. `any` adds `submit_any_plans` to `submit_plans`.
#[rstest]
#[case::member_user(Owner::Acme, false, true)]
#[case::outsider_user(Owner::OtherOrg, false, false)]
#[case::unaffiliated_user(Owner::Nobody, false, false)]
#[case::operator(Owner::Nobody, true, true)]
#[tokio::test]
async fn a_plan_names_a_private_benchmark_only_from_inside(
    #[case] owner: Owner,
    #[case] any: bool,
    #[case] accepted: bool,
) -> anyhow::Result<()> {
    let world = world().await?;
    let state = &world.state;

    let mut grants = vec![UserPermission::SubmitPlans];
    if any {
        grants.push(UserPermission::SubmitAnyPlans);
    }
    let signer = registered_user(state, &grants).await?;
    set_user_org(state, &signer.1, world.org(owner)).await?;
    let campaign_id = put_idle_campaign(state, &signer.1, "Private").await?;
    let uri = format!("/api/v1/campaigns/{campaign_id}/plans");
    let post =
        async |benchmark_id: &str| -> anyhow::Result<axum::http::Response<axum::body::Body>> {
            post_plan_document(state, &signer, &uri, &document(benchmark_id)).await
        };

    let resp = post(PRIVATE).await?;
    if accepted {
        assert_eq!(resp.status(), StatusCode::CREATED);
        return Ok(());
    }
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let refused = json_str(&body_json(resp).await?, "error")?.replace(PRIVATE, "{id}");
    let resp = post("prefill_throughput_nobody_keeps").await?;
    let unknown = json_str(&body_json(resp).await?, "error")?
        .replace("prefill_throughput_nobody_keeps", "{id}");
    assert_eq!(refused, unknown, "a private id reads as an unknown one");
    Ok(())
}

/// An unprefixed private id is typed by its catalog definition.
#[tokio::test]
async fn an_unprefixed_private_benchmark_takes_typed_flags() -> anyhow::Result<()> {
    const UNPREFIXED: &str = "acme_long_prompt";
    let world = world().await?;
    let state = &world.state;
    let mut entry = an_org_benchmark()?;
    let BenchmarkDefinition::PrefillThroughput(def) = &mut entry.def else {
        anyhow::bail!("the fixture is a prefill entry");
    };
    def.benchmark_id = BenchmarkId::try_new(UNPREFIXED)?;
    state
        .catalog_store
        .put_org_benchmark(&world.acme, &entry)
        .await?;

    let filer = registered_user(state, &[UserPermission::SubmitPlans]).await?;
    set_user_org(state, &filer.1, Some(world.acme.clone())).await?;
    let campaign_id = put_idle_campaign(state, &filer.1, "Unprefixed").await?;
    let flags = r#"runtime_flags = [{ benchmark_type = "prefill_throughput", runtime_type = "llamacpp_cli_stock_tools", model_type = "gguf_text", threads = 4 }]"#;
    let both = format!(r#"["{UNPREFIXED}", "{PRIVATE}"]"#);
    let text = document(UNPREFIXED).replace(&format!(r#"["{UNPREFIXED}"]"#), &both);
    let resp = post_plan_document(
        state,
        &filer,
        &format!("/api/v1/campaigns/{campaign_id}/plans"),
        &format!("{text}{flags}\n"),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let plan_id = json_str(&body_json(resp).await?, "plan_id")?.to_owned();

    let uri = format!("/api/v1/plans/{plan_id}/jobs?all=true");
    let resp = signed_empty(state, &filer, "GET", &uri).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    let planned = body["jobs"]
        .as_array()
        .context("a `jobs` array")?
        .iter()
        .map(|job| {
            let planned = &job["planned"];
            Ok((
                json_str(planned, "benchmark")?.to_owned(),
                json_str(planned, "benchmark_type")?.to_owned(),
                planned["runtime_flags"]["threads"].clone(),
            ))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let typed = |benchmark: &str| {
        (
            benchmark.to_owned(),
            "prefill_throughput".to_owned(),
            json!(4),
        )
    };
    assert_eq!(planned, [typed(UNPREFIXED), typed(PRIVATE)], "{body}");
    Ok(())
}

/// Filing records each benchmark's catalog type on the manifest, so a cell
/// with no flags still reads typed.
#[tokio::test]
async fn a_plan_records_its_benchmark_types() -> anyhow::Result<()> {
    const UNPREFIXED: &str = "acme_long_prompt";
    let world = world().await?;
    let state = &world.state;
    let mut entry = an_org_benchmark()?;
    let BenchmarkDefinition::PrefillThroughput(def) = &mut entry.def else {
        anyhow::bail!("the fixture is a prefill entry");
    };
    def.benchmark_id = BenchmarkId::try_new(UNPREFIXED)?;
    state
        .catalog_store
        .put_org_benchmark(&world.acme, &entry)
        .await?;

    let filer = registered_user(state, &[UserPermission::SubmitPlans]).await?;
    set_user_org(state, &filer.1, Some(world.acme.clone())).await?;
    let campaign_id = put_idle_campaign(state, &filer.1, "Flagless").await?;
    let resp = post_plan_document(
        state,
        &filer,
        &format!("/api/v1/campaigns/{campaign_id}/plans"),
        &document(UNPREFIXED),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let plan_id = PlanId::try_new(json_str(&body_json(resp).await?, "plan_id")?)?;

    let manifest = state
        .plan_store
        .get_plan(&plan_id)
        .await?
        .context("the plan was filed")?;
    assert_eq!(
        manifest.benchmark_types,
        BTreeMap::from([(
            BenchmarkId::try_new(UNPREFIXED)?,
            BenchmarkType::PrefillThroughput
        )])
    );
    let uri = format!("/api/v1/plans/{plan_id}/jobs?all=true");
    let resp = signed_empty(state, &filer, "GET", &uri).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    let job = body["jobs"].get(0).context("one job")?;
    assert_eq!(
        job["planned"]["benchmark_type"], "prefill_throughput",
        "{body}"
    );
    assert_eq!(job["planned"]["runtime_flags"], Value::Null, "{body}");
    Ok(())
}

/// A campaign that ran an organization's private benchmark stays inside it: a
/// filer who moves away cannot hand it to the new organization, whose readers
/// would read the private results, and can hand it back inside. With the
/// entry deleted, only a server admin hands it over.
#[tokio::test]
async fn a_campaign_with_private_results_stays_in_its_organization() -> anyhow::Result<()> {
    let world = world().await?;
    let state = &world.state;
    let filer = registered_user(state, &[UserPermission::SubmitPlans]).await?;
    set_user_org(state, &filer.1, Some(world.acme.clone())).await?;
    let campaign_id = put_idle_campaign(state, &filer.1, "Private").await?;
    let filed = format!("/api/v1/campaigns/{campaign_id}/plans");
    let resp = post_plan_document(state, &filer, &filed, &document(PRIVATE)).await?;
    assert_eq!(resp.status(), StatusCode::CREATED);

    let uri = format!("/api/v1/campaigns/{campaign_id}/owner");
    set_user_org(state, &filer.1, Some(world.labs.clone())).await?;
    let to_labs = json!({ "owner": { "type": "org", "org_id": world.labs } });
    let resp = signed_json(state, &filer, "PUT", &uri, &to_labs).await?;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let error = json_str(&body_json(resp).await?, "error")?.to_string();
    assert!(error.contains(PRIVATE), "{error}");

    set_user_org(state, &filer.1, Some(world.acme.clone())).await?;
    let to_acme = json!({ "owner": { "type": "org", "org_id": world.acme } });
    let resp = signed_json(state, &filer, "PUT", &uri, &to_acme).await?;
    assert_eq!(resp.status(), StatusCode::OK);

    // A person is held to the same line, by the organization they belong to.
    let root = registered_user(state, UserPermissionGroup::ServerAdmin.permissions()).await?;
    let outsider = registered_user(state, &[UserPermission::SubmitPlans]).await?;
    set_user_org(state, &outsider.1, Some(world.labs.clone())).await?;
    let to_outsider = json!({ "owner": { "type": "user", "user_id": outsider.1 } });
    let resp = signed_json(state, &root, "PUT", &uri, &to_outsider).await?;
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    // Once the entry is deleted nobody can say whose results these are, so
    // the hand-over is a server admin's.
    state
        .catalog_store
        .delete_org_benchmark(&world.acme, &BenchmarkId::try_new(PRIVATE)?)
        .await?;
    let lead = registered_user(
        state,
        &[UserPermission::ManageOrgs, UserPermission::SubmitPlans],
    )
    .await?;
    set_user_org(state, &lead.1, Some(world.acme.clone())).await?;
    let to_lead = json!({ "owner": { "type": "user", "user_id": lead.1 } });
    let resp = signed_json(state, &lead, "PUT", &uri, &to_lead).await?;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let error = json_str(&body_json(resp).await?, "error")?.to_string();
    assert!(error.contains("no catalog holds"), "{error}");
    assert!(error.contains(PRIVATE), "{error}");
    let resp = signed_json(state, &root, "PUT", &uri, &to_labs).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    Ok(())
}

/// The scoring passes resolve a private id: a member's result and a claimant's
/// terminal failure both leave `incoming/` scored.
#[tokio::test]
async fn private_results_are_scored() -> anyhow::Result<()> {
    let world = world().await?;
    let (key, client_id) = device(&world, Owner::Acme).await?;
    let (outsider_key, outsider) = device(&world, Owner::Nobody).await?;
    let failed = lease(&world, &outsider, PRIVATE)?;

    let resp = authed_post(
        &world.state,
        &key,
        &client_id,
        "/benchmarks",
        &valid_submission(PRIVATE),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let measured = job(json_str(&body_json(resp).await?, "job_id")?)?;
    let refused = submit(
        &world.state,
        &outsider_key,
        &outsider,
        Route::Single,
        failure(false),
    )
    .await?;
    assert_eq!(refused, None);

    run_full_score(&world.state.config).await?;

    let states: Vec<Option<JobState>> = futures::future::try_join_all(
        [&measured, &failed].map(|job_id| world.state.submission_store.find_job(job_id)),
    )
    .await?
    .into_iter()
    .map(|record| record.map(|r| r.state))
    .collect();
    assert_eq!(
        states,
        [Some(JobState::Processed), Some(JobState::Processed)]
    );
    Ok(())
}

#[tokio::test]
async fn an_expired_private_job_gets_a_synthetic_failure() -> anyhow::Result<()> {
    let world = world().await?;
    let deadline = Utc
        .with_ymd_and_hms(2020, 1, 2, 3, 4, 5)
        .single()
        .context("a valid date")?;
    let body = json!({"job_id": "job-1", "clients": ["ev1_member"], "spec": spec(PRIVATE)});
    let expired = seed_avail(world.dir.path(), "job-1", ExpiresAt::At(deadline), &body)?;

    run_qm(&world.state).await?;

    let record = world
        .state
        .submission_store
        .get_submission(&expired)
        .await?
        .context("the synthetic failure is recorded")?;
    assert_eq!(record.body["benchmark_type"], "prefill_throughput");
    assert_eq!(record.body["failure_source"], "expired");
    Ok(())
}

/// Serving a private benchmark's work widens nothing unsigned: the public
/// loader, the cache, and an anonymous read of the catalog and the id are
/// what they were.
#[tokio::test]
async fn public_surfaces_are_unchanged() -> anyhow::Result<()> {
    let world = world().await?;
    let state = &world.state;
    let (key, client_id) = device(&world, Owner::Acme).await?;
    let resp = authed_post(
        state,
        &key,
        &client_id,
        "/benchmarks",
        &valid_submission(PRIVATE),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    run_full_score(&state.config).await?;

    let id = BenchmarkId::try_new(PRIVATE)?;
    assert!(!state.catalog_store.load_catalog().await?.contains_key(&id));
    assert!(!state.catalog_cache.get().await?.contains_key(&id));
    for route in ["/benchmarks", "/api/v2/benchmarks"] {
        let listing = body_json(unauthed_get(state, route).await?).await?;
        assert!(!benchmark_ids(&listing).contains(&PRIVATE.to_string()));
        let resp = unauthed_get(state, &format!("{route}/{PRIVATE}")).await?;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
    Ok(())
}
