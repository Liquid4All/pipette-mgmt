//! [`S3ClientStore`] — the client keyspaces on an object store: device records,
//! their permissions and the pre-auth keys that register them.

use std::num::NonZeroUsize;
use std::sync::Arc;

use async_trait::async_trait;
use object_store::ObjectStore;

use pipette_core::client::Client;
use pipette_core::client_permission::ClientPermissionSet;
use pipette_core::client_preauth::{
    ClientPreauthConsumeOutcome, ClientPreauthKey, ClientPreauthRejectionError,
    ClientPreauthSecret, validate,
};
use pipette_plan_types::{ClientId, ClientPreauthKeyId, ClientPreauthUsage};
use pipette_store::{ClientPage, ClientStore};

use super::{client_layout, client_permission_layout, preauth_layout};

/// The client keyspaces under one bucket and prefix. Addresses no user
/// keyspace, so it cannot reach an account's records.
#[derive(Clone)]
pub struct S3ClientStore {
    store: Arc<dyn ObjectStore>,
    prefix: String,
}

impl S3ClientStore {
    pub fn new(store: Arc<dyn ObjectStore>, prefix: String) -> Self {
        Self { store, prefix }
    }
}

#[async_trait]
impl ClientStore for S3ClientStore {
    async fn get_client(&self, client_id: &ClientId) -> anyhow::Result<Option<Client>> {
        client_layout::load_client(&self.store, &self.prefix, client_id).await
    }

    async fn put_client(&self, client: &Client) -> anyhow::Result<()> {
        client_layout::save_client(&self.store, &self.prefix, client).await
    }

    async fn replace_client(&self, read: &Client, updated: &Client) -> anyhow::Result<bool> {
        client_layout::replace_client(&self.store, &self.prefix, read, updated).await
    }

    async fn delete_client(&self, client_id: &ClientId) -> anyhow::Result<()> {
        client_layout::delete_client(&self.store, &self.prefix, client_id).await?;
        client_permission_layout::delete(&self.store, &self.prefix, client_id.as_str()).await
    }

    async fn list_clients(
        &self,
        after: Option<&ClientId>,
        limit: NonZeroUsize,
    ) -> anyhow::Result<ClientPage> {
        client_layout::list_page(&self.store, &self.prefix, after, limit).await
    }

    async fn list_client_ids(&self) -> anyhow::Result<Vec<ClientId>> {
        client_layout::list_ids(&self.store, &self.prefix).await
    }

    async fn has_public_key(
        &self,
        public_key: &pipette_plan_types::PublicKeyHex,
    ) -> anyhow::Result<bool> {
        client_layout::find_by_public_key(&self.store, &self.prefix, public_key).await
    }

    async fn has_signature_migration(&self, client_id: &ClientId) -> anyhow::Result<bool> {
        client_layout::has_signature_migration(&self.store, &self.prefix, client_id).await
    }

    async fn record_signature_migration(
        &self,
        client_id: &ClientId,
        at: chrono::DateTime<chrono::Utc>,
    ) -> anyhow::Result<pipette_core::client::MigrationRecord> {
        client_layout::record_signature_migration(&self.store, &self.prefix, client_id, at).await
    }

    async fn list_signature_migrations(
        &self,
    ) -> anyhow::Result<Vec<(ClientId, pipette_core::client::SignatureMigration)>> {
        client_layout::list_signature_migrations(&self.store, &self.prefix).await
    }

    async fn get_client_permissions(
        &self,
        client_id: &ClientId,
    ) -> anyhow::Result<Option<ClientPermissionSet>> {
        client_permission_layout::load(&self.store, &self.prefix, client_id.as_str()).await
    }

    async fn list_client_permissions(
        &self,
    ) -> anyhow::Result<Vec<(ClientId, ClientPermissionSet)>> {
        client_permission_layout::list_all(&self.store, &self.prefix).await
    }

    async fn put_client_permissions(
        &self,
        client_id: &ClientId,
        set: &ClientPermissionSet,
    ) -> anyhow::Result<()> {
        client_permission_layout::save(&self.store, &self.prefix, client_id.as_str(), set).await
    }

    async fn delete_client_permissions(&self, client_id: &ClientId) -> anyhow::Result<()> {
        client_permission_layout::delete(&self.store, &self.prefix, client_id.as_str()).await
    }

