//! [`ModelParamsStore`] on SQL: one row per model key, with a column per
//! field rather than a body, so a row reads and edits on its own.

use std::num::NonZeroUsize;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use pipette_core::model_params::ModelParams;
use pipette_plan_types::{ModelKey, NonEmptyTrimmedString, PositiveCount, UserId};
use pipette_store::{ModelParamsPage, ModelParamsStore};

use crate::db::SqlDatabase;
use crate::listing::{Keyspace, Rows, fetch_after, page_rows};

const MODEL_PARAMS: Keyspace = Keyspace {
    table: "model_params",
    id_column: "key",
    report_prefix: "model-params/",
};

const COLUMNS: &str = "key, total_millions, active_millions, note, updated_at, updated_by";

/// The columns after the key.
type Row = (i32, i32, Option<String>, DateTime<Utc>, Option<String>);

/// A row as a record; the table's checks keep the counts positive.
fn record(
    key: ModelKey,
    (total, active, note, updated_at, updated_by): Row,
) -> anyhow::Result<ModelParams> {
    Ok(ModelParams {
        key,
        total_millions: PositiveCount::try_new(total)?,
        active_millions: PositiveCount::try_new(active)?,
        note: note.map(NonEmptyTrimmedString::try_new).transpose()?,
        updated_at,
        updated_by: updated_by.map(UserId::try_new).transpose()?,
    })
}

/// Whole-row upserts and deletes: last writer wins, as on the object stores.
#[derive(Clone)]
pub(crate) struct SqlModelParamsStore {
    db: SqlDatabase,
}

impl SqlModelParamsStore {
    pub(crate) fn new(db: SqlDatabase) -> Self {
        Self { db }
    }
}

#[async_trait]
impl Rows for SqlModelParamsStore {
    type Row = Row;

    async fn rows_after(
        &self,
        after: Option<&str>,
        limit: i64,
    ) -> anyhow::Result<Vec<(String, Row)>> {
        let rows: Vec<(
            String,
            i32,
            i32,
            Option<String>,
            DateTime<Utc>,
            Option<String>,
        )> = fetch_after(&self.db, MODEL_PARAMS, COLUMNS, after, limit).await?;
        Ok(rows
            .into_iter()
            .map(|(key, total, active, note, at, by)| (key, (total, active, note, at, by)))
            .collect())
    }

    async fn ids_after(&self, after: Option<&str>, limit: i64) -> anyhow::Result<Vec<String>> {
        let ids: Vec<(String,)> = fetch_after(&self.db, MODEL_PARAMS, "key", after, limit).await?;
        Ok(ids.into_iter().map(|(id,)| id).collect())
    }
}

#[async_trait]
impl ModelParamsStore for SqlModelParamsStore {
    async fn get_model_params(&self, key: &ModelKey) -> anyhow::Result<Option<ModelParams>> {
        let row: Option<Row> = self
            .db
            .fetch_optional("get_model_params", || {
                sqlx::query_as(
                    "SELECT total_millions, active_millions, note, updated_at, updated_by
                     FROM model_params WHERE key = $1",
                )
                .bind(key.as_str())
            })
            .await?;
        row.map(|row| record(key.clone(), row)).transpose()
    }

    async fn put_model_params(&self, params: &ModelParams) -> anyhow::Result<()> {
        self.db
            .execute("put_model_params", || {
                sqlx::query(
                    "INSERT INTO model_params
                         (key, total_millions, active_millions, note, updated_at, updated_by)
                     VALUES ($1, $2, $3, $4, $5, $6)
                     ON CONFLICT (key) DO UPDATE SET
                         total_millions = EXCLUDED.total_millions,
                         active_millions = EXCLUDED.active_millions,
                         note = EXCLUDED.note,
                         updated_at = EXCLUDED.updated_at,
                         updated_by = EXCLUDED.updated_by",
                )
                .bind(params.key.as_str())
                .bind(params.total_millions.into_inner())
                .bind(params.active_millions.into_inner())
                .bind(params.note.as_ref().map(AsRef::<str>::as_ref))
                .bind(params.updated_at)
                .bind(params.updated_by.as_ref().map(UserId::as_str))
            })
            .await?;
        Ok(())
    }

