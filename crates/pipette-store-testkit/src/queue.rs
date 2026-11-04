//! Queue fixtures that seed `avail/` and `leased/` through [`TodoStore`], so a
//! test that uses them runs on every backend.

use anyhow::Context;
use chrono::{DateTime, Utc};
use serde_json::Value;

use pipette_plan_types::{ClientId, ExpiresAt, JobId};
use pipette_store::{ClaimResult, TodoStore};

/// A complete queue job body with the public GGUF model and `b1000` runtime.
/// The caller supplies the job, its client roster, and its benchmark.
pub fn todo_job_body(job_id: &str, clients: &[&str], benchmark: &str) -> Value {
    serde_json::json!({
        "job_id": job_id,
        "clients": clients,
        "spec": {
            "benchmark": benchmark,
            "model": {"type": "gguf_text", "source": "huggingface", "org": "LiquidAI", "repo_name": "LFM2-700M-GGUF", "path": "LFM2-700M-Q4_0.gguf"},
            "runtime": {"type": "llamacpp_cli_stock_tools", "source": "github_release", "repository_url": "github.com/ggml-org/llama.cpp", "repository_version": "b1000", "flavor": "macos-arm64"},
        },
    })
}

/// Queue `body` in `avail/` as `job_id`, the way ingestion stages a job: a
/// `tmp/` write, then the promote.
pub async fn seed_avail(
    todo: &dyn TodoStore,
    job_id: &str,
    expires_at: ExpiresAt,
    body: &Value,
) -> anyhow::Result<JobId> {
    let job = JobId::try_new(job_id).context("valid test job id")?;
    todo.write_tmp(&job, body).await?;
    todo.promote_avail(&job, expires_at).await?;
    Ok(job)
}

/// Create a lease that expires in one hour, as `POST /plans/claim` does. The lease lets
/// a submission with `job_id` pass the claim check.
pub async fn seed_leased(
    todo: &dyn TodoStore,
    job_id: &str,
    client_id: &ClientId,
    body: &Value,
) -> anyhow::Result<JobId> {
    let lease_expiry = Utc::now() + chrono::Duration::hours(1);
    seed_leased_until(
        todo,
        job_id,
        ExpiresAt::Never,
        client_id,
        lease_expiry,
        body,
    )
    .await
}

/// Queue `body` and claim it for `client_id` until `lease_expiry`. A past
/// `lease_expiry` creates a lapsed lease.
pub async fn seed_leased_until(
    todo: &dyn TodoStore,
    job_id: &str,
    expires_at: ExpiresAt,
    client_id: &ClientId,
    lease_expiry: DateTime<Utc>,
    body: &Value,
) -> anyhow::Result<JobId> {
    let job = seed_avail(todo, job_id, expires_at, body).await?;
    match todo
        .claim_job(&job, expires_at, client_id, lease_expiry)
        .await?
    {
        ClaimResult::Claimed(_) => Ok(job),
        ClaimResult::Gone => anyhow::bail!("the seeded job {job} was gone at its claim"),
    }
}
