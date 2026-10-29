//! Tests for the `queue-maintenance` passes. Jobs are seeded into `avail/` by
//! writing the file directly: `TodoStore` has no `write_avail`.

use std::time::Duration;

use anyhow::Context;
use axum::http::StatusCode;
use chrono::{TimeZone, Utc};
use rstest::rstest;
use serde_json::{Value, json};

use pipette_core::client::{Client, ClientStatus};
use pipette_core::denial::DenialRecord;
use pipette_core::plan::PlanManifest;
use pipette_core::todo_filename::{avail_filename, denied_marker_filename, leased_key};
use pipette_httpapi::state::AppState;
use pipette_pipeline::client_admin;
use pipette_plan_types::{
    ClientId, ClientPermission, DeviceProfile, ExpiresAt, JobId, NonEmptyTrimmedString, PlanId,
    PlanStatus, Tag, UserPermission,
};
use pipette_store::{ClaimResult, JobState, RecycleResult, ScoreQueueStage};
use pipette_web_testkit::{
    approved_client, authed_post, denied_clients, grant, job, make_state, register_and_approve,
    registered_user, run_qm, run_qm_full, run_qm_with_tmp_age, seed_avail, setup_benchmarks,
    submit_plan_as, valid_submission,
};

/// Runs once so the client is already a worker: the run that sees a client join
/// rebuilds its partition, replacing any marker a test seeded before it.
async fn register_indexed_worker(state: &AppState) -> anyhow::Result<ClientId> {
    let (_, client) = register_and_approve(state).await?;
    run_qm(state).await?;
    Ok(client)
}

/// A job body carrying every field `system_failure_from_job_body` requires,
/// with a `spec.benchmark` the `setup_benchmarks` catalog resolves.
fn expirable_body(job_id: &str, clients: &[&ClientId]) -> Value {
    json!({
        "job_id": job_id,
        "clients": clients.iter().map(|c| c.as_str()).collect::<Vec<_>>(),
        "spec": {
            "benchmark": "prefill_throughput_256",
            "model": {"type": "gguf_text", "source": "huggingface", "org": "LiquidAI", "repo_name": "LFM2-700M-GGUF", "path": "LFM2-700M-Q4_0.gguf"},
            "runtime": {"type": "llamacpp_cli_stock_tools", "source": "github_release", "repository_url": "github.com/ggml-org/llama.cpp", "repository_version": "b1000", "flavor": "macos-arm64"},
        },
    })
}

async fn set_profile(
    state: &AppState,
    client_id: &ClientId,
    profile: DeviceProfile,
) -> anyhow::Result<()> {
    let mut client: Client = state
        .client_store
        .get_client(client_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("client {client_id} not found"))?;
    client.device_profile = profile;
    state.client_store.put_client(&client).await?;
    Ok(())
}

fn os_profile(os_name: &str) -> anyhow::Result<DeviceProfile> {
    Ok(DeviceProfile {
        device_os_name: Some(NonEmptyTrimmedString::try_new(os_name)?),
        ..Default::default()
    })
}

fn device_profile(os_name: &str, device_name: &str) -> anyhow::Result<DeviceProfile> {
    Ok(DeviceProfile {
        device_name: Some(NonEmptyTrimmedString::try_new(device_name)?),
        ..os_profile(os_name)?
    })
}

/// The two inputs the eligible index routes a job by.
#[derive(Debug, Clone, Copy)]
enum RoutedBy {
    Profile,
    Tag,
}

impl RoutedBy {
    fn requires(self) -> Value {
        match self {
            Self::Profile => json!(["os:macos"]),
            Self::Tag => json!(["tag:lab"]),
        }
    }

    /// Make `client` match [`Self::requires`], or stop it matching.
    async fn set(self, state: &AppState, client: &ClientId, matching: bool) -> anyhow::Result<()> {
        let lab = Tag::try_new("lab")?;
        match (self, matching) {
            (Self::Profile, true) => set_profile(state, client, os_profile("macOS")?).await,
            (Self::Profile, false) => set_profile(state, client, os_profile("Linux")?).await,
            (Self::Tag, true) => state.tag_store.add_client_tag(client, &lab).await,
            (Self::Tag, false) => state.tag_store.remove_client_tag(client, &lab).await,
        }
    }
}

async fn eligible_jobs(state: &AppState, client_id: &ClientId) -> anyhow::Result<Vec<JobId>> {
    Ok(state
        .todo_store
        .list_eligible_for_client(client_id)
        .await?
        .into_iter()
        .map(|(job_id, _)| job_id)
        .collect())
}

#[rstest]
#[case::profile(RoutedBy::Profile)]
#[case::tag(RoutedBy::Tag)]
#[tokio::test]
async fn test_new_job_indexed_only_for_matching_clients(
    #[case] by: RoutedBy,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let (_, matching) = register_and_approve(&state).await?;
    let (_, other) = register_and_approve(&state).await?;
    by.set(&state, &matching, true).await?;
    by.set(&state, &other, false).await?;

    let job = seed_avail(
        dir.path(),
        "job-1",
        ExpiresAt::Never,
        &json!({"requires": by.requires()}),
    )?;

    run_qm(&state).await?;

    assert_eq!(eligible_jobs(&state, &matching).await?, vec![job]);
    assert!(eligible_jobs(&state, &other).await?.is_empty());
    Ok(())
}

/// One check of `claim` that keeps a client out of the workers.
#[derive(Debug, Clone, Copy)]
enum Gate {
    Approval,
    PerformWork,
}

impl Gate {
    /// Close this gate for `client`, or open it again.
    async fn set(self, state: &AppState, client: &ClientId, open: bool) -> anyhow::Result<()> {
        match self {
            Self::Approval => {
                let to = if open {
                    ClientStatus::Approved
                } else {
                    ClientStatus::Pending
                };
                client_admin::set_status(state.client_store.as_ref(), client, to).await?;
                Ok(())
            }
            Self::PerformWork if open => {
                grant(
                    state,
                    client,
                    &[
                        ClientPermission::PerformWork,
                        ClientPermission::SubmitResults,
                    ],
                )
                .await
            }
            Self::PerformWork => grant(state, client, &[ClientPermission::SubmitResults]).await,
        }
    }
}

/// A client that joins the workers gets markers for jobs already queued; one
/// that leaves loses its markers.
#[rstest]
#[case::approval(Gate::Approval)]
#[case::perform_work(Gate::PerformWork)]
#[tokio::test]
async fn test_eligible_index_follows_the_workers(#[case] gate: Gate) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let (_, worker) = register_and_approve(&state).await?;
    let (_, outsider) = register_and_approve(&state).await?;
    gate.set(&state, &outsider, false).await?;
    let job = seed_avail(
        dir.path(),
        "job-1",
        ExpiresAt::Never,
        &json!({"clients": [worker.as_str(), outsider.as_str()]}),
    )?;

    run_qm(&state).await?;
    assert_eq!(eligible_jobs(&state, &worker).await?, vec![job.clone()]);
    assert!(eligible_jobs(&state, &outsider).await?.is_empty());

    gate.set(&state, &outsider, true).await?;
    run_qm(&state).await?;
    assert_eq!(eligible_jobs(&state, &outsider).await?, vec![job.clone()]);

    gate.set(&state, &worker, false).await?;
    run_qm(&state).await?;
    assert!(eligible_jobs(&state, &worker).await?.is_empty());
    assert_eq!(eligible_jobs(&state, &outsider).await?, vec![job]);
    Ok(())
}

#[tokio::test]
async fn test_any_of_job_indexed_only_for_group_matching_clients() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let (_, in_family) = register_and_approve(&state).await?;
    let (_, out_of_family) = register_and_approve(&state).await?;
    set_profile(&state, &in_family, device_profile("iOS", "iPhone 17 Pro")?).await?;
    set_profile(&state, &out_of_family, device_profile("iOS", "iPhone 15")?).await?;

    // Both clients satisfy `requires`; only the one in the device family
    // satisfies the `any_of` group.
    let job = seed_avail(
        dir.path(),
        "job-1",
        ExpiresAt::Never,
        &json!({
            "requires": ["os:ios"],
            "any_of": [["device:iphone17pro", "device:iphone18"]],
        }),
    )?;

    run_qm(&state).await?;

    assert_eq!(eligible_jobs(&state, &in_family).await?, vec![job]);
    assert!(eligible_jobs(&state, &out_of_family).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn test_clients_array_job_indexed() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let (_, listed) = register_and_approve(&state).await?;
    let (_, unlisted) = register_and_approve(&state).await?;

    // No device profiles set — eligibility is purely the explicit list.
    let job = seed_avail(
        dir.path(),
        "job-1",
        ExpiresAt::Never,
        &json!({"clients": [listed.as_str()]}),
    )?;

    run_qm(&state).await?;

    assert_eq!(eligible_jobs(&state, &listed).await?, vec![job]);
    assert!(eligible_jobs(&state, &unlisted).await?.is_empty());
    Ok(())
}

/// The backlog snapshot serves the console only, so a failed write must not fail
/// the run. A directory at its path makes the write fail.
#[tokio::test]
async fn test_a_failed_backlog_write_does_not_fail_the_run() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (_, client) = register_and_approve(&state).await?;
    std::fs::create_dir_all(dir.path().join("todo").join(".backlog"))?;
    let job = seed_avail(
        dir.path(),
        "job-1",
        ExpiresAt::Never,
        &json!({"clients": [client.as_str()]}),
    )?;

    run_qm(&state).await?;

    assert_eq!(eligible_jobs(&state, &client).await?, vec![job]);
    assert!(dir.path().join("todo").join(".backlog").is_dir());
    assert!(state.todo_store.read_backlog().await.is_err());
    Ok(())
}

/// A key sorting below an already-indexed one is still indexed: `Uuid::now_v7`
/// orders ids per process only, so "new" comes from the indexed set, not order.
#[tokio::test]
async fn test_out_of_order_promotion_is_indexed() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let (_, client) = register_and_approve(&state).await?;
    let body = json!({"clients": [client.as_str()]});

    let job_b = seed_avail(dir.path(), "job-b", ExpiresAt::Never, &body)?;
    run_qm(&state).await?;
    assert_eq!(eligible_jobs(&state, &client).await?, vec![job_b.clone()]);

    // `job-a` sorts below `job-b`, so an order-based pass would never see it.
    let job_a = seed_avail(dir.path(), "job-a", ExpiresAt::Never, &body)?;
    run_qm(&state).await?;

    let mut eligible = eligible_jobs(&state, &client).await?;
    eligible.sort();
    assert_eq!(eligible, vec![job_a, job_b]);
    Ok(())
}

#[tokio::test]
async fn test_already_indexed_jobs_are_skipped() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let (_, client) = register_and_approve(&state).await?;
    let job = seed_avail(
        dir.path(),
        "job-1",
        ExpiresAt::Never,
        &json!({"clients": [client.as_str()]}),
    )?;

    run_qm(&state).await?;
    assert_eq!(eligible_jobs(&state, &client).await?, vec![job.clone()]);

    // No new jobs and no reindex flags: the marker stays deleted only if the
    // incremental pass skipped the indexed key.
    state
        .todo_store
        .delete_eligible(&client, &job, ExpiresAt::Never)
        .await?;
    run_qm(&state).await?;
    assert!(eligible_jobs(&state, &client).await?.is_empty());
    Ok(())
}

/// The run's snapshot omits a departed key, which bounds the set's growth.
#[tokio::test]
async fn test_departed_job_is_pruned_from_the_indexed_set() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let (_, client) = register_and_approve(&state).await?;
    let job = seed_avail(
        dir.path(),
        "job-1",
        ExpiresAt::Never,
        &json!({"clients": [client.as_str()]}),
    )?;
    let key = avail_filename(&job, ExpiresAt::Never);

    run_qm(&state).await?;
    let indexed = state.todo_store.read_eligible_indexed().await?;
    assert!(indexed.contains(&key));

    std::fs::remove_file(dir.path().join("todo").join("avail").join(&key))?;
    run_qm(&state).await?;
    let indexed = state.todo_store.read_eligible_indexed().await?;
    assert!(!indexed.contains(&key));
    Ok(())
}

/// A lease does not un-index its job, so a recycled lease is not re-indexed: the
/// hand-deleted marker stays deleted only if the pass skipped the job.
#[tokio::test]
async fn test_recycled_lease_is_not_reindexed() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let (_, client) = register_and_approve(&state).await?;
    let (_, holder) = register_and_approve(&state).await?;
    let job = seed_avail(
        dir.path(),
        "job-1",
        ExpiresAt::Never,
        &json!({"job_id": "job-1", "clients": [client.as_str(), holder.as_str()]}),
    )?;

    run_qm(&state).await?;
    assert_eq!(eligible_jobs(&state, &client).await?, vec![job.clone()]);

    // The lease outlives this run, so the recycle below is ours, not the
    // expired-lease pass's.
    let lease_expiry = Utc::now() + chrono::Duration::hours(1);
    let claimed = state
        .todo_store
        .claim_job(&job, ExpiresAt::Never, &holder, lease_expiry)
        .await?;
    assert!(matches!(claimed, ClaimResult::Claimed(_)));
    run_qm(&state).await?;

    // A pass that treated the returning job as new would rebuild the marker.
    let recycled = state
        .todo_store
        .recycle_lease(&job, ExpiresAt::Never, &holder, lease_expiry)
        .await?;
    assert!(matches!(recycled, RecycleResult::Recycled));
    state
        .todo_store
        .delete_eligible(&client, &job, ExpiresAt::Never)
        .await?;

    run_qm(&state).await?;
    assert!(eligible_jobs(&state, &client).await?.is_empty());
    Ok(())
}

#[rstest]
#[case::profile(RoutedBy::Profile)]
#[case::tag(RoutedBy::Tag)]
#[tokio::test]
async fn test_pending_reindex_reevaluates_after_a_profile_or_tag_change(
    #[case] by: RoutedBy,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let (_, client) = register_and_approve(&state).await?;
    let job = seed_avail(
        dir.path(),
        "job-1",
        ExpiresAt::Never,
        &json!({"requires": by.requires()}),
    )?;

    // Not matched yet, and the job's key joins the indexed set.
    run_qm(&state).await?;
    assert!(eligible_jobs(&state, &client).await?.is_empty());

    // The client now matches; flag it. Only the reindex pass (not the
    // incremental new-jobs pass) can pick up the already-indexed job.
    by.set(&state, &client, true).await?;
    state.todo_store.write_pending_reindex(&client).await?;

    run_qm(&state).await?;
    assert_eq!(eligible_jobs(&state, &client).await?, vec![job]);
    assert!(state.todo_store.list_pending_reindex().await?.is_empty());

    // And back: the rebuild wipes the marker the client no longer earns.
    by.set(&state, &client, false).await?;
    state.todo_store.write_pending_reindex(&client).await?;

    run_qm(&state).await?;
    assert!(eligible_jobs(&state, &client).await?.is_empty());
    Ok(())
}

