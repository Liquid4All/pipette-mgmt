//! Tests for the planner job-queue endpoints: `POST /plans/claim`, `PUT
//! /plans/{job_id}/heartbeat`, and `POST /plans/{job_id}/reclaim`. Jobs
//! are seeded into `avail/` by writing the file directly — the `TodoStore`
//! trait has no `write_avail` (job creation is a separate, not-yet-built
//! concern), and the local_fs layout is stable.

mod helpers;

use std::path::Path;

use anyhow::Context;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Utc};
use ed25519_dalek::SigningKey;
use rstest::rstest;
use serde_json::{Value, json};
use tower::ServiceExt;

use helpers::{
    TEST_LIST_LIMIT, authed_post, authed_put_empty, body_json, job, make_state,
    register_and_approve, register_pending, setup_benchmarks, test_app,
};

use pipette_mgmt::handlers::AppState;
use pipette_mgmt::todo_filename::{leased_key, parse_leased_key};
use pipette_mgmt::types::{ClientId, ExpiresAt, JobId};

/// Write a job body straight into `todo/avail/{job_id}.never.json` so a
/// claim can pick it up. Returns the `JobId`.
fn seed_avail(dir: &Path, job_id: &str, body: &Value) -> anyhow::Result<JobId> {
    helpers::seed_avail(dir, job_id, ExpiresAt::Never, body)
}

/// Seed a job into `avail/` and write `client`'s eligible marker for it, using
/// the same `expires_at` for both — mirroring how `queue-maintenance` derives a
/// marker's expiry from the `avail/` filename (the claim handler relies on the
/// two matching to address the rename target). Returns the `JobId`.
async fn seed_eligible(
    state: &AppState,
    dir: &Path,
    client_id: &ClientId,
    job_id: &str,
    expires_at: ExpiresAt,
) -> anyhow::Result<JobId> {
    seed_eligible_body(state, dir, client_id, job_id, expires_at, &job_body(job_id)).await
}

/// Like [`seed_eligible`], but with a caller-supplied body — for tests that vary
/// what the *body* carries independently of the `avail/` filename's expiry.
async fn seed_eligible_body(
    state: &AppState,
    dir: &Path,
    client_id: &ClientId,
    job_id: &str,
    expires_at: ExpiresAt,
    body: &Value,
) -> anyhow::Result<JobId> {
    let job = helpers::seed_avail(dir, job_id, expires_at, body)?;
    state
        .todo_store
        .write_eligible(client_id, &job, expires_at)
        .await?;
    Ok(job)
}

/// Write a `leased/` file directly with the given expiry, simulating a job
/// already held by `client`, without going through the claim path.
fn seed_leased(
    dir: &Path,
    job: &JobId,
    client: &ClientId,
    expiry: chrono::DateTime<chrono::Utc>,
) -> anyhow::Result<()> {
    seed_leased_body(dir, job, client, expiry, &json!({}))
}

/// Like [`seed_leased`], but with a caller-supplied job body — used when the
/// test needs the lease's body to be read back (e.g. the idempotent-claim path).
fn seed_leased_body(
    dir: &Path,
    job: &JobId,
    client: &ClientId,
    expiry: chrono::DateTime<chrono::Utc>,
    body: &Value,
) -> anyhow::Result<()> {
    let path = dir
        .join("todo")
        .join("leased")
        .join(leased_key(job, client, expiry));
    // Partitioned layout: leased/{client_id}/{job_id}.{expiry}.json — create the
    // client partition dir before planting the file.
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(path, serde_json::to_vec(body)?)?;
    Ok(())
}

/// A stored job envelope: the server-owned fields plus the plan-authored `spec`
/// (`docs/planner.md`, "The contents of a job file"). The `time_window` is
/// deliberately present and wrong — the claim response must carry the server's
/// lease increment, never a stored value.
fn job_body(job_id: &str) -> Value {
    json!({
        "job_id": job_id,
        "time_window": "PT99M",
        "spec": {
            "benchmark": "eval_test",
            "model": {
                "type": "gguf_text",
                "source": "huggingface",
                "org": "LiquidAI",
                "repo_name": "LFM2-700M-GGUF",
                "path": "LFM2-700M-Q4_0.gguf",
            },
            "runtime": {
                "type": "llamacpp_cli_stock_tools",
                "source": "github_release",
                "repository_version": "b5000",
                "flavor": "macos-arm64",
            },
        },
    })
}

/// Common claim-test setup: a tempdir seeded with benchmarks, an `AppState`,
/// and a registered + approved client. The returned `TempDir` guard must stay
/// bound for the test's duration — dropping it deletes the backing `todo/` tree.
async fn approved_client() -> anyhow::Result<(tempfile::TempDir, AppState, SigningKey, ClientId)> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (key, client_id) = register_and_approve(&state).await?;
    Ok((dir, state, key, client_id))
}

