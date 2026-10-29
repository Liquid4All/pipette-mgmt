//! SQL records for catalog import and refresh runs.

use std::num::NonZeroUsize;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::FromRow;

use pipette_plan_types::{CatalogImportOutcome, CatalogImportRun, CatalogImportRuns, CatalogName};
use pipette_store::{CatalogImportRunFinish, CatalogImportRunStore};

use crate::db::SqlDatabase;

#[derive(Clone)]
pub(crate) struct SqlCatalogImportRunStore {
    db: SqlDatabase,
}

impl SqlCatalogImportRunStore {
    pub(crate) fn new(db: SqlDatabase) -> Self {
        Self { db }
    }
}

/// A `catalog_import_runs` row before the store parses it.
#[derive(FromRow)]
struct RunRow {
    id: String,
    started_at: DateTime<Utc>,
    finished_at: Option<DateTime<Utc>>,
    command: String,
    source: String,
    target_name: Option<String>,
    result: String,
    entries_created: i32,
    entries_hidden: i32,
    members_added: i32,
    unclassified_assets: serde_json::Value,
    error_summary: Option<String>,
}

impl RunRow {
    fn parse(self) -> anyhow::Result<CatalogImportRun> {
        let result = self.result.parse()?;
        let unclassified_assets: Vec<String> = serde_json::from_value(self.unclassified_assets)?;
        Ok(CatalogImportRun {
            id: self.id,
            started_at: self.started_at,
            finished_at: self.finished_at,
            command: self.command.parse()?,
            source: self.source,
            target_name: self.target_name,
            result,
            outcome: CatalogImportOutcome::of(
                result,
                &unclassified_assets,
                self.error_summary.as_deref(),
            ),
            entries_created: self.entries_created,
            entries_hidden: self.entries_hidden,
            members_added: self.members_added,
            unclassified_assets,
            error_summary: self.error_summary,
        })
    }
}

#[async_trait]
impl CatalogImportRunStore for SqlCatalogImportRunStore {
    async fn recent_runs(&self, limit: NonZeroUsize) -> anyhow::Result<CatalogImportRuns> {
        let rows: Vec<RunRow> = sqlx::query_as(
            "SELECT id, started_at, finished_at, command, source, target_name, result, \
             entries_created, entries_hidden, members_added, unclassified_assets, error_summary \
             FROM catalog_import_runs ORDER BY started_at DESC, id DESC LIMIT $1",
        )
        .bind(i64::try_from(limit.get())?)
        .fetch_all(self.db.pool())
        .await?;
        let items = rows
            .into_iter()
            .map(RunRow::parse)
            .collect::<anyhow::Result<Vec<_>>>()?;
        let (last_successful_import_at,): (Option<DateTime<Utc>>,) = sqlx::query_as(
            "SELECT MAX(finished_at) FROM catalog_import_runs \
             WHERE command = 'import' AND result = 'success'",
        )
        .fetch_one(self.db.pool())
        .await?;
        Ok(CatalogImportRuns {
            items,
            last_successful_import_at,
        })
    }

    async fn start_run(
        &self,
        id: &str,
        command: &str,
        target_name: Option<&CatalogName>,
        at: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        let name = target_name.map(CatalogName::as_str);
        self.db.execute("start catalog import run", || {
            sqlx::query("INSERT INTO catalog_import_runs (id, started_at, command, target_name, result) VALUES ($1, $2, $3, $4, 'running')")
                .bind(id).bind(at).bind(command).bind(name)
        }).await?;
        Ok(())
    }

    async fn finish_run(
        &self,
        id: &str,
        at: DateTime<Utc>,
        finish: &CatalogImportRunFinish,
    ) -> anyhow::Result<()> {
        let result = if finish.error_summary.is_some() {
            "error"
        } else {
            "success"
        };
        let assets = serde_json::to_value(&finish.unclassified_assets)?;
        let changed = self.db.execute("finish catalog import run", || {
            sqlx::query("UPDATE catalog_import_runs SET finished_at = $2, result = $3, entries_created = $4, members_added = $5, unclassified_assets = $6, error_summary = $7, entries_hidden = $8 WHERE id = $1 AND result = 'running'")
                .bind(id).bind(at).bind(result).bind(finish.entries_created)
                .bind(finish.members_added).bind(&assets).bind(&finish.error_summary)
                .bind(finish.entries_hidden)
        }).await?;
        anyhow::ensure!(changed == 1, "catalog import run {id} was not running");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Context;
    use chrono::SubsecRound;

    use super::*;

    #[tokio::test]
    async fn a_run_keeps_its_result_and_an_interrupted_run_stays_running() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = SqlCatalogImportRunStore::new(test_db.db().clone());
        let at = Utc::now().trunc_subsecs(6);
        store.start_run("finished", "import", None, at).await?;
        store.start_run("interrupted", "refresh", None, at).await?;
        store.start_run("succeeded", "import", None, at).await?;
        store
            .finish_run(
                "finished",
                at + chrono::Duration::minutes(1),
                &CatalogImportRunFinish {
                    entries_created: 2,
                    members_added: 4,
                    entries_hidden: 3,
                    unclassified_assets: vec!["b1/new-build.tar.gz".into()],
                    error_summary: Some("one asset is unknown".into()),
                },
            )
            .await?;
        let finished: (String, i32, i32, i32, serde_json::Value, Option<String>) = sqlx::query_as(
            "SELECT result, entries_created, members_added, entries_hidden, unclassified_assets, error_summary FROM catalog_import_runs WHERE id = 'finished'"
        ).fetch_one(test_db.db().pool()).await?;
        assert_eq!(
            finished,
            (
                "error".into(),
                2,
                4,
                3,
                serde_json::json!(["b1/new-build.tar.gz"]),
                Some("one asset is unknown".into())
            )
        );
        let running: (String, Option<DateTime<Utc>>) = sqlx::query_as(
            "SELECT result, finished_at FROM catalog_import_runs WHERE id = 'interrupted'",
        )
        .fetch_one(test_db.db().pool())
        .await?;
        store
            .finish_run("succeeded", at, &CatalogImportRunFinish::default())
            .await?;
        assert_eq!(running, ("running".into(), None));
        let succeeded: (String, i32, i32) = sqlx::query_as(
            "SELECT result, entries_created, members_added FROM catalog_import_runs WHERE id = 'succeeded'",
        )
        .fetch_one(test_db.db().pool())
        .await?;
        assert_eq!(succeeded, ("success".into(), 0, 0));
        let recent = store
            .recent_runs(NonZeroUsize::new(2).context("two")?)
            .await?;
        assert_eq!(recent.items.len(), 2);
        assert_eq!(recent.last_successful_import_at, Some(at));
        assert_eq!(recent.items[0].entries_hidden, 0);
        let all = store
            .recent_runs(NonZeroUsize::new(3).context("three")?)
            .await?;
        assert_eq!(
            all.items
                .iter()
                .find(|run| run.id == "finished")
                .context("finished run")?
                .entries_hidden,
            3
        );
        Ok(())
    }
}
