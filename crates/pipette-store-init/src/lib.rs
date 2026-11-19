//! Backend selection: the one crate that depends on both storage backends and
//! reads the configuration to choose between them. A caller above this tier
//! names a backend nowhere else.

pub mod conditional_put;
pub mod database;
pub mod storage_lock;

use std::sync::Arc;

use pipette_core::config::{Config, StorageConfig};
use pipette_store::{AuditStores, Stores, TodoStore};
use pipette_store_fs::{
    build_local_fs_client_store, build_local_fs_eval_package_store, build_local_fs_stores,
    build_local_fs_user_store, build_local_fs_vault_store,
};
use pipette_store_s3::{
    build_s3_client_store, build_s3_eval_package_store, build_s3_stores, build_s3_user_store,
    build_s3_vault_store,
};
use pipette_store_sql::SqlDatabase;

/// The record stores and the queue come from `[database]`. Eval packages
/// follow `[eval_packages_storage]` and vault blobs follow `[vault_storage]`.
/// Submissions and the warehouse use the object store. Reject a stale database
/// schema before initializing stores.
pub async fn build_stores(config: &Config) -> anyhow::Result<Stores> {
    build_stores_with_db(config, &SqlDatabase::connect(&config.database)).await
}

async fn build_stores_with_db(config: &Config, db: &SqlDatabase) -> anyhow::Result<Stores> {
    pipette_store_sql::check(db).await?;
    let stores = build_object_stores_with_db(config, db).await?;
    Ok(pipette_store_sql::overlay(db, stores))
}

/// The stores that the audit event of a host command reads and writes. It
/// opens no object storage.
pub fn build_audit_stores(config: &Config) -> AuditStores {
    pipette_store_sql::audit_stores(&SqlDatabase::connect(&config.database))
}

/// Like [`build_stores`] but returns the object-store record stores. The queue
/// is the database queue.
pub async fn build_object_stores(config: &Config) -> anyhow::Result<Stores> {
    build_object_stores_with_db(config, &SqlDatabase::connect(&config.database)).await
}

async fn build_object_stores_with_db(config: &Config, db: &SqlDatabase) -> anyhow::Result<Stores> {
    tracing::info!("initializing database todo storage");
    let todo: Arc<dyn TodoStore> = Arc::new(pipette_store_sql::SqlTodoStore::new(db.clone()));

    // With `[database]`, `overlay` wraps this store and keeps only its blobs.
    let vault = match config.vault_storage() {
        storage @ StorageConfig::LocalFs { .. } => {
            tracing::info!("initializing local_fs vault storage");
            build_local_fs_vault_store(storage)?
        }
        storage @ StorageConfig::S3 { bucket, .. } => {
            tracing::info!(bucket, "initializing s3 vault storage");
            build_s3_vault_store(storage).await?
        }
    };

    let mut stores = match &config.storage {
        StorageConfig::LocalFs { .. } => {
            tracing::info!("initializing local_fs storage");
            let mut stores = build_local_fs_stores(config, todo)?;
            stores.vault = vault;
            stores
        }
        StorageConfig::S3 { bucket, .. } => {
            tracing::info!(bucket, "initializing s3 storage");
            build_s3_stores(config, todo, vault)?
        }
    };

    // The fleet and the account roster share `[auth_storage]` as disjoint
    // keyspaces; organizations and the tag index stay on `[storage]`.
    match &config.auth_storage {
        StorageConfig::LocalFs { .. } => {
            tracing::info!("initializing local_fs auth storage");
            stores.clients = build_local_fs_client_store(&config.auth_storage)?;
            stores.users = build_local_fs_user_store(&config.auth_storage)?;
        }
        StorageConfig::S3 { bucket, .. } => {
            tracing::info!(bucket, "initializing s3 auth storage");
            stores.clients = build_s3_client_store(&config.auth_storage)?;
            stores.users = build_s3_user_store(&config.auth_storage)?;
        }
    }

    stores.eval_packages = match config.eval_packages_storage() {
        storage @ StorageConfig::LocalFs { .. } => {
            tracing::info!("initializing local_fs eval package storage");
            build_local_fs_eval_package_store(storage)?
        }
        storage @ StorageConfig::S3 { bucket, .. } => {
            tracing::info!(bucket, "initializing s3 eval package storage");
            build_s3_eval_package_store(storage)?
        }
    };

    Ok(stores)
}