#[tokio::test]
async fn test_claim_returns_eligible_job() -> anyhow::Result<()> {
    let (dir, state, key, client_id) = approved_client().await?;

    let job = seed_eligible(&state, dir.path(), &client_id, "job-1", ExpiresAt::Never).await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["job_id"], "job-1");
    // Lifted from `spec.benchmark`, so the envelope and the spec cannot disagree.
    assert_eq!(body["benchmark_id"], "eval_test");
    // Server stamps the lease increment (default 300s) as time_window so the
    // client knows its heartbeat interval — overriding the stored "PT99M".
    assert_eq!(body["time_window"], "PT5M");

    // The spec is forwarded verbatim, nested — not flattened into the envelope.
    assert_eq!(body["spec"]["benchmark"], "eval_test");
    assert_eq!(body["spec"]["model"]["type"], "gguf_text");
    assert_eq!(body["spec"]["runtime"]["repository_version"], "b5000");
    assert!(body.get("model").is_none(), "spec leaked into the envelope");

    // Scheduling fields stay server-side: they are spent by the time a job is
    // handed out, and a device has no use for the roster it was picked from.
    ["requires", "any_of", "clients"]
        .into_iter()
        .for_each(|scheduling| {
            assert!(
                body.get(scheduling).is_none(),
                "{scheduling} was forwarded to the client"
            );
        });

    // `expires_at` projection has its own test below, across every stored form.

    // The job moved avail/ → leased/, owned by this client.
    let leased = state.todo_store.list_leased().await?;
    assert_eq!(leased.len(), 1);
    let (got_job, got_client, _) = parse_leased_key(&leased[0])?;
    assert_eq!(got_job, job);
    assert_eq!(got_client, client_id);
    Ok(())
}

/// How each stored spelling of `expires_at` reaches the wire.
///
/// The wire contract is ISO 8601 **basic** format (`docs/httpapi.md` §2.9.2), and
/// the field is optional — so anything that is not a basic-format timestamp is
/// omitted rather than forwarded or converted. Converting would hide a body that
/// `recycle_lease` will also fail to parse, stranding the job in `leased/` when its
/// lease lapses (`docs/planner.md`).
///
/// The `avail/` filename stays `never` in every case so the job is always
/// claimable; only the body varies. That separation is the point — for a job
/// written straight into `avail/` the two need not agree, and the body's copy is
/// advisory while the filename is what the server schedules on.
#[rstest]
// Absent and the `never` sentinel are both "no expiry"; neither is a timestamp.
#[case::absent(None, None)]
#[case::never_sentinel(Some("never"), None)]
// The form every ingested job carries — ingestion resolves the expiry once and
// stamps it back in basic format, so this is the high-traffic path.
#[case::basic_format(Some("20260908T000000Z"), Some("20260908T000000Z"))]
// Accepted at the ingestion *handoff* (`validate_job` takes RFC 3339), so it is
// the likely mistake for a planner writing `avail/` directly — but it is not the
// wire format, so it is dropped rather than sent or silently converted.
#[case::rfc3339_dropped(Some("2026-09-08T00:00:00Z"), None)]
#[case::unparseable_dropped(Some("soon"), None)]
#[case::non_string_dropped(Some("42"), None)]
#[tokio::test]
async fn test_claim_projects_expires_at(
    #[case] stored: Option<&str>,
    #[case] expected: Option<&str>,
) -> anyhow::Result<()> {
    let (dir, state, key, client_id) = approved_client().await?;

    let mut body = job_body("job-1");
    match stored {
        // `"42"` stands in for the non-string case: parsed as JSON it is a number,
        // which is the shape a hand-written body would carry.
        Some("42") => body["expires_at"] = json!(42),
        Some(s) => body["expires_at"] = json!(s),
        None => {
            body.as_object_mut()
                .context("job body is an object")?
                .remove("expires_at");
        }
    }
    seed_eligible_body(
        &state,
        dir.path(),
        &client_id,
        "job-1",
        ExpiresAt::Never,
        &body,
    )
    .await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let got = body_json(resp).await?;

    match expected {
        Some(want) => assert_eq!(
            got["expires_at"], want,
            "stored {stored:?} should reach the wire as {want:?}"
        ),
        None => assert!(
            got.get("expires_at").is_none(),
            "stored {stored:?} should be omitted, got {:?}",
            got.get("expires_at")
        ),
    }
    Ok(())
}

#[tokio::test]
async fn test_claim_204_when_no_eligible_jobs() -> anyhow::Result<()> {
    let (dir, state, key, client_id) = approved_client().await?;

    // A job exists in avail/ but this client has no eligible marker for it.
    seed_avail(dir.path(), "job-1", &job_body("job-1"))?;

    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    Ok(())
}

