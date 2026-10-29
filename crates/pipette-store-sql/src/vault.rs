//! [`VaultStore`] with its records on SQL and its blobs on the wrapped object
//! store. No method touches both; the caller writes a blob before its record.

use std::num::{NonZeroU64, NonZeroUsize};
use std::sync::Arc;

use anyhow::Context;
use async_trait::async_trait;

use pipette_core::vault::{
    VAULT_IMPORT_PREFIX, VaultImport, VaultObjectKey, VaultOwner, VaultResource,
};
use pipette_plan_types::{Sha256, VaultResourceId};
use pipette_store::{
    ArtifactBody, BlobBody, BlobWrite, StagedWrite, UnreadableObjectError, VaultBlob, VaultRecords,
    VaultResourceListing, VaultResourcePage, VaultStore,
};

use crate::db::SqlDatabase;
use crate::listing::{Keyspace, page};

const RECORDS: Keyspace = Keyspace {
    table: "vault",
    id_column: "resource_id",
    report_prefix: "vault/",
};

/// The store never reads the records that already exist under `vault/` and
/// `vault-imports/` in the wrapped store. A move to SQL copies those records.
#[derive(Clone)]
pub(crate) struct SqlVaultStore {
    db: SqlDatabase,
    blobs: Arc<dyn VaultStore>,
}

impl SqlVaultStore {
    pub(crate) fn new(db: SqlDatabase, blobs: Arc<dyn VaultStore>) -> Self {
        Self { db, blobs }
    }
}

/// The vault's records on SQL, with no blob store: what the audit of a host
/// command reads.
pub(crate) struct SqlVaultRecords {
    db: SqlDatabase,
}

impl SqlVaultRecords {
    pub(crate) fn new(db: SqlDatabase) -> Self {
        Self { db }
    }
}

#[async_trait]
impl VaultRecords for SqlVaultRecords {
    async fn get_resource(
        &self,
        resource_id: &VaultResourceId,
    ) -> anyhow::Result<Option<VaultResource>> {
        let id = resource_id.as_str();
        let body: Option<(String,)> = self
            .db
            .fetch_optional("get_resource", || {
                sqlx::query_as("SELECT body FROM vault WHERE resource_id = $1").bind(id)
            })
            .await?;
        body.map(|(body,)| serde_json::from_str(&body).with_context(|| RECORDS.report_key(id)))
            .transpose()
    }
}

/// The object-store key of an import entry, for an error to name.
fn import_key(owner_id: &str, import: &str) -> String {
    format!("{VAULT_IMPORT_PREFIX}{owner_id}/{import}.json")
}

#[async_trait]
impl VaultStore for SqlVaultStore {
    async fn get_resource(
        &self,
        resource_id: &VaultResourceId,
    ) -> anyhow::Result<Option<VaultResource>> {
        SqlVaultRecords::new(self.db.clone())
            .get_resource(resource_id)
            .await
    }

