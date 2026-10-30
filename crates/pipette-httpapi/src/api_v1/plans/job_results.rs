//! A plan's jobs page with each job's result on it: what a job shows, given
//! its rows and how the runs were read.

use std::collections::HashMap;

use pipette_pipeline::plan_results::ResultRow;
use pipette_plan_types::{JobId, JobResult, JobState, ParentJob, PlanJobs, ResultsRead};

/// The job a run's rows score again: every row of one run carries the same.
pub(super) fn rescored_of(rows: &[ResultRow]) -> Option<JobId> {
    rows.first()?.rescored_from.clone()
}

/// What a job in `state` shows, given its rows and how the runs were read.
/// A lapse is this plan's, not an answer: such a job has a run only if
/// another plan's client answered it.
fn job_result(
    rows: Option<Vec<ResultRow>>,
    read: ResultsRead,
    state: JobState,
    lapsed: bool,
) -> JobResult {
    if let Some(rows) = rows {
        let rescored_from = rescored_of(&rows).map(|job_id| ParentJob {
            job_id,
            plan_id: None,
        });
        return JobResult::Scored {
            rows: rows.into_iter().map(Into::into).collect(),
            rescored_from,
            earlier_runs: Vec::new(),
            recalculable: false,
        };
    }
    let answered = match state {
        JobState::Unknown => return JobResult::Unknown,
        JobState::Finished | JobState::Failed => !lapsed,
        JobState::Available | JobState::Running | JobState::Cancelled => false,
    };
    match read {
        // The plan records the answer and its run is not yet republished.
        ResultsRead::Ok if answered => JobResult::Unknown,
        ResultsRead::Ok => JobResult::NotRun,
        ResultsRead::Off | ResultsRead::TimedOut | ResultsRead::Failed if answered || lapsed => {
            JobResult::Unknown
        }
        ResultsRead::Off | ResultsRead::TimedOut | ResultsRead::Failed => JobResult::NotRun,
    }
}