#[tokio::test]
async fn test_claim_204_when_eligible_marker_is_stale() -> anyhow::Result<()> {
    // `_dir` keeps the tempdir alive though this test seeds nothing on disk.
    let (_dir, state, key, client_id) = approved_client().await?;

    // Eligible marker points at a job that is no longer in avail/ (completed
    // or expired; marker awaiting GC). Claim must not 500 or hand out a
    // phantom job.
    let job = job("ghost");
    state
        .todo_store
        .write_eligible(&client_id, &job, ExpiresAt::Never)
        .await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    Ok(())
}

#[tokio::test]
async fn test_claim_403_when_pending() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (key, client_id) = register_pending(&state).await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    Ok(())
}

#[tokio::test]
async fn test_claim_401_when_unauthenticated() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let app = test_app(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/plans/claim")
                .header("Content-Type", "application/json")
                .body(Body::from("{}"))?,
        )
        .await?;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    Ok(())
}

#[tokio::test]
async fn test_claim_204_when_suspended() -> anyhow::Result<()> {
    let (dir, state, key, client_id) = approved_client().await?;

    let job = seed_eligible(&state, dir.path(), &client_id, "job-1", ExpiresAt::Never).await?;
    state
        .todo_store
        .write_suspension(&client_id, chrono::Utc::now(), &job)
        .await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    Ok(())
}