    async fn list_model_params(
        &self,
        after: Option<&ModelKey>,
        limit: NonZeroUsize,
    ) -> anyhow::Result<ModelParamsPage> {
        page_rows(
            self,
            MODEL_PARAMS,
            after,
            limit,
            ModelKey::try_new,
            |key, row| record(key.clone(), row),
            |_| true,
        )
        .await
    }

    async fn delete_model_params(&self, key: &ModelKey) -> anyhow::Result<()> {
        self.db
            .execute("delete_model_params", || {
                sqlx::query("DELETE FROM model_params WHERE key = $1").bind(key.as_str())
            })
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Context;

    use pipette_store::drain_model_params;

    use super::*;

    /// A store over a table without the migration's seed.
    async fn emptied(db: &SqlDatabase) -> anyhow::Result<SqlModelParamsStore> {
        sqlx::query("DELETE FROM model_params")
            .execute(db.pool())
            .await?;
        Ok(SqlModelParamsStore::new(db.clone()))
    }

    #[tokio::test]
    async fn meets_the_shared_contract() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = emptied(test_db.db()).await?;
        pipette_store_testkit::assert_model_params_store_roundtrip(&store).await
    }

    /// Every seeded key is one the store reads, with no writer.
    #[tokio::test]
    async fn the_migration_seeds_the_catalog_file() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = SqlModelParamsStore::new(test_db.db().clone());
        let seeded: i64 = sqlx::query_scalar("SELECT count(*) FROM model_params")
            .fetch_one(test_db.db().pool())
            .await?;

        let listing = drain_model_params(&store).await?;
        assert!(listing.unreadable.is_empty(), "{:?}", listing.unreadable);
        assert_eq!(i64::try_from(listing.items.len())?, seeded);
        assert_eq!(seeded, 183);
        assert!(listing.items.iter().all(|row| row.updated_by.is_none()));
        let noted = listing.items.iter().filter(|row| row.note.is_some());
        assert_eq!(noted.count(), 57);
        let moe = store
            .get_model_params(&ModelKey::try_new("zaya1-8b")?)
            .await?
            .context("a seeded MoE row")?;
        assert_eq!(
            (
                moe.total_millions.into_inner(),
                moe.active_millions.into_inner()
            ),
            (8840, 760)
        );
        assert_eq!(
            moe.note.as_ref().map(AsRef::as_ref),
            Some("MoE: card states 760M active")
        );
        Ok(())
    }

    /// The table refuses what `ModelParams::new` refuses.
    #[tokio::test]
    async fn the_checks_refuse_active_above_total() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = SqlModelParamsStore::new(test_db.db().clone());
        let params = ModelParams {
            active_millions: PositiveCount::try_new(4513)?,
            ..pipette_store_testkit::a_model_params("acmelm-4.5b-text", 4512)?
        };
        assert!(store.put_model_params(&params).await.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn an_unreadable_row_is_reported_by_its_object_key() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let store = emptied(db).await?;
        let good = pipette_store_testkit::a_model_params("acmelm-4.5b-text", 4512)?;
        store.put_model_params(&good).await?;
        sqlx::query(
            "INSERT INTO model_params (key, total_millions, active_millions, updated_at, updated_by)
             VALUES ('broken', 1, 1, now(), 'not a user id')",
        )
            .execute(db.pool())
            .await?;

        let listing = drain_model_params(&store).await?;
        assert_eq!(listing.items, vec![good]);
        let reported: Vec<&str> = listing.unreadable.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(reported, vec!["model-params/broken.json"]);
        Ok(())
    }
}
