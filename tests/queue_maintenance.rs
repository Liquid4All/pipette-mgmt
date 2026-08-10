//! Tests for the `queue-maintenance` passes (`queue_maintenance::run`). Jobs
//! are seeded into `avail/` by writing the file directly — the `TodoStore`
//! trait has no `write_avail` (job creation is a separate, not-yet-built
//! concern), and the local_fs layout is stable.

mod helpers;

use std::time::Duration;

use chrono::{TimeZone, Utc};
use rstest::rstest;
use serde_json::{Value, json};

use helpers::{job, make_state, register_and_approve, seed_avail, setup_benchmarks};

use pipette_mgmt::client::{Client, DeviceProfile};
use pipette_mgmt::handlers::AppState;
use pipette_mgmt::queue_maintenance;
use pipette_mgmt::stores::{ClaimResult, JobState, RecycleResult, ScoreQueueStage};
use pipette_mgmt::todo_filename::leased_key;
use pipette_mgmt::types::{ClientId, ExpiresAt, JobId};
use pipette_mgmt::validated::NonEmptyTrimmedString;

/// Run every maintenance pass with a tmp/ age far larger than any test's
/// runtime, so the tmp pass never interferes with unrelated assertions.
async fn run_qm(state: &AppState) -> anyhow::Result<()> {
    run_qm_with_tmp_age(state, Duration::from_secs(86_400)).await
}

async fn run_qm_with_tmp_age(state: &AppState, tmp_age: Duration) -> anyhow::Result<()> {
    let catalog = state.catalog_cache.get().await?;
    queue_maintenance::run(
        &*state.todo_store,
        &*state.auth_store,
        &*state.submission_store,
        &catalog,
        tmp_age,
    )
    .await
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
            "runtime": {"type": "llamacpp_cli_stock_tools", "source": "github_release", "repository_version": "b1000", "flavor": "macos-arm64"},
        },
    })
}

/// Overwrite a registered client's stored device profile.
async fn set_profile(
    state: &AppState,
    client_id: &ClientId,
    profile: DeviceProfile,
) -> anyhow::Result<()> {
    let mut client: Client = state
        .auth_store
        .get_client(client_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("client {client_id} not found"))?;
    client.device_profile = profile;
    state.auth_store.put_client(&client).await?;
    Ok(())
}

fn os_profile(os_name: &str) -> DeviceProfile {
    DeviceProfile {
        device_os_name: Some(NonEmptyTrimmedString::try_new(os_name).unwrap()),
        ..Default::default()
    }
}