/// Cruft in the flag directories (`.` is outside both id charsets) is skipped
/// with a warning and left in place; the run and real flags beside it proceed.
#[tokio::test]
async fn test_foreign_flag_files_do_not_wedge_the_run() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let (_, client) = register_and_approve(&state).await?;
    let job = seed_avail(
        dir.path(),
        "job-1",
        ExpiresAt::Never,
        &json!({"requires": ["os:macos"]}),
    )?;
    set_profile(&state, &client, os_profile("macOS")?).await?;
    state.todo_store.write_pending_reindex(&client).await?;

    let cruft_client = dir
        .path()
        .join("todo")
        .join("pending-reindex")
        .join(".DS_Store");
    let cruft_jobs = dir
        .path()
        .join("todo")
        .join("pending-reindex-jobs")
        .join(".DS_Store");
    std::fs::write(&cruft_client, b"")?;
    std::fs::write(&cruft_jobs, b"")?;

    run_qm(&state).await?;
    assert_eq!(eligible_jobs(&state, &client).await?, vec![job]);
    assert!(state.todo_store.list_pending_reindex().await?.is_empty());
    assert!(cruft_client.try_exists()?);
    assert!(cruft_jobs.try_exists()?);
    Ok(())
}

/// A profile-changing `PATCH /clients/me` writes two flag keys; one rebuild must
/// consume both, or a leftover triggers a redundant one.
#[tokio::test]
async fn test_reindex_consumes_all_flag_keys_in_one_rebuild() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let (_, client) = register_and_approve(&state).await?;
    let job = seed_avail(
        dir.path(),
        "job-1",
        ExpiresAt::Never,
        &json!({"requires": ["os:macos"]}),
    )?;
    set_profile(&state, &client, os_profile("macOS")?).await?;
    state.todo_store.write_pending_reindex(&client).await?;
    state.todo_store.write_pending_reindex(&client).await?;
    assert_eq!(state.todo_store.list_pending_reindex().await?.len(), 2);

    run_qm(&state).await?;
    assert_eq!(eligible_jobs(&state, &client).await?, vec![job]);
    assert!(state.todo_store.list_pending_reindex().await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn test_reindex_cleans_up_deleted_client() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let (_, client) = register_and_approve(&state).await?;
    let job = seed_avail(
        dir.path(),
        "job-1",
        ExpiresAt::Never,
        &json!({"clients": [client.as_str()]}),
    )?;
    state
        .todo_store
        .write_eligible(&client, &job, ExpiresAt::Never)
        .await?;

    // Gone from the auth store, but with a marker and the two flags a profile
    // change leaves.
    state.client_store.delete_client(&client).await?;
    state.todo_store.write_pending_reindex(&client).await?;
    state.todo_store.write_pending_reindex(&client).await?;

    run_qm(&state).await?;

    assert!(eligible_jobs(&state, &client).await?.is_empty());
    assert!(state.todo_store.list_pending_reindex().await?.is_empty());
    Ok(())
}

/// A job leased while a client reindexes is flagged, waits out the lease, and is
/// re-matched against current profiles and tags when it returns to `avail/`.
#[rstest]
#[case::profile_still_matches(RoutedBy::Profile, true)]
#[case::profile_no_longer_matches(RoutedBy::Profile, false)]
#[case::tag_still_matches(RoutedBy::Tag, true)]
#[case::tag_no_longer_matches(RoutedBy::Tag, false)]
#[tokio::test]
async fn test_deferred_reindex_settles_when_leased_job_recycles(
    #[case] by: RoutedBy,
    #[case] expect_marker: bool,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let (_, client) = register_and_approve(&state).await?;
    let (_, other) = register_and_approve(&state).await?;
    by.set(&state, &client, true).await?;
    let job = seed_avail(
        dir.path(),
        "job-1",
        ExpiresAt::Never,
        &json!({"job_id": "job-1", "requires": by.requires()}),
    )?;

    run_qm(&state).await?;
    assert_eq!(eligible_jobs(&state, &client).await?, vec![job.clone()]);

    let lease_expiry = Utc::now() + chrono::Duration::hours(1);
    let claimed = state
        .todo_store
        .claim_job(&job, ExpiresAt::Never, &other, lease_expiry)
        .await?;
    assert!(matches!(claimed, ClaimResult::Claimed(_)));
    by.set(&state, &client, expect_marker).await?;
    state.todo_store.write_pending_reindex(&client).await?;

    // Run 2: the leased job is invisible to the avail/-sourced rebuild, so it is
    // flagged for settling.
    run_qm(&state).await?;
    assert!(eligible_jobs(&state, &client).await?.is_empty());
    assert_eq!(
        state.todo_store.list_pending_reindex_jobs().await?,
        vec![job.clone()]
    );

    // Run 3: the job is still leased — the flag waits.
    run_qm(&state).await?;
    assert_eq!(
        state.todo_store.list_pending_reindex_jobs().await?,
        vec![job.clone()]
    );

    // Run 4 settles the flag against the current profiles and tags.
    let recycled = state
        .todo_store
        .recycle_lease(&job, ExpiresAt::Never, &other, lease_expiry)
        .await?;
    assert!(matches!(recycled, RecycleResult::Recycled));
    run_qm(&state).await?;

    assert!(
        state
            .todo_store
            .list_pending_reindex_jobs()
            .await?
            .is_empty()
    );
    assert_eq!(
        !eligible_jobs(&state, &client).await?.is_empty(),
        expect_marker,
        "marker after settle should reflect the current profile and tags"
    );
    // The client with no profile and no tag never matches, in either direction.
    assert!(eligible_jobs(&state, &other).await?.is_empty());
    Ok(())
}

/// A flagged job with a record has its flag cleared; one found nowhere and
/// recordless (maybe mid-transition) keeps it for a later run.
#[tokio::test]
async fn test_deferred_reindex_flag_cleared_for_terminal_job() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let done = job("job-done")?;
    let ghost = job("job-ghost")?;
    state
        .submission_store
        .write_processed(&done, &json!({"job_id": "job-done"}))
        .await?;
    state.todo_store.write_pending_reindex_job(&done).await?;
    state.todo_store.write_pending_reindex_job(&ghost).await?;

    run_qm(&state).await?;

    assert_eq!(
        state.todo_store.list_pending_reindex_jobs().await?,
        vec![ghost]
    );
    Ok(())
}

#[tokio::test]
async fn test_gc_removes_orphan_markers_but_keeps_live_ones() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let client = register_indexed_worker(&state).await?;

    let live = seed_avail(
        dir.path(),
        "job-live",
        ExpiresAt::Never,
        &json!({"clients": [client.as_str()]}),
    )?;
    let orphan = job("job-orphan")?;
    state
        .todo_store
        .write_eligible(&client, &orphan, ExpiresAt::Never)
        .await?;

    // Deletion needs two consecutive orphaned sightings, so one stale listing
    // cannot cost a live job its markers.
    run_qm(&state).await?;
    let eligible = eligible_jobs(&state, &client).await?;
    assert!(eligible.contains(&live), "live job marker should be kept");
    assert!(
        eligible.contains(&orphan),
        "first sighting should not delete the orphan marker"
    );

    run_qm(&state).await?;
    let eligible = eligible_jobs(&state, &client).await?;
    assert!(eligible.contains(&live), "live job marker should be kept");
    assert!(!eligible.contains(&orphan), "orphan marker should be GC'd");
    Ok(())
}

/// How run 2 clears the candidacy. `FailsFatally` dies after the live-set
/// listings but before the GC sweeps (a file swapped in for `pending-reindex`).
enum Run2 {
    Succeeds,
    FailsFatally,
}

/// Job absent, present, absent, absent: run 3 must be a fresh first sighting
/// (marker kept), and run 4 completes the pair and sweeps the marker.
async fn assert_candidate_resets(
    state: &AppState,
    dir: &std::path::Path,
    job_name: &str,
    run2: Run2,
) -> anyhow::Result<()> {
    let client = register_indexed_worker(state).await?;
    let job = job(job_name)?;
    // No denied marker: a fully-denied clients-only job would be escalated by
    // another pass.
    state
        .todo_store
        .write_eligible(&client, &job, ExpiresAt::Never)
        .await?;

    run_qm(state).await?;
    assert!(
        eligible_jobs(state, &client).await?.contains(&job),
        "first sighting must not delete the marker"
    );

    seed_avail(
        dir,
        job_name,
        ExpiresAt::Never,
        &json!({"clients": [client.as_str()]}),
    )?;
    match run2 {
        Run2::Succeeds => run_qm(state).await?,
        Run2::FailsFatally => {
            // Dies before the end-of-run candidate rewrite; the start-of-run consume
            // must already have cleared the candidacy.
            let pending_reindex = dir.join("todo").join("pending-reindex");
            std::fs::remove_dir(&pending_reindex)?;
            std::fs::write(&pending_reindex, b"")?;
            assert!(
                run_qm(state).await.is_err(),
                "run 2 should fail fatally at the reindex pass"
            );
            std::fs::remove_file(&pending_reindex)?;
            std::fs::create_dir(&pending_reindex)?;
        }
    }
    assert!(
        eligible_jobs(state, &client).await?.contains(&job),
        "live job's marker must be kept"
    );

    std::fs::remove_file(dir.join("todo").join("avail").join(
        pipette_core::todo_filename::avail_filename(&job, ExpiresAt::Never),
    ))?;
    run_qm(state).await?;
    assert!(
        eligible_jobs(state, &client).await?.contains(&job),
        "marker must survive run 3: run 2 cleared the candidacy, so this is a fresh first sighting"
    );

    run_qm(state).await?;
    assert!(
        !eligible_jobs(state, &client).await?.contains(&job),
        "marker should be GC'd on the second consecutive orphaned sighting"
    );
    Ok(())
}

/// A transient listing miss must not leave a "strike one" that a later,
/// unrelated miss completes.
#[tokio::test]
async fn test_gc_candidate_resets_when_job_reappears() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    assert_candidate_resets(&state, dir.path(), "job-flapping", Run2::Succeeds).await
}

/// The candidate set is consumed at the start of each run, so a run that dies
/// partway leaves no sightings for the next run to pair with.
#[tokio::test]
async fn test_gc_candidate_cleared_by_failed_run() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    assert_candidate_resets(&state, dir.path(), "job-strike", Run2::FailsFatally).await
}

/// A pre-migration `{job_id}.{expires_at}.json` key would drop out of the live
/// set as cruft, and its `job-plans/` markers would be swept for good.
#[tokio::test]
async fn test_a_single_segment_avail_key_stops_the_run() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (_, client) = register_and_approve(&state).await?;

    // Written directly: `seed_avail` builds the per-job form.
    let avail = dir.path().join("todo").join("avail");
    std::fs::create_dir_all(&avail)?;
    std::fs::write(
        avail.join("job-1.never.json"),
        serde_json::to_vec(&json!({"clients": [client.as_str()]}))?,
    )?;

    let err = run_qm(&state)
        .await
        .err()
        .context("a single-segment avail/ key stops the run")?;
    let msg = format!("{err:#}");
    assert!(
        msg.contains("drain the queue"),
        "the error says what fixes it: {msg}"
    );

    Ok(())
}

/// A live lease is left alone and an expired one recycled; either way the job
/// is live, so its eligible marker survives the GC sweep.
#[rstest]
#[case::live_lease(chrono::Duration::hours(1), false)]
#[case::expired_lease(-chrono::Duration::minutes(1), true)]
#[tokio::test]
async fn test_leased_job_survives_sweep_and_recycles_on_expiry(
    #[case] lease_offset: chrono::Duration,
    #[case] expect_recycled: bool,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let (_, client) = register_and_approve(&state).await?;
    let job = seed_avail(
        dir.path(),
        "job-1",
        ExpiresAt::Never,
        &json!({"clients": [client.as_str()]}),
    )?;
    run_qm(&state).await?;
    assert_eq!(eligible_jobs(&state, &client).await?, vec![job.clone()]);

    let claimed = state
        .todo_store
        .claim_job(&job, ExpiresAt::Never, &client, Utc::now() + lease_offset)
        .await?;
    assert!(matches!(claimed, ClaimResult::Claimed(_)));

    run_qm(&state).await?;

    if expect_recycled {
        assert!(state.todo_store.list_leased().await?.is_empty());
        assert!(
            state.todo_store.get_avail_by_job(&job).await?.is_some(),
            "recycled job should be back in avail/"
        );
    } else {
        assert_eq!(state.todo_store.list_leased().await?.len(), 1);
    }
    assert_eq!(eligible_jobs(&state, &client).await?, vec![job]);
    Ok(())
}

/// An expired body with no `spec` is deleted, not retried: unlike an
/// unresolvable benchmark (`unknown_benchmark_left_for_retry`), no catalog
/// change restores it. Ingestion refuses such a body, so no plan lists it.
#[rstest]
#[case::no_spec(json!({"job_id": "job-1"}))]
#[case::spec_without_benchmark(json!({"job_id": "job-1", "spec": {}}))]
#[case::empty_benchmark(json!({"job_id": "job-1", "spec": {"benchmark": ""}}))]
#[tokio::test]
async fn test_expired_job_with_unrecordable_body_is_deleted(
    #[case] body: Value,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let (_, client) = register_and_approve(&state).await?;
    let deadline = Utc
        .with_ymd_and_hms(2020, 1, 2, 3, 4, 5)
        .single()
        .ok_or_else(|| anyhow::anyhow!("ambiguous or invalid datetime"))?;
    let job = seed_avail(dir.path(), "job-1", ExpiresAt::At(deadline), &body)?;
    state
        .todo_store
        .write_denied(&job, &client, &DenialRecord::default())
        .await?;
    state
        .todo_store
        .write_eligible(&client, &job, ExpiresAt::At(deadline))
        .await?;

    // A permanent defect is resolved, not retried: as an error, cron monitoring
    // would alert forever.
    run_qm(&state).await?;

    assert!(state.todo_store.get_avail_by_job(&job).await?.is_none());
    assert!(state.submission_store.get_submission(&job).await?.is_none());

    // Positively removed, so swept in this run, not by the two-sighting orphan path.
    assert!(denied_clients(&state, &job).await?.is_empty());
    assert!(eligible_jobs(&state, &client).await?.is_empty());

    run_qm(&state).await?;
    Ok(())
}

