//! Recalculating an eval: scoring a job's recorded completions again under a
//! new job, which no device runs.

use std::collections::HashSet;

use anyhow::Context;
use chrono::{DateTime, Utc};
use serde_json::Value;

use pipette_plan_types::JobId;
use pipette_store::SubmissionStore;

use crate::submission::{Submission, SuccessSubmission, parse_stored_submission};

/// Whether `body` is a recorded result the scorer can take again.
pub(crate) fn rescorable(body: &Value) -> bool {
    eval_success(body).is_some()
}

/// `body` as an eval's, vlm_eval's or asr_eval's success. A failure carries no completions, and only an
/// eval's score is computed apart from the device.
fn eval_success(body: &Value) -> Option<Box<SuccessSubmission>> {
    match parse_stored_submission(body) {
        Ok(Submission::Success(success)) if success.benchmark_type.is_scored_via_server() => {
            Some(success)
        }
        _ => None,
    }
}

/// When a device submitted the run whose completions `success` records.
///
/// A copy written before copies carried the time names only its source, so the
/// walk follows `rescored_from` back to the device run. `None` when a record
/// along the chain is gone, is not a success, or names a record that the chain
/// already passed.
pub(crate) async fn run_submitted_at(
    submissions: &dyn SubmissionStore,
    success: &SuccessSubmission,
) -> anyhow::Result<Option<DateTime<Utc>>> {
    if let Some(at) = success.run_submitted_at {
        return Ok(Some(at));
    }
    let Some(mut source) = success.rescored_from.clone() else {
        return Ok(Some(success.submitted_at));
    };
    let mut seen = HashSet::from([success.job_id.clone()]);
    loop {
        if !seen.insert(source.clone()) {
            tracing::warn!(
                job_id = %success.job_id,
                rescored_from = %source,
                "rescore copy: run time unknown, ancestor chain loops"
            );
            return Ok(None);
        }
        let Some(record) = submissions.find_job(&source).await? else {
            tracing::warn!(
                job_id = %success.job_id,
                rescored_from = %source,
                "rescore copy: run time unknown, ancestor record missing"
            );
            return Ok(None);
        };
        let Ok(Submission::Success(parent)) = parse_stored_submission(&record.body) else {
            tracing::warn!(
                job_id = %success.job_id,
                rescored_from = %source,
                "rescore copy: run time unknown, ancestor record is not a success"
            );
            return Ok(None);
        };
        match (parent.run_submitted_at, parent.rescored_from) {
            (Some(at), _) => return Ok(Some(at)),
            (None, None) => return Ok(Some(parent.submitted_at)),
            (None, Some(next)) => source = next,
        }
    }
}