fn device_profile(os_name: &str, device_name: &str) -> DeviceProfile {
    DeviceProfile {
        device_name: Some(NonEmptyTrimmedString::try_new(device_name).unwrap()),
        ..os_profile(os_name)
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

#[tokio::test]
async fn test_new_job_indexed_only_for_matching_clients() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let (_, matching) = register_and_approve(&state).await?;
    let (_, other) = register_and_approve(&state).await?;
    set_profile(&state, &matching, os_profile("macOS")).await?;
    set_profile(&state, &other, os_profile("Linux")).await?;

    let job = seed_avail(
        dir.path(),
        "job-1",
        ExpiresAt::Never,
        &json!({"requires": ["os:macos"]}),
    )?;

    run_qm(&state).await?;

    assert_eq!(eligible_jobs(&state, &matching).await?, vec![job]);
    assert!(eligible_jobs(&state, &other).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn test_any_of_job_indexed_only_for_group_matching_clients() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let (_, in_family) = register_and_approve(&state).await?;
    let (_, out_of_family) = register_and_approve(&state).await?;
    set_profile(&state, &in_family, device_profile("iOS", "iPhone 17 Pro")).await?;
    set_profile(&state, &out_of_family, device_profile("iOS", "iPhone 15")).await?;

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

#[tokio::test]
async fn test_cursor_skips_already_indexed_jobs() -> anyhow::Result<()> {
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

    // First pass indexes the job.
    run_qm(&state).await?;
    assert_eq!(eligible_jobs(&state, &client).await?, vec![job.clone()]);

    // Delete the marker, then run again with no new jobs and no reindex flags.
    // The cursor is past this job, so the new-jobs pass must skip it and the
    // marker must stay deleted (proving the pass was incremental, not full).
    state
        .todo_store
        .delete_eligible(&client, &job, ExpiresAt::Never)
        .await?;
    run_qm(&state).await?;
    assert!(eligible_jobs(&state, &client).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn test_pending_reindex_reevaluates_after_profile_change() -> anyhow::Result<()> {
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

    // No profile yet → not matched, and the cursor now sits past the job.
    run_qm(&state).await?;
    assert!(eligible_jobs(&state, &client).await?.is_empty());

    // Profile now matches; flag the client. Only the reindex pass (not the
    // cursor-bound new-jobs pass) can pick up the pre-existing job.
    set_profile(&state, &client, os_profile("macOS")).await?;
    state.todo_store.write_pending_reindex(&client).await?;

    run_qm(&state).await?;
    assert_eq!(eligible_jobs(&state, &client).await?, vec![job]);
    // Flag consumed.
    assert!(state.todo_store.list_pending_reindex().await?.is_empty());
    Ok(())
}

/// Foreign cruft in the flag directories (a `.DS_Store`, a stray operator
/// file — `.` is outside both id charsets) is skipped with a warning, not an
/// error: one unparseable name must not abort the run — the passes that
/// follow the flag listings (settle, GC sweeps, tmp cleanup) still execute —
/// and real flags alongside it are still processed. The cruft itself is left
/// in place; it is not the system's to delete.
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
    set_profile(&state, &client, os_profile("macOS")).await?;
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

    // The run completes and the real flag is still reindexed around the cruft.
    run_qm(&state).await?;
    assert_eq!(eligible_jobs(&state, &client).await?, vec![job]);
    assert!(state.todo_store.list_pending_reindex().await?.is_empty());
    assert!(cruft_client.try_exists()?);
    assert!(cruft_jobs.try_exists()?);
    Ok(())
}

/// One client with several outstanding flag keys (every `PATCH /clients/me`
/// profile change writes two — pre-relinquish and post-persist) gets a single
/// rebuild that consumes them all: no flag left behind to trigger a redundant
/// rebuild, no key spuriously surviving to re-open the gate's latency window.
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
    set_profile(&state, &client, os_profile("macOS")).await?;
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

    // Client is removed from the auth store but still has a marker + flags
    // (two keys, as a real profile-change PATCH leaves).
    state.auth_store.delete_client(&client).await?;
    state.todo_store.write_pending_reindex(&client).await?;
    state.todo_store.write_pending_reindex(&client).await?;

    run_qm(&state).await?;

    assert!(eligible_jobs(&state, &client).await?.is_empty());
    assert!(state.todo_store.list_pending_reindex().await?.is_empty());
    Ok(())
}

/// End-to-end deferred job reindexing: a job leased by another client while a
/// profile-changed client reindexes cannot be evaluated eagerly — it is
/// flagged into `pending-reindex-jobs/`, waits out the lease, and is
/// re-matched against every client's *current* profile when it returns to
/// `avail/`. Covers both settle directions: the changed profile still matches
/// (marker restored) and no longer matches (marker stays gone).
#[rstest]
#[case::still_matches("macOS", true)]
#[case::no_longer_matches("linux", false)]
#[tokio::test]
async fn test_deferred_reindex_settles_when_leased_job_recycles(
    #[case] new_os: &str,
    #[case] expect_marker: bool,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let (_, client) = register_and_approve(&state).await?;
    let (_, other) = register_and_approve(&state).await?;
    set_profile(&state, &client, os_profile("macOS")).await?;
    let job = seed_avail(
        dir.path(),
        "job-1",
        ExpiresAt::Never,
        &json!({"job_id": "job-1", "requires": ["os:macos"]}),
    )?;

    // Run 1 indexes the job: the matching client gets a marker.
    run_qm(&state).await?;
    assert_eq!(eligible_jobs(&state, &client).await?, vec![job.clone()]);

    // Another client takes the job; then the first client's profile changes.
    let lease_expiry = Utc::now() + chrono::Duration::hours(1);
    let claimed = state
        .todo_store
        .claim_job(&job, ExpiresAt::Never, &other, lease_expiry)
        .await?;
    assert!(matches!(claimed, ClaimResult::Claimed(_)));
    set_profile(&state, &client, os_profile(new_os)).await?;
    state.todo_store.write_pending_reindex(&client).await?;

    // Run 2 reindexes the client. The leased job is invisible to the
    // avail/-sourced rebuild: partition wiped, job flagged for settling.
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

    // The job returns to avail/; run 4 settles the flag against the current
    // profiles.
    let recycled = state
        .todo_store
        .recycle_lease(&job, &other, lease_expiry)
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
        "marker after settle should reflect the current profile"
    );
    // The profile-less client never matches, in either direction.
    assert!(eligible_jobs(&state, &other).await?.is_empty());
    Ok(())
}

/// Deferred-reindex flags resolve without a recycle when the job is terminal:
/// a flagged job with a submission record has its flag cleared (its markers
/// are the GC sweeps' business), while a flagged job that is nowhere and
/// recordless — e.g. caught mid-transition — keeps its flag for a later run.
#[tokio::test]
async fn test_deferred_reindex_flag_cleared_for_terminal_job() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let done = job("job-done");
    let ghost = job("job-ghost");
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

    let (_, client) = register_and_approve(&state).await?;

    // A live job (in avail/, targets the client) and an orphan marker whose job
    // is not in avail/ (completed/expired/deleted).
    let live = seed_avail(
        dir.path(),
        "job-live",
        ExpiresAt::Never,
        &json!({"clients": [client.as_str()]}),
    )?;
    let orphan = job("job-orphan");
    state
        .todo_store
        .write_eligible(&client, &orphan, ExpiresAt::Never)
        .await?;

    // Marker deletion needs two consecutive orphaned sightings (a single
    // stale listing must not cost a live job its markers), so the first run
    // only records the candidate and the second run deletes.
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

/// How run 2 of [`assert_candidate_resets`] clears the candidacy: a normal
/// run that sees the job live, or a run that fails fatally *after* its
/// live-set listings but before the GC sweeps (the `todo/pending-reindex`
/// directory is swapped for a file, which the reindex pass's listing
/// propagates), exercising the consume-at-start rule.
enum Run2 {
    Succeeds,
    FailsFatally,
}

/// Shared 4-run scaffold for the candidate-reset rule. Job absent → present →
/// absent → absent: run 1 records a first sighting (marker kept); run 2 sees
/// the job live and clears the candidacy per `run2`; run 3 must therefore be
/// a *fresh* first sighting (marker kept — the assertion that pins the
/// reset); run 4 completes a genuine consecutive pair and sweeps the marker.
async fn assert_candidate_resets(
    state: &AppState,
    dir: &std::path::Path,
    job_name: &str,
    run2: Run2,
) -> anyhow::Result<()> {
    let (_, client) = register_and_approve(state).await?;
    let job = job(job_name);
    // An eligible marker whose job is not in avail/ or leased/. No denied
    // marker: a fully-denied clients-only job would be escalated, which is a
    // different pass.
    state
        .todo_store
        .write_eligible(&client, &job, ExpiresAt::Never)
        .await?;

    // Run 1: job absent → first sighting recorded, marker kept.
    run_qm(state).await?;
    assert!(
        eligible_jobs(state, &client).await?.contains(&job),
        "first sighting must not delete the marker"
    );

    // Job reappears in avail/. Run 2: job live → marker kept AND candidacy
    // cleared (the reset under test).
    seed_avail(
        dir,
        job_name,
        ExpiresAt::Never,
        &json!({"clients": [client.as_str()]}),
    )?;
    match run2 {
        Run2::Succeeds => run_qm(state).await?,
        Run2::FailsFatally => {
            // The run saw the job live but dies before its end-of-run
            // candidate rewrite; the start-of-run consume must already have
            // cleared the candidacy.
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

    // Job removed again. Run 3: run 1's sighting must not pair with this one —
    // a fresh first sighting, marker kept.
    std::fs::remove_file(dir.join("todo").join("avail").join(
        pipette_mgmt::todo_filename::avail_filename(&job, ExpiresAt::Never),
    ))?;
    run_qm(state).await?;
    assert!(
        eligible_jobs(state, &client).await?.contains(&job),
        "marker must survive run 3: run 2 cleared the candidacy, so this is a fresh first sighting"
    );

    // Run 4: now two consecutive orphaned sightings → swept.
    run_qm(state).await?;
    assert!(
        !eligible_jobs(state, &client).await?.contains(&job),
        "marker should be GC'd on the second consecutive orphaned sighting"
    );
    Ok(())
}

/// The safety half of the two-sighting rule: a first sighting must not persist
/// across a run in which the job is visibly live again. A single transient
/// listing miss (the mid-transition race the rule exists to survive) must not
/// leave a permanent "strike one" that a later, unrelated miss completes — if
/// the candidate set accumulated instead of resetting each run, run 3 would
/// wrongly complete a two-sighting pair and delete the marker.
#[tokio::test]
async fn test_gc_candidate_resets_when_job_reappears() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    assert_candidate_resets(&state, dir.path(), "job-flapping", Run2::Succeeds).await
}

/// The failed-run half of the two-sighting rule: the candidate set is consumed
/// (read, then cleared) at the *start* of each run, so a run that dies partway
/// leaves no sightings behind for a later run to mistake as the previous
/// run's. Without that, a first sighting would survive across a failed run in
/// which the job was visibly live, and a later unrelated listing miss would
/// complete a "consecutive" pair that never was.
#[tokio::test]
async fn test_gc_candidate_cleared_by_failed_run() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    assert_candidate_resets(&state, dir.path(), "job-strike", Run2::FailsFatally).await
}

/// Maintenance against a claimed job, mid-lease vs. after lease expiry. A
/// *live* lease is left alone; an *expired* one is recycled back to `avail/`
/// so the job is claimable again. In both cases the job is live, not
/// orphaned, so its eligible marker survives the GC sweep.
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
        // Recycled: lease gone, job back in avail/.
        assert!(state.todo_store.list_leased().await?.is_empty());
        assert!(
            state.todo_store.get_avail_by_job(&job).await?.is_some(),
            "recycled job should be back in avail/"
        );
    } else {
        // The lease is live: not recycled, still held.
        assert_eq!(state.todo_store.list_leased().await?.len(), 1);
    }
    // Either way the eligible marker survives the sweep.
    assert_eq!(eligible_jobs(&state, &client).await?, vec![job]);
    Ok(())
}

