//! The lines each `queue-maintenance` run logs: per plan, per phase, and the
//! summary. Its own test target: `tracing` caches callsite interest per process,
//! so a `suite` test reaching the same `event!` unsubscribed would mute it here.

#![expect(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    reason = "the seeded submission record is a raw JSON body (see clippy.toml)"
)]

use std::collections::BTreeMap;

use serde_json::json;

use pipette_core::plan::sample_manifest;
use pipette_plan_types::{ExpiresAt, PlanId, PlanStatus};
use pipette_web_testkit::{
    capture_logs_at_level, job, job_spec, make_state, run_qm, seed_avail, setup_benchmarks,
};

/// A `field` ending in `=` matches by key only; any other must match a whole
/// token, so `plans=1` does not match `plans=10`.
fn has_field(line: &str, field: &str) -> bool {
    line.split_whitespace()
        .any(|token| token == field || (field.ends_with('=') && token.starts_with(field)))
}

/// Two of the plan's jobs left the queue, one with a record; a third in `avail/`
/// keeps the plan live. The first run stores the recorded outcome, so the second
/// probes only the recordless job.
#[test]
fn a_run_logs_its_summary() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (outcome, logs) = capture_logs_at_level(tracing::Level::INFO, |runtime| {
        runtime.block_on(async {
            setup_benchmarks(dir.path())?;
            let state = make_state(dir.path()).await?;
            let finished = job("job-finished")?;
            let cancelled = job("job-cancelled")?;
            // A job that requires a tag can match a later client, so the run
            // keeps it in `avail/`.
            let mut body = job_spec();
            body["requires"] = json!(["gpu"]);
            let queued = seed_avail(dir.path(), "job-queued", ExpiresAt::Never, &body)?;
            state
                .submission_store
                .write_incoming(
                    &finished,
                    &json!({"message_type": "success", "job_id": "job-finished"}),
                )
                .await?;
            let manifest = pipette_core::plan::PlanManifest {
                job_ids: vec![finished, cancelled, queued],
                content_keys: BTreeMap::new(),
                rerun_of: BTreeMap::new(),
                expired_jobs: BTreeMap::new(),
                ..sample_manifest(
                    PlanId::from_uuid(uuid::Uuid::nil()),
                    PlanStatus::Active,
                    false,
                )?
            };
            state.plan_store.clone().put_plan(&manifest).await?;
            run_qm(&state).await?;
            run_qm(&state).await
        })
    })?;
    outcome?;

    let summaries: Vec<&str> = logs
        .lines()
        .filter(|line| line.contains("queue-maintenance: run finished"))
        .collect();
    let [first, second] = summaries[..] else {
        anyhow::bail!("two summaries are expected: {logs}");
    };
    let fields = |probes, slowest_probes| {
        [
            "duration_ms=",
            "queue_passes_ms=",
            "reconcile_plans_ms=",
            "plan_index_gc_ms=",
            "plans=1",
            "plans_reconciled=1",
            probes,
            "count_jobs_ms=",
            "starvation_ms=",
            "put_plan_ms=",
            "cancel_markers_ms=",
            "slowest_plan_id=plan-00000000-0000-0000-0000-000000000000",
            "slowest_plan_ms=",
            slowest_probes,
            "content_keys_read=0",
            "client_records_drained=0",
            "cpu_user_ms=",
            "cpu_system_ms=",
            "failed_items=0",
        ]
    };
    [
        (first, "count_jobs_probes=2", "slowest_plan_probes=2"),
        (second, "count_jobs_probes=1", "slowest_plan_probes=1"),
    ]
    .into_iter()
    .for_each(|(summary, probes, slowest_probes)| {
        assert_fields(summary, &fields(probes, slowest_probes));
    });

    // The stores are local files, so no phase sends an S3 request or decodes a body.
    let phases: Vec<&str> = logs
        .lines()
        .filter(|line| line.contains("queue-maintenance: S3 and CPU work of a phase"))
        .collect();
    assert_eq!(phases.len(), 6, "three phases in each of two runs: {logs}");
    phases
        .iter()
        .zip(
            ["queue_passes", "reconcile_plans", "plan_index_gc"]
                .into_iter()
                .cycle(),
        )
        .for_each(|(line, phase)| {
            assert_fields(
                line,
                &[
                    &format!("phase=\"{phase}\""),
                    "requests=0",
                    "transport_errors=0",
                    "peak_in_flight=0",
                    "wait_ms=0",
                    "bodies_decoded=0",
                    "cpu_ms=",
                ],
            );
        });
    Ok(())
}