    /// An exhausted retry budget is an error naming the client, so the operator
    /// learns the grant did not land.
    async fn mutate_client_permissions(
        &self,
        client_id: &ClientId,
        f: &(dyn for<'s> Fn(&'s mut ClientPermissionSet) -> bool + Send + Sync),
    ) -> anyhow::Result<pipette_store::ClientPermissionMutation> {
        let (changed, resulting) =
            client_permission_layout::mutate(&self.store, &self.prefix, client_id.as_str(), f)
                .await?
                .ok_or_else(|| client_permission_layout::contention_error(client_id.as_str()))?;
        Ok(pipette_store::ClientPermissionMutation { changed, resulting })
    }

    async fn put_client_preauth_key(&self, key: &ClientPreauthKey) -> anyhow::Result<()> {
        preauth_layout::save(
            &self.store,
            &self.prefix,
            preauth_layout::CLIENT_KEYSPACE,
            key.key_id.as_str(),
            key,
        )
        .await
    }

    async fn get_client_preauth_key(
        &self,
        key_id: &ClientPreauthKeyId,
    ) -> anyhow::Result<Option<ClientPreauthKey>> {
        preauth_layout::load(
            &self.store,
            &self.prefix,
            preauth_layout::CLIENT_KEYSPACE,
            key_id.as_str(),
        )
        .await
    }

    async fn consume_client_preauth_key(
        &self,
        key_id: &ClientPreauthKeyId,
        secret: &ClientPreauthSecret,
    ) -> anyhow::Result<ClientPreauthConsumeOutcome> {
        let keyspace = preauth_layout::CLIENT_KEYSPACE;
        let Some(key): Option<ClientPreauthKey> =
            preauth_layout::load(&self.store, &self.prefix, keyspace, key_id.as_str()).await?
        else {
            return Ok(ClientPreauthConsumeOutcome::Rejected(
                ClientPreauthRejectionError::NotFound,
            ));
        };
        let now = chrono::Utc::now();
        let grant = match validate(&key, secret, now) {
            Ok(grant) => grant,
            Err(rejection) => return Ok(ClientPreauthConsumeOutcome::Rejected(rejection)),
        };
        if matches!(key.usage, ClientPreauthUsage::SingleUse) {
            match preauth_layout::claim_spent(
                &self.store,
                &self.prefix,
                keyspace,
                key_id.as_str(),
                now,
            )
            .await?
            {
                // Reported as unknown, like every other reason a key will not
                // grant.
                preauth_layout::Spend::AlreadySpent => {
                    return Ok(ClientPreauthConsumeOutcome::Rejected(
                        ClientPreauthRejectionError::NotFound,
                    ));
                }
                preauth_layout::Spend::Claimed => {
                    preauth_layout::delete_record(
                        &self.store,
                        &self.prefix,
                        keyspace,
                        key_id.as_str(),
                    )
                    .await?;
                }
            }
        }
        Ok(ClientPreauthConsumeOutcome::Granted(grant))
    }

    async fn list_client_preauth_keys(&self) -> anyhow::Result<Vec<ClientPreauthKey>> {
        preauth_layout::list(&self.store, &self.prefix, preauth_layout::CLIENT_KEYSPACE).await
    }

    async fn delete_client_preauth_key(&self, key_id: &ClientPreauthKeyId) -> anyhow::Result<()> {
        preauth_layout::delete_record(
            &self.store,
            &self.prefix,
            preauth_layout::CLIENT_KEYSPACE,
            key_id.as_str(),
        )
        .await
    }

    async fn list_spent_client_preauth_markers(&self) -> anyhow::Result<Vec<ClientPreauthKeyId>> {
        preauth_layout::list_spent(&self.store, &self.prefix, preauth_layout::CLIENT_KEYSPACE).await
    }

    async fn delete_spent_client_preauth_marker(
        &self,
        key_id: &ClientPreauthKeyId,
    ) -> anyhow::Result<()> {
        preauth_layout::delete_spent(
            &self.store,
            &self.prefix,
            preauth_layout::CLIENT_KEYSPACE,
            key_id.as_str(),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    //! The client store, over an in-memory object store.

    use std::sync::Arc;

    use anyhow::Context;
    use object_store::memory::InMemory;
    use object_store::path::Path as ObjPath;
    use object_store::{ObjectStore, ObjectStoreExt};

    use pipette_core::client_permission::ClientPermissionSet;
    use pipette_plan_types::ClientId;
    use pipette_store::ClientStore;

    use crate::auth::S3ClientStore;
    use crate::auth::tests::{client_store, test_client};
    use crate::tests::write_raw_under;
    use crate::{get_bytes, obj_path, put_bytes};

    #[tokio::test]
    async fn s3_client_listing_meets_the_shared_pagination_contract() -> anyhow::Result<()> {
        let (store, clients) = client_store();
        let write = write_raw_under(&store);
        pipette_store_testkit::assert_client_listing_pagination(&clients, |leaf, bytes| {
            write(format!("clients/{leaf}"), bytes)
        })
        .await
    }

    /// Here the first write wins by conditional create, i.e. `If-None-Match: *`.
    #[tokio::test]
    async fn s3_signature_migration_keeps_the_first_sighting() -> anyhow::Result<()> {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let clients = S3ClientStore::new(store.clone(), "test-prefix".to_string());
        pipette_store_testkit::assert_signature_migration_keeps_first_sighting(
            &clients,
            write_raw_under(&store),
        )
        .await
    }

    #[tokio::test]
    async fn s3_client_store_list_clients_self_heals_legacy_records() -> anyhow::Result<()> {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let clients = S3ClientStore::new(store.clone(), "test-prefix".to_string());

        clients
            .put_client(&test_client(
                "ev1_valid",
                "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
                1_767_225_600,
            )?)
            .await?;

        put_bytes(
            &store,
            &obj_path("test-prefix", "clients/ev1_bad.json"),
            br#"{
      "client_id": "ev1_bad",
      "public_key": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
      "client_details": "",
      "contact_email": "bad@example.com",
      "status": "pending",
      "registered_at": "2026-01-01T00:00:00Z"
    }"#
            .to_vec(),
        )
        .await?;

        let listed = pipette_store::drain_clients(&clients).await?.items;

        assert_eq!(listed.len(), 2);

        let repaired = get_bytes(&store, &obj_path("test-prefix", "clients/ev1_bad.json"))
            .await?
            .context("expected repaired record")?;
        let repaired: serde_json::Value = serde_json::from_slice(&repaired)?;
        assert_eq!(repaired["organization"], "<unset>");
        assert_eq!(repaired["client_details"], "legacy client");
        Ok(())
    }

    #[tokio::test]
    async fn s3_client_store_get_client_self_heals_legacy_record() -> anyhow::Result<()> {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let clients = S3ClientStore::new(store.clone(), "test-prefix".to_string());
        let path = obj_path("test-prefix", "clients/ev1_bad.json");

        put_bytes(
            &store,
            &path,
            br#"{
      "client_id": "ev1_bad",
      "public_key": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
      "client_details": "",
      "contact_email": "bad@example.com",
      "status": "pending",
      "registered_at": "2026-01-01T00:00:00Z"
    }"#
            .to_vec(),
        )
        .await?;

        let client = clients
            .get_client(&ClientId::try_new("ev1_bad")?)
            .await?
            .context("expected repaired client")?;
        assert_eq!(client.organization.as_str(), "<unset>");
        assert_eq!(client.client_details.as_str(), "legacy client");

        let repaired = get_bytes(&store, &path)
            .await?
            .context("expected repaired record")?;
        let repaired: serde_json::Value = serde_json::from_slice(&repaired)?;
        assert_eq!(repaired["organization"], "<unset>");
        assert_eq!(repaired["client_details"], "legacy client");
        Ok(())
    }

    #[tokio::test]
    async fn s3_client_replace_is_conditional() -> anyhow::Result<()> {
        let (_store, clients) = client_store();
        pipette_store_testkit::assert_client_replace_is_conditional(
            &clients,
            &test_client(
                "ev1_a",
                "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456700",
                1,
            )?,
        )
        .await
    }

    #[tokio::test]
    async fn s3_client_store_permissions() -> anyhow::Result<()> {
        let (store, clients) = client_store();

        let a = test_client(
            "ev1_a",
            "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456700",
            1,
        )?;
        clients.put_client(&a).await?;

        pipette_store_testkit::assert_client_permission_store_roundtrip(
            &clients,
            &a.client_id,
            write_raw_under(&store),
        )
        .await
    }

    /// Each mutation retries from a fresh read, so the last write holds every
    /// grant; `put_client_permissions` alone would lose one.
    #[tokio::test]
    async fn s3_concurrent_permission_grants_all_land() -> anyhow::Result<()> {
        use pipette_plan_types::ClientPermission;

        let (_store, clients) = client_store();
        let client_id = ClientId::try_new("ev1_contested")?;

        // Seed a cleared object: an absent one resolves to the legacy default,
        // which already holds `submit_results`, so that grant would not race.
        clients
            .put_client_permissions(&client_id, &ClientPermissionSet::default())
            .await?;

        let grants = [
            ClientPermission::PerformWork,
            ClientPermission::SubmitResults,
        ];
        let outcomes = futures::future::join_all(grants.map(|permission| {
            let clients = clients.clone();
            let client_id = client_id.clone();
            async move {
                clients
                    .mutate_client_permissions(&client_id, &move |set| set.insert_all([permission]))
                    .await
            }
        }))
        .await;
        outcomes.into_iter().try_for_each(|outcome| {
            assert!(outcome?.changed);
            anyhow::Ok(())
        })?;

        let resulting = clients
            .get_client_permissions(&client_id)
            .await?
            .context("expected the granted set")?;
        // Stored names, not `holds`: the store never reads the id's kind, so it
        // owes only that every name it accepted is in the object.
        let stored: Vec<&str> = resulting.iter().map(|member| member.wire_name()).collect();
        grants.iter().for_each(|permission| {
            assert!(
                stored.contains(&permission.as_ref()),
                "a grant that reported success must be in the stored set: {permission}"
            );
        });
        Ok(())
    }

    /// Pins the keyspace constant: moving it would strand every record written
    /// before the move.
    #[tokio::test]
    async fn s3_client_store_preauth_keys_use_the_client_preauth_prefix() -> anyhow::Result<()> {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let clients = S3ClientStore::new(store.clone(), "test-prefix".to_string());

        let minted = pipette_core::client_preauth::mint(
            pipette_core::client_preauth::ClientPreauthParams {
                usage: pipette_plan_types::ClientPreauthUsage::SingleUse,
                expires_at: None,
                default_tags: Default::default(),
                default_organization: None,
                default_owner: None,
                default_permissions: None,
                note: None,
            },
            chrono::Utc::now(),
        )?;
        let key_id = minted.key.key_id.clone();
        clients.put_client_preauth_key(&minted.key).await?;

        let record = ObjPath::from(format!("test-prefix/client-preauth/{key_id}.json"));
        assert!(
            store.get(&record).await.is_ok(),
            "the record must land at client-preauth/{key_id}.json"
        );
        assert_eq!(clients.list_client_preauth_keys().await?.len(), 1);

        let (_, secret) = minted
            .token
            .parse_parts()
            .context("the minted token must parse")?;
        assert!(matches!(
            clients.consume_client_preauth_key(&key_id, &secret).await?,
            pipette_core::client_preauth::ClientPreauthConsumeOutcome::Granted(_)
        ));

        let marker = ObjPath::from(format!("test-prefix/client-preauth/{key_id}.spent"));
        assert!(
            store.get(&marker).await.is_ok(),
            "spending must leave the marker beside the record"
        );
        assert_eq!(
            clients.list_spent_client_preauth_markers().await?,
            std::slice::from_ref(&key_id)
        );

        // The spend deleted the record, so a re-use reads as `NotFound`; the
        // marker settles the concurrent case, before the delete lands.
        assert!(matches!(
            clients.consume_client_preauth_key(&key_id, &secret).await?,
            pipette_core::client_preauth::ClientPreauthConsumeOutcome::Rejected(_)
        ));

        clients.delete_spent_client_preauth_marker(&key_id).await?;
        assert!(
            clients
                .list_spent_client_preauth_markers()
                .await?
                .is_empty()
        );
        Ok(())
    }

    #[tokio::test]
    async fn s3_client_store_preauth_contract() -> anyhow::Result<()> {
        let (_store, clients) = client_store();

        pipette_store_testkit::assert_client_preauth_store_roundtrip(&clients).await
    }
}