/// An expired job whose body can never produce a failure record is **deleted**,
/// not retried.
///
/// The contrast with `unknown_benchmark_left_for_retry` below is the whole point:
/// an unresolvable `benchmark_id` is operator-restorable, so that job is kept and
/// the run exits non-zero until someone acts. A body with no `spec` is not
/// restorable by any catalog change — retrying it would re-warn every run forever
/// while the entry stayed claimable, lapsing and being re-served. Deleting it
/// strands no plan: ingestion refuses such a body, so no manifest lists it.
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
    state.todo_store.write_denied(&job, &client).await?;
    state
        .todo_store
        .write_eligible(&client, &job, ExpiresAt::At(deadline))
        .await?;

    // The run succeeds: a permanent defect is resolved, not counted as a failure
    // to retry. Left as an error, cron monitoring would alert forever.
    run_qm(&state).await?;

    // Entry gone, and no record written — there was nothing to attribute one to.
    assert!(state.todo_store.get_avail_by_job(&job).await?.is_none());
    assert!(state.submission_store.get_submission(&job).await?.is_none());

    // Markers go with it: the job is positively removed, so they are swept in the
    // same run rather than waiting for the two-sighting orphan path.
    assert!(state.todo_store.list_denied_for_job(&job).await?.is_empty());
    assert!(eligible_jobs(&state, &client).await?.is_empty());

    // Idempotent — a second run has nothing left to do and still succeeds.
    run_qm(&state).await?;
    Ok(())
}

