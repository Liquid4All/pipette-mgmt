//! Listings sort as the object stores do, which the shared contract requires:
//! `{id}.json` in byte order, so `plan-a-1.json` before `plan-a.json`. That
//! needs the `C` collation, which `migrate` checks.

use std::num::NonZeroUsize;

use async_trait::async_trait;
use sqlx::postgres::PgRow;
use sqlx::{AssertSqlSafe, FromRow};

use pipette_store::{Page, UnreadableObjectError};

use crate::db::SqlDatabase;

#[derive(Debug, Clone, Copy)]
pub(crate) struct Keyspace {
    pub(crate) table: &'static str,
    pub(crate) id_column: &'static str,
    /// The object-store prefix, `plans/` for example. An unreadable report then
    /// names the key that an operator would look for on either backend.
    pub(crate) report_prefix: &'static str,
}

impl Keyspace {
    pub(crate) fn report_key(&self, id: &str) -> String {
        format!("{}{id}.json", self.report_prefix)
    }
}

/// The most rows that one round reads. A broken or filtered row fills no slot,
/// so a page may take several rounds.
const BATCH: i64 = 200;

/// The rows that a listing pages through, in `{id}.json` order.
#[async_trait]
pub(crate) trait Rows: Sync {
    /// What `parse` reads to make a record.
    type Row: Send;

    /// At most `limit` rows, with their ids, after the id `after`.
    async fn rows_after(
        &self,
        after: Option<&str>,
        limit: i64,
    ) -> anyhow::Result<Vec<(String, Self::Row)>>;

    /// The ids of the same rows as [`rows_after`](Self::rows_after).
    async fn ids_after(&self, after: Option<&str>, limit: i64) -> anyhow::Result<Vec<String>>;
}

/// The rows of a keyspace that keeps each record as a `body` column.
struct Bodies<'a> {
    db: &'a SqlDatabase,
    keyspace: Keyspace,
}

#[async_trait]
impl Rows for Bodies<'_> {
    type Row = String;

    async fn rows_after(
        &self,
        after: Option<&str>,
        limit: i64,
    ) -> anyhow::Result<Vec<(String, String)>> {
        let columns = format!("{}, body", self.keyspace.id_column);
        fetch_after(self.db, self.keyspace, &columns, after, limit).await
    }

    async fn ids_after(&self, after: Option<&str>, limit: i64) -> anyhow::Result<Vec<String>> {
        let ids: Vec<(String,)> = fetch_after(
            self.db,
            self.keyspace,
            self.keyspace.id_column,
            after,
            limit,
        )
        .await?;
        Ok(ids.into_iter().map(|(id,)| id).collect())
    }
}

/// [`page_rows`] over a keyspace that keeps each record as a `body` column.
pub(crate) async fn page<T, Id, E, P>(
    db: &SqlDatabase,
    keyspace: Keyspace,
    after: Option<&Id>,
    limit: NonZeroUsize,
    parse_id: fn(String) -> Result<Id, E>,
    mut parse: impl FnMut(&Id, &str) -> Result<T, P> + Send,
    admit: impl Fn(&T) -> bool + Send,
) -> anyhow::Result<Page<T, Id>>
where
    T: Send,
    Id: std::fmt::Display + Send + Sync,
    E: std::fmt::Display,
    P: std::fmt::Display,
{
    page_rows(
        &Bodies { db, keyspace },
        keyspace,
        after,
        limit,
        parse_id,
        |id, body: String| parse(id, &body),
        admit,
    )
    .await
}