/// `page` with each job's result from `runs` (read for the same window) and
/// `results_read` set; none is recalculable until the caller judges the rows.
pub(super) fn with_job_results(
    mut page: PlanJobs,
    mut runs: HashMap<JobId, Vec<ResultRow>>,
    read: ResultsRead,
) -> PlanJobs {
    page.jobs.iter_mut().for_each(|row| {
        row.result = Some(job_result(
            runs.remove(&row.job.job_id),
            read,
            row.job.state,
            row.job.plan.lapsed.is_some(),
        ));
    });
    page.results_read = Some(read);
    page
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use rstest::rstest;
    use serde_json::Value;

    use pipette_plan_types::{FleetDetail, Job, JobInPlan, PlanId, PlanJob};

    use super::*;
    fn job(job_id: &str) -> anyhow::Result<PlanJob> {
        let job = Job {
            job_id: JobId::try_new(job_id)?,
            state: JobState::Finished,
            planned: None,
            queue: None,
            plan: JobInPlan {
                lapsed: None,
                rerun_of: None,
                rerun_history: Vec::new(),
            },
        };
        Ok(PlanJob {
            job,
            result: None,
            planned_runtime_sha256: None,
            planned_model_sha256: None,
        })
    }

    fn row(job_id: &str, value: Option<f32>) -> anyhow::Result<ResultRow> {
        Ok(ResultRow {
            job_id: JobId::try_new(job_id)?,
            benchmark_id: "prefill_throughput_256".to_string(),
            benchmark_type: "prefill_throughput".to_string(),
            metric: "prefill_throughput".to_string(),
            value,
            value_stddev: None,
            unit: "tokens/sec".to_string(),
            failed: Some(value.is_none()),
            failed_reason: None,
            model_descriptor: None,
            runtime_descriptor: None,
            runtime_descriptor_sha256: None,
            model_descriptor_sha256: None,
            model_vault_descriptor: None,
            model_flags: None,
            runtime_flags: None,
            benchmark_flags: None,
            requested_model_flags: None,
            requested_runtime_flags: None,
            requested_benchmark_flags: None,
            eval_metadata: None,
            client_extras: None,
            parameters: BTreeMap::new(),
            observations: BTreeMap::new(),
            client_id: None,
            device_name: None,
            device: BTreeMap::new(),
            client_version: None,
            submitted_at: None,
            scored_at: None,
            device_time_ms: None,
            failure_source: None,
            rescored_from: None,
            run_submitted_at: None,
            score_runtime_version: None,
            model_params_total_millions: None,
            model_params_active_millions: None,
        })
    }

    #[test]
    fn each_job_carries_its_own_result_flattened_beside_it() -> anyhow::Result<()> {
        let jobs = PlanJobs {
            fleet_detail: FleetDetail::Full,
            plan_id: PlanId::try_new("plan-0192b3c4-0000-7000-8000-000000000001")?,
            total: 2,
            offset: 0,
            limit: 100,
            jobs: vec![job("job-a")?, job("job-b")?],
            descriptor_names: None,
            results_read: None,
            rerun_campaign_id: None,
            may_cancel: None,
            cancel_permission: None,
            may_edit_note: None,
            document_state: None,
        };
        let rescored = ResultRow {
            rescored_from: Some(JobId::try_new("job-0")?),
            run_submitted_at: Some("2026-09-01T08:00:00Z".parse()?),
            score_runtime_version: Some("1.2.3".to_string()),
            ..row("job-b", Some(812.5))?
        };
        let page = with_job_results(
            jobs,
            HashMap::from([(JobId::try_new("job-b")?, vec![rescored])]),
            ResultsRead::Ok,
        );

        let json = serde_json::to_value(&page)?;
        assert_eq!(json["results_read"], "ok");
        assert_eq!(json["total"], 2);
        assert_eq!(json["jobs"][0]["job_id"], "job-a");
        assert_eq!(
            json["jobs"][0]["result"],
            serde_json::json!({"status": "unknown"})
        );
        assert_eq!(json["jobs"][1]["state"], "finished");
        assert_eq!(json["jobs"][1]["result"]["status"], "scored");
        assert_eq!(json["jobs"][1]["result"]["rows"][0]["value"], 812.5);
        assert_eq!(
            json["jobs"][1]["result"]["rows"][0]["run_submitted_at"],
            "2026-09-01T08:00:00Z"
        );
        assert_eq!(
            json["jobs"][1]["result"]["rows"][0]["score_runtime_version"],
            "1.2.3"
        );
        assert_eq!(
            json["jobs"][1]["result"]["rescored_from"],
            serde_json::json!({"job_id": "job-0", "plan_id": null}),
            "the page resolves the plan later"
        );
        assert_eq!(json["jobs"][1]["result"]["recalculable"], false);
        assert!(json["jobs"][1].get("job").is_none(), "flattened: {json}");
        Ok(())
    }

    /// A run shows whatever else is true; without one, a job nothing settled,
    /// or one answered or lapsed whose runs were not read, may have one.
    #[rstest]
    #[case::a_run(true, ResultsRead::Ok, JobState::Running, false, "scored")]
    #[case::unsettled(false, ResultsRead::Ok, JobState::Unknown, false, "unknown")]
    #[case::lapsed(false, ResultsRead::Ok, JobState::Failed, true, "not_run")]
    #[case::finished_unpublished(false, ResultsRead::Ok, JobState::Finished, false, "unknown")]
    #[case::failed_unpublished(false, ResultsRead::Ok, JobState::Failed, false, "unknown")]
    #[case::queued(false, ResultsRead::Ok, JobState::Available, false, "not_run")]
    #[case::cancelled(false, ResultsRead::Ok, JobState::Cancelled, false, "not_run")]
    #[case::finished_unread(false, ResultsRead::TimedOut, JobState::Finished, false, "unknown")]
    #[case::lapsed_unread(false, ResultsRead::Off, JobState::Failed, true, "unknown")]
    #[case::running_unread(false, ResultsRead::Failed, JobState::Running, false, "not_run")]
    fn a_result_says_why_it_has_no_run(
        #[case] scored: bool,
        #[case] read: ResultsRead,
        #[case] state: JobState,
        #[case] lapsed: bool,
        #[case] status: &str,
    ) -> anyhow::Result<()> {
        let rows = if scored {
            Some(vec![row("job-a", Some(1.0))?])
        } else {
            None
        };
        let shown = serde_json::to_value(job_result(rows, read, state, lapsed))?;
        assert_eq!(shown.get("status"), Some(&Value::from(status)));
        Ok(())
    }
}