/// An expired job with a resolvable benchmark becomes a synthetic `"system"`
/// failure, torn down in one run. An unresolvable one stays in `avail/` with
/// its markers and the run fails, so cron monitoring surfaces it.
#[rstest]
#[case::known_benchmark_recorded(None, true)]
#[case::unknown_benchmark_left_for_retry(Some("no_such_benchmark"), false)]
#[tokio::test]
async fn test_expired_job_teardown(
    #[case] benchmark_override: Option<&str>,
    #[case] expect_recorded: bool,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let (_, client) = register_and_approve(&state).await?;
    let deadline = Utc
        .with_ymd_and_hms(2020, 1, 2, 3, 4, 5)
        .single()
        .ok_or_else(|| anyhow::anyhow!("ambiguous or invalid datetime"))?;
    let mut body = expirable_body("job-1", &[&client]);
    if let Some(benchmark_id) = benchmark_override {
        body["spec"]["benchmark"] = json!(benchmark_id);
    }
    let job = seed_avail(dir.path(), "job-1", ExpiresAt::At(deadline), &body)?;
    state
        .todo_store
        .write_denied(&job, &client, &DenialRecord::default())
        .await?;
    state
        .todo_store
        .write_eligible(&client, &job, ExpiresAt::At(deadline))
        .await?;

    let run_result = run_qm(&state).await;

    if expect_recorded {
        run_result?;
        let record = state
            .submission_store
            .get_submission(&job)
            .await?
            .ok_or_else(|| anyhow::anyhow!("no submission record for expired job"))?;
        assert_eq!(record.state, JobState::Incoming);
        assert_eq!(record.body["client_id"], "system");
        assert_eq!(record.body["retriable"], false);
        assert_eq!(record.body["failure_source"], "expired");
        assert_eq!(
            record.body["failure_reason"],
            "Job expired at 2020-01-02T03:04:05Z before any client completed it"
        );

        assert!(state.todo_store.get_avail_by_job(&job).await?.is_none());
        assert!(denied_clients(&state, &job).await?.is_empty());
        assert!(eligible_jobs(&state, &client).await?.is_empty());
    } else {
        assert!(run_result.is_err());
        assert!(
            state.todo_store.get_avail_by_job(&job).await?.is_some(),
            "unexpirable job should stay in avail/ for the next run"
        );
        assert!(state.submission_store.get_submission(&job).await?.is_none());
        // A swept marker is never rebuilt (the key is in the indexed set), so
        // sweeping it would leave the job unclaimable.
        assert_eq!(denied_clients(&state, &job).await?, vec![client.clone()]);
        assert_eq!(eligible_jobs(&state, &client).await?, vec![job]);
    }
    Ok(())
}

/// The body carries `expires_at`: `recycle_lease` derives the `avail/` key from
/// it, and without it the entry recycles to `.never.json` and never expires.
#[tokio::test]
async fn test_expired_lease_on_expired_job_resolves_in_one_run() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let (_, client) = register_and_approve(&state).await?;
    let deadline = Utc
        .with_ymd_and_hms(2020, 1, 2, 3, 4, 5)
        .single()
        .ok_or_else(|| anyhow::anyhow!("ambiguous or invalid datetime"))?;
    let mut body = expirable_body("job-1", &[&client]);
    body["expires_at"] = json!(ExpiresAt::At(deadline).to_string());
    let job = seed_avail(dir.path(), "job-1", ExpiresAt::At(deadline), &body)?;

    let claimed = state
        .todo_store
        .claim_job(
            &job,
            ExpiresAt::At(deadline),
            &client,
            Utc::now() - chrono::Duration::minutes(1),
        )
        .await?;
    assert!(matches!(claimed, ClaimResult::Claimed(_)));

    state
        .todo_store
        .write_eligible(&client, &job, ExpiresAt::At(deadline))
        .await?;
    state
        .todo_store
        .write_denied(&job, &client, &DenialRecord::default())
        .await?;

    run_qm(&state).await?;

    assert!(state.todo_store.list_leased().await?.is_empty());
    assert!(state.todo_store.get_avail_by_job(&job).await?.is_none());
    let record = state
        .submission_store
        .get_submission(&job)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no synthetic failure for expired job"))?;
    assert_eq!(record.state, JobState::Incoming);
    assert_eq!(record.body["client_id"], "system");

    assert!(
        eligible_jobs(&state, &client).await?.is_empty(),
        "eligible marker should be swept in the same run the job expired"
    );
    assert!(
        denied_clients(&state, &job).await?.is_empty(),
        "denied marker should be swept in the same run the job expired"
    );
    Ok(())
}

/// A stale lease whose job has a record is deleted, not recycled: recycling
/// would make a finished job claimable again. The never-denied second client
/// keeps the all-denied escalation from resolving the job instead.
#[tokio::test]
async fn test_stale_lease_for_recorded_job_deleted_not_recycled() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let (_, client) = register_and_approve(&state).await?;
    let other = ClientId::try_new("never-denied")?;
    let job = seed_avail(
        dir.path(),
        "job-1",
        ExpiresAt::Never,
        &expirable_body("job-1", &[&client, &other]),
    )?;
    let claimed = state
        .todo_store
        .claim_job(
            &job,
            ExpiresAt::Never,
            &client,
            Utc::now() - chrono::Duration::minutes(1),
        )
        .await?;
    assert!(matches!(claimed, ClaimResult::Claimed(_)));

    // The best-effort teardown left the expired lease behind.
    let real_record = json!({
        "job_id": "job-1",
        "client_id": client.as_str(),
        "message_type": "success",
        "marker": "real result, not synthetic",
    });
    state
        .submission_store
        .write_processed(&job, &real_record)
        .await?;

    state
        .todo_store
        .write_eligible(&client, &job, ExpiresAt::Never)
        .await?;
    state
        .todo_store
        .write_denied(&job, &client, &DenialRecord::default())
        .await?;

    run_qm(&state).await?;

    assert!(state.todo_store.list_leased().await?.is_empty());
    assert!(state.todo_store.get_avail_by_job(&job).await?.is_none());
    // The existing record survives byte-for-byte — no synthetic write.
    let record = state
        .submission_store
        .find_job(&job)
        .await?
        .ok_or_else(|| anyhow::anyhow!("existing record vanished"))?;
    assert_eq!(record.body, real_record);
    assert!(
        eligible_jobs(&state, &client).await?.is_empty(),
        "eligible marker should be swept in the same run the stale lease resolved"
    );
    assert!(
        denied_clients(&state, &job).await?.is_empty(),
        "denied marker should be swept in the same run the stale lease resolved"
    );
    Ok(())
}

async fn lose_lease(
    state: &AppState,
    job: &JobId,
    client: &ClientId,
    minutes_ago: i64,
) -> anyhow::Result<()> {
    let lapsed = Utc::now() - chrono::Duration::minutes(minutes_ago);
    let claimed = state
        .todo_store
        .claim_job(job, ExpiresAt::Never, client, lapsed)
        .await?;
    anyhow::ensure!(matches!(claimed, ClaimResult::Claimed(_)), "job was queued");
    run_qm(state).await
}

/// Losses below `plan_lost_lease_limit` recycle the job, and the holder may
/// claim it again: a lost lease bars no one.
#[tokio::test]
async fn test_job_retired_once_its_lost_leases_reach_the_limit() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (_, client) = register_and_approve(&state).await?;
    let job = seed_avail(
        dir.path(),
        "job-1",
        ExpiresAt::Never,
        &expirable_body("job-1", &[&client]),
    )?;
    run_qm(&state).await?;

    for (loss, minutes_ago) in [(1, 30), (2, 20)] {
        lose_lease(&state, &job, &client, minutes_ago).await?;
        assert!(
            state.todo_store.get_avail_by_job(&job).await?.is_some(),
            "loss {loss} recycles the job"
        );
        assert_eq!(eligible_jobs(&state, &client).await?, vec![job.clone()]);
        assert!(denied_clients(&state, &job).await?.is_empty());
        assert_eq!(state.todo_store.list_lost_leases(&job).await?.len(), loss);
    }
    assert!(state.submission_store.get_submission(&job).await?.is_none());

    lose_lease(&state, &job, &client, 10).await?;

    let record = state
        .submission_store
        .get_submission(&job)
        .await?
        .context("the third loss retires the job")?;
    assert_eq!(record.body["client_id"], "system");
    assert_eq!(record.body["failure_source"], "lease_lost");
    assert_eq!(record.body["retriable"], false);
    assert_eq!(
        record.body["failure_reason"],
        format!("Lease lost without a result 3 times (clients: {client})")
    );
    assert!(state.todo_store.list_leased().await?.is_empty());
    assert!(state.todo_store.get_avail_by_job(&job).await?.is_none());
    assert!(state.todo_store.list_lost_lease_jobs().await?.is_empty());
    assert!(eligible_jobs(&state, &client).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn test_lost_lease_limit_zero_always_recycles() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = pipette_web_testkit::make_state_from_config(pipette_core::config::Config {
        plan_lost_lease_limit: 0,
        ..pipette_web_testkit::test_config(dir.path())
    })
    .await?;
    let (_, client) = register_and_approve(&state).await?;
    let job = seed_avail(
        dir.path(),
        "job-1",
        ExpiresAt::Never,
        &expirable_body("job-1", &[&client]),
    )?;
    run_qm(&state).await?;

    for minutes_ago in [40, 30, 20, 10] {
        lose_lease(&state, &job, &client, minutes_ago).await?;
        assert!(state.todo_store.get_avail_by_job(&job).await?.is_some());
    }
    assert!(state.todo_store.list_lost_lease_jobs().await?.is_empty());
    assert!(state.submission_store.get_submission(&job).await?.is_none());
    Ok(())
}

/// A job's `lost/` markers go once it leaves the queue (here, by a result), on
/// the second orphaned sighting; a live job keeps them.
#[tokio::test]
async fn test_lost_lease_markers_swept_with_their_job() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (_, client) = register_and_approve(&state).await?;
    let live = seed_avail(
        dir.path(),
        "job-live",
        ExpiresAt::Never,
        &expirable_body("job-live", &[&client]),
    )?;
    let done = job("job-done")?;
    let lapsed = Utc::now() - chrono::Duration::hours(1);
    for job_id in [&live, &done] {
        state
            .todo_store
            .write_lost_lease(job_id, &client, lapsed)
            .await?;
    }

    let lost_jobs = || async {
        let mut jobs = state.todo_store.list_lost_lease_jobs().await?;
        jobs.sort();
        anyhow::Ok(jobs)
    };
    run_qm(&state).await?;
    assert_eq!(
        lost_jobs().await?,
        vec![done.clone(), live.clone()],
        "the first orphaned sighting only stages the dead job's markers"
    );
    run_qm(&state).await?;
    assert_eq!(lost_jobs().await?, vec![live]);
    Ok(())
}

/// Only a planner double-write puts a job in both `avail/` and `leased/`. Expiry
/// may resolve the `avail/` entry, but the lease is held, so markers must stay.
#[tokio::test]
async fn test_expired_duplicate_avail_entry_keeps_leased_jobs_markers() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let client = register_indexed_worker(&state).await?;
    let holder = ClientId::try_new("lease-holder")?;
    let deadline = Utc
        .with_ymd_and_hms(2020, 1, 2, 3, 4, 5)
        .single()
        .ok_or_else(|| anyhow::anyhow!("ambiguous or invalid datetime"))?;
    let job = seed_avail(
        dir.path(),
        "job-1",
        ExpiresAt::At(deadline),
        &expirable_body("job-1", &[&client, &holder]),
    )?;
    // Written directly: no valid transition produces this state.
    let lease_path = dir.path().join("todo").join("leased").join(leased_key(
        &job,
        ExpiresAt::At(deadline),
        &holder,
        Utc::now() + chrono::Duration::hours(1),
    ));
    std::fs::create_dir_all(
        lease_path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("leased key has no parent dir"))?,
    )?;
    std::fs::write(&lease_path, b"{}")?;

    state
        .todo_store
        .write_eligible(&client, &job, ExpiresAt::At(deadline))
        .await?;
    state
        .todo_store
        .write_denied(&job, &client, &DenialRecord::default())
        .await?;

    run_qm(&state).await?;

    assert!(state.todo_store.get_avail_by_job(&job).await?.is_none());
    assert!(state.submission_store.find_job(&job).await?.is_some());
    assert_eq!(eligible_jobs(&state, &client).await?, vec![job.clone()]);
    assert_eq!(denied_clients(&state, &job).await?, vec![client]);
    Ok(())
}

/// Best-effort teardown can leave the `avail/` entry of a recorded job behind;
/// expiry removes it and keeps the record, in `processed/` or mid-scoring.
#[rstest]
#[case::record_in_processed(false)]
#[case::record_mid_scoring(true)]
#[tokio::test]
async fn test_expired_job_with_existing_record_not_overwritten(
    #[case] mid_scoring: bool,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let (_, client) = register_and_approve(&state).await?;
    let deadline = Utc
        .with_ymd_and_hms(2020, 1, 2, 3, 4, 5)
        .single()
        .ok_or_else(|| anyhow::anyhow!("ambiguous or invalid datetime"))?;
    let job = seed_avail(
        dir.path(),
        "job-1",
        ExpiresAt::At(deadline),
        &expirable_body("job-1", &[&client]),
    )?;

    let real_record = json!({
        "job_id": "job-1",
        "client_id": client.as_str(),
        "message_type": "success",
        "marker": "real result, not synthetic",
    });
    if mid_scoring {
        state
            .submission_store
            .enqueue(ScoreQueueStage::ToDo, &job, &real_record)
            .await?;
    } else {
        state
            .submission_store
            .write_processed(&job, &real_record)
            .await?;
    }

    run_qm(&state).await?;

    assert!(state.todo_store.get_avail_by_job(&job).await?.is_none());
    let record = state
        .submission_store
        .find_job(&job)
        .await?
        .ok_or_else(|| anyhow::anyhow!("existing record vanished"))?;
    assert_eq!(record.body, real_record);
    Ok(())
}

/// The durable backstop for the submit-path escalation: without it, a `Never`
/// job every roster member denied would sit unclaimable forever. The reasons
/// differ only in timestamp, so they form one group; `c1`, listed twice,
/// counts once.
#[tokio::test]
async fn test_qm_escalates_all_denied_clients_only_job() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let (_, c1) = register_and_approve(&state).await?;
    let c2 = ClientId::try_new("ev1_other")?;
    let mut body = expirable_body("job-1", &[&c1]);
    body["clients"] = json!([c1.as_str(), c2.as_str(), c1.as_str()]);
    let job = seed_avail(dir.path(), "job-1", ExpiresAt::Never, &body)?;

    state
        .todo_store
        .write_eligible(&c1, &job, ExpiresAt::Never)
        .await?;
    for (client, minute) in [(&c1, 2), (&c2, 40)] {
        let at = format!("2026-09-21T14:{minute:02}:11Z");
        let reason = format!("[{at}] llama-server failed to load model: out of memory");
        let record = DenialRecord::new(at.parse()?, &reason, Some("1.4.2"), None);
        state.todo_store.write_denied(&job, client, &record).await?;
    }

    run_qm(&state).await?;

    let record = state
        .submission_store
        .get_submission(&job)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no synthetic failure for all-denied job"))?;
    assert_eq!(record.state, JobState::Incoming);
    assert_eq!(record.body["client_id"], "system");
    assert_eq!(record.body["failure_source"], "all_clients_denied");
    // A registered id is `ev1_` and hex digits, so it sorts before `ev1_other`.
    assert_eq!(
        record.body["failure_reason"],
        format!(
            "All eligible clients reported failure (2 clients, 1 distinct reason):\n\
             - 2 clients ({c1}, ev1_other): \
             llama-server failed to load model: out of memory"
        )
    );
    assert!(state.todo_store.get_avail_by_job(&job).await?.is_none());

    assert!(
        eligible_jobs(&state, &c1).await?.is_empty(),
        "eligible marker should be swept in the same run the job escalated"
    );
    assert!(
        denied_clients(&state, &job).await?.is_empty(),
        "denied markers should be swept in the same run the job escalated"
    );
    Ok(())
}