/// `admit` runs after `parse`, so a filtered listing reports every unreadable
/// row it passes; a row that `parse_id` refuses is skipped unreported.
/// `resume_after` is `None` if no record is left, even on a full page.
pub(crate) async fn page_rows<R, T, Id, E, P>(
    rows: &R,
    keyspace: Keyspace,
    after: Option<&Id>,
    limit: NonZeroUsize,
    parse_id: fn(String) -> Result<Id, E>,
    mut parse: impl FnMut(&Id, R::Row) -> Result<T, P> + Send,
    admit: impl Fn(&T) -> bool + Send,
) -> anyhow::Result<Page<T, Id>>
where
    R: Rows,
    T: Send,
    Id: std::fmt::Display + Send + Sync,
    E: std::fmt::Display,
    P: std::fmt::Display,
{
    let mut page = Page::default();
    let mut cursor: Option<String> = after.map(ToString::to_string);
    // The first round reads one row past the page, so the row after the last
    // item usually shows whether a next page exists. A later round runs only
    // after rows that filled no slot, so it reads a full batch.
    let mut size = i64::try_from(limit.get().saturating_add(1))
        .unwrap_or(BATCH)
        .min(BATCH);
    loop {
        let batch = rows.rows_after(cursor.as_deref(), size).await?;
        let full_batch = i64::try_from(batch.len()).unwrap_or(i64::MAX) == size;
        let batch_last = batch.last().map(|(id, _)| id.clone());
        let mut batch = batch.into_iter();
        while let Some((raw_id, row)) = batch.next() {
            cursor = Some(raw_id.clone());
            let id = match parse_id(raw_id.clone()) {
                Ok(id) => id,
                Err(e) => {
                    tracing::warn!(table = keyspace.table, id = %raw_id, error = %e,
                        "skipping a row whose id is not a valid record id");
                    continue;
                }
            };
            match parse(&id, row) {
                Ok(record) if admit(&record) => page.items.push(record),
                Ok(_) => {}
                Err(e) => page.unreadable.push(UnreadableObjectError {
                    key: keyspace.report_key(&raw_id),
                    reason: e.to_string(),
                }),
            }
            if page.items.len() == limit.get() {
                // Only a row that names a record keeps the listing going: the
                // object stores never see the others at all.
                let more = batch.any(|(raw, _)| parse_id(raw).is_ok())
                    || (full_batch
                        && any_record_after(rows, batch_last.as_deref(), parse_id).await?);
                page.resume_after = more.then_some(id);
                return Ok(page);
            }
        }
        if !full_batch {
            return Ok(page);
        }
        size = BATCH;
    }
}

async fn any_record_after<R: Rows, Id, E>(
    rows: &R,
    after: Option<&str>,
    parse_id: fn(String) -> Result<Id, E>,
) -> anyhow::Result<bool> {
    let mut cursor = after.map(str::to_string);
    loop {
        let ids = rows.ids_after(cursor.as_deref(), BATCH).await?;
        let full_batch = i64::try_from(ids.len()).unwrap_or(i64::MAX) == BATCH;
        cursor = ids.last().cloned();
        if ids.into_iter().any(|raw| parse_id(raw).is_ok()) {
            return Ok(true);
        }
        if !full_batch {
            return Ok(false);
        }
    }
}