#[cfg(test)]
mod tests {
    use pipette_plan_types::{ClientId, ExpiresAt, JobId};
    use pipette_store::ClaimResult;
    use pipette_store_sql::scratch::TestDb;

    use super::*;

    #[tokio::test]
    async fn an_unmigrated_database_is_rejected_before_initializing_stores() -> anyhow::Result<()> {
        let Some(db) = TestDb::unmigrated().await? else {
            return Ok(());
        };
        let dir = tempfile::tempdir()?;
        let config = Config {
            storage: StorageConfig::local_fs(dir.path().to_path_buf()),
            auth_storage: StorageConfig::local_fs(dir.path().to_path_buf()),
            ..Config::default()
        };

        let Err(error) = build_stores_with_db(&config, db.db()).await else {
            anyhow::bail!("an unmigrated database must be rejected");
        };
        assert!(error.to_string().contains("pipette-mgmt db migrate"));
        assert!(std::fs::read_dir(dir.path())?.next().is_none());
        Ok(())
    }

    #[rstest::rstest]
    #[case::object_records(false)]
    #[case::database_records(true)]
    #[tokio::test]
    async fn vault_blobs_land_in_vault_storage(#[case] database: bool) -> anyhow::Result<()> {
        let Some(db) = TestDb::new().await? else {
            return Ok(());
        };
        let (storage, vault) = (tempfile::tempdir()?, tempfile::tempdir()?);
        let config = Config {
            storage: StorageConfig::local_fs(storage.path().to_path_buf()),
            auth_storage: StorageConfig::local_fs(storage.path().to_path_buf()),
            vault_storage: Some(StorageConfig::local_fs(vault.path().to_path_buf())),
            ..Config::default()
        };
        let stores = if database {
            build_stores_with_db(&config, db.db()).await?
        } else {
            build_object_stores_with_db(&config, db.db()).await?
        };
        let resource = pipette_store_testkit::a_vault_resource()?;
        let written = stores
            .vault
            .put_blob(
                &resource.object_key,
                &resource.sha256,
                pipette_store_testkit::blob_body(pipette_store_testkit::VAULT_BLOB),
            )
            .await?;
        assert!(
            matches!(written, pipette_store::BlobWrite::Stored { .. }),
            "{written:?}"
        );
        let key = format!(
            "vault-resources/{}/blobs/{}",
            resource.owner.label(),
            resource.sha256
        );
        assert!(
            vault.path().join(&key).is_file(),
            "{key} must land in [vault_storage]"
        );
        assert!(
            !storage.path().join(&key).exists(),
            "{key} must not land in [storage]"
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_database_queue_claims_through_the_supplied_database() -> anyhow::Result<()> {
        let Some(db) = TestDb::new().await? else {
            return Ok(());
        };
        let dir = tempfile::tempdir()?;
        let config = Config {
            storage: StorageConfig::local_fs(dir.path().to_path_buf()),
            auth_storage: StorageConfig::local_fs(dir.path().to_path_buf()),
            ..Config::default()
        };
        let stores = build_stores_with_db(&config, db.db()).await?;
        let job = JobId::try_new("sql-queue-selected")?;
        let body = serde_json::json!({"job_id": job.as_str()});
        stores.todo.write_tmp(&job, &body).await?;
        stores
            .todo
            .promote_avail(&job, ExpiresAt::Never, None)
            .await?;

        let direct = pipette_store_sql::SqlTodoStore::new(db.db().clone());
        assert_eq!(direct.get_avail_by_job(&job).await?, Some(body.clone()));
        let client = ClientId::try_new("ev1_sql_queue")?;
        let lease_expiry = jiff::Timestamp::now() + jiff::SignedDuration::from_mins(5);
        let claimed = stores
            .todo
            .claim_job(&job, ExpiresAt::Never, &client, lease_expiry)
            .await?;
        assert!(matches!(claimed, ClaimResult::Claimed(claimed) if claimed == body));
        assert_eq!(direct.get_avail_by_job(&job).await?, None);
        Ok(())
    }
}