/// A roster member yet to deny may still succeed, and a `requires` job is
/// open-ended (left to `expires_at`), so neither escalates.
#[rstest]
#[case::roster_not_exhausted(
    json!({ "clients": ["ev1_a", "ev1_b"] }),
    &["ev1_a"]
)]
#[case::requires_job_open_ended(
    json!({ "clients": ["ev1_a"], "requires": ["os:linux"] }),
    &["ev1_a"]
)]
#[tokio::test]
async fn test_qm_all_denied_escalation_skips(
    #[case] roster: Value,
    #[case] denied: &[&str],
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let mut body = expirable_body("job-1", &[]);
    body["clients"] = roster["clients"].clone();
    if let Some(requires) = roster.get("requires") {
        body["requires"] = requires.clone();
    }
    let job = seed_avail(dir.path(), "job-1", ExpiresAt::Never, &body)?;
    for c in denied {
        state
            .todo_store
            .write_denied(&job, &ClientId::try_new(*c)?, &DenialRecord::default())
            .await?;
    }

    run_qm(&state).await?;

    assert!(
        state.todo_store.get_avail_by_job(&job).await?.is_some(),
        "job should remain claimable"
    );
    assert!(
        state.submission_store.find_job(&job).await?.is_none(),
        "no synthetic failure should be written"
    );
    Ok(())
}

/// `exclude_clients` shrinks the roster, so two denials exhaust a roster of
/// three; without it the job stays open (`test_qm_all_denied_escalation_skips`).
#[tokio::test]
async fn test_qm_escalates_when_an_exclusion_closes_the_roster() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let mut body = expirable_body("job-1", &[]);
    body["clients"] = json!(["ev1_a", "ev1_b", "ev1_reserved"]);
    body["exclude_clients"] = json!(["ev1_reserved"]);
    let job = seed_avail(dir.path(), "job-1", ExpiresAt::Never, &body)?;
    for id in ["ev1_a", "ev1_b"] {
        state
            .todo_store
            .write_denied(&job, &ClientId::try_new(id)?, &DenialRecord::default())
            .await?;
    }

    run_qm(&state).await?;

    let record = state
        .submission_store
        .get_submission(&job)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no synthetic failure for the exhausted roster"))?;
    assert_eq!(
        record.body["failure_reason"],
        "All eligible clients reported failure (2 clients, 0 distinct reasons):\n\
         - 2 clients (ev1_a, ev1_b): no reason recorded"
    );
    assert!(state.todo_store.get_avail_by_job(&job).await?.is_none());
    Ok(())
}

/// An unparseable marker costs the summary only its reason: escalation rests on
/// the `denied/` listing.
#[tokio::test]
async fn test_qm_escalates_past_an_unreadable_denied_marker() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let mut body = expirable_body("job-1", &[]);
    body["clients"] = json!(["ev1_a", "ev1_b"]);
    let job = seed_avail(dir.path(), "job-1", ExpiresAt::Never, &body)?;
    let readable = DenialRecord::new(Utc::now(), "runtime OOM", None, None);
    state
        .todo_store
        .write_denied(&job, &ClientId::try_new("ev1_a")?, &readable)
        .await?;
    let unreadable = denied_marker_filename(&job, &ClientId::try_new("ev1_b")?);
    std::fs::write(
        dir.path().join("todo").join("denied").join(unreadable),
        b"{",
    )?;

    run_qm(&state).await?;

    let record = state
        .submission_store
        .get_submission(&job)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no synthetic failure past the unreadable marker"))?;
    assert_eq!(
        record.body["failure_reason"],
        "All eligible clients reported failure (2 clients, 1 distinct reason):\n\
         - 1 client (ev1_a): runtime OOM\n\
         - 1 client (ev1_b): reason unreadable"
    );
    assert!(state.todo_store.get_avail_by_job(&job).await?.is_none());
    Ok(())
}

/// A capability exclusion never closes a roster: a `PATCH /clients/me` can
/// reverse it. Counted, only the denier would remain and the job would escalate.
#[tokio::test]
async fn test_qm_capability_exclusion_does_not_close_the_roster() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let (_, excluded) = register_and_approve(&state).await?;
    set_profile(&state, &excluded, os_profile("iOS")?).await?;
    let denier = ClientId::try_new("ev1_denier")?;
    let mut body = expirable_body("job-1", &[]);
    body["clients"] = json!([excluded.as_str(), denier.as_str()]);
    body["excludes"] = json!(["os:ios"]);
    let job = seed_avail(dir.path(), "job-1", ExpiresAt::Never, &body)?;
    state
        .todo_store
        .write_denied(&job, &denier, &DenialRecord::default())
        .await?;

    run_qm(&state).await?;

    assert!(
        state.todo_store.get_avail_by_job(&job).await?.is_some(),
        "job should remain claimable"
    );
    assert!(
        state.submission_store.find_job(&job).await?.is_none(),
        "no synthetic failure should be written"
    );
    Ok(())
}

/// A body no client can ever match is retired instead of sitting in `avail/`
/// until `expires_at`. Ingestion refuses these; only a bypassing writer makes one.
#[rstest]
#[case::roster_excluded_away(json!({"clients": ["ev1_a"], "exclude_clients": ["ev1_a"]}))]
#[case::required_flag_excluded(json!({"requires": ["os:ios"], "excludes": ["os:ios"]}))]
#[case::any_of_group_excluded(json!({
    "requires": ["os:ios"],
    "any_of": [["chip:a15", "chip:a16"]],
    "excludes": ["chip:a15", "chip:a16"],
}))]
// An empty `not_all_of` group is in every capability set, so it kills the
// roster branch too.
#[case::empty_not_all_of_group(json!({"requires": ["os:ios"], "not_all_of": [[]]}))]
#[case::empty_not_all_of_group_with_roster(json!({"clients": ["ev1_a"], "not_all_of": [[]]}))]
// A malformed clause fails closed for every client, roster included.
#[case::malformed_excludes(json!({"clients": ["ev1_a"], "excludes": "chip:a15"}))]
#[case::malformed_exclude_clients(json!({"clients": ["ev1_a"], "exclude_clients": {}}))]
#[tokio::test]
async fn test_qm_retires_a_job_no_client_can_match(
    #[case] eligibility: Value,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let (_, client) = register_and_approve(&state).await?;
    let mut body = expirable_body("job-1", &[]);
    body.as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("body is an object"))?
        .remove("clients");
    eligibility
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("eligibility is an object"))?
        .iter()
        .for_each(|(field, value)| body[field] = value.clone());
    let job = seed_avail(dir.path(), "job-1", ExpiresAt::Never, &body)?;

    run_qm(&state).await?;

    let record = state
        .submission_store
        .get_submission(&job)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no synthetic failure for the unmatchable job"))?;
    assert_eq!(record.state, JobState::Incoming);
    assert_eq!(record.body["client_id"], "system");
    assert_eq!(record.body["failure_source"], "unmatchable");
    let reason = record.body["failure_reason"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("failure_reason is a string"))?;
    assert!(
        reason.starts_with("No client can ever be eligible for this job:"),
        "the reason must name the rule that fired: {reason}"
    );
    assert!(state.todo_store.get_avail_by_job(&job).await?.is_none());
    // Retirement is confirmed terminal, so nothing indexes the job afterwards.
    assert!(eligible_jobs(&state, &client).await?.is_empty());
    Ok(())
}

/// Matching nobody in today's fleet is not unmatchable: a conforming client can
/// register tomorrow.
#[tokio::test]
async fn test_qm_keeps_a_job_only_the_fleet_cannot_match() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let (_, client) = register_and_approve(&state).await?;
    set_profile(&state, &client, os_profile("iOS")?).await?;
    let mut body = expirable_body("job-1", &[]);
    body["clients"] = json!([]);
    body["requires"] = json!(["os:ios"]);
    body["exclude_clients"] = json!([client.as_str()]);
    let job = seed_avail(dir.path(), "job-1", ExpiresAt::Never, &body)?;

    run_qm(&state).await?;

    assert!(
        state.todo_store.get_avail_by_job(&job).await?.is_some(),
        "a fleet-dependent empty match is not a defect in the body"
    );
    assert!(state.submission_store.find_job(&job).await?.is_none());
    assert!(eligible_jobs(&state, &client).await?.is_empty());
    Ok(())
}

/// No client carries two `os:` flags, so the group excludes nobody. The pass
/// warns and keeps the job: only its author knows which clients were meant.
#[tokio::test]
async fn test_qm_keeps_a_job_whose_exclusion_group_is_vacuous() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let (_, client) = register_and_approve(&state).await?;
    set_profile(&state, &client, os_profile("iOS")?).await?;
    let mut body = expirable_body("job-1", &[]);
    body["clients"] = json!([]);
    body["requires"] = json!(["os:ios"]);
    body["not_all_of"] = json!([["os:ios", "os:android"]]);
    let job = seed_avail(dir.path(), "job-1", ExpiresAt::Never, &body)?;

    run_qm(&state).await?;

    assert!(
        state.todo_store.get_avail_by_job(&job).await?.is_some(),
        "a clause that excludes nobody is the author's to fix, not the pass's"
    );
    assert!(state.submission_store.find_job(&job).await?.is_none());
    assert_eq!(eligible_jobs(&state, &client).await?, vec![job.clone()]);
    Ok(())
}

/// The index writes an excluded client no marker and the claim reads markers
/// only, so the client gets `204`.
#[tokio::test]
async fn test_qm_excluded_client_is_indexed_out_and_claims_nothing() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let (key, excluded) = register_and_approve(&state).await?;
    set_profile(&state, &excluded, device_profile("iOS", "iPhone 17 Pro")?).await?;
    let mut body = expirable_body("job-1", &[&excluded]);
    body["requires"] = json!(["os:ios"]);
    body["not_all_of"] = json!([["os:ios", "device:iphone17pro"]]);
    let job = seed_avail(dir.path(), "job-1", ExpiresAt::Never, &body)?;

    run_qm(&state).await?;

    assert!(
        state.todo_store.get_avail_by_job(&job).await?.is_some(),
        "the job is excluded for this client, not defective"
    );
    assert!(
        eligible_jobs(&state, &excluded).await?.is_empty(),
        "an excluded client gets no eligible marker, roster entry notwithstanding"
    );
    let resp = authed_post(&state, &key, &excluded, "/plans/claim", &json!({})).await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    Ok(())
}

/// Shares `expire_one_job`'s guard: an existing record stays untouched and only
/// the stale `avail/` entry goes.
#[tokio::test]
async fn test_qm_all_denied_escalation_preserves_existing_record() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let c1 = ClientId::try_new("ev1_a")?;
    let mut body = expirable_body("job-1", &[&c1]);
    body["clients"] = json!([c1.as_str()]);
    let job = seed_avail(dir.path(), "job-1", ExpiresAt::Never, &body)?;
    state
        .todo_store
        .write_denied(&job, &c1, &DenialRecord::default())
        .await?;

    let real_record = json!({
        "job_id": "job-1",
        "client_id": c1.as_str(),
        "message_type": "success",
        "marker": "real result, not synthetic",
    });
    state
        .submission_store
        .write_processed(&job, &real_record)
        .await?;

    run_qm(&state).await?;

    assert!(state.todo_store.get_avail_by_job(&job).await?.is_none());
    let record = state
        .submission_store
        .find_job(&job)
        .await?
        .ok_or_else(|| anyhow::anyhow!("existing record vanished"))?;
    assert_eq!(record.body, real_record);
    Ok(())
}

#[tokio::test]
async fn test_denied_gc_sweeps_terminal_jobs_only() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let (_, client) = register_and_approve(&state).await?;
    // A second roster member keeps the job short of all-denied, which the
    // reconciliation pass would escalate.
    let live = seed_avail(
        dir.path(),
        "job-live",
        ExpiresAt::Never,
        &json!({"clients": [client.as_str(), "ev1_other"]}),
    )?;
    let terminal = job("job-terminal")?;
    state
        .todo_store
        .write_denied(&live, &client, &DenialRecord::default())
        .await?;
    state
        .todo_store
        .write_denied(&terminal, &client, &DenialRecord::default())
        .await?;

    // The two-sighting rule: the first run only records the candidate.
    run_qm(&state).await?;
    assert_eq!(
        denied_clients(&state, &terminal).await?,
        vec![client.clone()],
        "first sighting should not delete the orphan marker"
    );
    run_qm(&state).await?;

    assert_eq!(denied_clients(&state, &live).await?, vec![client.clone()]);
    assert!(denied_clients(&state, &terminal).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn test_stale_tmp_deleted_only_past_max_age() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let tmp_file = dir.path().join("todo").join("tmp").join("partial.json");
    std::fs::write(&tmp_file, b"{")?;

    // Far above the file's age → kept.
    run_qm(&state).await?;
    assert!(tmp_file.exists());

    // Zero age → everything is stale → deleted.
    run_qm_with_tmp_age(&state, Duration::ZERO).await?;
    assert!(!tmp_file.exists());
    Ok(())
}

// ─── plan teardown, status reconciliation & progress snapshot ───────────────

/// A manifest over `job_ids`, created "now" so the stranded-`creating` deadline
/// only fires when a test asks for a zero threshold.
fn plan_manifest(plan_id: &PlanId, status: PlanStatus, job_ids: &[&JobId]) -> PlanManifest {
    PlanManifest {
        campaign_id: None,
        previous_plan_id: None,
        source_sha256: None,
        submitted_sha256: None,
        plan_warnings: Vec::new(),
        plan_id: plan_id.clone(),
        status,
        created_at: Utc::now(),
        submitted_by: None,
        benchmark_types: Default::default(),
        job_ids: job_ids.iter().map(|j| (*j).clone()).collect(),
        content_keys: Default::default(),
        rerun_of: Default::default(),
        warnings: vec![],
        expired_jobs: std::collections::BTreeMap::new(),
        job_expiry: None,
        settled_jobs: Default::default(),
        progress_snapshot: None,
        terminal_at: None,
    }
}

fn test_plan_id() -> PlanId {
    PlanId::from_uuid(uuid::Uuid::nil())
}

fn other_plan_id() -> PlanId {
    PlanId::from_uuid(uuid::Uuid::from_u128(1))
}

async fn read_plan(state: &AppState, plan_id: &PlanId) -> anyhow::Result<PlanManifest> {
    state
        .plan_store
        .clone()
        .get_plan(plan_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("manifest {plan_id} vanished"))
}

async fn seed_plan(
    state: &AppState,
    manifest: &PlanManifest,
    cancel_requested: bool,
) -> anyhow::Result<()> {
    let plans = state.plan_store.clone();
    plans.put_plan(manifest).await?;
    if cancel_requested {
        plans.write_cancel_marker(&manifest.plan_id).await?;
    }
    Ok(())
}