fn assert_fields(line: &str, fields: &[&str]) {
    let missing = fields.iter().find(|field| !has_field(line, field));
    assert!(missing.is_none(), "`{missing:?}` is missing: {line}");
}

/// A terminal, a still-creating, and a quiet plan (nothing probed, status
/// unchanged) log no `INFO` line. The live plan's recorded job is probed once,
/// then counted from the stored outcome.
#[test]
fn a_run_logs_each_live_plan() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let live = PlanId::from_uuid(uuid::Uuid::from_u128(1));
    let terminal = PlanId::from_uuid(uuid::Uuid::from_u128(2));
    let creating = PlanId::from_uuid(uuid::Uuid::from_u128(3));
    let quiet = PlanId::from_uuid(uuid::Uuid::from_u128(4));
    // A young plan, so that a `creating` plan is not stranded.
    let created_at = chrono::Utc::now();
    let (outcome, logs) = capture_logs_at_level(tracing::Level::INFO, |runtime| {
        runtime.block_on(async {
            setup_benchmarks(dir.path())?;
            let state = make_state(dir.path()).await?;
            let finished = job("job-finished")?;
            let cancelled = job("job-cancelled")?;
            let queued = seed_avail(
                dir.path(),
                "job-queued",
                ExpiresAt::Never,
                &json!({"spec": job_spec(), "requires": ["os:macos"]}),
            )?;
            state
                .submission_store
                .write_incoming(
                    &finished,
                    &json!({"message_type": "success", "job_id": "job-finished"}),
                )
                .await?;
            let all_jobs = vec![finished.clone(), cancelled.clone(), queued.clone()];
            let manifest = |plan_id: &PlanId, status, job_ids: &[pipette_plan_types::JobId]| {
                anyhow::Ok(pipette_core::plan::PlanManifest {
                    job_ids: job_ids.to_vec(),
                    content_keys: BTreeMap::new(),
                    rerun_of: BTreeMap::new(),
                    expired_jobs: BTreeMap::new(),
                    settled_jobs: BTreeMap::new(),
                    created_at,
                    ..sample_manifest(plan_id.clone(), status, false)?
                })
            };
            state
                .plan_store
                .put_plan(&manifest(&live, PlanStatus::Active, &all_jobs)?)
                .await?;
            state
                .plan_store
                .put_plan(&manifest(&terminal, PlanStatus::Complete, &all_jobs)?)
                .await?;
            state
                .plan_store
                .put_plan(&manifest(&creating, PlanStatus::Creating, &all_jobs)?)
                .await?;
            state
                .plan_store
                .put_plan(&manifest(
                    &quiet,
                    PlanStatus::PendingClients,
                    std::slice::from_ref(&queued),
                )?)
                .await?;
            run_qm(&state).await?;
            run_qm(&state).await
        })
    })?;
    outcome?;

    // A plan that fails also logs no `INFO` line, so the checks below need
    // every plan to reconcile.
    assert!(
        !logs.contains("queue-maintenance: failed to reconcile a plan"),
        "every plan reconciles: {logs}"
    );
    let lines: Vec<&str> = logs
        .lines()
        .filter(|line| line.contains("queue-maintenance: refreshed a plan's progress snapshot"))
        .collect();
    let live_field = format!("plan_id={live}");
    [
        (&terminal, "terminal"),
        (&creating, "creating"),
        (&quiet, "quiet"),
    ]
    .into_iter()
    .for_each(|(plan_id, why)| {
        let field = format!("plan_id={plan_id}");
        assert!(
            !lines.iter().any(|line| has_field(line, &field)),
            "a {why} plan logs no `INFO` line: {logs}"
        );
    });
    let live_lines: Vec<&str> = lines
        .into_iter()
        .filter(|line| has_field(line, &live_field))
        .collect();
    let [first, second] = live_lines[..] else {
        anyhow::bail!("two runs log two lines for the live plan: {logs}");
    };
    [
        (first, "probes=2", "settled=0"),
        (second, "probes=1", "settled=1"),
    ]
    .into_iter()
    .for_each(|(line, probes, settled)| {
        assert_fields(
            line,
            &["status=\"pending_clients\"", probes, settled, "elapsed_ms="],
        );
    });
    Ok(())
}