/// Answer `job_id` with the completions `source` recorded, as a copy written
/// to `incoming/` for the scorer. Submitted `now`: the warehouse refuses
/// writes to an old day.
pub(crate) async fn answer_from(
    submissions: &dyn SubmissionStore,
    source: &JobId,
    job_id: &JobId,
    now: DateTime<Utc>,
) -> anyhow::Result<()> {
    let record = submissions
        .find_job(source)
        .await?
        .with_context(|| format!("job {source} has no recorded result to recalculate"))?;
    let mut success = eval_success(&record.body)
        .with_context(|| format!("job {source} recorded no eval completions to recalculate"))?;
    success.run_submitted_at = run_submitted_at(submissions, &success).await?;
    // Typed, so a key the scorer added to the record is not copied.
    success.job_id = job_id.clone();
    success.submitted_at = now;
    success.rescored_from = Some(source.clone());
    let run_submitted_at = success.run_submitted_at;
    let body = serde_json::to_value(Submission::Success(success))?;
    submissions.write_incoming(job_id, &body).await?;
    tracing::debug!(
        job_id = %job_id,
        rescored_from = %source,
        run_submitted_at = ?run_submitted_at,
        "recalculation: copy written to incoming"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use pipette_core::config::{Config, StorageConfig};
    use pipette_store::Stores;
    use pipette_store_fs::build_local_fs_stores;
    use pipette_store_testkit::success_record;

    use super::*;

    fn stores(data_dir: &Path) -> anyhow::Result<Stores> {
        build_local_fs_stores(&Config {
            storage: StorageConfig::local_fs(data_dir.to_path_buf()),
            auth_storage: StorageConfig::local_fs(data_dir.to_path_buf()),
            ..Config::default()
        })
    }

    #[test]
    fn only_an_eval_success_is_rescorable() -> anyhow::Result<()> {
        let job_id = JobId::try_new("job-a")?;
        assert!(rescorable(&success_record(&job_id, "eval")));
        assert!(!rescorable(&success_record(&job_id, "decode_throughput")));
        assert!(!rescorable(&serde_json::json!({
            "message_type": "failure",
            "benchmark_type": "eval",
            "benchmark_id": "eval_ifbench_original",
            "client_id": "ev1_test",
            "job_id": "job-a",
            "submitted_at": "2026-05-29T14:49:43Z",
            "failure_reason": "crashed",
        })));
        Ok(())
    }

    /// The copy changes only the job, the submission time and the lineage, and
    /// records the time of the run it copies.
    #[tokio::test]
    async fn the_copy_answers_the_new_job_now() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores(dir.path())?;
        let (source, job_id) = (JobId::try_new("job-a")?, JobId::try_new("job-b")?);
        let recorded = success_record(&source, "eval");
        stores
            .submissions
            .write_processed(&source, &recorded)
            .await?;
        let now = Utc::now();

        answer_from(stores.submissions.as_ref(), &source, &job_id, now).await?;

        let copy = stores
            .submissions
            .find_job(&job_id)
            .await?
            .context("no copy")?
            .body;
        let mut expected = serde_json::to_value(parse_stored_submission(&recorded)?)?;
        expected["job_id"] = serde_json::json!("job-b");
        expected["submitted_at"] = serde_json::to_value(now)?;
        expected["rescored_from"] = serde_json::json!("job-a");
        expected["run_submitted_at"] = recorded["submitted_at"].clone();
        assert_eq!(copy, expected);
        Ok(())
    }

    /// Recalculating a copy names the copy, one level up, not the job a device
    /// ran. Its run time is still the device run's.
    #[tokio::test]
    async fn a_copy_of_a_copy_names_its_parent() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores(dir.path())?;
        let [device, first, second] = ["job-a", "job-b", "job-c"].map(JobId::new_unchecked);
        stores
            .submissions
            .write_processed(&device, &success_record(&device, "eval"))
            .await?;
        let submissions = stores.submissions.as_ref();

        answer_from(submissions, &device, &first, Utc::now()).await?;
        answer_from(submissions, &first, &second, Utc::now()).await?;

        let copy = submissions.find_job(&second).await?.context("no copy")?;
        assert_eq!(copy.body["rescored_from"], "job-b");
        assert_eq!(copy.body["run_submitted_at"], "2026-05-29T14:49:43Z");
        Ok(())
    }

    /// The store holds a device run `job-a`, its copy `job-b`, a failure
    /// `job-f`, and a copy `job-l` that names itself. `job-b` was written
    /// before copies carried the run time. Each case gives the lineage of
    /// `job-c` and the run time that it resolves to.
    #[rstest::rstest]
    #[case::device_run(serde_json::json!({}), Some("2026-06-10T08:00:00Z"))]
    #[case::carried(
        serde_json::json!({"rescored_from": "job-gone", "run_submitted_at": "2026-04-01T00:00:00Z"}),
        Some("2026-04-01T00:00:00Z")
    )]
    #[case::through_older_copies(serde_json::json!({"rescored_from": "job-b"}), Some("2026-05-29T14:49:43Z"))]
    #[case::ancestor_gone(serde_json::json!({"rescored_from": "job-gone"}), None)]
    #[case::ancestor_failed(serde_json::json!({"rescored_from": "job-f"}), None)]
    #[case::chain_loops(serde_json::json!({"rescored_from": "job-l"}), None)]
    #[tokio::test]
    async fn the_run_time_is_the_device_runs(
        #[case] lineage: Value,
        #[case] expected: Option<&str>,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores(dir.path())?;
        let [device, older_copy, job] = ["job-a", "job-b", "job-c"].map(JobId::new_unchecked);
        stores
            .submissions
            .write_processed(&device, &success_record(&device, "eval"))
            .await?;
        let mut older = success_record(&older_copy, "eval");
        older["rescored_from"] = serde_json::json!("job-a");
        older["submitted_at"] = serde_json::json!("2026-06-05T08:00:00Z");
        stores
            .submissions
            .write_processed(&older_copy, &older)
            .await?;
        let failed = JobId::new_unchecked("job-f");
        let failure = serde_json::json!({
            "message_type": "failure",
            "benchmark_type": "eval",
            "benchmark_id": "eval_ifbench_original",
            "client_id": "ev1_test",
            "job_id": "job-f",
            "submitted_at": "2026-05-29T14:49:43Z",
            "failure_reason": "crashed",
        });
        stores
            .submissions
            .write_processed(&failed, &failure)
            .await?;
        let looped = JobId::new_unchecked("job-l");
        let mut looping = success_record(&looped, "eval");
        looping["rescored_from"] = serde_json::json!("job-l");
        stores
            .submissions
            .write_processed(&looped, &looping)
            .await?;
        let mut body = success_record(&job, "eval");
        body["submitted_at"] = serde_json::json!("2026-06-10T08:00:00Z");
        lineage
            .as_object()
            .context("lineage is not an object")?
            .iter()
            .for_each(|(key, value)| body[key] = value.clone());
        let success = eval_success(&body).context("not an eval success")?;

        let at = run_submitted_at(stores.submissions.as_ref(), &success).await?;

        let expected = expected.map(str::parse::<DateTime<Utc>>).transpose()?;
        assert_eq!(at, expected);
        Ok(())
    }

    #[tokio::test]
    async fn a_job_with_no_eval_completions_is_not_copied() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores(dir.path())?;
        let (source, job_id) = (JobId::try_new("job-a")?, JobId::try_new("job-b")?);
        stores
            .submissions
            .write_processed(&source, &success_record(&source, "decode_throughput"))
            .await?;

        let refused = answer_from(stores.submissions.as_ref(), &source, &job_id, Utc::now()).await;

        assert!(refused.is_err());
        assert!(stores.submissions.find_job(&job_id).await?.is_none());
        Ok(())
    }
}