/// `columns` of at most `limit` rows of `keyspace` past the id `after`, in
/// `{id}.json` byte order.
pub(crate) async fn fetch_after<T>(
    db: &SqlDatabase,
    keyspace: Keyspace,
    columns: &str,
    after: Option<&str>,
    limit: i64,
) -> anyhow::Result<Vec<T>>
where
    T: for<'r> FromRow<'r, PgRow> + Send + Unpin,
{
    let Keyspace {
        table,
        id_column: id,
        ..
    } = keyspace;
    let sql = format!(
        "SELECT {columns} FROM {table}
         WHERE $1::text IS NULL OR ({id} || '.json') > ($1::text || '.json')
         ORDER BY ({id} || '.json')
         LIMIT $2"
    );
    db.fetch_all(table, || {
        sqlx::query_as(AssertSqlSafe(sql.clone()))
            .bind(after)
            .bind(limit)
    })
    .await
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;

    use anyhow::Context;

    use super::*;

    const SCRATCH: Keyspace = Keyspace {
        table: "listing_scratch",
        id_column: "id",
        report_prefix: "scratch/",
    };

    /// Refuses a `.` in an id, as the record ids do for a key like `x.bak`.
    fn parse_id(raw: String) -> Result<String, String> {
        if raw.contains('.') {
            Err(format!("{raw:?} is not an id"))
        } else {
            Ok(raw)
        }
    }

    async fn seed(db: &SqlDatabase, ids: &[&str]) -> anyhow::Result<()> {
        sqlx::query("CREATE TABLE listing_scratch (id text PRIMARY KEY, body text NOT NULL)")
            .execute(db.pool())
            .await?;
        for id in ids {
            sqlx::query("INSERT INTO listing_scratch (id, body) VALUES ($1, 'ok')")
                .bind(id)
                .execute(db.pool())
                .await?;
        }
        Ok(())
    }

    async fn first_page(db: &SqlDatabase, limit: usize) -> anyhow::Result<Page<String, String>> {
        let limit = NonZeroUsize::new(limit).context("limit")?;
        page(
            db,
            SCRATCH,
            None::<&String>,
            limit,
            parse_id,
            |id, _| Ok::<_, Infallible>(id.clone()),
            |_| true,
        )
        .await
    }

    /// Only a row that names a record keeps the listing going past a full page.
    #[rstest::rstest]
    #[case::a_full_last_page_ends_the_listing_past_rows_that_name_no_record(
        &["a", "b", "c.bak"],
        None
    )]
    #[case::a_record_past_a_row_that_names_none_keeps_the_listing_going(
        &["a", "b", "c.bak", "d"],
        Some("b")
    )]
    #[tokio::test]
    async fn a_full_page_resumes_only_before_a_record(
        #[case] ids: &[&str],
        #[case] resume_after: Option<&str>,
    ) -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        seed(test_db.db(), ids).await?;
        let page = first_page(test_db.db(), 2).await?;
        assert_eq!(page.items, ["a", "b"]);
        assert_eq!(page.resume_after.as_deref(), resume_after);
        Ok(())
    }

    /// Rows held in memory, recording the size of each read.
    struct Counted {
        ids: Vec<String>,
        reads: std::sync::Mutex<Vec<i64>>,
    }

    #[async_trait]
    impl Rows for Counted {
        type Row = ();

        async fn rows_after(
            &self,
            after: Option<&str>,
            limit: i64,
        ) -> anyhow::Result<Vec<(String, ())>> {
            self.reads
                .lock()
                .map_err(|_| anyhow::anyhow!("reads lock poisoned"))?
                .push(limit);
            let ids = self.ids_after(after, limit).await?;
            Ok(ids.into_iter().map(|id| (id, ())).collect())
        }

        async fn ids_after(&self, after: Option<&str>, limit: i64) -> anyhow::Result<Vec<String>> {
            Ok(self
                .ids
                .iter()
                .filter(|id| after.is_none_or(|a| id.as_str() > a))
                .take(usize::try_from(limit)?)
                .cloned()
                .collect())
        }
    }

    /// The first round reads one row past the page. A run of rows that fill no
    /// slot is crossed in full batches.
    #[rstest::rstest]
    // (ids, limit, items, resume_after, reads)
    #[case::one_row_past_the_page(&["a", "b", "c", "d", "e"], 2, &["a", "b"], Some("b"), &[3])]
    #[case::strays_in_full_batches(&["a.bak", "b.bak", "c.bak", "d", "e"], 1, &["d"], Some("d"), &[2, BATCH])]
    #[tokio::test]
    async fn a_page_sizes_its_reads(
        #[case] ids: &[&str],
        #[case] limit: usize,
        #[case] items: &[&str],
        #[case] resume_after: Option<&str>,
        #[case] reads: &[i64],
    ) -> anyhow::Result<()> {
        let rows = Counted {
            ids: ids.iter().map(ToString::to_string).collect(),
            reads: std::sync::Mutex::default(),
        };
        let page = page_rows(
            &rows,
            SCRATCH,
            None::<&String>,
            NonZeroUsize::new(limit).context("limit")?,
            parse_id,
            |id, ()| Ok::<_, Infallible>(id.clone()),
            |_| true,
        )
        .await?;
        assert_eq!(page.items, items);
        assert_eq!(page.resume_after.as_deref(), resume_after);
        let made = rows
            .reads
            .lock()
            .map_err(|_| anyhow::anyhow!("reads lock poisoned"))?
            .clone();
        assert_eq!(made, reads);
        Ok(())
    }
}