    /// One statement for every record, where the default makes one per record.
    async fn get_resources(
        &self,
        resource_ids: &[VaultResourceId],
    ) -> anyhow::Result<Vec<VaultResource>> {
        if resource_ids.is_empty() {
            return Ok(Vec::new());
        }
        let ids: Vec<&str> = resource_ids.iter().map(VaultResourceId::as_str).collect();
        let rows: Vec<(String, String)> = self
            .db
            .fetch_all("get_resources", || {
                sqlx::query_as("SELECT resource_id, body FROM vault WHERE resource_id = ANY($1)")
                    .bind(ids.as_slice())
            })
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(resource_id, body)| {
                serde_json::from_str(&body)
                    .inspect_err(|e| {
                        tracing::warn!(%resource_id, error = %e, "a vault record did not read");
                    })
                    .ok()
            })
            .collect())
    }

    async fn put_resource(&self, resource: &VaultResource) -> anyhow::Result<()> {
        let body = serde_json::to_string_pretty(resource)?;
        // Upsert, as both object stores overwrite a repeated id.
        self.db
            .execute("put_resource", || {
                sqlx::query(
                    "INSERT INTO vault (resource_id, body) VALUES ($1, $2)
                     ON CONFLICT (resource_id) DO UPDATE SET body = EXCLUDED.body",
                )
                .bind(resource.resource_id.as_str())
                .bind(body.as_str())
            })
            .await?;
        Ok(())
    }

    async fn list_resources(
        &self,
        after: Option<&VaultResourceId>,
        limit: NonZeroUsize,
    ) -> anyhow::Result<VaultResourcePage> {
        // No filter: ownership is a caller's scope, not a store-side selection.
        page(
            &self.db,
            RECORDS,
            after,
            limit,
            VaultResourceId::try_new,
            |_, body| serde_json::from_str::<VaultResource>(body),
            |_| true,
        )
        .await
    }

    async fn list_resources_of(&self, owner: &VaultOwner) -> anyhow::Result<VaultResourceListing> {
        let owner_id = owner.label();
        // A row that names no owner comes back too, and reads as unreadable.
        let rows: Vec<(String, String)> = self
            .db
            .fetch_all("list_resources_of", || {
                sqlx::query_as(
                    "SELECT resource_id, body FROM
                       (SELECT resource_id, body,
                               COALESCE(body::jsonb -> 'owner' ->> 'org_id',
                                        body::jsonb -> 'owner' ->> 'user_id') AS owner_id
                        FROM vault) AS owned
                     WHERE owner_id = $1 OR owner_id IS NULL
                     ORDER BY resource_id",
                )
                .bind(owner_id.as_str())
            })
            .await?;
        let mut listing = VaultResourceListing::default();
        for (id, body) in rows {
            match serde_json::from_str::<VaultResource>(&body) {
                Ok(resource) if &resource.owner == owner => listing.items.push(resource),
                Ok(_) => {}
                Err(e) => listing.unreadable.push(UnreadableObjectError {
                    key: RECORDS.report_key(&id),
                    reason: e.to_string(),
                }),
            }
        }
        Ok(listing)
    }

    async fn delete_resource(&self, resource_id: &VaultResourceId) -> anyhow::Result<()> {
        // No row is success, and the blob is not this half's to touch.
        self.db
            .execute("delete_resource", || {
                sqlx::query("DELETE FROM vault WHERE resource_id = $1").bind(resource_id.as_str())
            })
            .await?;
        Ok(())
    }

    async fn list_blobs(&self) -> anyhow::Result<Vec<VaultBlob>> {
        self.blobs.list_blobs().await
    }

    async fn stat_blob(&self, key: &VaultObjectKey) -> anyhow::Result<Option<u64>> {
        self.blobs.stat_blob(key).await
    }

    async fn delete_blob(&self, key: &VaultObjectKey) -> anyhow::Result<()> {
        self.blobs.delete_blob(key).await
    }

    async fn put_blob(
        &self,
        key: &VaultObjectKey,
        sha256: &Sha256,
        body: BlobBody,
    ) -> anyhow::Result<BlobWrite> {
        self.blobs.put_blob(key, sha256, body).await
    }

    async fn open_blob(
        &self,
        key: &VaultObjectKey,
        from: u64,
    ) -> anyhow::Result<Option<ArtifactBody>> {
        self.blobs.open_blob(key, from).await
    }

    async fn put_staged_blob(
        &self,
        owner: &VaultOwner,
        body: BlobBody,
        limit: NonZeroU64,
    ) -> anyhow::Result<StagedWrite> {
        // The wrapped store logs the promote event; logging it here too would
        // count each write twice.
        self.blobs.put_staged_blob(owner, body, limit).await
    }

    async fn get_import(
        &self,
        owner: &VaultOwner,
        import: &Sha256,
    ) -> anyhow::Result<Option<VaultImport>> {
        let owner_id = owner.label();
        let owner_id = owner_id.as_str();
        let import: &str = import.as_ref();
        let body: Option<(String,)> = self
            .db
            .fetch_optional("get_import", || {
                sqlx::query_as(
                    "SELECT body FROM vault_imports WHERE owner_id = $1 AND import_sha256 = $2",
                )
                .bind(owner_id)
                .bind(import)
            })
            .await?;
        body.map(|(body,)| {
            serde_json::from_str(&body).with_context(|| import_key(owner_id, import))
        })
        .transpose()
    }

    async fn put_import(
        &self,
        owner: &VaultOwner,
        import: &Sha256,
        entry: &VaultImport,
    ) -> anyhow::Result<()> {
        let owner_id = owner.label();
        let owner_id = owner_id.as_str();
        let import_sha256: &str = import.as_ref();
        let body = serde_json::to_string_pretty(entry)?;
        self.db
            .execute("put_import", || {
                sqlx::query(
                    "INSERT INTO vault_imports (owner_id, import_sha256, body) VALUES ($1, $2, $3)
                     ON CONFLICT (owner_id, import_sha256) DO UPDATE SET body = EXCLUDED.body",
                )
                .bind(owner_id)
                .bind(import_sha256)
                .bind(body.as_str())
            })
            .await?;
        tracing::info!(
            owner = %owner_id,
            %import,
            resources = entry.resource_ids.len(),
            "vault import indexed"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use sqlx::postgres::PgConnectOptions;

    use pipette_core::config::{Config, StorageConfig};
    use pipette_core::types::Mint;
    use pipette_plan_types::OrgId;
    use pipette_store::drain_vault_resources;
    use pipette_store_testkit::{
        VAULT_BLOB, a_huggingface_origin, a_vault_resource, blob_body, vault_blob_sha256,
    };

    use super::*;
    use crate::scratch::TestDb;

    fn local_fs_vault(dir: &std::path::Path) -> anyhow::Result<Arc<dyn VaultStore>> {
        let storage = StorageConfig::local_fs(dir.to_path_buf());
        let config = Config {
            storage: storage.clone(),
            auth_storage: storage,
            ..Default::default()
        };
        Ok(pipette_store_fs::build_local_fs_stores(&config)?.vault)
    }

    fn vault_over(
        test_db: &TestDb,
        dir: &std::path::Path,
    ) -> anyhow::Result<(SqlVaultStore, Arc<dyn VaultStore>)> {
        let blobs = local_fs_vault(dir)?;
        Ok((
            SqlVaultStore::new(test_db.db().clone(), Arc::clone(&blobs)),
            blobs,
        ))
    }

    async fn write_raw(db: &SqlDatabase, id: &str, body: &str) -> anyhow::Result<()> {
        sqlx::query("INSERT INTO vault (resource_id, body) VALUES ($1, $2)")
            .bind(id)
            .bind(body)
            .execute(db.pool())
            .await?;
        Ok(())
    }

    async fn stored_body(db: &SqlDatabase, id: &VaultResourceId) -> anyhow::Result<Option<String>> {
        Ok(
            sqlx::query_scalar("SELECT body FROM vault WHERE resource_id = $1")
                .bind(id.as_str())
                .fetch_optional(db.pool())
                .await?,
        )
    }

    #[tokio::test]
    async fn meets_the_shared_contract() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let dir = tempfile::tempdir()?;
        let (store, _) = vault_over(&test_db, dir.path())?;
        pipette_store_testkit::assert_vault_store_roundtrip(&store).await
    }

    /// One test for the contract and its log: `tracing` caches callsite interest
    /// globally, so a parallel test with no subscriber would hide the event.
    #[test]
    fn meets_the_staged_write_contract() -> anyhow::Result<()> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let dir = tempfile::tempdir()?;
        let store = {
            // A staged write never reaches the pool, so it needs no server; the
            // pool spawns its upkeep on this runtime.
            let _entered = runtime.enter();
            let db = SqlDatabase::from_options(
                PgConnectOptions::new_without_pgpass().host("pipette.invalid"),
                1,
            );
            SqlVaultStore::new(db, local_fs_vault(dir.path())?)
        };
        pipette_store_testkit::assert_vault_staged_write_logs_once(&store)?;
        runtime.block_on(pipette_store_testkit::assert_vault_staged_write(&store))
    }

    #[tokio::test]
    async fn meets_the_import_contract() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let dir = tempfile::tempdir()?;
        let (store, _) = vault_over(&test_db, dir.path())?;
        pipette_store_testkit::assert_vault_import_roundtrip(&store).await
    }

    #[tokio::test]
    async fn records_go_to_sql_and_blobs_stay_behind() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let dir = tempfile::tempdir()?;
        let (store, blobs) = vault_over(&test_db, dir.path())?;
        let resource = a_vault_resource()?;
        let sha256 = vault_blob_sha256()?;

        store
            .put_blob(&resource.object_key, &sha256, blob_body(VAULT_BLOB))
            .await?;
        store.put_resource(&resource).await?;
        let entry = VaultImport {
            resource_ids: vec![resource.resource_id.clone()],
        };
        store.put_import(&resource.owner, &sha256, &entry).await?;

        let len = Some(u64::try_from(VAULT_BLOB.len())?);
        assert_eq!(
            blobs.stat_blob(&resource.object_key).await?,
            len,
            "the blob is on the wrapped store"
        );
        assert!(
            dir.path()
                .join(format!(
                    "vault-resources/{}/blobs/{sha256}",
                    resource.owner.label()
                ))
                .is_file()
        );
        assert_eq!(blobs.get_resource(&resource.resource_id).await?, None);
        assert_eq!(blobs.get_import(&resource.owner, &sha256).await?, None);
        assert_eq!(
            stored_body(test_db.db(), &resource.resource_id).await?,
            Some(serde_json::to_string_pretty(&resource)?),
            "the row holds what `vault/{{id}}.json` would"
        );

        store.delete_resource(&resource.resource_id).await?;
        assert_eq!(
            stored_body(test_db.db(), &resource.resource_id).await?,
            None
        );
        assert_eq!(blobs.stat_blob(&resource.object_key).await?, len);
        Ok(())
    }

    /// Read with others, a row that is no vault record is left out.
    #[tokio::test]
    async fn a_corrupt_record_does_not_cost_the_batch() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let dir = tempfile::tempdir()?;
        let (store, _) = vault_over(&test_db, dir.path())?;
        let kept = a_vault_resource()?;
        let broken = a_vault_resource()?.resource_id;
        store.put_resource(&kept).await?;
        write_raw(test_db.db(), broken.as_str(), "{not json").await?;

        let read = store
            .get_resources(&[broken, kept.resource_id.clone()])
            .await?;
        assert_eq!(read, vec![kept]);
        Ok(())
    }

    #[tokio::test]
    async fn an_unreadable_row_is_reported_once_and_kept() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let dir = tempfile::tempdir()?;
        let (store, _) = vault_over(&test_db, dir.path())?;
        let first = a_vault_resource()?;
        let future = VaultResource {
            origin: Some(a_huggingface_origin()?),
            ..a_vault_resource()?
        };
        let last = a_vault_resource()?;
        store.put_resource(&first).await?;
        store.put_resource(&last).await?;
        let mut body = serde_json::to_value(&future)?;
        let origin = body
            .get_mut("origin")
            .and_then(|origin| origin.as_object_mut())
            .context("the record has an origin")?;
        origin.insert("source".into(), "gitlab".into());
        let body = serde_json::to_string_pretty(&body)?;
        write_raw(db, future.resource_id.as_str(), &body).await?;

        let read = store.get_resource(&future.resource_id).await;
        assert!(
            read.as_ref()
                .is_err_and(|e| e.root_cause().is::<serde_json::Error>()),
            "{read:?}"
        );
        let listing = drain_vault_resources(&store).await?;
        assert_eq!(
            listing
                .items
                .iter()
                .map(|r| &r.resource_id)
                .collect::<Vec<_>>(),
            vec![&first.resource_id, &last.resource_id]
        );
        assert_eq!(
            listing
                .unreadable
                .iter()
                .map(|r| r.key.as_str())
                .collect::<Vec<_>>(),
            vec![format!("vault/{}.json", future.resource_id)]
        );
        assert_eq!(
            stored_body(db, &future.resource_id).await?,
            Some(body),
            "an unreadable row is never rewritten"
        );

        // An owner's listing reports an unreadable row only where it may be
        // that owner's: one naming it, or one naming no owner at all.
        let unreadable_of = |owner: VaultOwner| {
            let store = &store;
            async move {
                anyhow::Ok(
                    store
                        .list_resources_of(&owner)
                        .await?
                        .unreadable
                        .into_iter()
                        .map(|report| report.key)
                        .collect::<Vec<_>>(),
                )
            }
        };
        let unreadable = vec![format!("vault/{}.json", future.resource_id)];
        assert_eq!(unreadable_of(future.owner.clone()).await?, unreadable);
        assert!(unreadable_of(first.owner.clone()).await?.is_empty());
        let ownerless = a_vault_resource()?.resource_id;
        write_raw(db, ownerless.as_str(), "{}").await?;
        assert_eq!(
            unreadable_of(first.owner.clone()).await?,
            vec![format!("vault/{ownerless}.json")]
        );

        let owner = VaultOwner::Org {
            org_id: OrgId::mint(),
        };
        let import = vault_blob_sha256()?;
        sqlx::query(
            "INSERT INTO vault_imports (owner_id, import_sha256, body) VALUES ($1, $2, '[1')",
        )
        .bind(owner.label())
        .bind(import.as_ref())
        .execute(db.pool())
        .await?;
        assert!(store.get_import(&owner, &import).await.is_err());
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn racing_record_writes_are_last_writer_wins() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let dir = tempfile::tempdir()?;
        let (store, _) = vault_over(&test_db, dir.path())?;
        let base = a_vault_resource()?;
        crate::testing::assert_racing_writes_are_last_writer_wins(
            |n| {
                Ok(VaultResource {
                    bytes: u64::try_from(n)?,
                    ..base.clone()
                })
            },
            |resource: VaultResource| {
                let store = store.clone();
                async move { store.put_resource(&resource).await }
            },
            || store.get_resource(&base.resource_id),
        )
        .await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn racing_record_deletes_are_idempotent() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let dir = tempfile::tempdir()?;
        let (store, _) = vault_over(&test_db, dir.path())?;
        let resource = a_vault_resource()?;
        crate::testing::assert_racing_deletes_are_idempotent(
            resource.clone(),
            |resource: VaultResource| {
                let store = store.clone();
                async move { store.put_resource(&resource).await }
            },
            |resource: VaultResource| {
                let store = store.clone();
                async move { store.delete_resource(&resource.resource_id).await }
            },
            || store.get_resource(&resource.resource_id),
        )
        .await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn racing_import_writes_are_last_writer_wins() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let dir = tempfile::tempdir()?;
        let (store, _) = vault_over(&test_db, dir.path())?;
        let owner = VaultOwner::Org {
            org_id: OrgId::mint(),
        };
        let bystander = a_vault_resource()?.owner;
        let import = vault_blob_sha256()?;
        let kept = VaultImport {
            resource_ids: vec![VaultResourceId::mint()],
        };
        store.put_import(&bystander, &import, &kept).await?;

        crate::testing::assert_racing_writes_are_last_writer_wins(
            |_| {
                Ok(VaultImport {
                    resource_ids: vec![VaultResourceId::mint(), VaultResourceId::mint()],
                })
            },
            |entry: VaultImport| {
                let (store, owner, import) = (store.clone(), owner.clone(), import.clone());
                async move { store.put_import(&owner, &import, &entry).await }
            },
            || store.get_import(&owner, &import),
        )
        .await?;
        assert_eq!(store.get_import(&bystander, &import).await?, Some(kept));
        Ok(())
    }
}
