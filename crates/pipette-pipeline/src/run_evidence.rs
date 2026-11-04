//! Run evidence: what happened when a descriptor ran on a device. The scorer
//! and `db backfill-run-evidence` make the index rows from warehouse rows. The
//! catalog reads give a member the rows of its digests.

use std::collections::HashSet;

use chrono::DateTime;

use pipette_core::warehouse::{MetricRow, is_system_client};
use pipette_plan_types::{
    AnyClientRunEvidence, ClientId, ClientRunEvidence, DescriptorMemberEvidence, RunMark,
};
use pipette_store::{RunEvidenceFold, RunEvidenceRow, RunEvidenceStore, WarehouseStore};

/// The index rows of one warehouse row: one for each descriptor digest that
/// it carries.
///
/// A row of the `system` client ran on no device. A rescored copy repeats a
/// device run that has its own row. Neither gives an index row. A row from a
/// file older than the `failed` column reads as a success, as
/// `failed IS NOT TRUE` does in a warehouse query.
pub fn rows_of(row: &MetricRow) -> Vec<RunEvidenceRow> {
    let Some(at) = DateTime::from_timestamp_micros(row.submitted_at) else {
        return Vec::new();
    };
    if is_system_client(&row.client_id) || row.rescored_from.is_some() {
        return Vec::new();
    }
    let failed = row.failed == Some(true);
    [&row.runtime_descriptor_sha256, &row.model_descriptor_sha256]
        .into_iter()
        .flatten()
        .map(|digest| {
            RunEvidenceRow::of_run(
                digest.clone(),
                row.client_id.clone(),
                failed,
                RunMark {
                    at,
                    client_version: row.client_version.clone(),
                },
            )
        })
        .collect()
}

/// The index rows of a scored job's warehouse rows, one for each digest.
pub fn job_rows(rows: &[MetricRow]) -> Vec<RunEvidenceRow> {
    rows.iter()
        .flat_map(rows_of)
        .collect::<RunEvidenceFold>()
        .into_rows()
}

/// The evidence of one member from the index rows of its digests. `digests`
/// is the member's digest and its alias, if any. With `device`, the evidence
/// also gives the runs on that device.
pub fn member_evidence(
    digests: &[&str],
    rows: &[RunEvidenceRow],
    device: Option<&ClientId>,
) -> DescriptorMemberEvidence {
    let own: Vec<&RunEvidenceRow> = rows
        .iter()
        .filter(|row| digests.contains(&row.digest.as_str()))
        .collect();
    // A member with an alias can have two rows for the device. Each side
    // keeps the newer run.
    let this_device = device.map(|device| {
        own.iter().filter(|row| &row.client_id == device).fold(
            ClientRunEvidence::default(),
            |acc, row| ClientRunEvidence {
                last_success: newer(acc.last_success, row.last_success.clone()),
                last_failure: newer(acc.last_failure, row.last_failure.clone()),
            },
        )
    });
    let succeeded: Vec<&RunEvidenceRow> = own
        .iter()
        .copied()
        .filter(|row| row.last_success.is_some())
        .collect();
    DescriptorMemberEvidence {
        this_device,
        any_device: AnyClientRunEvidence {
            last_success_at: succeeded
                .iter()
                .filter_map(|row| row.last_success.as_ref().map(|m| m.at))
                .max(),
            devices_with_success: succeeded
                .iter()
                .map(|row| &row.client_id)
                .collect::<HashSet<_>>()
                .len(),
        },
    }
}

fn newer(a: Option<RunMark>, b: Option<RunMark>) -> Option<RunMark> {
    match (a, b) {
        (Some(a), Some(b)) => Some(if b.at > a.at { b } else { a }),
        (a, b) => a.or(b),
    }
}

/// What [`backfill`] wrote, or would write on a dry run.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RunEvidenceBackfill {
    /// Warehouse rows read.
    pub rows_read: usize,
    /// Index rows written: one for each digest and device.
    pub rows_written: usize,
}

