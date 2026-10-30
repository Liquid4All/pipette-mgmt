use std::sync::Arc;

use serde_json::json;

use pipette_core::config::{Config, StorageConfig};
use pipette_core::types::Mint;
use pipette_plan_types::{ClientId, JobId, Tag, UserId};
use pipette_store::{ClientStore, Stores, TagStore};

use crate::clients::SqlClientStore;
use crate::tags::SqlTagStore;
use crate::testing::{a_client, public_key, stored_outcome};
use crate::users::SqlUserStore;

#[tokio::test]
async fn users_are_not_clients() -> anyhow::Result<()> {
    let test_db = crate::testing::test_db_or_skip!();
    pipette_store_testkit::assert_users_are_not_clients(
        &SqlClientStore::new(test_db.db().clone()),
        &SqlUserStore::new(test_db.db().clone()),
        &a_client(ClientId::try_new("ev1_fleet")?, public_key(0x02)?)?,
    )
    .await
}

#[tokio::test]
async fn permission_sets_are_per_principal() -> anyhow::Result<()> {
    let test_db = crate::testing::test_db_or_skip!();
    let clients = SqlClientStore::new(test_db.db().clone());
    let client = a_client(ClientId::try_new("ev1_shared")?, public_key(0x01)?)?;
    clients.put_client(&client).await?;
    pipette_store_testkit::assert_permission_objects_are_per_principal(
        &clients,
        &SqlUserStore::new(test_db.db().clone()),
        &client.client_id,
        &UserId::mint(),
    )
    .await
}

/// Both stores on one database, so a key in the other kind's tables would show.
#[tokio::test]
async fn user_preauth_keys_stay_out_of_the_client_tables() -> anyhow::Result<()> {
    let test_db = crate::testing::test_db_or_skip!();
    pipette_store_testkit::assert_user_preauth_store_contract(
        &SqlUserStore::new(test_db.db().clone()),
        &SqlClientStore::new(test_db.db().clone()),
    )
    .await
}

/// One table holds both directions, so there is no drift to add back.
#[tokio::test]
async fn reindex_collects_a_deleted_clients_tags() -> anyhow::Result<()> {
    let test_db = crate::testing::test_db_or_skip!();
    let clients = SqlClientStore::new(test_db.db().clone());
    let tags = SqlTagStore::new(test_db.db().clone());
    let kept = a_client(ClientId::try_new("ev1_kept")?, public_key(0x03)?)?;
    clients.put_client(&kept).await?;
    let tag = Tag::try_new("lab")?;
    tags.add_client_tag(&kept.client_id, &tag).await?;
    let gone = ClientId::try_new("ev1_gone")?;
    tags.add_client_tag(&gone, &tag).await?;

    let report = pipette_store::reindex_tags(&clients, &tags).await?;
    assert_eq!(report.added, 0);
    assert!(report.removed > 0, "{report:?}");
    assert_eq!(
        tags.list_client_ids_by_tag(&tag).await?,
        std::slice::from_ref(&kept.client_id)
    );
    assert!(tags.get_client_tags(&gone).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn overlay_replaces_the_record_stores_only() -> anyhow::Result<()> {
    let test_db = crate::testing::test_db_or_skip!();
    let dir = tempfile::tempdir()?;
    let config = Config {
        storage: StorageConfig::local_fs(dir.path().to_path_buf()),
        auth_storage: StorageConfig::local_fs(dir.path().to_path_buf()),
        ..Config::default()
    };
    let object = pipette_store_fs::build_local_fs_stores(&config)?;
    let stores = crate::overlay(test_db.db(), object.clone());

    let kept = |a: &Stores| {
        [
            Arc::as_ptr(&a.todo).cast::<()>(),
            Arc::as_ptr(&a.warehouse).cast(),
            Arc::as_ptr(&a.eval_sample_results).cast(),
            Arc::as_ptr(&a.dataset_cache).cast(),
        ]
    };
    assert_eq!(kept(&stores), kept(&object));

    let client = a_client(ClientId::try_new("ev1_routed")?, public_key(0x04)?)?;
    stores.clients.put_client(&client).await?;
    assert!(
        object
            .clients
            .get_client(&client.client_id)
            .await?
            .is_none()
    );
    assert!(
        SqlClientStore::new(test_db.db().clone())
            .get_client(&client.client_id)
            .await?
            .is_some()
    );

    // A submission keeps its record on the wrapped store and its outcome on SQL.
    let job_id = JobId::try_new("job-overlay")?;
    stores
        .submissions
        .write_incoming(&job_id, &json!({ "message_type": "failure" }))
        .await?;
    assert!(object.submissions.has_record(&job_id).await?);
    assert_eq!(
        stored_outcome(test_db.db(), &job_id).await?.as_deref(),
        Some("failed")
    );
    Ok(())
}
