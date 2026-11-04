//! Backend selection: the one crate that depends on both storage backends and
//! reads the configuration to choose between them. A caller above this tier
//! names a backend nowhere else.

pub mod conditional_put;
pub mod database;
pub mod storage_lock;

use std::sync::Arc;

use pipette_core::config::{Config, MissingDatabaseError, StorageConfig, TodoStorageRef};
use pipette_store::{AuditStores, LivenessImport, Stores, TodoStore};
use pipette_store_fs::{
    build_local_fs_client_store, build_local_fs_eval_package_store, build_local_fs_stores,
    build_local_fs_todo_store, build_local_fs_user_store,
};
use pipette_store_s3::{
    build_s3_client_store, build_s3_eval_package_store, build_s3_stores, build_s3_todo_store,
    build_s3_user_store,
};

/// If `[database]` is set, the record stores come from the database. The queue
/// follows `[todo_storage]`, the eval packages `[eval_packages_storage]`.
/// Submissions, the warehouse, and blobs use the object store.
pub async fn build_stores(config: &Config) -> anyhow::Result<Stores> {
    build_stores_with_db(config, connect_database(config).as_ref()).await
}

async fn build_stores_with_db(
    config: &Config,
    db: Option<&pipette_store_sql::SqlDatabase>,
) -> anyhow::Result<Stores> {
    let stores = build_object_stores_with_db(config, db).await?;
    match db {
        None => Ok(stores),
        Some(db) => Ok(pipette_store_sql::overlay(db, stores)),
    }
}

/// The stores that the audit event of a host command reads and writes, or
/// `None` without `[database]`, which alone keeps the audit log. It opens no
/// object storage.
pub fn build_audit_stores(config: &Config) -> Option<AuditStores> {
    let database = config.database.as_ref()?;
    Some(pipette_store_sql::audit_stores(
        &pipette_store_sql::SqlDatabase::connect(database),
    ))
}

/// Like [`build_stores`] but returns the object-store record stores: what
/// `db import` reads and `db export` writes. The queue follows `[todo_storage]`.
pub async fn build_object_stores(config: &Config) -> anyhow::Result<Stores> {
    build_object_stores_with_db(config, connect_database(config).as_ref()).await
}

fn connect_database(config: &Config) -> Option<pipette_store_sql::SqlDatabase> {
    config
        .database
        .as_ref()
        .map(pipette_store_sql::SqlDatabase::connect)
}