#[tokio::test]
async fn test_claim_idempotent_when_holding_single_lease() -> anyhow::Result<()> {
    let (dir, state, key, client_id) = approved_client().await?;

    // The client already holds one live lease on job-a with ~90s of life left.
    // Seeding it directly (rather than via a real claim) fixes a known remaining
    // life so the reported window is checkable, and 90 is not a whole number of
    // minutes so the window renders as `PTnS`.
    let job_a = job("job-a");
    let expiry = Utc::now() + chrono::Duration::seconds(90);
    seed_leased_body(dir.path(), &job_a, &client_id, expiry, &job_body("job-a"))?;

    // A second eligible job exists; a client holding a lease must not be handed it.
    seed_eligible(&state, dir.path(), &client_id, "job-b", ExpiresAt::Never).await?;

    let leased_before = state.todo_store.list_leased().await?;

    // Re-polling — as when the original claim response was lost in transit and
    // the client, never having learned the job_id, cannot reclaim — hands the
    // *same* job back idempotently rather than suspending.
    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["job_id"], "job-a");

    // `time_window` is the lease's *remaining* life (~90s), not the stored body's
    // "PT99M" nor a fresh full increment — proving the handler recomputed it.
    let tw = body["time_window"]
        .as_str()
        .context("time_window should be a string")?;
    let secs: u64 = tw
        .strip_prefix("PT")
        .and_then(|s| s.strip_suffix('S'))
        .context("time_window should be an ISO 8601 second duration")?
        .parse()?;
    assert!(
        (1..=90).contains(&secs),
        "time_window {tw} should reflect ~90s remaining"
    );

    // The lease is not renewed and no second lease is granted: the leased/ set is
    // unchanged across the idempotent claim (a renewal or a new lease on job-b
    // would change or add a key).
    assert_eq!(state.todo_store.list_leased().await?, leased_before);

    // The idempotent path must not suspend the client.
    assert!(
        state
            .todo_store
            .read_suspension(&client_id)
            .await?
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn test_claim_suspends_client_holding_multiple_leases() -> anyhow::Result<()> {
    let (dir, state, key, client_id) = approved_client().await?;

    // Two simultaneous live leases for one client is a protocol anomaly — a
    // fast-rebooting client that accumulated leases across crashes. Claiming
    // again suspends it, recording one held job_id as a triage breadcrumb.
    let future = Utc::now() + chrono::Duration::hours(1);
    let job_a = job("job-a");
    let job_b = job("job-b");
    seed_leased(dir.path(), &job_a, &client_id, future)?;
    seed_leased(dir.path(), &job_b, &client_id, future)?;

    seed_eligible(&state, dir.path(), &client_id, "job-c", ExpiresAt::Never).await?;
    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let suspension = state
        .todo_store
        .read_suspension(&client_id)
        .await?
        .context("client holding two live leases should be suspended")?;
    assert!([job_a, job_b].contains(&suspension.conflicting_job_id));
    Ok(())
}

#[tokio::test]
async fn test_claim_not_suspended_when_one_job_appears_under_two_lease_keys() -> anyhow::Result<()>
{
    let (dir, state, key, client_id) = approved_client().await?;

    // A lease renewal renames `{job}_{old}` → `{job}_{new}`; a non-snapshot
    // paginated listing can surface both keys for the *same* job at once. That is
    // one logical lease, not an accumulation, so the client must not be suspended
    // — it gets the job back idempotently.
    let job_a = job("job-a");
    let now = Utc::now();
    seed_leased_body(
        dir.path(),
        &job_a,
        &client_id,
        now + chrono::Duration::seconds(60),
        &job_body("job-a"),
    )?;
    seed_leased_body(
        dir.path(),
        &job_a,
        &client_id,
        now + chrono::Duration::seconds(120),
        &job_body("job-a"),
    )?;

    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await?["job_id"], "job-a");
    assert!(
        state
            .todo_store
            .read_suspension(&client_id)
            .await?
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn test_claim_skips_job_denied_for_client() -> anyhow::Result<()> {
    let (dir, state, key, client_id) = approved_client().await?;

    let job = seed_eligible(&state, dir.path(), &client_id, "job-1", ExpiresAt::Never).await?;
    state.todo_store.write_denied(&job, &client_id).await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    Ok(())
}

#[tokio::test]
async fn test_claim_skips_job_leased_to_another_client() -> anyhow::Result<()> {
    let (dir, state, key, client_id) = approved_client().await?;

    let job = seed_eligible(&state, dir.path(), &client_id, "job-1", ExpiresAt::Never).await?;
    // Another client holds an active lease on the same job.
    let other = ClientId::try_new("someone-else")?;
    seed_leased(
        dir.path(),
        &job,
        &other,
        chrono::Utc::now() + chrono::Duration::hours(1),
    )?;

    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    // Another client's lease must not suspend *us*.
    assert!(
        state
            .todo_store
            .read_suspension(&client_id)
            .await?
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn test_claim_succeeds_when_own_lease_is_expired() -> anyhow::Result<()> {
    let (dir, state, key, client_id) = approved_client().await?;

    // This client holds an *expired* lease on a previous job — that is a
    // timed-out device, not a double-claim, so it must not be suspended.
    let old_job = job("old-job");
    seed_leased(
        dir.path(),
        &old_job,
        &client_id,
        chrono::Utc::now() - chrono::Duration::hours(1),
    )?;

    seed_eligible(&state, dir.path(), &client_id, "job-1", ExpiresAt::Never).await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["job_id"], "job-1");
    assert!(
        state
            .todo_store
            .read_suspension(&client_id)
            .await?
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn test_claim_ignores_denial_by_another_client() -> anyhow::Result<()> {
    let (dir, state, key, client_id) = approved_client().await?;

    let job = seed_eligible(&state, dir.path(), &client_id, "job-1", ExpiresAt::Never).await?;
    // A *different* client failed this job; that must not block us.
    let other = ClientId::try_new("someone-else")?;
    state.todo_store.write_denied(&job, &other).await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["job_id"], "job-1");
    Ok(())
}

#[tokio::test]
async fn test_claim_skips_denied_job_but_claims_another() -> anyhow::Result<()> {
    let (dir, state, key, client_id) = approved_client().await?;

    // Two eligible+available jobs; this client has already failed job-a.
    let job_a = seed_eligible(&state, dir.path(), &client_id, "job-a", ExpiresAt::Never).await?;
    let job_b = seed_eligible(&state, dir.path(), &client_id, "job-b", ExpiresAt::Never).await?;
    state.todo_store.write_denied(&job_a, &client_id).await?;

    // The denied job is skipped (regardless of candidate order) and the
    // other job is claimed.
    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["job_id"], "job-b");

    // job-a remains in avail/ (never leased); only job-b moved to leased/.
    let leased = state.todo_store.list_leased().await?;
    assert_eq!(leased.len(), 1);
    let (got_job, _, _) = parse_leased_key(&leased[0])?;
    assert_eq!(got_job, job_b);
    Ok(())
}

#[tokio::test]
async fn test_claim_prefers_soonest_expiring_job() -> anyhow::Result<()> {
    let (dir, state, key, client_id) = approved_client().await?;

    // Three eligible jobs: one never-expiring, one far out, one soon. The
    // soonest-expiring must be chosen regardless of the (unordered) eligible
    // listing. Both expiries are in the future so neither is filtered as
    // expired (which would change which job is claimable).
    let soon = ExpiresAt::At(Utc::now() + chrono::Duration::hours(1));
    let later = ExpiresAt::At(Utc::now() + chrono::Duration::days(30));

    seed_eligible(
        &state,
        dir.path(),
        &client_id,
        "job-never",
        ExpiresAt::Never,
    )
    .await?;
    seed_eligible(&state, dir.path(), &client_id, "job-later", later).await?;
    seed_eligible(&state, dir.path(), &client_id, "job-soon", soon).await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["job_id"], "job-soon");
    Ok(())
}

#[tokio::test]
async fn test_claim_picks_within_soonest_expiry_tier() -> anyhow::Result<()> {
    let (dir, state, key, client_id) = approved_client().await?;

    // Several jobs share the soonest expiry; one later. The claim must land in
    // the soonest tier (which exact one is randomised, so assert membership).
    // Both expiries are in the future so neither is filtered as expired.
    let soon = ExpiresAt::At(Utc::now() + chrono::Duration::hours(1));
    let later = ExpiresAt::At(Utc::now() + chrono::Duration::days(30));

    let mut tier: Vec<JobId> = Vec::new();
    for id in ["soon-a", "soon-b", "soon-c"] {
        tier.push(seed_eligible(&state, dir.path(), &client_id, id, soon).await?);
    }
    seed_eligible(&state, dir.path(), &client_id, "job-later", later).await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    let claimed = body["job_id"]
        .as_str()
        .context("claim response missing job_id")?;
    assert!(
        tier.iter().any(|j| j.as_str() == claimed),
        "claimed {claimed:?} should be in the soonest-expiry tier, not the later job"
    );
    Ok(())
}

#[tokio::test]
async fn test_claim_204_when_only_eligible_job_is_expired() -> anyhow::Result<()> {
    let (dir, state, key, client_id) = approved_client().await?;

    // The only eligible job is already past its expires_at. An expired job is
    // never handed out (planner.md §Expiration), even though queue-maintenance
    // has not yet swept it from avail/.
    let expired = ExpiresAt::At(Utc::now() - chrono::Duration::hours(1));
    let job = seed_eligible(&state, dir.path(), &client_id, "job-1", expired).await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // The expired job is left untouched in avail/ — claim must not lease it.
    assert!(state.todo_store.list_leased().await?.is_empty());
    assert!(state.todo_store.get_avail(&job, expired).await?.is_some());
    Ok(())
}

#[tokio::test]
async fn test_claim_skips_expired_job_and_claims_valid_one() -> anyhow::Result<()> {
    let (dir, state, key, client_id) = approved_client().await?;

    // An expired job sorts soonest-first (earliest `At`), so without the
    // expiry filter the soonest-first ranking would actively prefer it. It must
    // be skipped and the still-valid job claimed instead.
    let expired = ExpiresAt::At(Utc::now() - chrono::Duration::hours(1));
    let valid = ExpiresAt::At(Utc::now() + chrono::Duration::days(7));
    seed_eligible(&state, dir.path(), &client_id, "job-expired", expired).await?;
    let good = seed_eligible(&state, dir.path(), &client_id, "job-valid", valid).await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await?;
    assert_eq!(body["job_id"], "job-valid");

    // Only the valid job moved to leased/; the expired one stays in avail/.
    let leased = state.todo_store.list_leased().await?;
    assert_eq!(leased.len(), 1);
    let (got_job, _, _) = parse_leased_key(&leased[0])?;
    assert_eq!(got_job, good);
    Ok(())
}

// ── heartbeat ────────────────────────────────────────────────

/// Read this client's single lease for `job` from disk and return its expiry,
/// so a test can confirm a heartbeat actually advanced the lease file.
fn lease_expiry(dir: &Path, job: &JobId, client: &ClientId) -> anyhow::Result<DateTime<Utc>> {
    let part = dir.join("todo").join("leased").join(client.as_str());
    let prefix = format!("{}.", job.as_str());
    std::fs::read_dir(&part)?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .find(|name| name.starts_with(&prefix))
        .map(|name| {
            let key = format!("{}/{name}", client.as_str());
            parse_leased_key(&key).map(|(_, _, expiry)| expiry)
        })
        .transpose()?
        .ok_or_else(|| anyhow::anyhow!("no lease for {} held by {}", job.as_str(), client.as_str()))
}

#[tokio::test]
async fn test_heartbeat_renews_active_lease() -> anyhow::Result<()> {
    let (dir, state, key, client_id) = approved_client().await?;

    // This client holds a lease expiring soon. A heartbeat must extend it.
    let job = job("job-1");
    let old = Utc::now() + chrono::Duration::seconds(30);
    seed_leased(dir.path(), &job, &client_id, old)?;

    let resp = authed_put_empty(&state, &key, &client_id, "/plans/job-1/heartbeat").await?;
    assert_eq!(resp.status(), StatusCode::OK);

    let renewed = lease_expiry(dir.path(), &job, &client_id)?;
    assert!(
        renewed > old,
        "lease expiry {renewed} should advance past the original {old}"
    );
    Ok(())
}

#[tokio::test]
async fn test_heartbeat_404_when_no_lease() -> anyhow::Result<()> {
    // Client holds no lease for the job (e.g. it was recycled). → 404.
    let (_dir, state, key, client_id) = approved_client().await?;

    let resp = authed_put_empty(&state, &key, &client_id, "/plans/ghost/heartbeat").await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    Ok(())
}

/// A pending-reindex flag gates both lease-renewal paths — heartbeat, and
/// reclaim even when the client still holds the lease: the profile change
/// that set the flag relinquished the client's standing, so a renewal
/// arriving while it is up is the client renewing a lease it gave up.
/// Refused with 404 — and crucially *without renaming the lease*, which
/// would resurrect it mid-relinquish. Clearing the flag (what the reindex
/// pass does) lifts the gate, and the held lease renews again (reclaim's
/// renewal accepts an expired-but-present lease, so the expired case also
/// turns 200).
#[rstest]
#[case::heartbeat(chrono::Duration::seconds(30), "PUT", "/plans/job-1/heartbeat")]
#[case::reclaim_held_lease(-chrono::Duration::hours(1), "POST", "/plans/job-1/reclaim")]
#[tokio::test]
async fn test_renewal_gated_while_pending_reindex(
    #[case] lease_offset: chrono::Duration,
    #[case] method: &str,
    #[case] path: &str,
) -> anyhow::Result<()> {
    let (dir, state, key, client_id) = approved_client().await?;
    let job = job("job-1");
    seed_leased(dir.path(), &job, &client_id, Utc::now() + lease_offset)?;
    let before = lease_expiry(dir.path(), &job, &client_id)?;
    state.todo_store.write_pending_reindex(&client_id).await?;

    let resp = match method {
        "PUT" => authed_put_empty(&state, &key, &client_id, path).await?,
        _ => authed_post(&state, &key, &client_id, path, &json!({})).await?,
    };
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        lease_expiry(dir.path(), &job, &client_id)?,
        before,
        "a gated renewal must not rename the lease"
    );

    // Clearing the flag (what the reindex pass does) lifts the gate.
    helpers::clear_pending_reindex(&*state.todo_store, &client_id).await?;
    let resp = match method {
        "PUT" => authed_put_empty(&state, &key, &client_id, path).await?,
        _ => authed_post(&state, &key, &client_id, path, &json!({})).await?,
    };
    assert_eq!(resp.status(), StatusCode::OK);
    Ok(())
}

#[tokio::test]
async fn test_invalid_job_id_in_path_is_400() -> anyhow::Result<()> {
    // A `job_id` with a path-significant character is rejected at the boundary
    // (`JobId::try_new`) with `400`, before it can reach a store as a key —
    // closing the path-traversal / key-injection vector. `bad.id` is a single
    // path segment (so it routes) but fails the `[A-Za-z0-9-]` charset.
    let (_dir, state, key, client_id) = approved_client().await?;

    let hb = authed_put_empty(&state, &key, &client_id, "/plans/bad.id/heartbeat").await?;
    assert_eq!(hb.status(), StatusCode::BAD_REQUEST);

    let rc = authed_post(
        &state,
        &key,
        &client_id,
        "/plans/bad.id/reclaim",
        &json!({}),
    )
    .await?;
    assert_eq!(rc.status(), StatusCode::BAD_REQUEST);
    Ok(())
}

#[tokio::test]
async fn test_heartbeat_409_when_leased_to_another_client() -> anyhow::Result<()> {
    let (dir, state, key, client_id) = approved_client().await?;

    // The job is leased to a *different* client — caller is a zombie. → 409.
    let job = job("job-1");
    let other = ClientId::try_new("someone-else")?;
    seed_leased(
        dir.path(),
        &job,
        &other,
        Utc::now() + chrono::Duration::hours(1),
    )?;

    let resp = authed_put_empty(&state, &key, &client_id, "/plans/job-1/heartbeat").await?;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    Ok(())
}

#[tokio::test]
async fn test_heartbeat_403_when_pending() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (key, client_id) = register_pending(&state).await?;

    let resp = authed_put_empty(&state, &key, &client_id, "/plans/job-1/heartbeat").await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    Ok(())
}

