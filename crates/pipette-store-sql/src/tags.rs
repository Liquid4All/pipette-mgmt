//! [`TagStore`] on SQL: one row per `(client_id, tag)`, indexed both ways, in
//! place of the object stores' two marker trees. The directions cannot drift,
//! so [`pipette_store::reindex_tags`] only removes deleted clients' tags.

use std::collections::BTreeSet;

use async_trait::async_trait;

use pipette_core::types::id_from_key;
use pipette_plan_types::{ClientId, Tag};
use pipette_store::TagStore;

use crate::db::SqlDatabase;

/// `None` (logged) for an unparseable row. The id is read verbatim, so a padded
/// stray is not aliased onto a real client.
fn marker(client_id: &str, tag: &str) -> Option<(ClientId, Tag)> {
    id_from_key(client_id, ClientId::try_new)
        .map_err(anyhow::Error::from)
        .and_then(|id| Ok((id, Tag::try_new(tag)?)))
        .inspect_err(|e| {
            tracing::warn!(client_id, tag, error = %e, "skipping unparseable client tag row");
        })
        .ok()
}

pub(crate) struct SqlTagStore {
    db: SqlDatabase,
}

impl SqlTagStore {
    pub(crate) fn new(db: SqlDatabase) -> Self {
        Self { db }
    }

    async fn markers(&self, sql: &'static str) -> anyhow::Result<Vec<(ClientId, Tag)>> {
        let rows: Vec<(String, String)> = self
            .db
            .fetch_all("client_tags", || sqlx::query_as(sql))
            .await?;
        Ok(rows
            .iter()
            .filter_map(|(client_id, tag)| marker(client_id, tag))
            .collect())
    }
}

#[async_trait]
impl TagStore for SqlTagStore {
    async fn add_client_tag(&self, client_id: &ClientId, tag: &Tag) -> anyhow::Result<()> {
        self.db
            .execute("add_client_tag", || {
                sqlx::query(
                    "INSERT INTO client_tags (client_id, tag) VALUES ($1, $2)
                     ON CONFLICT (client_id, tag) DO NOTHING",
                )
                .bind(client_id.as_str())
                .bind(tag.as_str())
            })
            .await?;
        Ok(())
    }

    async fn remove_client_tag(&self, client_id: &ClientId, tag: &Tag) -> anyhow::Result<()> {
        self.db
            .execute("remove_client_tag", || {
                sqlx::query("DELETE FROM client_tags WHERE client_id = $1 AND tag = $2")
                    .bind(client_id.as_str())
                    .bind(tag.as_str())
            })
            .await?;
        Ok(())
    }

    async fn delete_client_tags(&self, client_id: &ClientId) -> anyhow::Result<()> {
        self.db
            .execute("delete_client_tags", || {
                sqlx::query("DELETE FROM client_tags WHERE client_id = $1").bind(client_id.as_str())
            })
            .await?;
        Ok(())
    }

    async fn get_client_tags(&self, client_id: &ClientId) -> anyhow::Result<BTreeSet<Tag>> {
        let id = client_id.as_str();
        let tags: Vec<(String,)> = self
            .db
            .fetch_all("get_client_tags", || {
                sqlx::query_as("SELECT tag FROM client_tags WHERE client_id = $1").bind(id)
            })
            .await?;
        Ok(tags
            .iter()
            .filter_map(|(tag,)| marker(id, tag).map(|(_, tag)| tag))
            .collect())
    }

    async fn list_client_ids_by_tag(&self, tag: &Tag) -> anyhow::Result<Vec<ClientId>> {
        let tag = tag.as_str();
        let ids: Vec<(String,)> = self
            .db
            .fetch_all("list_client_ids_by_tag", || {
                sqlx::query_as(
                    "SELECT client_id FROM client_tags WHERE tag = $1 ORDER BY client_id",
                )
                .bind(tag)
            })
            .await?;
        Ok(ids
            .iter()
            .filter_map(|(id,)| marker(id, tag).map(|(id, _)| id))
            .collect())
    }

    async fn list_forward_tag_markers(&self) -> anyhow::Result<Vec<(ClientId, Tag)>> {
        self.markers("SELECT client_id, tag FROM client_tags ORDER BY client_id, tag")
            .await
    }

