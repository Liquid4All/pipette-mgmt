//! Backend selection: the one crate that depends on both storage backends and
//! reads the configuration to choose between them. A caller above this tier
//! names a backend nowhere else.

pub mod conditional_put;
pub mod database;
pub mod storage_lock;

use pipette_core::config::{Config, StorageConfig};
use pipette_store::{AuditStores, Stores};
use pipette_store_fs::{
    build_local_fs_client_store, build_local_fs_stores, build_local_fs_todo_store,
    build_local_fs_user_store,
};
use pipette_store_s3::{
    build_s3_client_store, build_s3_stores, build_s3_todo_store, build_s3_user_store,
};

/// If `[database]` is set, the record stores come from the database. The queue,
/// the submissions, the warehouse and the blobs stay on object storage.
pub async fn build_stores(config: &Config) -> anyhow::Result<Stores> {
    let stores = build_object_stores(config).await?;
    match &config.database {
        None => Ok(stores),
        Some(database) => Ok(pipette_store_sql::overlay(
            &pipette_store_sql::SqlDatabase::connect(database),
            stores,
        )),
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

/// Like [`build_stores`] but ignoring `[database]`: what `db import` reads
/// and `db export` writes.
pub async fn build_object_stores(config: &Config) -> anyhow::Result<Stores> {
    // The `todo/` queue must be S3 Express One Zone in production for atomic
    // claims. Resolved first: the S3 builder takes it, and the local_fs arm
    // replaces the seed store `build_local_fs_stores` makes.
    let todo = match config.todo_storage() {
        StorageConfig::LocalFs { .. } => {
            tracing::info!("initializing local_fs todo storage");
            build_local_fs_todo_store(config.todo_storage())?
        }
        StorageConfig::S3 { bucket, .. } => {
            tracing::info!(bucket, "initializing s3 todo storage");
            build_s3_todo_store(
                config.todo_storage(),
                std::time::Duration::from_secs(config.todo_attempt_timeout_secs.get()),
                std::time::Duration::from_secs(config.todo_operation_timeout_secs.get()),
            )
            .await?
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

    Ok(stores)
}