#[tokio::test]
async fn test_heartbeat_401_when_unauthenticated() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let app = test_app(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/plans/job-1/heartbeat")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    Ok(())
}

/// A pending-reindex flag gates the marker-driven handout paths: the client's
/// markers reflect its old profile (or, for a fresh registration, don't exist
/// yet), so no work is handed out — even against a present, matching marker —
/// until `queue-maintenance` re-evaluates the client and clears the flag.
/// `claim` hides the gate behind its usual no-work 204; reclaim's re-acquire
/// reports 404 like the no-marker case.
#[rstest]
#[case::claim("/plans/claim", StatusCode::NO_CONTENT)]
#[case::reclaim_reacquire("/plans/job-1/reclaim", StatusCode::NOT_FOUND)]
#[tokio::test]
async fn test_gated_while_pending_reindex(
    #[case] path: &str,
    #[case] gated_status: StatusCode,
) -> anyhow::Result<()> {
    let (dir, state, key, client_id) = approved_client().await?;
    let job = seed_eligible(&state, dir.path(), &client_id, "job-1", ExpiresAt::Never).await?;
    state.todo_store.write_pending_reindex(&client_id).await?;

    let resp = authed_post(&state, &key, &client_id, path, &json!({})).await?;
    assert_eq!(resp.status(), gated_status);
    assert!(
        state.todo_store.get_avail_by_job(&job).await?.is_some(),
        "job must not be handed out while the gate is up"
    );

    // Clearing the flag (what the reindex pass does) lifts the gate.
    helpers::clear_pending_reindex(&*state.todo_store, &client_id).await?;
    let resp = authed_post(&state, &key, &client_id, path, &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    Ok(())
}

// ── reclaim ───────────────────────────────────────────────────

#[tokio::test]
async fn test_reclaim_renews_own_held_lease() -> anyhow::Result<()> {
    let (dir, state, key, client_id) = approved_client().await?;

    // The client still holds a lease that already expired (the lease the cron
    // would recycle, but hasn't yet) — the canonical outage-recovery case. The
    // in-progress path renews it without an expiry check on the job.
    let job = job("job-1");
    let old = Utc::now() - chrono::Duration::hours(1);
    seed_leased(dir.path(), &job, &client_id, old)?;

    let resp = authed_post(&state, &key, &client_id, "/plans/job-1/reclaim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::OK);

    // The lease was renewed to a future expiry, still owned by this client.
    let renewed = lease_expiry(dir.path(), &job, &client_id)?;
    assert!(
        renewed > old && renewed > Utc::now(),
        "lease expiry {renewed} should have advanced into the future"
    );
    Ok(())
}

#[tokio::test]
async fn test_reclaim_reacquires_job_from_avail() -> anyhow::Result<()> {
    let (dir, state, key, client_id) = approved_client().await?;

    // The lease was recycled back to avail/ (no leased entry for this client),
    // but the client is still eligible and the job is unclaimed. Reclaim
    // re-acquires it.
    let job = seed_eligible(&state, dir.path(), &client_id, "job-1", ExpiresAt::Never).await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/job-1/reclaim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    // 200 carries an empty body (httpapi.md §2.11.3) — the client already has the
    // job JSON from its original claim.
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await?;
    assert!(body.is_empty(), "reclaim 200 body should be empty");

    // The job moved avail/ → leased/, owned by this client.
    let leased = state.todo_store.list_leased().await?;
    assert_eq!(leased.len(), 1);
    let (got_job, got_client, _) = parse_leased_key(&leased[0])?;
    assert_eq!(got_job, job);
    assert_eq!(got_client, client_id);
    Ok(())
}

#[tokio::test]
async fn test_reclaim_409_when_leased_to_another_client() -> anyhow::Result<()> {
    let (dir, state, key, client_id) = approved_client().await?;

    // A different client holds an active lease — the caller is a zombie. → 409.
    let job = job("job-1");
    let other = ClientId::try_new("someone-else")?;
    seed_leased(
        dir.path(),
        &job,
        &other,
        Utc::now() + chrono::Duration::hours(1),
    )?;
    // Even with an eligible marker, the live foreign lease wins.
    state
        .todo_store
        .write_eligible(&client_id, &job, ExpiresAt::Never)
        .await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/job-1/reclaim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    Ok(())
}

#[tokio::test]
async fn test_reclaim_404_when_not_eligible() -> anyhow::Result<()> {
    // No lease and no eligible marker for the job → 404.
    let (_dir, state, key, client_id) = approved_client().await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/ghost/reclaim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn test_reclaim_404_when_gone_from_avail() -> anyhow::Result<()> {
    let (_dir, state, key, client_id) = approved_client().await?;

    // Eligible marker exists but the job is no longer in avail/ (completed, or
    // expired-and-swept; marker awaiting GC). The atomic re-acquire finds no
    // source → 404.
    let job = job("ghost");
    state
        .todo_store
        .write_eligible(&client_id, &job, ExpiresAt::Never)
        .await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/ghost/reclaim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn test_reclaim_404_when_job_expired() -> anyhow::Result<()> {
    let (dir, state, key, client_id) = approved_client().await?;

    // The job is eligible and still in avail/, but past its expires_at — an
    // expired job is never re-acquired (planner.md §Expiration). → 404.
    let expired = ExpiresAt::At(Utc::now() - chrono::Duration::hours(1));
    let job = seed_eligible(&state, dir.path(), &client_id, "job-1", expired).await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/job-1/reclaim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    // The job is left untouched in avail/ for queue-maintenance to expire.
    assert!(state.todo_store.list_leased().await?.is_empty());
    assert!(state.todo_store.get_avail(&job, expired).await?.is_some());
    Ok(())
}

#[tokio::test]
async fn test_reclaim_404_when_denied_for_client() -> anyhow::Result<()> {
    let (dir, state, key, client_id) = approved_client().await?;

    // The client already reported a retriable failure for this job, so it may
    // not take it again. → 404.
    let job = seed_eligible(&state, dir.path(), &client_id, "job-1", ExpiresAt::Never).await?;
    state.todo_store.write_denied(&job, &client_id).await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/job-1/reclaim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn test_reclaim_403_when_pending() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (key, client_id) = register_pending(&state).await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/job-1/reclaim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    Ok(())
}

#[tokio::test]
async fn test_reclaim_403_when_suspended() -> anyhow::Result<()> {
    let (dir, state, key, client_id) = approved_client().await?;

    // A suspended client is refused with 403 (httpapi.md §2.11.4) — not the 204
    // `claim` would return. The check fires even though the client holds a lease.
    let job = job("job-1");
    seed_leased(
        dir.path(),
        &job,
        &client_id,
        Utc::now() + chrono::Duration::hours(1),
    )?;
    state
        .todo_store
        .write_suspension(&client_id, Utc::now(), &job)
        .await?;

    let resp = authed_post(&state, &key, &client_id, "/plans/job-1/reclaim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    Ok(())
}

#[tokio::test]
async fn test_reclaim_401_when_unauthenticated() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let app = test_app(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/plans/job-1/reclaim")
                .header("Content-Type", "application/json")
                .body(Body::from("{}"))?,
        )
        .await?;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    Ok(())
}

// ─── job-completion teardown (POST /benchmarks) ──────────────────────────────

/// Canonical claimed `job_id` for the teardown tests, in the server-minted
/// shape (`job-{uuid}`, see `JobId::from_uuid`) a real claim would carry.
const TEARDOWN_JOB_ID: &str = "job-550e8400-e29b-41d4-a716-446655440000";

/// A lease expiry an hour out — active, and generated rather than a hardcoded
/// date that would rot into the past.
fn future_expiry() -> DateTime<Utc> {
    Utc::now() + chrono::Duration::hours(1)
}

/// Minimal valid `prefill_throughput` success body echoing `job_id` — the
/// shape a planner client submits when a claimed run completes.
fn prefill_success_body(job_id: &str) -> Value {
    json!({
        "benchmark_id": "prefill_throughput_256",
        "job_id": job_id,
        "device_name": "d",
        "device_form_factor": "embedded",
        "device_os_name": "Linux",
        "device_os_version": "1",
        "device_chip_model": "c",
        "device_ram_bytes": 16_000_000_000_i64,
        "model_name": "m",
        "model_quant": "q",
        "runtime_name": "rt",
        "runtime_version": "v1",
        "prefill_time_ms": 10.0,
    })
}

/// A successful submission echoing a claimed `job_id` deletes the submitting
/// client's lease, so `queue-maintenance` can't later recycle it back into
/// `avail/` and hand the finished job out again.
#[tokio::test]
async fn test_submit_success_tears_down_own_lease() -> anyhow::Result<()> {
    let (dir, state, key, client_id) = approved_client().await?;
    let job = job(TEARDOWN_JOB_ID);
    seed_leased(dir.path(), &job, &client_id, future_expiry())?;

    let resp = authed_post(
        &state,
        &key,
        &client_id,
        "/benchmarks",
        &prefill_success_body(TEARDOWN_JOB_ID),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    assert!(
        state.todo_store.list_leased().await?.is_empty(),
        "lease should be deleted on completion"
    );
    Ok(())
}

/// A client whose lease was recycled back to `avail/` (it lost the lease) and
/// submits late is rejected with `404`, not silently accepted: `verify_claim`
/// authorizes against `leased/`, and a job sitting in `avail/` is owned by no
/// one. The client must reclaim first. The `avail/` entry is left untouched —
/// the submission never reaches teardown.
#[tokio::test]
async fn test_submit_recycled_job_without_lease_returns_404() -> anyhow::Result<()> {
    let (dir, state, key, client_id) = approved_client().await?;
    seed_avail(dir.path(), TEARDOWN_JOB_ID, &job_body(TEARDOWN_JOB_ID))?;

    let resp = authed_post(
        &state,
        &key,
        &client_id,
        "/benchmarks",
        &prefill_success_body(TEARDOWN_JOB_ID),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // Rejected before teardown — the recycled job stays claimable for reclaim.
    let avail = state.todo_store.list_avail(None, TEST_LIST_LIMIT).await?;
    assert_eq!(
        avail.len(),
        1,
        "recycled avail/ entry must be left for reclaim"
    );
    Ok(())
}

/// Teardown leaves `denied/` markers alone — they affect only claim
/// eligibility, not re-handout, and are reconciled by `queue-maintenance`
/// (consistent with how `eligible/` markers are handled). A held lease is
/// seeded so the submission passes claim-binding and actually reaches teardown.
#[tokio::test]
async fn test_submit_success_leaves_denied_markers() -> anyhow::Result<()> {
    let (dir, state, key, client_id) = approved_client().await?;
    let job = job(TEARDOWN_JOB_ID);
    seed_leased(dir.path(), &job, &client_id, future_expiry())?;
    let other = ClientId::try_new("ev1_other")?;
    state.todo_store.write_denied(&job, &other).await?;

    let resp = authed_post(
        &state,
        &key,
        &client_id,
        "/benchmarks",
        &prefill_success_body(TEARDOWN_JOB_ID),
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    let denied = state.todo_store.list_denied_for_job(&job).await?;
    assert_eq!(
        denied,
        vec![other],
        "denied/ markers must survive completion"
    );
    Ok(())
}