/// The run that retires the jobs latches `cancelling`; the next, whose listings
/// it did not perturb, settles `cancelled`. Teardown writes no submission
/// record: a cancel has no outcome to record.
#[tokio::test]
async fn test_cancel_marker_tears_down_avail_and_leased_jobs() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (_, client) = register_and_approve(&state).await?;

    let queued = seed_avail(
        dir.path(),
        "job-queued",
        ExpiresAt::Never,
        &expirable_body("job-queued", &[&client]),
    )?;
    let running = seed_avail(
        dir.path(),
        "job-running",
        ExpiresAt::Never,
        &expirable_body("job-running", &[&client]),
    )?;
    // Index the queued job so the run has an eligible marker to collect.
    run_qm(&state).await?;
    assert!(eligible_jobs(&state, &client).await?.contains(&queued));

    let lease_expiry = Utc::now() + chrono::Duration::hours(1);
    state
        .todo_store
        .claim_job(&running, ExpiresAt::Never, &client, lease_expiry)
        .await?;

    let plan_id = test_plan_id();
    seed_plan(
        &state,
        &plan_manifest(&plan_id, PlanStatus::Active, &[&queued, &running]),
        true,
    )
    .await?;

    run_qm(&state).await?;

    assert!(
        state.todo_store.get_avail_by_job(&queued).await?.is_none(),
        "the queued job is deleted from avail/"
    );
    assert!(
        state.todo_store.list_leased().await?.is_empty(),
        "the running job's lease is deleted"
    );
    assert!(
        !eligible_jobs(&state, &client).await?.contains(&queued),
        "a retired job's eligible marker is swept in the same run"
    );
    for job in [&queued, &running] {
        assert!(
            state.submission_store.find_job(job).await?.is_none(),
            "cancellation teardown writes no submission record for {job}"
        );
    }

    let manifest = read_plan(&state, &plan_id).await?;
    assert_eq!(
        manifest.status,
        PlanStatus::Cancelling,
        "the pass that retired the jobs does not settle the plan"
    );
    assert!(
        manifest.terminal_at.is_none(),
        "terminal_at dates the settle"
    );
    let snapshot = manifest
        .progress_snapshot
        .ok_or_else(|| anyhow::anyhow!("the pass writes a snapshot"))?;
    assert_eq!(snapshot.counts.total, 2);
    assert_eq!(snapshot.counts.cancelled, 2, "both jobs were retired");
    assert_eq!(snapshot.counts.available, 0);
    assert_eq!(snapshot.counts.running, 0);
    assert!(
        !state.plan_store.clone().has_cancel_marker(&plan_id).await?,
        "the marker is collected at the latch"
    );

    run_qm(&state).await?;
    let settled = read_plan(&state, &plan_id).await?;
    assert_eq!(settled.status, PlanStatus::Cancelled);
    assert!(
        settled.terminal_at.is_some(),
        "the settle stamps terminal_at"
    );
    Ok(())
}

/// Nothing else tells a client that is mid-job to stop.
#[tokio::test]
async fn test_teardown_makes_the_holders_next_heartbeat_404() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (key, client) = register_and_approve(&state).await?;

    let running = seed_avail(
        dir.path(),
        "job-running",
        ExpiresAt::Never,
        &expirable_body("job-running", &[&client]),
    )?;
    state
        .todo_store
        .claim_job(
            &running,
            ExpiresAt::Never,
            &client,
            Utc::now() + chrono::Duration::hours(1),
        )
        .await?;

    let resp = pipette_web_testkit::authed_put_empty(
        &state,
        &key,
        &client,
        "/plans/job-running/heartbeat",
    )
    .await?;
    assert_eq!(resp.status(), StatusCode::OK);

    let plan_id = test_plan_id();
    seed_plan(
        &state,
        &plan_manifest(&plan_id, PlanStatus::Active, &[&running]),
        true,
    )
    .await?;

    run_qm(&state).await?;

    let resp = pipette_web_testkit::authed_put_empty(
        &state,
        &key,
        &client,
        "/plans/job-running/heartbeat",
    )
    .await?;
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "teardown removed the lease, so the holder is told to stop"
    );
    Ok(())
}

/// The result counts as `finished`, and the plan settles in the latching run
/// because teardown finds nothing left to retire.
#[tokio::test]
async fn test_a_result_submitted_before_teardown_lands() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (key, client) = register_and_approve(&state).await?;

    let running = seed_avail(
        dir.path(),
        "job-early",
        ExpiresAt::Never,
        &expirable_body("job-early", &[&client]),
    )?;
    state
        .todo_store
        .claim_job(
            &running,
            ExpiresAt::Never,
            &client,
            Utc::now() + chrono::Duration::hours(1),
        )
        .await?;

    let mut body = valid_submission("prefill_throughput_256");
    body["job_id"] = json!("job-early");
    let resp = authed_post(&state, &key, &client, "/benchmarks", &body).await?;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    let plan_id = test_plan_id();
    seed_plan(
        &state,
        &plan_manifest(&plan_id, PlanStatus::Active, &[&running]),
        true,
    )
    .await?;

    run_qm(&state).await?;

    assert!(
        state.submission_store.find_job(&running).await?.is_some(),
        "the cancel does not retract a result already recorded"
    );
    let manifest = read_plan(&state, &plan_id).await?;
    assert_eq!(
        manifest.status,
        PlanStatus::Cancelled,
        "the run retires nothing, so it settles the plan"
    );
    let counts = manifest
        .progress_snapshot
        .ok_or_else(|| anyhow::anyhow!("snapshot"))?
        .counts;
    assert_eq!(
        counts.finished, 1,
        "a recorded job is finished, not cancelled"
    );
    assert_eq!(counts.cancelled, 0);
    Ok(())
}

/// `verify_claim` renews the lease to authorize the write, so a deleted lease
/// 404s before the record write and the job stays `cancelled`.
#[tokio::test]
async fn test_a_result_submitted_after_teardown_404s_and_writes_nothing() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (key, client) = register_and_approve(&state).await?;

    let running = seed_avail(
        dir.path(),
        "job-late",
        ExpiresAt::Never,
        &expirable_body("job-late", &[&client]),
    )?;
    state
        .todo_store
        .claim_job(
            &running,
            ExpiresAt::Never,
            &client,
            Utc::now() + chrono::Duration::hours(1),
        )
        .await?;

    let plan_id = test_plan_id();
    seed_plan(
        &state,
        &plan_manifest(&plan_id, PlanStatus::Active, &[&running]),
        true,
    )
    .await?;

    run_qm(&state).await?;

    let mut body = valid_submission("prefill_throughput_256");
    body["job_id"] = json!("job-late");
    let resp = authed_post(&state, &key, &client, "/benchmarks", &body).await?;
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "teardown removed the lease that authorizes the write"
    );
    assert!(
        state.submission_store.find_job(&running).await?.is_none(),
        "a refused submission records nothing"
    );

    run_qm(&state).await?;
    let manifest = read_plan(&state, &plan_id).await?;
    assert_eq!(manifest.status, PlanStatus::Cancelled);
    let counts = manifest
        .progress_snapshot
        .ok_or_else(|| anyhow::anyhow!("snapshot"))?
        .counts;
    assert_eq!(counts.cancelled, 1);
    assert_eq!(counts.finished, 0);
    assert_eq!(
        counts.finished + counts.running + counts.available + counts.failed + counts.cancelled,
        counts.total,
        "the six buckets partition the plan"
    );
    Ok(())
}

/// A job recycled after teardown is retired again; the `cancelled` latch is
/// neither revisited nor re-stamped.
#[tokio::test]
async fn test_teardown_redeletes_a_resurrected_job() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (_, client) = register_and_approve(&state).await?;

    let job = seed_avail(
        dir.path(),
        "job-back",
        ExpiresAt::Never,
        &expirable_body("job-back", &[&client]),
    )?;
    let plan_id = test_plan_id();
    seed_plan(
        &state,
        &plan_manifest(&plan_id, PlanStatus::Active, &[&job]),
        true,
    )
    .await?;

    // Run 1 retires the job and latches `cancelling`; run 2 settles the plan.
    run_qm(&state).await?;
    run_qm(&state).await?;
    let settled = read_plan(&state, &plan_id).await?;
    assert_eq!(settled.status, PlanStatus::Cancelled);
    let stamped = settled.terminal_at;

    // The lease recycler returns the job to `avail/` after teardown ran.
    seed_avail(
        dir.path(),
        "job-back",
        ExpiresAt::Never,
        &expirable_body("job-back", &[&client]),
    )?;
    run_qm(&state).await?;

    assert!(
        state.todo_store.get_avail_by_job(&job).await?.is_none(),
        "the next pass deletes the resurrected job again"
    );
    let after = read_plan(&state, &plan_id).await?;
    assert_eq!(after.status, PlanStatus::Cancelled);
    assert_eq!(after.terminal_at, stamped, "the latch is not re-stamped");
    Ok(())
}

/// A failed teardown fails the run and holds `cancelling`; `terminal_at` dates
/// the settle, not the request.
#[tokio::test]
async fn test_plan_stays_cancelling_until_teardown_finishes() -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (_, client) = register_and_approve(&state).await?;

    let job = seed_avail(
        dir.path(),
        "job-stuck",
        ExpiresAt::Never,
        &expirable_body("job-stuck", &[&client]),
    )?;
    let lease_expiry = Utc::now() + chrono::Duration::hours(1);
    state
        .todo_store
        .claim_job(&job, ExpiresAt::Never, &client, lease_expiry)
        .await?;

    let plan_id = test_plan_id();
    seed_plan(
        &state,
        &plan_manifest(&plan_id, PlanStatus::Active, &[&job]),
        true,
    )
    .await?;

    // A read-only lease partition fails this one deletion while the rest of the
    // run proceeds.
    let lease_dir = dir.path().join("todo").join("leased").join(client.as_str());
    let original = std::fs::metadata(&lease_dir)?.permissions();
    std::fs::set_permissions(&lease_dir, std::fs::Permissions::from_mode(0o555))?;
    let result = run_qm(&state).await;
    std::fs::set_permissions(&lease_dir, original)?;
    assert!(
        result.is_err(),
        "a failed teardown makes the run exit non-zero"
    );

    let midway = read_plan(&state, &plan_id).await?;
    assert_eq!(
        midway.status,
        PlanStatus::Cancelling,
        "the plan cannot settle while a job is still live"
    );
    assert!(
        midway.terminal_at.is_none(),
        "terminal_at dates the settle, not the request"
    );
    let snapshot = midway
        .progress_snapshot
        .ok_or_else(|| anyhow::anyhow!("the snapshot refreshes while cancelling"))?;
    assert_eq!(snapshot.counts.running, 1, "the job is still leased");
    assert_eq!(snapshot.counts.cancelled, 0);
    assert!(
        !state.plan_store.clone().has_cancel_marker(&plan_id).await?,
        "the marker is collected at the latch, not at the settle"
    );

    // Teardown is driven by the manifest status, so it finishes although the
    // marker is already collected.
    run_qm(&state).await?;
    let torn_down = read_plan(&state, &plan_id).await?;
    assert_eq!(torn_down.status, PlanStatus::Cancelling);
    assert!(torn_down.terminal_at.is_none());

    run_qm(&state).await?;
    let settled = read_plan(&state, &plan_id).await?;
    assert_eq!(settled.status, PlanStatus::Cancelled);
    assert!(settled.terminal_at.is_some());
    assert_eq!(
        settled
            .progress_snapshot
            .ok_or_else(|| anyhow::anyhow!("snapshot"))?
            .counts
            .cancelled,
        1
    );
    Ok(())
}

/// A plan whose job this run retired holds `cancelling` while another settles.
/// A run-wide `!retired.is_empty()` passes every other test and fails this one.
#[tokio::test]
async fn test_the_settle_is_decided_per_plan() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (_, client) = register_and_approve(&state).await?;

    let live = seed_avail(
        dir.path(),
        "job-live",
        ExpiresAt::Never,
        &expirable_body("job-live", &[&client]),
    )?;
    let retiring = test_plan_id();
    seed_plan(
        &state,
        &plan_manifest(&retiring, PlanStatus::Active, &[&live]),
        true,
    )
    .await?;

    let done = pipette_web_testkit::job("job-done")?;
    state
        .submission_store
        .write_processed(
            &done,
            &json!({"message_type": "success", "job_id": "job-done"}),
        )
        .await?;
    let settling = other_plan_id();
    seed_plan(
        &state,
        &plan_manifest(&settling, PlanStatus::Active, &[&done]),
        true,
    )
    .await?;

    run_qm(&state).await?;

    let held = read_plan(&state, &retiring).await?;
    assert_eq!(
        held.status,
        PlanStatus::Cancelling,
        "this run retired the plan's own job"
    );
    assert!(held.terminal_at.is_none());

    let settled = read_plan(&state, &settling).await?;
    assert_eq!(
        settled.status,
        PlanStatus::Cancelled,
        "another plan's teardown does not delay this plan's settle"
    );
    assert!(
        settled.terminal_at.is_some(),
        "the settle stamps terminal_at"
    );
    Ok(())
}

/// A cancel that loses the race to a terminal latch must not resurrect the plan.
/// The plan is old: a young `cancelled` one still holds its marker (see
/// `test_cancelling_a_creating_plan_keeps_its_marker_while_its_ingest_could_run`).
#[rstest]
#[case::complete(PlanStatus::Complete)]
#[case::cancelled(PlanStatus::Cancelled)]
#[tokio::test]
async fn test_marker_for_terminal_plan_is_collected_without_touching_status(
    #[case] status: PlanStatus,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let plan_id = test_plan_id();
    let stamped = Utc::now() - chrono::Duration::days(1);
    let manifest = PlanManifest {
        created_at: stamped,
        terminal_at: Some(stamped),
        ..plan_manifest(&plan_id, status, &[])
    };
    seed_plan(&state, &manifest, true).await?;

    run_qm(&state).await?;

    let after = read_plan(&state, &plan_id).await?;
    assert_eq!(after.status, status, "a terminal latch is never revisited");
    assert_eq!(after.terminal_at, Some(stamped), "and never re-stamped");
    assert!(
        !state.plan_store.clone().has_cancel_marker(&plan_id).await?,
        "the stray marker is collected"
    );
    Ok(())
}

/// An orphan marker would otherwise linger with no surface that shows it.
#[tokio::test]
async fn test_marker_without_manifest_is_collected() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let plan_id = test_plan_id();
    let plans = state.plan_store.clone();
    plans.write_cancel_marker(&plan_id).await?;

    run_qm(&state).await?;

    assert!(!plans.has_cancel_marker(&plan_id).await?);
    Ok(())
}