/// The expiry pass, per `benchmark_id` resolvability. A job past its
/// `expires_at` whose benchmark the catalog resolves is converted to a
/// terminal synthetic `"system"` failure in `processed/`, with its `avail/`
/// entry and `denied/`/`eligible/` markers all gone after one run. One whose
/// benchmark can't be resolved is skipped — left in `avail/` with its markers
/// intact for the next run — and the run exits non-zero so the persistent
/// misconfiguration surfaces through cron monitoring.
#[rstest]
#[case::known_benchmark_processed(None, true)]
#[case::unknown_benchmark_left_for_retry(Some("no_such_benchmark"), false)]
#[tokio::test]
async fn test_expired_job_teardown(
    #[case] benchmark_override: Option<&str>,
    #[case] expect_processed: bool,
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
    state.todo_store.write_denied(&job, &client).await?;
    state
        .todo_store
        .write_eligible(&client, &job, ExpiresAt::At(deadline))
        .await?;

    let run_result = run_qm(&state).await;

    if expect_processed {
        run_result?;
        // The synthetic failure landed directly in processed/.
        let record = state
            .submission_store
            .get_submission(&job)
            .await?
            .ok_or_else(|| anyhow::anyhow!("no submission record for expired job"))?;
        assert_eq!(record.state, JobState::Processed);
        assert_eq!(record.body["client_id"], "system");
        assert_eq!(record.body["retriable"], false);
        assert_eq!(
            record.body["failure_reason"],
            "Job expired at 2020-01-02T03:04:05Z before any client completed it"
        );

        // All claimable and index state is torn down in the same run.
        assert!(state.todo_store.get_avail_by_job(&job).await?.is_none());
        assert!(state.todo_store.list_denied_for_job(&job).await?.is_empty());
        assert!(eligible_jobs(&state, &client).await?.is_empty());
    } else {
        assert!(run_result.is_err());
        assert!(
            state.todo_store.get_avail_by_job(&job).await?.is_some(),
            "unexpirable job should stay in avail/ for the next run"
        );
        assert!(state.submission_store.get_submission(&job).await?.is_none());
        // The job is still live, so the GC sweeps must keep its markers — a
        // swept marker would never be rebuilt (the job's avail/ key is behind
        // the eligible-index cursor), leaving the job permanently unclaimable.
        assert_eq!(
            state.todo_store.list_denied_for_job(&job).await?,
            vec![client.clone()]
        );
        assert_eq!(eligible_jobs(&state, &client).await?, vec![job]);
    }
    Ok(())
}

