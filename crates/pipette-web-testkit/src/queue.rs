//! The `todo/` tree: seeding it by hand, reading it back, and running the
//! maintenance passes over it. `TodoStore` has no writer for `avail/` or
//! `leased/`, so the seeds write the stable local_fs layout directly.

use anyhow::Context;
use serde_json::{Value, json};

use pipette_core::todo_filename::{avail_filename, leased_key};
use pipette_httpapi::state::AppState;
use pipette_plan_types::{ClientId, DeviceProfile, ExpiresAt, JobId, Tag};
use pipette_store::{TodoStore, drain_plans};

/// Build a validated [`JobId`] from a test literal (`new_unchecked` is
/// `#[cfg(test)]`-only). Fallible because the panic ban covers tests too.
pub fn job(s: impl Into<String>) -> anyhow::Result<JobId> {
    JobId::try_new(s).context("valid test job id")
}

/// Every client that holds a `denied/` marker for one job, sorted so the
/// assertion does not depend on the backend's key order.
pub async fn denied_clients(state: &AppState, job_id: &JobId) -> anyhow::Result<Vec<ClientId>> {
    let mut clients: Vec<ClientId> = state
        .todo_store
        .list_all_denied()
        .await?
        .into_iter()
        .filter(|(denied_job, _)| denied_job == job_id)
        .map(|(_, client_id)| client_id)
        .collect();
    clients.sort();
    Ok(clients)
}

/// The `spec` object a plan-ingestion fixture carries, on a benchmark the
/// `setup_benchmarks` catalog resolves. Eligibility is left to the wrapping job,
/// which each fixture varies.
pub fn job_spec() -> Value {
    job_spec_with_model_path("LFM2-700M-Q4_0.gguf")
}

/// [`job_spec`], with the model file at `path`.
pub(crate) fn job_spec_with_model_path(path: &str) -> Value {
    json!({
        "benchmark": "eval_test",
        "model": {"type": "gguf_text", "source": "huggingface", "org": "LiquidAI", "repo_name": "LFM2-700M-GGUF", "path": path},
        "runtime": {"type": "llamacpp_cli_stock_tools", "source": "github_release", "repository_url": "github.com/ggml-org/llama.cpp", "repository_version": "b5000", "flavor": "macos-arm64"},
    })
}

/// Nothing reached the queue: no plan manifest, and an empty `avail/`. A plan
/// is refused as a unit, so a partial stage is as much a failure as a wrong status.
pub async fn assert_nothing_staged(state: &AppState) -> anyhow::Result<()> {
    assert!(
        drain_plans(state.plan_store.as_ref(), None)
            .await?
            .items
            .is_empty(),
        "a refused submission wrote a plan manifest"
    );
    assert!(
        state.todo_store.list_avail().await?.is_empty(),
        "a refused submission staged a job"
    );
    Ok(())
}

/// Write a job body straight into `todo/avail/`, bypassing the server.
pub fn seed_avail(
    dir: &std::path::Path,
    job_id: &str,
    expires_at: ExpiresAt,
    body: &Value,
) -> anyhow::Result<JobId> {
    let job = job(job_id)?;
    let path = dir
        .join("todo")
        .join("avail")
        .join(avail_filename(&job, expires_at));
    std::fs::create_dir_all(path.parent().context("an avail key has a job directory")?)?;
    std::fs::write(path, serde_json::to_vec(body)?)?;
    Ok(job)
}

/// Plant an active lease, as `POST /plans/claim` would, so a submission that
/// echoes `job_id` passes the claim-binding check.
pub fn seed_leased(
    dir: &std::path::Path,
    job_id: &str,
    client_id: &ClientId,
    body: &Value,
) -> anyhow::Result<JobId> {
    let job = job(job_id)?;
    let expiry = chrono::Utc::now() + chrono::Duration::hours(1);
    let path =
        dir.join("todo")
            .join("leased")
            .join(leased_key(&job, ExpiresAt::Never, client_id, expiry));
    std::fs::create_dir_all(
        path.parent()
            .ok_or_else(|| anyhow::anyhow!("lease path has no parent"))?,
    )?;
    std::fs::write(path, serde_json::to_vec(body)?)?;
    Ok(job)
}

/// Overwrite what a registered client is matched by: its device profile, the
/// capabilities it reports, and its tags. Written straight to the stores, so
/// no reindex flag gates its next claim.
pub async fn set_matching_input(
    state: &AppState,
    client_id: &ClientId,
    profile: DeviceProfile,
    capabilities: &[&str],
    tags: &[&str],
) -> anyhow::Result<()> {
    let mut client = state
        .client_store
        .get_client(client_id)
        .await?
        .context("the client is registered")?;
    client.device_profile = profile;
    client.capabilities = capabilities.iter().map(|f| (*f).to_string()).collect();
    state.client_store.put_client(&client).await?;
    for tag in tags {
        state
            .tag_store
            .add_client_tag(client_id, &Tag::try_new(*tag)?)
            .await?;
    }
    Ok(())
}

/// Delete every pending-reindex flag for `client_id` — the test stand-in for
/// the reindex pass lifting the gate.
pub async fn clear_pending_reindex(
    todo: &dyn TodoStore,
    client_id: &ClientId,
) -> anyhow::Result<()> {
    for (_, key) in todo
        .list_pending_reindex()
        .await?
        .into_iter()
        .filter(|(flagged, _)| flagged == client_id)
    {
        todo.delete_pending_reindex(&key).await?;
    }
    Ok(())
}

/// Run every `queue-maintenance` pass with the production default ages.
pub async fn run_qm(state: &AppState) -> anyhow::Result<()> {
    run_qm_with_tmp_age(state, std::time::Duration::from_secs(86_400)).await
}

/// Run every pass, choosing the `todo/tmp/` age.
pub async fn run_qm_with_tmp_age(
    state: &AppState,
    tmp_age: std::time::Duration,
) -> anyhow::Result<()> {
    run_qm_full(state, tmp_age, std::time::Duration::from_secs(21_600)).await
}

/// Run every pass, choosing both age thresholds, so a plan test can have a
/// fresh `creating` manifest treated as stranded without backdating it.
pub async fn run_qm_full(
    state: &AppState,
    tmp_age: std::time::Duration,
    plan_creating_max_age: std::time::Duration,
) -> anyhow::Result<()> {
    // Every organization's entries, as `pipette-mgmt queue-maintenance` reads them.
    let public = state.catalog_cache.get().await?;
    let catalog =
        pipette_pipeline::org_catalog::server_catalog(&*state.catalog_store, &public).await?;
    let plans = state.plan_store.clone();
    pipette_pipeline::queue_maintenance::run(
        &*state.todo_store,
        &*state.client_store,
        &*state.tag_store,
        &*state.submission_store,
        &*plans,
        &catalog,
        pipette_pipeline::queue_maintenance::MaintenanceSettings {
            tmp_max: tmp_age,
            plan_creating_max: plan_creating_max_age,
            lost_lease_limit: state.config.plan_lost_lease_limit,
        },
    )
    .await
}