#[tokio::test]
async fn test_a_cancelled_plan_keeps_its_links_and_sources() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _key, client_id) = approved_client(dir.path()).await?;
    let (key, user_id, key_id) = registered_user(&state, &[UserPermission::SubmitPlans]).await?;
    let plan_id = submit_plan_as(&state, &key, &user_id, &key_id, &client_id, "doomed", 2).await?;
    let job_ids = read_plan(&state, &plan_id).await?.job_ids;
    state.plan_store.write_cancel_marker(&plan_id).await?;

    run_qm(&state).await?;
    run_qm(&state).await?;
    let settled = read_plan(&state, &plan_id).await?;
    assert_eq!(settled.status, PlanStatus::Cancelled);
    for job_id in &job_ids {
        assert!(state.todo_store.get_avail_by_job(job_id).await?.is_none());
        assert!(
            state.plan_store.get_job_source(job_id).await?.is_some(),
            "teardown keeps the source of {job_id}"
        );
        assert_eq!(
            state.plan_store.list_job_plan_links(job_id).await?,
            vec![plan_id.clone()],
            "teardown keeps the link of {job_id}"
        );
    }
    Ok(())
}

/// An ingest cut before its promote leaves a `creating` manifest, a link and a
/// job source, and nothing queued.
#[tokio::test]
async fn test_a_stranded_ingest_keeps_its_link_and_source() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let plan_id = test_plan_id();
    let job_id = JobId::from_uuid(uuid::Uuid::from_u128(7));
    seed_plan(
        &state,
        &plan_manifest(&plan_id, PlanStatus::Creating, &[&job_id]),
        false,
    )
    .await?;
    state
        .plan_store
        .put_job_source(&job_id, &json!({ "job_id": job_id.as_str() }))
        .await?;
    state
        .plan_store
        .put_job_plan_link(&job_id, &plan_id)
        .await?;

    // A zero deadline makes the manifest stranded on both runs.
    run_qm_full(&state, Duration::from_secs(86_400), Duration::ZERO).await?;
    run_qm_full(&state, Duration::from_secs(86_400), Duration::ZERO).await?;
    let settled = read_plan(&state, &plan_id).await?;
    assert_eq!(settled.status, PlanStatus::Cancelled);
    assert_eq!(
        pipette_store_testkit::local_fs_job_record_count(dir.path()),
        2,
        "teardown keeps the link and the job source"
    );
    Ok(())
}

/// Nothing deletes a plan by age: it keeps its document, job source and link.
#[rstest]
#[case::complete(PlanStatus::Complete, true)]
#[case::cancelled(PlanStatus::Cancelled, true)]
#[case::complete_without_a_stamp(PlanStatus::Complete, false)]
#[tokio::test]
async fn test_terminal_plan_is_kept_however_old(
    #[case] status: PlanStatus,
    #[case] stamped: bool,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let plan_id = test_plan_id();
    let job_id = JobId::from_uuid(uuid::Uuid::from_u128(7));
    let job_source = json!({ "job_id": job_id.as_str() });
    let ancient = Utc::now() - chrono::Duration::days(3650);
    let source = pipette_core::plan::PlanSource::new("benchmarks = [\"mmlu\"]\n".to_string())?;
    let terminal_at = stamped.then_some(ancient);
    let manifest = PlanManifest {
        created_at: ancient,
        terminal_at,
        source_sha256: Some(source.sha256().clone()),
        submitted_sha256: None,
        ..plan_manifest(&plan_id, status, &[&job_id])
    };
    state.plan_store.put_plan_source(&plan_id, &source).await?;
    state
        .plan_store
        .put_job_source(&job_id, &job_source)
        .await?;
    state
        .plan_store
        .put_job_plan_link(&job_id, &plan_id)
        .await?;
    seed_plan(&state, &manifest, false).await?;

    run_qm(&state).await?;

    let after = read_plan(&state, &plan_id).await?;
    assert_eq!(after.status, status);
    assert_eq!(after.terminal_at, terminal_at);
    let kept = state
        .plan_store
        .get_plan_source(&plan_id)
        .await?
        .context("the plan's document is kept too")?;
    assert_eq!(kept.sha256(), source.sha256());
    assert_eq!(
        state.plan_store.get_job_source(&job_id).await?,
        Some(job_source),
        "the job's source is kept too"
    );
    assert_eq!(
        state.plan_store.list_job_plan_links(&job_id).await?,
        vec![plan_id],
        "the job's link is kept too"
    );
    Ok(())
}

#[tokio::test]
async fn test_an_old_live_plan_is_kept() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (_, client) = register_and_approve(&state).await?;

    let ancient = Utc::now() - chrono::Duration::days(365);

    let job = seed_avail(
        dir.path(),
        "job-ancient",
        ExpiresAt::Never,
        &expirable_body("job-ancient", &[&client]),
    )?;
    let live_id = test_plan_id();
    seed_plan(
        &state,
        &PlanManifest {
            created_at: ancient,
            ..plan_manifest(&live_id, PlanStatus::Active, &[&job])
        },
        false,
    )
    .await?;

    run_qm(&state).await?;

    assert_eq!(
        read_plan(&state, &live_id).await?.status,
        PlanStatus::Active
    );
    Ok(())
}

/// Status counts only clients the claim path admits: approved and holding
/// `perform_work`. A fresh registration holds `submit_results` alone, so
/// counting it would report `active` for a job it is refused.
#[rstest]
#[case::matched(
    RoutedBy::Profile,
    true,
    &[ClientPermission::PerformWork, ClientPermission::SubmitResults],
    PlanStatus::Active
)]
#[case::unmatched(
    RoutedBy::Profile,
    false,
    &[ClientPermission::PerformWork, ClientPermission::SubmitResults],
    PlanStatus::PendingClients
)]
#[case::matches_but_cannot_perform_work(
    RoutedBy::Profile,
    true,
    &[ClientPermission::SubmitResults],
    PlanStatus::PendingClients
)]
#[case::matched_by_tag(
    RoutedBy::Tag,
    true,
    &[ClientPermission::PerformWork, ClientPermission::SubmitResults],
    PlanStatus::Active
)]
#[case::unmatched_by_tag(
    RoutedBy::Tag,
    false,
    &[ClientPermission::PerformWork, ClientPermission::SubmitResults],
    PlanStatus::PendingClients
)]
#[tokio::test]
async fn test_plan_status_follows_the_fleet(
    #[case] by: RoutedBy,
    #[case] matching: bool,
    #[case] granted: &[ClientPermission],
    #[case] expected: PlanStatus,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (_, client) = pipette_web_testkit::register_approve_and_grant(&state, granted).await?;
    by.set(&state, &client, matching).await?;

    let mut body = expirable_body("job-fleet", &[]);
    body["requires"] = by.requires();
    let job = seed_avail(dir.path(), "job-fleet", ExpiresAt::Never, &body)?;

    let plan_id = test_plan_id();
    seed_plan(
        &state,
        &plan_manifest(&plan_id, PlanStatus::Active, &[&job]),
        false,
    )
    .await?;

    run_qm(&state).await?;

    let manifest = read_plan(&state, &plan_id).await?;
    assert_eq!(manifest.status, expected);
    assert!(
        manifest.terminal_at.is_none(),
        "a live plan carries no terminal_at"
    );
    let snapshot = manifest
        .progress_snapshot
        .ok_or_else(|| anyhow::anyhow!("snapshot"))?;
    assert_eq!(snapshot.counts.available, 1);
    if expected == PlanStatus::PendingClients {
        assert_eq!(snapshot.starved.len(), 1, "the unmatched job is reported");
        assert_eq!(json!(snapshot.starved[0].clauses.requires), by.requires());
        assert_eq!(snapshot.starved[0].job_ids, vec![job]);
    } else {
        assert!(snapshot.starved.is_empty());
    }
    Ok(())
}

#[tokio::test]
async fn test_complete_latch_counts_finished_and_failed() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let done = pipette_web_testkit::job("job-done")?;
    let failed = pipette_web_testkit::job("job-failed")?;
    state
        .submission_store
        .write_processed(
            &done,
            &json!({"message_type": "success", "job_id": "job-done"}),
        )
        .await?;
    state
        .submission_store
        .write_processed(
            &failed,
            &json!({"message_type": "failure", "job_id": "job-failed"}),
        )
        .await?;

    let plan_id = test_plan_id();
    seed_plan(
        &state,
        &plan_manifest(&plan_id, PlanStatus::Active, &[&done, &failed]),
        false,
    )
    .await?;

    run_qm(&state).await?;

    let manifest = read_plan(&state, &plan_id).await?;
    assert_eq!(manifest.status, PlanStatus::Complete);
    let stamped = manifest.terminal_at;
    assert!(stamped.is_some(), "the complete latch stamps terminal_at");
    let counts = manifest
        .progress_snapshot
        .ok_or_else(|| anyhow::anyhow!("snapshot"))?
        .counts;
    assert_eq!(counts.total, 2);
    assert_eq!(counts.finished, 1);
    assert_eq!(counts.failed, 1);
    assert_eq!(counts.cancelled, 0, "a recorded job is never counted here");
    assert_eq!(
        counts.finished + counts.running + counts.available + counts.failed + counts.cancelled,
        counts.total,
        "the six buckets partition the plan"
    );

    run_qm(&state).await?;
    assert_eq!(read_plan(&state, &plan_id).await?.terminal_at, stamped);
    Ok(())
}

/// A `creating` manifest lists job ids before ingestion promotes any, so every
/// job reads as absent; that must not latch it `complete`.
#[tokio::test]
async fn test_creating_plan_is_not_latched_complete() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let plan_id = test_plan_id();
    let staged = pipette_web_testkit::job("job-staging")?;
    seed_plan(
        &state,
        &plan_manifest(&plan_id, PlanStatus::Creating, &[&staged]),
        false,
    )
    .await?;

    run_qm(&state).await?;

    let manifest = read_plan(&state, &plan_id).await?;
    assert_eq!(manifest.status, PlanStatus::Creating);
    assert!(manifest.terminal_at.is_none());
    assert!(
        manifest.progress_snapshot.is_none(),
        "ingestion still owns the manifest, so the pass writes nothing"
    );
    Ok(())
}

/// A crash between the promote loop and the status write strands a `creating`
/// manifest; teardown handles it like a cancel.
#[tokio::test]
async fn test_stranded_creating_plan_is_torn_down() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (_, client) = register_and_approve(&state).await?;

    let job = seed_avail(
        dir.path(),
        "job-stranded",
        ExpiresAt::Never,
        &expirable_body("job-stranded", &[&client]),
    )?;
    let plan_id = test_plan_id();
    seed_plan(
        &state,
        &plan_manifest(&plan_id, PlanStatus::Creating, &[&job]),
        false,
    )
    .await?;

    // A zero deadline makes the just-written manifest stranded.
    run_qm_full(&state, Duration::from_secs(86_400), Duration::ZERO).await?;
    assert_eq!(
        read_plan(&state, &plan_id).await?.status,
        PlanStatus::Cancelling
    );
    run_qm(&state).await?;

    assert!(
        state.todo_store.get_avail_by_job(&job).await?.is_none(),
        "the stranded plan's promoted jobs are retired"
    );
    let manifest = read_plan(&state, &plan_id).await?;
    assert_eq!(manifest.status, PlanStatus::Cancelled);
    assert!(
        manifest.terminal_at.is_some(),
        "a stranded plan is stamped when it latches"
    );
    assert_eq!(
        manifest
            .progress_snapshot
            .ok_or_else(|| anyhow::anyhow!("snapshot"))?
            .counts
            .cancelled,
        1
    );
    Ok(())
}

/// The ingest that owns it is still running.
#[tokio::test]
async fn test_young_creating_plan_is_left_alone() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (_, client) = register_and_approve(&state).await?;

    let job = seed_avail(
        dir.path(),
        "job-staging",
        ExpiresAt::Never,
        &expirable_body("job-staging", &[&client]),
    )?;
    let plan_id = test_plan_id();
    seed_plan(
        &state,
        &plan_manifest(&plan_id, PlanStatus::Creating, &[&job]),
        false,
    )
    .await?;

    run_qm(&state).await?;

    assert!(
        state.todo_store.get_avail_by_job(&job).await?.is_some(),
        "a live ingest's promoted jobs are untouched"
    );
    assert_eq!(
        read_plan(&state, &plan_id).await?.status,
        PlanStatus::Creating
    );
    Ok(())
}

/// Ingestion writes the manifest until it hands off and `put_plan` keeps no
/// latch, so the marker outlives the latch until the ingest deadline. The latch
/// is this pass's own write, so it proves no handoff.
#[tokio::test]
async fn test_cancelling_a_creating_plan_keeps_its_marker_while_its_ingest_could_run()
-> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (_, client) = register_and_approve(&state).await?;

    let job = seed_avail(
        dir.path(),
        "job-midingest",
        ExpiresAt::Never,
        &expirable_body("job-midingest", &[&client]),
    )?;
    let plan_id = test_plan_id();
    seed_plan(
        &state,
        &plan_manifest(&plan_id, PlanStatus::Creating, &[&job]),
        true,
    )
    .await?;

    run_qm(&state).await?;

    assert!(
        state.todo_store.get_avail_by_job(&job).await?.is_none(),
        "a marker tears the plan down without waiting out the deadline"
    );
    assert_eq!(
        read_plan(&state, &plan_id).await?.status,
        PlanStatus::Cancelling
    );
    assert!(
        state.plan_store.clone().has_cancel_marker(&plan_id).await?,
        "the marker is held while the plan could still be overwritten by its ingest"
    );

    // The second run retires nothing, so it settles; the marker still stays.
    run_qm(&state).await?;
    assert_eq!(
        read_plan(&state, &plan_id).await?.status,
        PlanStatus::Cancelled
    );
    assert!(
        state.plan_store.clone().has_cancel_marker(&plan_id).await?,
        "the latch is this pass's own write, so it proves no handoff"
    );

    // Past the ingest deadline no ingest can hold the manifest, so the marker goes.
    let stamped = read_plan(&state, &plan_id).await?.terminal_at;
    run_qm_full(&state, Duration::from_secs(86_400), Duration::ZERO).await?;
    assert!(!state.plan_store.clone().has_cancel_marker(&plan_id).await?);
    let settled = read_plan(&state, &plan_id).await?;
    assert_eq!(settled.status, PlanStatus::Cancelled);
    assert_eq!(
        settled.terminal_at, stamped,
        "a terminal latch is stamped once"
    );
    Ok(())
}

/// All-or-nothing cases cannot catch a pass that latches `pending_clients` on
/// any unmatched job, or skips `starved` while `active`.
#[tokio::test]
async fn test_partial_starvation_stays_active_and_reports_starved() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (_, client) = register_and_approve(&state).await?;
    set_profile(&state, &client, os_profile("macOS")?).await?;

    let mut matched_body = expirable_body("job-macos", &[]);
    matched_body["requires"] = json!(["os:macos"]);
    let matched = seed_avail(dir.path(), "job-macos", ExpiresAt::Never, &matched_body)?;

    let mut starved_body = expirable_body("job-ios", &[]);
    starved_body["requires"] = json!(["os:ios"]);
    let starved = seed_avail(dir.path(), "job-ios", ExpiresAt::Never, &starved_body)?;

    let plan_id = test_plan_id();
    seed_plan(
        &state,
        &plan_manifest(&plan_id, PlanStatus::Active, &[&matched, &starved]),
        false,
    )
    .await?;

    run_qm(&state).await?;

    let manifest = read_plan(&state, &plan_id).await?;
    assert_eq!(
        manifest.status,
        PlanStatus::Active,
        "one matching job keeps the plan active"
    );
    let snapshot = manifest
        .progress_snapshot
        .ok_or_else(|| anyhow::anyhow!("snapshot"))?;
    assert_eq!(snapshot.counts.available, 2);
    assert_eq!(
        snapshot.starved.len(),
        1,
        "only the unmatched job is starved"
    );
    assert_eq!(snapshot.starved[0].job_ids, vec![starved]);
    assert_eq!(
        snapshot.starved[0].clauses.requires,
        vec!["os:ios".to_string()]
    );
    Ok(())
}