/// A lease that outlives its job's deadline is fully resolved in one run:
/// pass 1 recycles it `leased/ → avail/`, pass 2 expires the recycled entry
/// into a synthetic system failure. The job body must carry `expires_at` —
/// `recycle_lease` derives the `avail/` rename target from the body, so
/// without it the entry recycles to `.never.json` and the expiry pass never
/// sees the deadline.
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

    // Seed the job's `eligible/` and `denied/` markers. Because pass 2 expires
    // the recycled entry — a positively terminal removal — the GC sweeps must
    // collect both markers in this same run, not on a later one.
    state
        .todo_store
        .write_eligible(&client, &job, ExpiresAt::At(deadline))
        .await?;
    state.todo_store.write_denied(&job, &client).await?;

    run_qm(&state).await?;

    assert!(state.todo_store.list_leased().await?.is_empty());
    assert!(state.todo_store.get_avail_by_job(&job).await?.is_none());
    let record = state
        .submission_store
        .get_submission(&job)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no synthetic failure for expired job"))?;
    assert_eq!(record.state, JobState::Processed);
    assert_eq!(record.body["client_id"], "system");

    // Markers swept in the same run, not left for the two-sighting orphan path.
    assert!(
        eligible_jobs(&state, &client).await?.is_empty(),
        "eligible marker should be swept in the same run the job expired"
    );
    assert!(
        state.todo_store.list_denied_for_job(&job).await?.is_empty(),
        "denied marker should be swept in the same run the job expired"
    );
    Ok(())
}