/// Make the run evidence index again from every warehouse row. The upsert
/// keeps the newest run of each side, so a second run changes nothing, and a
/// run beside the scorer loses no newer run. The visitor marks no row dirty,
/// so the walk needs no mutate lock.
pub async fn backfill(
    warehouse: &dyn WarehouseStore,
    index: &dyn RunEvidenceStore,
    dry_run: bool,
) -> anyhow::Result<RunEvidenceBackfill> {
    let mut fold = RunEvidenceFold::default();
    let mut rows_read = 0;
    warehouse
        .for_each_metric_row(&mut |row| {
            rows_read += 1;
            rows_of(row).into_iter().for_each(|r| fold.add(r));
            false
        })
        .await?;
    let rows = fold.into_rows();
    if !dry_run {
        index.upsert(&rows).await?;
    }
    let report = RunEvidenceBackfill {
        rows_read,
        rows_written: rows.len(),
    };
    tracing::info!(
        dry_run,
        rows_read = report.rows_read,
        rows_written = report.rows_written,
        "run evidence index backfilled"
    );
    Ok(report)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use anyhow::Context;
    use async_trait::async_trait;
    use chrono::{DateTime, Utc};
    use rstest::rstest;

    use pipette_core::warehouse::SYSTEM_CLIENT_ID;
    use pipette_plan_types::BenchmarkId;

    use super::*;

    fn client(id: &str) -> anyhow::Result<ClientId> {
        Ok(ClientId::try_new(id)?)
    }

    fn at(micros: i64) -> anyhow::Result<DateTime<Utc>> {
        DateTime::from_timestamp_micros(micros).context("a valid time")
    }

    fn mark(micros: i64, version: &str) -> anyhow::Result<Option<RunMark>> {
        Ok(Some(RunMark {
            at: at(micros)?,
            client_version: Some(version.into()),
        }))
    }

    fn row(client_id: &str, failed: Option<bool>, submitted_at: i64) -> anyhow::Result<MetricRow> {
        Ok(MetricRow {
            result_id: "job_0".into(),
            benchmark_id: BenchmarkId::try_new("bench1")?,
            metric: "ttft".into(),
            client_id: client(client_id)?,
            runtime_descriptor_sha256: Some("rt".into()),
            model_descriptor_sha256: Some("md".into()),
            client_version: Some("v1".into()),
            failed,
            unit: "ms".into(),
            submitted_at,
            scored_at: submitted_at,
            ..MetricRow::default()
        })
    }

    #[rstest]
    #[case::measured(Some(false), false)]
    #[case::before_the_failed_column(None, false)]
    #[case::failure(Some(true), true)]
    fn a_device_run_gives_a_row_for_each_digest(
        #[case] failed: Option<bool>,
        #[case] as_failure: bool,
    ) -> anyhow::Result<()> {
        let rows = rows_of(&row("ev1_a", failed, 5)?);
        let expected = ["rt", "md"]
            .into_iter()
            .map(|digest| -> anyhow::Result<_> {
                Ok(RunEvidenceRow::of_run(
                    digest.into(),
                    client("ev1_a")?,
                    as_failure,
                    mark(5, "v1")?.context("a mark")?,
                ))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        assert_eq!(rows, expected);
        Ok(())
    }

    #[rstest]
    #[case::system_failure(SYSTEM_CLIENT_ID, None)]
    #[case::rescored_copy("ev1_a", Some("job_parent"))]
    fn a_row_that_no_device_ran_gives_no_row(
        #[case] client_id: &str,
        #[case] rescored_from: Option<&str>,
    ) -> anyhow::Result<()> {
        let mut source = row(client_id, Some(true), 5)?;
        source.rescored_from = rescored_from.map(str::to_owned);
        assert_eq!(rows_of(&source), []);
        Ok(())
    }

    #[test]
    fn a_job_folds_its_metric_rows_to_one_row_for_each_digest() -> anyhow::Result<()> {
        let rows = job_rows(&[row("ev1_a", Some(false), 5)?, row("ev1_a", Some(false), 5)?]);
        assert_eq!(
            rows.iter().map(|r| r.digest.as_str()).collect::<Vec<_>>(),
            ["md", "rt"]
        );
        Ok(())
    }

    fn index_row(
        digest: &str,
        client_id: &str,
        success: Option<RunMark>,
        failure: Option<RunMark>,
    ) -> anyhow::Result<RunEvidenceRow> {
        Ok(RunEvidenceRow {
            digest: digest.into(),
            client_id: client(client_id)?,
            last_success: success,
            last_failure: failure,
        })
    }

    #[test]
    fn a_member_gets_the_evidence_of_its_own_digests_only() -> anyhow::Result<()> {
        let rows = [
            // The CPU member succeeded on the device; the CUDA member failed.
            index_row("cpu", "ev1_dev", mark(9, "v9")?, None)?,
            index_row("cuda", "ev1_dev", None, mark(7, "v7")?)?,
            // The CUDA member succeeded on two other devices, once through
            // its alias digest on a device that also ran the digest.
            index_row("cuda", "ev1_b", mark(3, "v3")?, None)?,
            index_row("cuda-alias", "ev1_b", mark(4, "v4")?, None)?,
            index_row("cuda", "ev1_c", mark(2, "v2")?, mark(1, "v1")?)?,
        ];
        let device = client("ev1_dev")?;
        let evidence = member_evidence(&["cuda", "cuda-alias"], &rows, Some(&device));
        assert_eq!(
            evidence,
            DescriptorMemberEvidence {
                this_device: Some(ClientRunEvidence {
                    last_success: None,
                    last_failure: mark(7, "v7")?,
                }),
                any_device: AnyClientRunEvidence {
                    last_success_at: Some(at(4)?),
                    devices_with_success: 2,
                },
            }
        );
        let fleet_only = member_evidence(&["cuda", "cuda-alias"], &rows, None);
        assert_eq!(fleet_only.this_device, None);
        assert_eq!(fleet_only.any_device, evidence.any_device);
        Ok(())
    }

    #[test]
    fn an_alias_and_its_digest_merge_on_one_device() -> anyhow::Result<()> {
        let rows = [
            index_row("d", "ev1_dev", mark(3, "old")?, mark(8, "fail-new")?)?,
            index_row("alias", "ev1_dev", mark(6, "new")?, mark(2, "fail-old")?)?,
        ];
        let device = client("ev1_dev")?;
        let evidence = member_evidence(&["d", "alias"], &rows, Some(&device));
        assert_eq!(
            evidence.this_device,
            Some(ClientRunEvidence {
                last_success: mark(6, "new")?,
                last_failure: mark(8, "fail-new")?,
            })
        );
        assert_eq!(evidence.any_device.devices_with_success, 1);
        Ok(())
    }

    /// An index that keeps what the backfill writes.
    #[derive(Default)]
    struct Recorded(Mutex<Vec<RunEvidenceRow>>);

    #[async_trait]
    impl RunEvidenceStore for Recorded {
        async fn upsert(&self, rows: &[RunEvidenceRow]) -> anyhow::Result<()> {
            self.0
                .lock()
                .map_err(|_| anyhow::anyhow!("poisoned"))?
                .extend_from_slice(rows);
            Ok(())
        }

        async fn find(&self, _: &[String]) -> anyhow::Result<Vec<RunEvidenceRow>> {
            Ok(Vec::new())
        }
    }

    #[rstest]
    #[case::writes(false, 2)]
    #[case::dry_run(true, 0)]
    #[tokio::test]
    async fn the_backfill_folds_every_warehouse_row(
        #[case] dry_run: bool,
        #[case] written: usize,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let config = pipette_core::config::Config {
            storage: pipette_core::config::StorageConfig::local_fs(dir.path().to_path_buf()),
            auth_storage: pipette_core::config::StorageConfig::local_fs(dir.path().to_path_buf()),
            ..pipette_core::config::Config::default()
        };
        let stores = pipette_store_fs::build_local_fs_stores(&config)?;
        let rows = [
            row("ev1_a", Some(false), 5)?,
            row("ev1_a", Some(true), 9)?,
            row(SYSTEM_CLIENT_ID, Some(true), 9)?,
        ];
        stores
            .warehouse
            .write_partition_metrics(
                &BenchmarkId::try_new("bench1")?,
                &client("ev1_a")?,
                "2026-10-30",
                &rows[..2],
            )
            .await?;
        stores
            .warehouse
            .write_partition_metrics(
                &BenchmarkId::try_new("bench1")?,
                &client(SYSTEM_CLIENT_ID)?,
                "2026-10-30",
                &rows[2..],
            )
            .await?;
        let index = Recorded::default();
        let report = backfill(&*stores.warehouse, &index, dry_run).await?;
        assert_eq!(
            report,
            RunEvidenceBackfill {
                rows_read: 3,
                rows_written: 2,
            }
        );
        let kept = index.0.lock().map_err(|_| anyhow::anyhow!("poisoned"))?;
        assert_eq!(kept.len(), written);
        if !dry_run {
            assert_eq!(
                kept[1],
                index_row("rt", "ev1_a", mark(5, "v1")?, mark(9, "v1")?)?
            );
        }
        Ok(())
    }
}