/// Negatives split a group too: jobs that differ only in what they exclude are
/// starved for different reasons. A job id is its name here.
#[rstest]
#[case::group_by_requirement_set(
    &[
        ("job-ios-a", json!({"requires": ["os:ios"]})),
        ("job-ios-b", json!({"requires": ["os:ios"]})),
        ("job-macos", json!({"requires": ["os:macos"]})),
    ],
    // First-appearance order: the two os:ios jobs were seeded first.
    json!([
        {"requires": ["os:ios"], "any_of": [], "clients": [],
         "job_ids": ["job-ios-a", "job-ios-b"]},
        {"requires": ["os:macos"], "any_of": [], "clients": [],
         "job_ids": ["job-macos"]},
    ])
)]
#[case::split_by_exclusions(
    &[
        ("job-plain", json!({"requires": ["os:ios"]})),
        ("job-excluded", json!({
            "requires": ["os:ios"],
            "excludes": ["chip:a15"],
            "not_all_of": [["chip:a16", "runtime:mlx"]],
            "exclude_clients": ["ev1_reserved"],
        })),
    ],
    // Identical `requires`, different exclusions: two groups, and the negatives
    // are omitted from the group that carries none.
    json!([
        {"requires": ["os:ios"], "any_of": [], "clients": [],
         "job_ids": ["job-plain"]},
        {"requires": ["os:ios"], "any_of": [], "clients": [],
         "exclude_clients": ["ev1_reserved"],
         "excludes": ["chip:a15"],
         "not_all_of": [["chip:a16", "runtime:mlx"]],
         "job_ids": ["job-excluded"]},
    ])
)]
#[tokio::test]
async fn test_starved_jobs_group_by_shared_eligibility(
    #[case] jobs: &[(&str, Value)],
    #[case] expected: Value,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    // An approved client that matches nothing, so every job is starved.
    let (_, client) = register_and_approve(&state).await?;
    set_profile(&state, &client, os_profile("Windows")?).await?;

    let seeded: Vec<JobId> = jobs
        .iter()
        .map(|(name, eligibility)| {
            let mut body = expirable_body(name, &[]);
            eligibility
                .as_object()
                .into_iter()
                .flatten()
                .for_each(|(field, value)| body[field] = value.clone());
            seed_avail(dir.path(), name, ExpiresAt::Never, &body)
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    let plan_id = test_plan_id();
    let refs: Vec<&JobId> = seeded.iter().collect();
    seed_plan(
        &state,
        &plan_manifest(&plan_id, PlanStatus::Active, &refs),
        false,
    )
    .await?;

    run_qm(&state).await?;

    let snapshot = read_plan(&state, &plan_id)
        .await?
        .progress_snapshot
        .ok_or_else(|| anyhow::anyhow!("snapshot"))?;
    assert_eq!(serde_json::to_value(&snapshot.starved)?, expected);
    Ok(())
}

#[tokio::test]
async fn test_leased_job_keeps_plan_active_without_a_matching_client() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (_, client) = register_and_approve(&state).await?;
    set_profile(&state, &client, os_profile("Windows")?).await?;

    let mut body = expirable_body("job-leased", &[]);
    body["requires"] = json!(["os:ios"]);
    let job = seed_avail(dir.path(), "job-leased", ExpiresAt::Never, &body)?;
    state
        .todo_store
        .claim_job(
            &job,
            ExpiresAt::Never,
            &client,
            Utc::now() + chrono::Duration::hours(1),
        )
        .await?;

    let plan_id = test_plan_id();
    seed_plan(
        &state,
        &plan_manifest(&plan_id, PlanStatus::PendingClients, &[&job]),
        false,
    )
    .await?;

    run_qm(&state).await?;

    let manifest = read_plan(&state, &plan_id).await?;
    assert_eq!(
        manifest.status,
        PlanStatus::Active,
        "a leased job flips the plan back to active"
    );
    let snapshot = manifest
        .progress_snapshot
        .ok_or_else(|| anyhow::anyhow!("snapshot"))?;
    assert_eq!(snapshot.counts.running, 1);
    assert!(
        snapshot.starved.is_empty(),
        "a leased job is never reported starved"
    );
    Ok(())
}

/// The client is done with it, so progress must not wait on the scorer.
#[tokio::test]
async fn test_incoming_record_counts_as_finished() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let job = pipette_web_testkit::job("job-scoring")?;
    state
        .submission_store
        .write_incoming(
            &job,
            &json!({"message_type": "success", "job_id": "job-scoring"}),
        )
        .await?;

    let plan_id = test_plan_id();
    seed_plan(
        &state,
        &plan_manifest(&plan_id, PlanStatus::Active, &[&job]),
        false,
    )
    .await?;

    run_qm(&state).await?;

    let manifest = read_plan(&state, &plan_id).await?;
    assert_eq!(manifest.status, PlanStatus::Complete);
    let counts = manifest
        .progress_snapshot
        .ok_or_else(|| anyhow::anyhow!("snapshot"))?
        .counts;
    assert_eq!(counts.finished, 1, "an incoming/ record is finished");
    assert_eq!(counts.cancelled, 0);
    Ok(())
}

// ─── shared jobs: membership, release & the plan-index GC ───────────────────

async fn seed_membership(
    state: &AppState,
    job_id: &JobId,
    plan_id: &PlanId,
    expires_at: ExpiresAt,
) -> anyhow::Result<()> {
    state
        .plan_store
        .clone()
        .write_job_plan_marker(job_id, plan_id, expires_at)
        .await
}

/// One `avail/` job held by two active plans, with a cancel on the first.
async fn seed_shared_job(
    state: &AppState,
    dir: &std::path::Path,
    client: &ClientId,
) -> anyhow::Result<(JobId, PlanId, PlanId)> {
    let shared = seed_avail(
        dir,
        "job-shared",
        ExpiresAt::Never,
        &expirable_body("job-shared", &[client]),
    )?;
    let cancelling = test_plan_id();
    let surviving = other_plan_id();
    seed_membership(state, &shared, &cancelling, ExpiresAt::Never).await?;
    seed_membership(state, &shared, &surviving, ExpiresAt::Never).await?;
    seed_plan(
        state,
        &plan_manifest(&cancelling, PlanStatus::Active, &[&shared]),
        true,
    )
    .await?;
    seed_plan(
        state,
        &plan_manifest(&surviving, PlanStatus::Active, &[&shared]),
        false,
    )
    .await?;
    Ok((shared, cancelling, surviving))
}

async fn members(state: &AppState, job_id: &JobId) -> anyhow::Result<Vec<PlanId>> {
    let mut plans: Vec<PlanId> = state
        .plan_store
        .clone()
        .list_job_plan_markers(job_id)
        .await?
        .into_iter()
        .map(|(plan_id, _)| plan_id)
        .collect();
    plans.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    Ok(plans)
}

/// The releasing plan settles in the same pass, since a release deletes nothing
/// from the queue, and counts the job `available`, not `cancelled`.
#[tokio::test]
async fn test_cancel_of_one_member_releases_a_shared_job() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (_, client) = register_and_approve(&state).await?;

    let (shared, cancelling, surviving) = seed_shared_job(&state, dir.path(), &client).await?;

    run_qm(&state).await?;

    assert!(
        state.todo_store.get_avail_by_job(&shared).await?.is_some(),
        "the shared job stays claimable for the plan that still wants it"
    );
    assert_eq!(
        members(&state, &shared).await?,
        vec![surviving.clone()],
        "only the cancelling plan's membership is dropped"
    );

    let released = read_plan(&state, &cancelling).await?;
    assert_eq!(
        released.status,
        PlanStatus::Cancelled,
        "a release does not hold the settle"
    );
    assert!(released.terminal_at.is_some());
    let counts = released
        .progress_snapshot
        .ok_or_else(|| anyhow::anyhow!("the pass writes a snapshot"))?
        .counts;
    assert_eq!(counts.total, 1);
    assert_eq!(
        counts.available, 1,
        "the job is queued, so the honest bucket is available"
    );
    assert_eq!(
        counts.cancelled, 0,
        "cancelled counts jobs teardown retired, and this one was released"
    );

    let survivor = read_plan(&state, &surviving).await?;
    assert_eq!(survivor.status, PlanStatus::Active);
    assert!(
        eligible_jobs(&state, &client).await?.contains(&shared),
        "the release keeps the job's eligible markers"
    );

    // A later pass does not revive the released job as outstanding.
    run_qm(&state).await?;
    assert_eq!(
        read_plan(&state, &cancelling).await?.status,
        PlanStatus::Cancelled
    );
    assert!(state.todo_store.get_avail_by_job(&shared).await?.is_some());
    Ok(())
}

/// Only teardown releases a job, on an ending plan. Read on an active plan, one
/// lost marker beside a co-owner's would latch it `complete` while the job is
/// still claimable.
#[tokio::test]
async fn test_active_plan_keeps_a_job_whose_membership_marker_is_missing() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (_, client) = register_and_approve(&state).await?;

    let shared = seed_avail(
        dir.path(),
        "job-shared",
        ExpiresAt::Never,
        &expirable_body("job-shared", &[&client]),
    )?;
    let forgotten = test_plan_id();
    let co_owner = other_plan_id();

    // Both plans hold the job, and only the co-owner has a marker to show for it.
    seed_membership(&state, &shared, &co_owner, ExpiresAt::Never).await?;
    for plan_id in [&forgotten, &co_owner] {
        seed_plan(
            &state,
            &plan_manifest(plan_id, PlanStatus::Active, &[&shared]),
            false,
        )
        .await?;
    }

    run_qm(&state).await?;

    let plan = read_plan(&state, &forgotten).await?;
    assert_eq!(
        plan.status,
        PlanStatus::Active,
        "a missing marker does not settle a plan whose job is still queued"
    );
    assert!(plan.terminal_at.is_none());
    assert_eq!(
        plan.progress_snapshot
            .ok_or_else(|| anyhow::anyhow!("the pass writes a snapshot"))?
            .counts
            .available,
        1,
        "the job is still the plan's to wait on"
    );
    assert!(state.todo_store.get_avail_by_job(&shared).await?.is_some());
    Ok(())
}

#[tokio::test]
async fn test_cancel_of_the_last_member_retires_the_shared_job() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (_, client) = register_and_approve(&state).await?;

    let (shared, _first, last) = seed_shared_job(&state, dir.path(), &client).await?;

    run_qm(&state).await?;
    assert!(state.todo_store.get_avail_by_job(&shared).await?.is_some());

    state.plan_store.clone().write_cancel_marker(&last).await?;
    run_qm(&state).await?;

    assert!(
        state.todo_store.get_avail_by_job(&shared).await?.is_none(),
        "the last membership going retires the job"
    );
    assert!(members(&state, &shared).await?.is_empty());
    let retiring = read_plan(&state, &last).await?;
    assert_eq!(
        retiring.status,
        PlanStatus::Cancelling,
        "the pass that retired the job does not settle the plan"
    );
    let counts = retiring
        .progress_snapshot
        .ok_or_else(|| anyhow::anyhow!("snapshot"))?
        .counts;
    assert_eq!(
        counts.cancelled, 1,
        "teardown retired it, so it counts here"
    );

    run_qm(&state).await?;
    assert_eq!(
        read_plan(&state, &last).await?.status,
        PlanStatus::Cancelled
    );
    Ok(())
}

/// A job with no membership marker predates the tree. Absence is not a release,
/// or the plan would settle while its only job kept running.
#[tokio::test]
async fn test_a_job_with_no_membership_marker_still_retires() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (_, client) = register_and_approve(&state).await?;

    let legacy = seed_avail(
        dir.path(),
        "job-legacy",
        ExpiresAt::Never,
        &expirable_body("job-legacy", &[&client]),
    )?;
    let plan_id = test_plan_id();
    seed_plan(
        &state,
        &plan_manifest(&plan_id, PlanStatus::Active, &[&legacy]),
        true,
    )
    .await?;

    run_qm(&state).await?;

    assert!(
        state.todo_store.get_avail_by_job(&legacy).await?.is_none(),
        "a job with no marker is retired, not read as released"
    );
    let manifest = read_plan(&state, &plan_id).await?;
    assert_eq!(manifest.status, PlanStatus::Cancelling);
    assert_eq!(
        manifest
            .progress_snapshot
            .ok_or_else(|| anyhow::anyhow!("snapshot"))?
            .counts
            .cancelled,
        1
    );
    Ok(())
}

/// Inside `plan_creating_max_age_secs` an orphan may belong to an ingest still
/// staging, and collecting it could let a later last-member teardown retire a
/// live plan's job. Past the gate, two sightings are still needed.
#[tokio::test]
async fn test_plan_index_gc_waits_for_the_age_gate_and_a_second_sighting() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let plans = state.plan_store.clone();

    // A record and a marker for a job that is in neither `avail/` nor `leased/`.
    let dead = pipette_web_testkit::job("job-dead")?;
    let owner = test_plan_id();
    let digest = "c".repeat(64);
    plans
        .create_content_key(
            &digest,
            &pipette_store::ContentKeyRecord {
                job_id: dead.clone(),
                plan_id: owner.clone(),
                created_at: Utc::now(),
            },
        )
        .await?;
    seed_membership(&state, &dead, &owner, ExpiresAt::Never).await?;
    seed_plan(
        &state,
        &plan_manifest(&owner, PlanStatus::Active, &[&dead]),
        false,
    )
    .await?;

    // The default six-hour gate: both were written a moment ago.
    run_qm(&state).await?;
    run_qm(&state).await?;
    assert_eq!(
        plans.list_content_keys().await?.len(),
        1,
        "a record younger than the gate belongs to an ingest that may still be staging"
    );
    assert_eq!(members(&state, &dead).await?, vec![owner.clone()]);

    // A zero-length gate: the first run stages them, the second collects.
    run_qm_full(&state, Duration::from_secs(86_400), Duration::ZERO).await?;
    assert_eq!(
        plans.list_content_keys().await?.len(),
        1,
        "a first sighting is staged, not collected"
    );
    run_qm_full(&state, Duration::from_secs(86_400), Duration::ZERO).await?;
    assert!(
        plans.list_content_keys().await?.is_empty(),
        "the second consecutive sighting collects the record"
    );
    assert!(
        members(&state, &dead).await?.is_empty(),
        "the marker of a job that left the queue is collected too"
    );
    Ok(())
}

