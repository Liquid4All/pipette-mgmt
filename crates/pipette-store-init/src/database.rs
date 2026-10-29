use pipette_core::config::{Config, DatabaseConfig};
use pipette_store_sql::{JobCellBackfill, SchemaMove, SqlDatabase};

/// A rollout migrates once, before the binaries that need the new schema start.
pub async fn migrate(
    config: &DatabaseConfig,
    to: Option<i64>,
    dry_run: bool,
) -> anyhow::Result<SchemaMove> {
    let db = SqlDatabase::connect(config);
    let moved = pipette_store_sql::migrate(&db, to, dry_run).await;
    db.close().await;
    moved
}

/// The newest migration this binary embeds.
pub fn latest_migration() -> Option<i64> {
    pipette_store_sql::latest_migration()
}

/// Fail unless the database has the schema this binary expects. A no-op
/// without `[database]`, so `serve` calls it unconditionally.
pub async fn check(config: &Config) -> anyhow::Result<()> {
    let Some(database) = &config.database else {
        return Ok(());
    };
    let db = SqlDatabase::connect(database);
    let checked = pipette_store_sql::check(&db).await;
    db.close().await;
    checked
}

/// Index the cell of each job source that has no cell row. Also write the
/// outcome row and the run flags of each job whose row does not have them,
/// from the submission records in the object store. `cell_of` gives the cell
/// digest of a body. The function fails if the config has no `[database]`
/// section, or if the schema is stale.
pub async fn backfill_job_cells(
    config: &Config,
    cell_of: &(dyn Fn(&serde_json::Value) -> String + Sync),
    dry_run: bool,
) -> anyhow::Result<JobCellBackfill> {
    let Some(database) = &config.database else {
        anyhow::bail!("the config has no [database] section");
    };
    let db = SqlDatabase::connect(database);
    let report = async {
        pipette_store_sql::check(&db).await?;
        let records = crate::build_object_stores(config).await?.submissions;
        pipette_store_sql::backfill_job_cells(&db, records.as_ref(), cell_of, dry_run).await
    }
    .await;
    db.close().await;
    report
}