async fn build_object_stores_with_db(
    config: &Config,
    db: Option<&pipette_store_sql::SqlDatabase>,
) -> anyhow::Result<Stores> {
    // The code resolves the queue first. The S3 builder takes it, and the
    // `local_fs` arm replaces the seed store that `build_local_fs_stores` makes.
    let todo = match config.todo_storage() {
        TodoStorageRef::ObjectStore(storage) => build_object_todo_store(config, storage).await?,
        TodoStorageRef::Database => {
            tracing::info!("initializing database todo storage");
            let db = db.ok_or(MissingDatabaseError)?;
            Arc::new(pipette_store_sql::SqlTodoStore::new(db.clone()))
        }
    };

    let mut stores = match &config.storage {
        StorageConfig::LocalFs { .. } => {
            tracing::info!("initializing local_fs storage");
            let mut stores = build_local_fs_stores(config)?;
            stores.todo = todo;
            stores
        }
        StorageConfig::S3 { bucket, .. } => {
            tracing::info!(bucket, "initializing s3 storage");
            build_s3_stores(config, todo).await?
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

async fn build_object_todo_store(
    config: &Config,
    storage: &StorageConfig,
) -> anyhow::Result<Arc<dyn TodoStore>> {
    match storage {
        StorageConfig::LocalFs { .. } => {
            tracing::info!("initializing local_fs todo storage");
            build_local_fs_todo_store(storage)
        }
        StorageConfig::S3 { bucket, .. } => {
            tracing::info!(bucket, "initializing s3 todo storage");
            build_s3_todo_store(
                storage,
                std::time::Duration::from_secs(config.todo_attempt_timeout_secs.get()),
                std::time::Duration::from_secs(config.todo_operation_timeout_secs.get()),
            )
            .await
        }
    }
}

/// The two queues that `queue import` and `queue export` copy between.
pub struct QueueCopyStores {
    /// The queue that `[todo_storage]` names, or `[storage]` without it.
    pub objects: Arc<dyn TodoStore>,
    /// The queue in `[database]`.
    pub database: Arc<dyn TodoStore>,
    /// The database queue, to set each copied last-seen time.
    pub database_liveness: Arc<dyn LivenessImport>,
}

/// A config that cannot name both queues of a copy.
#[derive(Debug, thiserror::Error)]
pub enum QueueCopyConfigError {
    #[error("the queue copy needs [database]")]
    NoDatabase,
    #[error(
        "[todo_storage] backend = \"database\" names no object-store queue to copy; run the \
         command with the config that names the object-store queue in [todo_storage]"
    )]
    NoObjectStore,
}

/// Opens the object-store queue and the database queue of `config`.
pub async fn build_queue_copy_stores(config: &Config) -> anyhow::Result<QueueCopyStores> {
    let db = connect_database(config).ok_or(QueueCopyConfigError::NoDatabase)?;
    let TodoStorageRef::ObjectStore(storage) = config.todo_storage() else {
        return Err(QueueCopyConfigError::NoObjectStore.into());
    };
    let objects = build_object_todo_store(config, storage).await?;
    let database = Arc::new(pipette_store_sql::SqlTodoStore::new(db));
    Ok(QueueCopyStores {
        objects,
        database: database.clone(),
        database_liveness: database,
    })
}

#[cfg(test)]
mod tests {
    use pipette_core::config::TodoStorageConfig;
    use pipette_plan_types::{ClientId, ExpiresAt, JobId};
    use pipette_store::ClaimResult;
    use pipette_store_sql::scratch::TestDb;

    use super::*;

    #[rstest::rstest]
    #[case::no_database(None, TodoStorageConfig::ObjectStore(StorageConfig::default()))]
    #[case::database_queue(Some(()), TodoStorageConfig::Database)]
    #[tokio::test]
    async fn the_queue_copy_needs_both_queues_in_the_config(
        #[case] database: Option<()>,
        #[case] todo_storage: TodoStorageConfig,
    ) -> anyhow::Result<()> {
        let config = Config {
            database: database.map(|()| pipette_core::config::DatabaseConfig {
                host: None,
                port: None,
                user: None,
                database: None,
                ssl_mode: None,
                max_connections: std::num::NonZeroU32::MIN,
            }),
            todo_storage: Some(todo_storage),
            ..Config::default()
        };
        let Err(error) = build_queue_copy_stores(&config).await else {
            anyhow::bail!("the copy opened a config that names one queue");
        };
        assert!(error.is::<QueueCopyConfigError>(), "{error:#}");
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
            todo_storage: Some(TodoStorageConfig::Database),
            ..Config::default()
        };
        let stores = build_stores_with_db(&config, Some(db.db())).await?;
        let job = JobId::try_new("sql-queue-selected")?;
        let body = serde_json::json!({"job_id": job.as_str()});
        stores.todo.write_tmp(&job, &body).await?;
        stores.todo.promote_avail(&job, ExpiresAt::Never).await?;

        let direct = pipette_store_sql::SqlTodoStore::new(db.db().clone());
        assert_eq!(direct.get_avail_by_job(&job).await?, Some(body.clone()));
        let client = ClientId::try_new("ev1_sql_queue")?;
        let lease_expiry = chrono::Utc::now() + chrono::Duration::minutes(5);
        let claimed = stores
            .todo
            .claim_job(&job, ExpiresAt::Never, &client, lease_expiry)
            .await?;
        assert!(matches!(claimed, ClaimResult::Claimed(claimed) if claimed == body));
        assert_eq!(direct.get_avail_by_job(&job).await?, None);
        Ok(())
    }
}
