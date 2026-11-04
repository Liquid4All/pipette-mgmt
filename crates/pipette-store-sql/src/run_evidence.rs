//! The run evidence index on PostgreSQL: one row for each descriptor digest
//! and device.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::FromRow;

use pipette_plan_types::{ClientId, RunMark};
use pipette_store::{RunEvidenceFold, RunEvidenceRow, RunEvidenceStore};

use crate::db::SqlDatabase;

const UPSERT_ROWS: usize = 1_000;

#[derive(Clone)]
pub(crate) struct SqlRunEvidenceStore {
    db: SqlDatabase,
}

impl SqlRunEvidenceStore {
    pub(crate) fn new(db: SqlDatabase) -> Self {
        Self { db }
    }
}

/// A `run_evidence` row before the store parses its `client_id`.
#[derive(FromRow)]
struct Row {
    digest: String,
    client_id: String,
    last_success_at: Option<DateTime<Utc>>,
    last_success_client_version: Option<String>,
    last_failure_at: Option<DateTime<Utc>>,
    last_failure_client_version: Option<String>,
}

impl Row {
    fn parse(self) -> anyhow::Result<RunEvidenceRow> {
        let mark =
            |at: Option<DateTime<Utc>>, client_version| at.map(|at| RunMark { at, client_version });
        Ok(RunEvidenceRow {
            client_id: ClientId::try_new(&self.client_id)?,
            digest: self.digest,
            last_success: mark(self.last_success_at, self.last_success_client_version),
            last_failure: mark(self.last_failure_at, self.last_failure_client_version),
        })
    }
}

/// Each side takes the new run only when that run is newer, so an older
/// run changes nothing. `GREATEST` ignores a NULL.
const UPSERT: &str = "\
INSERT INTO run_evidence AS r (digest, client_id, last_success_at, last_success_client_version, \
                               last_failure_at, last_failure_client_version) \
SELECT * FROM unnest($1::text[], $2::text[], $3::timestamptz[], $4::text[], $5::timestamptz[], $6::text[]) \
ON CONFLICT (digest, client_id) DO UPDATE SET \
  last_success_at = GREATEST(r.last_success_at, EXCLUDED.last_success_at), \
  last_success_client_version = CASE \
    WHEN COALESCE(EXCLUDED.last_success_at > r.last_success_at, EXCLUDED.last_success_at IS NOT NULL) \
    THEN EXCLUDED.last_success_client_version ELSE r.last_success_client_version END, \
  last_failure_at = GREATEST(r.last_failure_at, EXCLUDED.last_failure_at), \
  last_failure_client_version = CASE \
    WHEN COALESCE(EXCLUDED.last_failure_at > r.last_failure_at, EXCLUDED.last_failure_at IS NOT NULL) \
    THEN EXCLUDED.last_failure_client_version ELSE r.last_failure_client_version END";

#[async_trait]
impl RunEvidenceStore for SqlRunEvidenceStore {
    async fn upsert(&self, rows: &[RunEvidenceRow]) -> anyhow::Result<()> {
        // One statement cannot change a row twice, so the rows first fold to
        // one row for each digest and device.
        let rows = rows
            .iter()
            .cloned()
            .collect::<RunEvidenceFold>()
            .into_rows();
        for chunk in rows.chunks(UPSERT_ROWS) {
            let digests: Vec<&str> = chunk.iter().map(|r| r.digest.as_str()).collect();
            let clients: Vec<&str> = chunk.iter().map(|r| r.client_id.as_str()).collect();
            let side = |pick: fn(&RunEvidenceRow) -> &Option<RunMark>| {
                let at: Vec<Option<DateTime<Utc>>> = chunk
                    .iter()
                    .map(|r| pick(r).as_ref().map(|m| m.at))
                    .collect();
                let version: Vec<Option<&str>> = chunk
                    .iter()
                    .map(|r| pick(r).as_ref().and_then(|m| m.client_version.as_deref()))
                    .collect();
                (at, version)
            };
            let (success_at, success_version) = side(|r| &r.last_success);
            let (failure_at, failure_version) = side(|r| &r.last_failure);
            self.db
                .execute("upsert run evidence", || {
                    sqlx::query(UPSERT)
                        .bind(&digests)
                        .bind(&clients)
                        .bind(&success_at)
                        .bind(&success_version)
                        .bind(&failure_at)
                        .bind(&failure_version)
                })
                .await?;
        }
        Ok(())
    }

    async fn find(&self, digests: &[String]) -> anyhow::Result<Vec<RunEvidenceRow>> {
        if digests.is_empty() {
            return Ok(Vec::new());
        }
        let rows: Vec<Row> = self
            .db
            .fetch_all("find run evidence", || {
                sqlx::query_as(
                    "SELECT digest, client_id, last_success_at, last_success_client_version, \
                     last_failure_at, last_failure_client_version FROM run_evidence \
                     WHERE digest = ANY($1) ORDER BY digest, client_id",
                )
                .bind(digests)
            })
            .await?;
        rows.into_iter().map(Row::parse).collect()
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, SubsecRound};

    use super::*;

    fn run(
        digest: &str,
        client: &str,
        at: DateTime<Utc>,
        version: &str,
        failed: bool,
    ) -> anyhow::Result<RunEvidenceRow> {
        Ok(RunEvidenceRow::of_run(
            digest.into(),
            ClientId::try_new(client)?,
            failed,
            RunMark {
                at,
                client_version: Some(version.into()),
            },
        ))
    }

    fn mark(at: DateTime<Utc>, version: &str) -> Option<RunMark> {
        Some(RunMark {
            at,
            client_version: Some(version.into()),
        })
    }

    #[tokio::test]
    async fn each_side_keeps_its_newest_run_and_an_older_run_changes_nothing() -> anyhow::Result<()>
    {
        let test_db = crate::testing::test_db_or_skip!();
        let store = SqlRunEvidenceStore::new(test_db.db().clone());
        let t0 = Utc::now().trunc_subsecs(6);
        let (t1, t2) = (t0 + Duration::minutes(1), t0 + Duration::minutes(2));
        // One batch with two runs of one pair: the fold keeps the newer.
        store
            .upsert(&[
                run("d1", "ev1_a", t0, "v0", false)?,
                run("d1", "ev1_a", t1, "v1", false)?,
                run("d1", "ev1_a", t1, "v1-fail", true)?,
                run("d1", "ev1_b", t0, "v0", false)?,
                run("d2", "ev1_a", t2, "v2", true)?,
            ])
            .await?;
        // A later batch: an older success and a newer failure.
        store
            .upsert(&[
                run("d1", "ev1_a", t0, "old", false)?,
                run("d1", "ev1_a", t2, "v2-fail", true)?,
            ])
            .await?;

        let rows = store.find(&["d1".into(), "d2".into()]).await?;
        assert_eq!(
            rows,
            vec![
                RunEvidenceRow {
                    digest: "d1".into(),
                    client_id: ClientId::try_new("ev1_a")?,
                    last_success: mark(t1, "v1"),
                    last_failure: mark(t2, "v2-fail"),
                },
                RunEvidenceRow {
                    digest: "d1".into(),
                    client_id: ClientId::try_new("ev1_b")?,
                    last_success: mark(t0, "v0"),
                    last_failure: None,
                },
                RunEvidenceRow {
                    digest: "d2".into(),
                    client_id: ClientId::try_new("ev1_a")?,
                    last_success: None,
                    last_failure: mark(t2, "v2"),
                },
            ]
        );
        assert_eq!(store.find(&["d3".into()]).await?, Vec::new());
        assert_eq!(store.find(&[]).await?, Vec::new());
        Ok(())
    }
}