/// A stale lease left behind by the best-effort terminal teardown — the job's
/// real result is already in `processed/` — is deleted by the recycle pass,
/// not recycled: recycling would put the finished job back in `avail/` with
/// its markers intact (claimable forever, for `ExpiresAt::Never`), and the
/// re-run's result would land at the existing record's `processed/` key. The
/// resolution is positively terminal, so the GC sweeps collect the job's
/// markers in this same run. The job's roster carries a second, never-denied
/// client so the all-denied escalation pass cannot resolve the job itself —
/// without the recycle-pass guard, the job visibly survives in `avail/`.
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

    // The real result landed, but the teardown that should have deleted the
    // lease did not (it is best-effort), leaving the expired lease behind.
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
    state.todo_store.write_denied(&job, &client).await?;

    run_qm(&state).await?;

    // The stale lease is gone and the job was not resurrected into avail/.
    assert!(state.todo_store.list_leased().await?.is_empty());
    assert!(state.todo_store.get_avail_by_job(&job).await?.is_none());
    // The existing record survives byte-for-byte — no synthetic write.
    let record = state
        .submission_store
        .find_job(&job)
        .await?
        .ok_or_else(|| anyhow::anyhow!("existing record vanished"))?;
    assert_eq!(record.body, real_record);
    // Markers swept in the same run, not left for the two-sighting orphan path.
    assert!(
        eligible_jobs(&state, &client).await?.is_empty(),
        "eligible marker should be swept in the same run the stale lease resolved"
    );
    assert!(
        state.todo_store.list_denied_for_job(&job).await?.is_empty(),
        "denied marker should be swept in the same run the stale lease resolved"
    );
    Ok(())
}

/// A duplicate-entry state — the same job id in both `avail/` and `leased/`,
/// which only a planner double-write produces — must not turn destructive:
/// the expiry pass may resolve the leftover `avail/` entry (making the job
/// `terminal_now`), but a client holds the lease right now, so the job is
/// live and the GC sweeps must keep its markers — a swept marker is never
/// rebuilt, and the holder could never reclaim.
#[tokio::test]
async fn test_expired_duplicate_avail_entry_keeps_leased_jobs_markers() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let (_, client) = register_and_approve(&state).await?;
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
    // The duplicate: a live lease on the same job id, held by another client
    // — written directly, since no valid transition produces this state.
    let lease_path = dir.path().join("todo").join("leased").join(leased_key(
        &job,
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
    state.todo_store.write_denied(&job, &client).await?;

    run_qm(&state).await?;

    // The expiry pass resolved the duplicate avail/ entry...
    assert!(state.todo_store.get_avail_by_job(&job).await?.is_none());
    assert!(state.submission_store.find_job(&job).await?.is_some());
    // ...but the lease is held, so the job is live: markers survive the sweep.
    assert_eq!(eligible_jobs(&state, &client).await?, vec![job.clone()]);
    assert_eq!(
        state.todo_store.list_denied_for_job(&job).await?,
        vec![client]
    );
    Ok(())
}

/// An expired job that already has a submission record — terminal teardown is
/// best-effort and can leave the `avail/` entry behind — is not converted to
/// a synthetic failure: the leftover `avail/` entry is removed and the
/// existing record is untouched. Covers a record in `processed/` and one
/// mid-scoring in the score-queue (the guard uses `find_job`, which searches
/// both).
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

    // The leftover avail/ entry is finished off...
    assert!(state.todo_store.get_avail_by_job(&job).await?.is_none());
    // ...and the existing record survives byte-for-byte — no synthetic write.
    let record = state
        .submission_store
        .find_job(&job)
        .await?
        .ok_or_else(|| anyhow::anyhow!("existing record vanished"))?;
    assert_eq!(record.body, real_record);
    Ok(())
}