/// Even while the job is live: a deleted plan's marker would otherwise pin a
/// `never`-expiry shared job in the queue for good.
#[tokio::test]
async fn test_plan_index_gc_collects_a_marker_whose_manifest_is_gone() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (_, client) = register_and_approve(&state).await?;

    let live = seed_avail(
        dir.path(),
        "job-live",
        ExpiresAt::Never,
        &expirable_body("job-live", &[&client]),
    )?;
    let kept = test_plan_id();
    let forgotten = other_plan_id();
    seed_membership(&state, &live, &kept, ExpiresAt::Never).await?;
    seed_membership(&state, &live, &forgotten, ExpiresAt::Never).await?;
    seed_plan(
        &state,
        &plan_manifest(&kept, PlanStatus::Active, &[&live]),
        false,
    )
    .await?;

    run_qm_full(&state, Duration::from_secs(86_400), Duration::ZERO).await?;
    run_qm_full(&state, Duration::from_secs(86_400), Duration::ZERO).await?;

    assert_eq!(
        members(&state, &live).await?,
        vec![kept],
        "the marker of a plan with no manifest is collected; the live plan's is kept"
    );
    assert!(
        state.todo_store.get_avail_by_job(&live).await?.is_some(),
        "the job itself is untouched — it still has a member"
    );
    Ok(())
}

/// A confirmed join always lands in `job_ids`, so past the age gate an omission
/// proves an abandoned join. Kept, the ghost would make the owner's teardown
/// release the job instead of retiring it.
#[tokio::test]
async fn test_plan_index_gc_collects_a_marker_whose_plan_omits_the_job() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (_, client) = register_and_approve(&state).await?;

    let shared = seed_avail(
        dir.path(),
        "job-shared",
        ExpiresAt::Never,
        &expirable_body("job-shared", &[&client]),
    )?;
    let fresh = seed_avail(
        dir.path(),
        "job-fresh",
        ExpiresAt::Never,
        &expirable_body("job-fresh", &[&client]),
    )?;
    // The abandoner lost the target inside the merge window and failed its marker
    // delete, so its manifest lists only its own job.
    let owner = test_plan_id();
    let abandoner = other_plan_id();
    seed_membership(&state, &shared, &owner, ExpiresAt::Never).await?;
    seed_membership(&state, &shared, &abandoner, ExpiresAt::Never).await?;
    seed_membership(&state, &fresh, &abandoner, ExpiresAt::Never).await?;
    seed_plan(
        &state,
        &plan_manifest(&owner, PlanStatus::Active, &[&shared]),
        false,
    )
    .await?;
    seed_plan(
        &state,
        &plan_manifest(&abandoner, PlanStatus::Active, &[&fresh]),
        false,
    )
    .await?;

    run_qm_full(&state, Duration::from_secs(86_400), Duration::ZERO).await?;
    run_qm_full(&state, Duration::from_secs(86_400), Duration::ZERO).await?;

    assert_eq!(
        members(&state, &shared).await?,
        vec![owner],
        "the ghost marker is collected; the owner's is kept"
    );
    assert_eq!(
        members(&state, &fresh).await?,
        vec![abandoner],
        "a marker the manifest backs is kept"
    );
    assert!(
        state.todo_store.get_avail_by_job(&shared).await?.is_some(),
        "the job itself is untouched — its owner still holds it"
    );
    Ok(())
}

#[tokio::test]
async fn test_one_result_advances_both_plans_holding_the_job() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (key, client) = register_and_approve(&state).await?;

    let shared = seed_avail(
        dir.path(),
        "job-shared",
        ExpiresAt::Never,
        &expirable_body("job-shared", &[&client]),
    )?;
    let first = test_plan_id();
    let second = other_plan_id();
    for plan_id in [&first, &second] {
        seed_membership(&state, &shared, plan_id, ExpiresAt::Never).await?;
        seed_plan(
            &state,
            &plan_manifest(plan_id, PlanStatus::Active, &[&shared]),
            false,
        )
        .await?;
    }

    state
        .todo_store
        .claim_job(
            &shared,
            ExpiresAt::Never,
            &client,
            Utc::now() + chrono::Duration::hours(1),
        )
        .await?;
    let mut body = valid_submission("prefill_throughput_256");
    body["job_id"] = json!("job-shared");
    let resp = authed_post(&state, &key, &client, "/benchmarks", &body).await?;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    run_qm(&state).await?;

    for plan_id in [&first, &second] {
        let manifest = read_plan(&state, plan_id).await?;
        assert_eq!(
            manifest.status,
            PlanStatus::Complete,
            "the one result completes {plan_id}"
        );
        let counts = manifest
            .progress_snapshot
            .ok_or_else(|| anyhow::anyhow!("snapshot"))?
            .counts;
        assert_eq!(counts.total, 1);
        assert_eq!(counts.finished, 1, "both plans read the same record");
        assert_eq!(counts.cancelled, 0);
    }
    Ok(())
}

// ─── per-plan job expirations ───────────────────────────────────────────────

fn lapsed() -> ExpiresAt {
    ExpiresAt::At(Utc.with_ymd_and_hms(2020, 1, 2, 3, 4, 5).unwrap())
}

fn distant() -> ExpiresAt {
    ExpiresAt::At(Utc.with_ymd_and_hms(2099, 1, 1, 0, 0, 0).unwrap())
}

async fn lapse_of(
    state: &AppState,
    plan_id: &PlanId,
    job_id: &JobId,
) -> anyhow::Result<Option<ExpiresAt>> {
    Ok(read_plan(state, plan_id)
        .await?
        .expired_jobs
        .get(job_id)
        .map(|entry| ExpiresAt::At(entry.horizon)))
}

/// The lapsing plan records the job in `expired_jobs`, counts it `failed` and
/// loses its markers; the job is re-dated onto the other horizon in one run.
/// The reconciliation pass rewrites the manifest later in the same run, and
/// must not clobber the lapse.
#[tokio::test]
async fn test_a_lapsed_member_re_dates_the_job_for_its_other_plan() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (_, client) = register_and_approve(&state).await?;

    // The sooner plan minted the job, so the queue carries its horizon.
    let shared = seed_avail(
        dir.path(),
        "job-shared",
        lapsed(),
        &expirable_body("job-shared", &[&client]),
    )?;
    // A second job keeps the sooner plan non-terminal, so the reconciliation
    // pass rewrites its manifest rather than returning early.
    let solo = seed_avail(
        dir.path(),
        "job-solo",
        distant(),
        &expirable_body("job-solo", &[&client]),
    )?;
    let sooner = test_plan_id();
    let later = other_plan_id();
    seed_membership(&state, &shared, &sooner, lapsed()).await?;
    seed_membership(&state, &shared, &later, distant()).await?;
    seed_membership(&state, &solo, &sooner, distant()).await?;
    seed_plan(
        &state,
        &plan_manifest(&sooner, PlanStatus::Active, &[&shared, &solo]),
        false,
    )
    .await?;
    seed_plan(
        &state,
        &plan_manifest(&later, PlanStatus::Active, &[&shared]),
        false,
    )
    .await?;

    run_qm(&state).await?;

    assert_eq!(lapse_of(&state, &sooner, &shared).await?, Some(lapsed()));
    let sooner_manifest = read_plan(&state, &sooner).await?;
    assert_eq!(sooner_manifest.status, PlanStatus::Active);
    let counts = sooner_manifest
        .progress_snapshot
        .ok_or_else(|| anyhow::anyhow!("snapshot"))?
        .counts;
    assert_eq!((counts.total, counts.failed, counts.available), (2, 1, 1));

    assert_eq!(members(&state, &shared).await?, vec![later.clone()]);
    assert_eq!(lapse_of(&state, &later, &shared).await?, None);
    let later_counts = read_plan(&state, &later)
        .await?
        .progress_snapshot
        .ok_or_else(|| anyhow::anyhow!("snapshot"))?
        .counts;
    assert_eq!(
        (
            later_counts.total,
            later_counts.available,
            later_counts.failed
        ),
        (1, 1, 0)
    );

    let mut avail = state.todo_store.list_avail().await?;
    avail.sort();
    assert_eq!(
        avail,
        vec![
            avail_filename(&shared, distant()),
            avail_filename(&solo, distant()),
        ]
    );
    let mut markers = state.todo_store.list_eligible_for_client(&client).await?;
    markers.sort();
    assert_eq!(
        markers,
        vec![(shared, distant()), (solo, distant())],
        "the eligible markers follow the re-dated key in the same run"
    );
    Ok(())
}

/// As for a single-plan job: one synthetic `expired` failure. Both plans record
/// their own lapse and count the job `failed`.
#[tokio::test]
async fn test_the_last_member_horizon_lapsing_retires_the_job() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (_, client) = register_and_approve(&state).await?;

    let shared = seed_avail(
        dir.path(),
        "job-shared",
        lapsed(),
        &expirable_body("job-shared", &[&client]),
    )?;
    let first = test_plan_id();
    let second = other_plan_id();
    for plan_id in [&first, &second] {
        seed_membership(&state, &shared, plan_id, lapsed()).await?;
        seed_plan(
            &state,
            &plan_manifest(plan_id, PlanStatus::Active, &[&shared]),
            false,
        )
        .await?;
    }

    run_qm(&state).await?;

    let record = state
        .submission_store
        .get_submission(&shared)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no submission record for the retired job"))?;
    assert_eq!(record.body["failure_source"], "expired");
    assert!(state.todo_store.get_avail_by_job(&shared).await?.is_none());
    assert!(members(&state, &shared).await?.is_empty());

    for plan_id in [&first, &second] {
        assert_eq!(lapse_of(&state, plan_id, &shared).await?, Some(lapsed()));
        let manifest = read_plan(&state, plan_id).await?;
        assert_eq!(
            manifest.status,
            PlanStatus::Complete,
            "a plan whose only job lapsed has nothing left to wait on"
        );
        let counts = manifest
            .progress_snapshot
            .ok_or_else(|| anyhow::anyhow!("snapshot"))?
            .counts;
        assert_eq!((counts.total, counts.failed), (1, 1));
    }
    Ok(())
}

/// The job runs on for another plan, so liveness alone would hold this one open.
#[tokio::test]
async fn test_a_plan_completes_when_its_last_job_lapses() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (_, client) = register_and_approve(&state).await?;

    let shared = seed_avail(
        dir.path(),
        "job-shared",
        lapsed(),
        &expirable_body("job-shared", &[&client]),
    )?;
    let sooner = test_plan_id();
    let later = other_plan_id();
    seed_membership(&state, &shared, &sooner, lapsed()).await?;
    seed_membership(&state, &shared, &later, distant()).await?;
    seed_plan(
        &state,
        &plan_manifest(&sooner, PlanStatus::Active, &[&shared]),
        false,
    )
    .await?;
    seed_plan(
        &state,
        &plan_manifest(&later, PlanStatus::Active, &[&shared]),
        false,
    )
    .await?;

    run_qm(&state).await?;

    let manifest = read_plan(&state, &sooner).await?;
    assert_eq!(manifest.status, PlanStatus::Complete);
    assert!(manifest.terminal_at.is_some());
    let counts = manifest
        .progress_snapshot
        .ok_or_else(|| anyhow::anyhow!("snapshot"))?
        .counts;
    assert_eq!((counts.total, counts.failed, counts.available), (1, 1, 0));
    assert!(
        state.todo_store.get_avail_by_job(&shared).await?.is_some(),
        "the job runs on for the plan whose horizon stands"
    );
    Ok(())
}

/// The lapsing plan counts the job `failed`, not `running`: any result now
/// arrives too late for it.
#[tokio::test]
async fn test_a_lapse_outranks_a_live_lease() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (_, client) = register_and_approve(&state).await?;

    let shared = pipette_web_testkit::seed_leased(
        dir.path(),
        "job-shared",
        &client,
        &expirable_body("job-shared", &[&client]),
    )?;
    let sooner = test_plan_id();
    let later = other_plan_id();
    seed_membership(&state, &shared, &sooner, lapsed()).await?;
    seed_membership(&state, &shared, &later, distant()).await?;
    seed_plan(
        &state,
        &plan_manifest(&sooner, PlanStatus::Active, &[&shared]),
        false,
    )
    .await?;
    seed_plan(
        &state,
        &plan_manifest(&later, PlanStatus::Active, &[&shared]),
        false,
    )
    .await?;

    run_qm(&state).await?;

    assert_eq!(lapse_of(&state, &sooner, &shared).await?, Some(lapsed()));
    let counts = read_plan(&state, &sooner)
        .await?
        .progress_snapshot
        .ok_or_else(|| anyhow::anyhow!("snapshot"))?
        .counts;
    assert_eq!((counts.failed, counts.running), (1, 0));
    let later_counts = read_plan(&state, &later)
        .await?
        .progress_snapshot
        .ok_or_else(|| anyhow::anyhow!("snapshot"))?
        .counts;
    assert_eq!(
        (later_counts.failed, later_counts.running),
        (0, 1),
        "the plan whose horizon stands still sees the job running"
    );
    assert_eq!(state.todo_store.list_leased().await?.len(), 1);
    Ok(())
}

/// The record decides the bucket, as for a plan holding the job alone.
#[tokio::test]
async fn test_a_recorded_job_is_never_recorded_as_lapsed() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let (key, client) = register_and_approve(&state).await?;

    let done = pipette_web_testkit::seed_leased(
        dir.path(),
        "job-done",
        &client,
        &expirable_body("job-done", &[&client]),
    )?;
    let mut body = valid_submission("prefill_throughput_256");
    body["job_id"] = json!("job-done");
    let resp = authed_post(&state, &key, &client, "/benchmarks", &body).await?;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    // The plan's horizon has since passed, but the answer arrived first.
    let plan_id = test_plan_id();
    seed_membership(&state, &done, &plan_id, lapsed()).await?;
    seed_plan(
        &state,
        &plan_manifest(&plan_id, PlanStatus::Active, &[&done]),
        false,
    )
    .await?;

    run_qm(&state).await?;

    let manifest = read_plan(&state, &plan_id).await?;
    assert!(
        manifest.expired_jobs.is_empty(),
        "a job with a record is not a lapse"
    );
    let counts = manifest
        .progress_snapshot
        .ok_or_else(|| anyhow::anyhow!("snapshot"))?
        .counts;
    assert_eq!((counts.finished, counts.failed), (1, 0));
    Ok(())
}

/// The two-sighting rule guards against listing races; a key naming a
/// superseded deadline is dead regardless.
#[tokio::test]
async fn test_a_marker_naming_a_superseded_deadline_is_collected_at_once() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let client = register_indexed_worker(&state).await?;

    let job = seed_avail(
        dir.path(),
        "job-1",
        distant(),
        &expirable_body("job-1", &[&client]),
    )?;
    // The residue of a re-date whose run died before the index passes ran.
    state
        .todo_store
        .write_eligible(&client, &job, lapsed())
        .await?;

    run_qm(&state).await?;

    assert_eq!(
        state.todo_store.list_eligible_for_client(&client).await?,
        vec![(job, distant())],
        "only the marker naming the job's current key survives"
    );
    Ok(())
}