    async fn list_reverse_tag_markers(&self) -> anyhow::Result<Vec<(ClientId, Tag)>> {
        self.markers("SELECT client_id, tag FROM client_tags ORDER BY tag, client_id")
            .await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::Arc;

    use futures::future::BoxFuture;

    use pipette_plan_types::{ClientId, Tag};
    use pipette_store::{ClientStore, ReindexReport, TagStore};

    use super::SqlTagStore;
    use crate::db::SqlDatabase;
    use crate::testing::{a_client, public_key, race};

    /// The row a marker key from either tree names; `None` for a nested or
    /// foreign key.
    fn marker_row(key: &str) -> Option<(String, String)> {
        let (client_id, tag) = match key.strip_prefix("tags-index/by-client/") {
            Some(rest) => rest.split_once('/')?,
            None => {
                let (tag, client_id) = key.strip_prefix("tags-index/by-tag/")?.split_once('/')?;
                (client_id, tag)
            }
        };
        (!client_id.contains('/') && !tag.contains('/'))
            .then(|| (client_id.to_string(), tag.to_string()))
    }

    /// The contract's raw-write hook: writes the named row unvalidated, so a
    /// padded id lands for the listings to refuse.
    fn write_raw_marker(
        db: &SqlDatabase,
    ) -> impl Fn(String, Vec<u8>) -> BoxFuture<'static, anyhow::Result<()>> + '_ {
        move |key, _bytes| {
            let db = db.clone();
            Box::pin(async move {
                let Some((client_id, tag)) = marker_row(&key) else {
                    return Ok(());
                };
                sqlx::query(
                    "INSERT INTO client_tags (client_id, tag) VALUES ($1, $2)
                     ON CONFLICT DO NOTHING",
                )
                .bind(client_id)
                .bind(tag)
                .execute(db.pool())
                .await?;
                Ok(())
            })
        }
    }

    async fn row_count(db: &SqlDatabase) -> anyhow::Result<i64> {
        Ok(sqlx::query_scalar("SELECT count(*) FROM client_tags")
            .fetch_one(db.pool())
            .await?)
    }

    #[tokio::test]
    async fn sql_tag_store_roundtrip() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = SqlTagStore::new(test_db.db().clone());
        pipette_store_testkit::assert_tag_store_roundtrip(
            &store,
            &ClientId::try_new("ev1_a")?,
            &ClientId::try_new("ev1_b")?,
            write_raw_marker(test_db.db()),
        )
        .await?;
        // The padded strays did land; the listings refused them.
        assert!(row_count(test_db.db()).await? > 2);
        Ok(())
    }

    #[tokio::test]
    async fn sql_tag_is_one_row() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = SqlTagStore::new(test_db.db().clone());
        store
            .add_client_tag(&ClientId::try_new("ev1_rack")?, &Tag::try_new("lab")?)
            .await?;
        let rows: Vec<(String, String)> = sqlx::query_as("SELECT client_id, tag FROM client_tags")
            .fetch_all(test_db.db().pool())
            .await?;
        assert_eq!(rows, [("ev1_rack".to_string(), "lab".to_string())]);
        Ok(())
    }

    #[tokio::test]
    async fn sql_reindex_drops_only_orphans() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = SqlTagStore::new(test_db.db().clone());
        let dir = tempfile::tempdir()?;
        let clients = pipette_store_fs::LocalFsClientStore::new(dir.path().to_path_buf());
        let kept = ClientId::try_new("ev1_kept")?;
        let gone = ClientId::try_new("ev1_gone")?;
        clients
            .put_client(&a_client(kept.clone(), public_key(1)?)?)
            .await?;
        let lab = Tag::try_new("lab")?;
        let east = Tag::try_new("us-east")?;
        store.add_client_tag(&kept, &lab).await?;
        store.add_client_tag(&gone, &lab).await?;
        store.add_client_tag(&gone, &east).await?;

        assert_eq!(
            pipette_store::reindex_tags(&clients, &store).await?,
            ReindexReport {
                removed: 2,
                added: 0
            }
        );
        assert_eq!(
            store.list_forward_tag_markers().await?,
            vec![(kept.clone(), lab.clone())]
        );
        assert_eq!(
            pipette_store::reindex_tags(&clients, &store).await?,
            ReindexReport::default(),
            "a second run finds nothing to repair"
        );
        Ok(())
    }

    /// Every racer succeeds: a write conflict is retried, not reported.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn sql_racing_adds_make_one_row() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = Arc::new(SqlTagStore::new(test_db.db().clone()));
        for round in 0..10 {
            let client_id = ClientId::try_new(format!("ev1_race{round}"))?;
            let tag = Tag::try_new("lab")?;
            race(|_| {
                let (store, client_id, tag) = (store.clone(), client_id.clone(), tag.clone());
                async move { store.add_client_tag(&client_id, &tag).await }
            })
            .await?;
            assert_eq!(
                store.get_client_tags(&client_id).await?,
                BTreeSet::from([tag])
            );
        }
        assert_eq!(row_count(test_db.db()).await?, 10);
        Ok(())
    }
}