/// The all-denied reconciliation pass: a `clients`-only job whose every
/// listed client has a `denied/` marker is escalated to a synthetic system
/// failure — record written, `avail/` entry deleted, and both marker kinds
/// swept in the same run (the pass's removals are confirmed terminal). This
/// is the durable backstop for the submit-path escalation check; an
/// `ExpiresAt::Never` job it didn't catch would sit unclaimable forever.
#[tokio::test]
async fn test_qm_escalates_all_denied_clients_only_job() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let (_, c1) = register_and_approve(&state).await?;
    let c2 = ClientId::try_new("ev1_other")?;
    let mut body = expirable_body("job-1", &[&c1]);
    body["clients"] = json!([c1.as_str(), c2.as_str()]);
    let job = seed_avail(dir.path(), "job-1", ExpiresAt::Never, &body)?;

    state
        .todo_store
        .write_eligible(&c1, &job, ExpiresAt::Never)
        .await?;
    state.todo_store.write_denied(&job, &c1).await?;
    state.todo_store.write_denied(&job, &c2).await?;

    run_qm(&state).await?;

    // Escalated: synthetic system failure recorded, avail/ entry gone.
    let record = state
        .submission_store
        .get_submission(&job)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no synthetic failure for all-denied job"))?;
    assert_eq!(record.state, JobState::Processed);
    assert_eq!(record.body["client_id"], "system");
    assert_eq!(
        record.body["failure_reason"],
        "All eligible clients reported failure"
    );
    assert!(state.todo_store.get_avail_by_job(&job).await?.is_none());

    // Markers swept in the same run — the escalation is confirmed terminal.
    assert!(
        eligible_jobs(&state, &c1).await?.is_empty(),
        "eligible marker should be swept in the same run the job escalated"
    );
    assert!(
        state.todo_store.list_denied_for_job(&job).await?.is_empty(),
        "denied markers should be swept in the same run the job escalated"
    );
    Ok(())
}

/// The reconciliation pass leaves these alone: a `clients`-only job with a
/// roster member yet to deny may still succeed, and a job with `requires` flags
/// is open-ended no matter how many denials it has (left to the `expires_at`
/// backstop). In both cases the job stays in `avail/` and no record is
/// written. `denied` names the clients to mark denied before the run.
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
            .write_denied(&job, &ClientId::try_new(*c)?)
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

/// The reconciliation pass shares `expire_one_job`'s teardown-leftover guard:
/// an all-denied job that already has a submission record keeps it untouched;
/// only the stale `avail/` entry is removed.
#[tokio::test]
async fn test_qm_all_denied_escalation_preserves_existing_record() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let c1 = ClientId::try_new("ev1_a")?;
    let mut body = expirable_body("job-1", &[&c1]);
    body["clients"] = json!([c1.as_str()]);
    let job = seed_avail(dir.path(), "job-1", ExpiresAt::Never, &body)?;
    state.todo_store.write_denied(&job, &c1).await?;

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

/// The denied sweep mirrors the eligible sweep: markers for a job still in
/// `avail/` stay, markers whose job is permanently removed are dropped.
#[tokio::test]
async fn test_denied_gc_sweeps_terminal_jobs_only() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let (_, client) = register_and_approve(&state).await?;
    // A second roster member keeps the job short of all-denied — a fully-denied
    // `clients`-only job would be escalated by the reconciliation pass, and
    // this test is about the denied sweep, not escalation.
    let live = seed_avail(
        dir.path(),
        "job-live",
        ExpiresAt::Never,
        &json!({"clients": [client.as_str(), "ev1_other"]}),
    )?;
    let terminal = job("job-terminal");
    state.todo_store.write_denied(&live, &client).await?;
    state.todo_store.write_denied(&terminal, &client).await?;

    // Two runs: the first records the orphan as a candidate, the second
    // deletes it (the two-sighting rule — see the eligible-sweep test).
    run_qm(&state).await?;
    assert_eq!(
        state.todo_store.list_denied_for_job(&terminal).await?,
        vec![client.clone()],
        "first sighting should not delete the orphan marker"
    );
    run_qm(&state).await?;

    assert_eq!(
        state.todo_store.list_denied_for_job(&live).await?,
        vec![client.clone()]
    );
    assert!(
        state
            .todo_store
            .list_denied_for_job(&terminal)
            .await?
            .is_empty()
    );
    Ok(())
}

/// The tmp/ pass deletes only files older than the configured age.
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
