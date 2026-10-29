//! [`LocalFsClientStore`] — the client trees on a filesystem: device records,
//! their permissions and the pre-auth keys that register them.

use std::num::NonZeroUsize;
use std::path::PathBuf;

use async_trait::async_trait;
use chrono::Utc;

use pipette_core::client::Client;
use pipette_core::client_permission::ClientPermissionSet;
use pipette_core::client_preauth::{
    ClientPreauthConsumeOutcome, ClientPreauthKey, ClientPreauthRejectionError,
    ClientPreauthSecret, validate,
};
use pipette_plan_types::{ClientId, ClientPreauthKeyId, ClientPreauthUsage};
use pipette_store::{ClientPage, ClientStore};

use super::{client_layout, client_permission_layout, preauth_layout, run_blocking};

/// The client trees under the `[auth_storage]` root. Holds no user directory,
/// so it cannot address an account's records.
#[derive(Clone)]
pub struct LocalFsClientStore {
    clients_dir: PathBuf,
    client_preauth_dir: PathBuf,
    signature_migration_dir: PathBuf,
    client_permissions_dir: PathBuf,
}

impl LocalFsClientStore {
    pub fn new(root: PathBuf) -> Self {
        Self {
            clients_dir: root.join("clients"),
            client_preauth_dir: root.join("client-preauth"),
            signature_migration_dir: root.join("signature-migration"),
            client_permissions_dir: root.join("client-permissions"),
        }
    }
}

#[async_trait]
impl ClientStore for LocalFsClientStore {
    async fn get_client(&self, client_id: &ClientId) -> anyhow::Result<Option<Client>> {
        let clients_dir = self.clients_dir.clone();
        let client_id = client_id.clone();
        run_blocking(move || client_layout::load_client(&clients_dir, &client_id)).await
    }

    async fn put_client(&self, client: &Client) -> anyhow::Result<()> {
        let clients_dir = self.clients_dir.clone();
        let client = client.clone();
        run_blocking(move || client_layout::save_client(&clients_dir, &client)).await
    }

    async fn replace_client(&self, read: &Client, updated: &Client) -> anyhow::Result<bool> {
        let clients_dir = self.clients_dir.clone();
        let (read, updated) = (read.clone(), updated.clone());
        run_blocking(move || client_layout::replace_client(&clients_dir, &read, &updated)).await
    }

    async fn delete_client(&self, client_id: &ClientId) -> anyhow::Result<()> {
        let clients_dir = self.clients_dir.clone();
        let permissions_dir = self.client_permissions_dir.clone();
        let client_id = client_id.clone();
        run_blocking(move || {
            client_layout::delete_client(&clients_dir, &client_id)?;
            client_permission_layout::delete(&permissions_dir, client_id.as_str())
        })
        .await
    }

    async fn list_clients(
        &self,
        after: Option<&ClientId>,
        limit: NonZeroUsize,
    ) -> anyhow::Result<ClientPage> {
        let clients_dir = self.clients_dir.clone();
        let after = after.cloned();
        run_blocking(move || client_layout::list_page(&clients_dir, after.as_ref(), limit)).await
    }

    async fn list_client_ids(&self) -> anyhow::Result<Vec<ClientId>> {
        let clients_dir = self.clients_dir.clone();
        run_blocking(move || client_layout::list_ids(&clients_dir)).await
    }

    async fn has_public_key(
        &self,
        public_key: &pipette_plan_types::PublicKeyHex,
    ) -> anyhow::Result<bool> {
        let clients_dir = self.clients_dir.clone();
        let public_key = public_key.clone();
        run_blocking(move || client_layout::find_by_public_key(&clients_dir, &public_key)).await
    }

    async fn has_signature_migration(&self, client_id: &ClientId) -> anyhow::Result<bool> {
        let dir = self.signature_migration_dir.clone();
        let client_id = client_id.clone();
        run_blocking(move || client_layout::has_signature_migration(&dir, &client_id)).await
    }

    async fn record_signature_migration(
        &self,
        client_id: &ClientId,
        at: chrono::DateTime<chrono::Utc>,
    ) -> anyhow::Result<pipette_core::client::MigrationRecord> {
        let dir = self.signature_migration_dir.clone();
        let client_id = client_id.clone();
        run_blocking(move || client_layout::record_signature_migration(&dir, &client_id, at)).await
    }

    async fn list_signature_migrations(
        &self,
    ) -> anyhow::Result<Vec<(ClientId, pipette_core::client::SignatureMigration)>> {
        let dir = self.signature_migration_dir.clone();
        run_blocking(move || client_layout::list_signature_migrations(&dir)).await
    }

    async fn get_client_permissions(
        &self,
        client_id: &ClientId,
    ) -> anyhow::Result<Option<ClientPermissionSet>> {
        let dir = self.client_permissions_dir.clone();
        let client_id = client_id.clone();
        run_blocking(move || client_permission_layout::load(&dir, client_id.as_str())).await
    }

    async fn list_client_permissions(
        &self,
    ) -> anyhow::Result<Vec<(ClientId, ClientPermissionSet)>> {
        let dir = self.client_permissions_dir.clone();
        run_blocking(move || client_permission_layout::list_all(&dir)).await
    }

    async fn put_client_permissions(
        &self,
        client_id: &ClientId,
        set: &ClientPermissionSet,
    ) -> anyhow::Result<()> {
        let dir = self.client_permissions_dir.clone();
        let client_id = client_id.clone();
        let set = set.clone();
        run_blocking(move || client_permission_layout::save(&dir, client_id.as_str(), set)).await
    }

    async fn delete_client_permissions(&self, client_id: &ClientId) -> anyhow::Result<()> {
        let dir = self.client_permissions_dir.clone();
        let client_id = client_id.clone();
        run_blocking(move || client_permission_layout::delete(&dir, client_id.as_str())).await
    }

    /// The layout's lost-update race is accepted: an operator is the only writer
    /// after registration, and can read the set back.
    async fn mutate_client_permissions(
        &self,
        client_id: &ClientId,
        f: &(dyn for<'s> Fn(&'s mut ClientPermissionSet) -> bool + Send + Sync),
    ) -> anyhow::Result<pipette_store::ClientPermissionMutation> {
        let (changed, resulting) =
            client_permission_layout::mutate(&self.client_permissions_dir, client_id.as_str(), f)
                .await?;
        Ok(pipette_store::ClientPermissionMutation { changed, resulting })
    }

    async fn put_client_preauth_key(&self, key: &ClientPreauthKey) -> anyhow::Result<()> {
        let dir = self.client_preauth_dir.clone();
        let key = key.clone();
        run_blocking(move || preauth_layout::save(&dir, key.key_id.as_str(), &key)).await
    }

    async fn get_client_preauth_key(
        &self,
        key_id: &ClientPreauthKeyId,
    ) -> anyhow::Result<Option<ClientPreauthKey>> {
        let dir = self.client_preauth_dir.clone();
        let key_id = key_id.clone();
        run_blocking(move || preauth_layout::load(&dir, key_id.as_str())).await
    }

    async fn consume_client_preauth_key(
        &self,
        key_id: &ClientPreauthKeyId,
        secret: &ClientPreauthSecret,
    ) -> anyhow::Result<ClientPreauthConsumeOutcome> {
        let dir = self.client_preauth_dir.clone();
        let key_id = key_id.clone();
        let secret = secret.clone();
        run_blocking(move || {
            let Some(key): Option<ClientPreauthKey> = preauth_layout::load(&dir, key_id.as_str())?
            else {
                return Ok(ClientPreauthConsumeOutcome::Rejected(
                    ClientPreauthRejectionError::NotFound,
                ));
            };
            let now = Utc::now();
            let grant = match validate(&key, &secret, now) {
                Ok(grant) => grant,
                Err(rejection) => return Ok(ClientPreauthConsumeOutcome::Rejected(rejection)),
            };
            if matches!(key.usage, ClientPreauthUsage::SingleUse) {
                match preauth_layout::claim_spent(&dir, key_id.as_str(), now)? {
                    // Reported as unknown, like every other reason a key will
                    // not grant.
                    preauth_layout::Spend::AlreadySpent => {
                        return Ok(ClientPreauthConsumeOutcome::Rejected(
                            ClientPreauthRejectionError::NotFound,
                        ));
                    }
                    preauth_layout::Spend::Claimed => {
                        preauth_layout::delete_record(&dir, key_id.as_str())?;
                    }
                }
            }
            Ok(ClientPreauthConsumeOutcome::Granted(grant))
        })
        .await
    }

    async fn list_client_preauth_keys(&self) -> anyhow::Result<Vec<ClientPreauthKey>> {
        let dir = self.client_preauth_dir.clone();
        run_blocking(move || preauth_layout::list(&dir)).await
    }

    async fn delete_client_preauth_key(&self, key_id: &ClientPreauthKeyId) -> anyhow::Result<()> {
        let dir = self.client_preauth_dir.clone();
        let key_id = key_id.clone();
        run_blocking(move || preauth_layout::delete_record(&dir, key_id.as_str())).await
    }

    async fn list_spent_client_preauth_markers(&self) -> anyhow::Result<Vec<ClientPreauthKeyId>> {
        let dir = self.client_preauth_dir.clone();
        run_blocking(move || preauth_layout::list_spent(&dir)).await
    }

    async fn delete_spent_client_preauth_marker(
        &self,
        key_id: &ClientPreauthKeyId,
    ) -> anyhow::Result<()> {
        let dir = self.client_preauth_dir.clone();
        let key_id = key_id.clone();
        run_blocking(move || preauth_layout::delete_spent(&dir, key_id.as_str())).await
    }
}

#[cfg(test)]
mod tests {
    //! The client store, over a tempdir.

    use anyhow::Context;
    use chrono::Utc;

    use pipette_core::client::Client;
    use pipette_core::client_permission::ClientPermissionSet;
    use pipette_core::types::DeriveFromKey;
    use pipette_plan_types::ClientId;
    use pipette_store::ClientStore;

    use super::LocalFsClientStore;
    use crate::auth::tests::{auth_test_client, client_store};
    use crate::tests::write_raw_under;

    #[tokio::test]
    async fn test_localfs_client_listing_meets_the_shared_pagination_contract() -> anyhow::Result<()>
    {
        let dir = tempfile::tempdir()?;
        let write = write_raw_under(dir.path());
        pipette_store_testkit::assert_client_listing_pagination(
            &client_store(&dir)?,
            |leaf, bytes| write(format!("clients/{leaf}"), bytes),
        )
        .await
    }

    /// A record that vanishes between the listing and the read is skipped.
    /// Only this backend can express the race deterministically: a dangling
    /// symlink lists like a record and reads as `NotFound`.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_localfs_a_client_listing_skips_a_vanished_record() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = client_store(&dir)?;
        store
            .put_client(&auth_test_client("ev1_live", "aa", "org")?)
            .await?;
        std::os::unix::fs::symlink(
            dir.path().join("clients").join("nowhere.json"),
            dir.path().join("clients").join("ev1_ghost.json"),
        )?;

        let page = pipette_store::drain_clients(&store).await?;

        assert_eq!(page.items.len(), 1, "the live record still comes back");
        assert!(
            page.unreadable.is_empty(),
            "a vanished record is not an unreadable one — we never saw its contents"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_localfs_client_store_crud() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::create_dir_all(dir.path().join("clients"))?;

        let store = LocalFsClientStore::new(dir.path().to_path_buf());

        let public_key = pipette_plan_types::PublicKeyHex::try_new(
            "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
        )?;
        let client = Client {
            client_id: ClientId::derive(&public_key),
            public_key: public_key.clone(),
            organization: pipette_plan_types::NonEmptyTrimmedString::try_new("test-org")?,
            client_details: pipette_plan_types::NonEmptyTrimmedString::try_new("details")?,
            contact_email: pipette_plan_types::ContactEmail::try_new("test@example.com")?,
            status: pipette_core::client::ClientStatus::Pending,
            registered_at: Utc::now(),
            device_profile: Default::default(),
            capabilities: Default::default(),
        };

        store.put_client(&client).await?;
        let loaded = store
            .get_client(&client.client_id)
            .await?
            .context("expected client")?;
        assert_eq!(loaded.client_id, client.client_id);
        assert_eq!(pipette_store::drain_clients(&store).await?.items.len(), 1);
        assert!(store.has_public_key(&public_key).await?);

        store.delete_client(&client.client_id).await?;
        assert!(store.get_client(&client.client_id).await?.is_none());

        Ok(())
    }

    #[tokio::test]
    async fn test_localfs_client_replace_is_conditional() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        pipette_store_testkit::assert_client_replace_is_conditional(
            &client_store(&dir)?,
            &auth_test_client("ev1_a", "00", "acme")?,
        )
        .await
    }

    /// Writers racing from one read: the record's lock lets exactly one land.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_localfs_racing_replaces_land_once() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = client_store(&dir)?;
        let read = auth_test_client("ev1_a", "00", "acme")?;
        store.put_client(&read).await?;
        let racers = (0..16).map(|n| {
            let (store, read) = (store.clone(), read.clone());
            tokio::spawn(async move {
                let updated = Client {
                    client_details: pipette_plan_types::NonEmptyTrimmedString::try_new(format!(
                        "edit {n}"
                    ))?,
                    ..read.clone()
                };
                store.replace_client(&read, &updated).await
            })
        });
        let landed = futures::future::try_join_all(racers)
            .await?
            .into_iter()
            .collect::<anyhow::Result<Vec<bool>>>()?
            .into_iter()
            .filter(|landed| *landed)
            .count();
        assert_eq!(landed, 1);
        Ok(())
    }

    #[tokio::test]
    async fn test_localfs_client_store_permissions() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = client_store(&dir)?;

        let a = auth_test_client("ev1_a", "00", "acme")?;
        store.put_client(&a).await?;

        pipette_store_testkit::assert_client_permission_store_roundtrip(
            &store,
            &a.client_id,
            write_raw_under(dir.path()),
        )
        .await
    }

    /// An interrupted write's staged file is neither read as a set nor in the
    /// way of the next write.
    #[tokio::test]
    async fn test_localfs_a_stranded_staged_permission_set_is_not_a_set() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = LocalFsClientStore::new(dir.path().to_path_buf());
        let client_id = ClientId::try_new("ev1_interrupted")?;

        let permissions = dir.path().join("client-permissions");
        std::fs::create_dir_all(&permissions)?;
        std::fs::write(
            permissions.join(format!(
                "{client_id}.00000000-0000-4000-8000-000000000000.staged"
            )),
            serde_json::to_vec(&ClientPermissionSet::from_iter([
                pipette_plan_types::ClientPermission::PerformWork,
            ]))?,
        )?;

        assert!(store.get_client_permissions(&client_id).await?.is_none());
        let set =
            ClientPermissionSet::from_iter([pipette_plan_types::ClientPermission::SubmitResults]);
        store.put_client_permissions(&client_id, &set).await?;
        assert_eq!(store.get_client_permissions(&client_id).await?, Some(set));
        Ok(())
    }

    /// Asserted over the bytes on disk, not a round trip, which would pass just
    /// as well if both ends dropped the `{"permissions": [...]}` key together.
    #[tokio::test]
    async fn test_localfs_permissions_keep_the_stored_envelope() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = LocalFsClientStore::new(dir.path().to_path_buf());
        let client_id = ClientId::try_new("ev1_envelope")?;
        let path = dir
            .path()
            .join("client-permissions")
            .join(format!("{client_id}.json"));

        store
            .put_client_permissions(
                &client_id,
                &ClientPermissionSet::from_iter([
                    pipette_plan_types::ClientPermission::PerformWork,
                ]),
            )
            .await?;
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&std::fs::read(&path)?)?,
            serde_json::json!({ "permissions": ["perform_work"] }),
            "a write must produce the envelope an older binary reads"
        );

        // A hand-written envelope reads back as the set it names.
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({ "permissions": ["submit_results"] }))?,
        )?;
        assert_eq!(
            store.get_client_permissions(&client_id).await?,
            Some(ClientPermissionSet::from_iter([
                pipette_plan_types::ClientPermission::SubmitResults
            ])),
        );
        Ok(())
    }

    /// Here the first write wins by `hard_link` refusing an occupied path.
    #[tokio::test]
    async fn test_localfs_signature_migration_keeps_the_first_sighting() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = LocalFsClientStore::new(dir.path().to_path_buf());
        pipette_store_testkit::assert_signature_migration_keeps_first_sighting(
            &store,
            write_raw_under(dir.path()),
        )
        .await
    }

    /// A staged file stranded before its link neither appears as a migration nor
    /// blocks the client's next attempt from recording one.
    #[tokio::test]
    async fn test_localfs_a_stranded_staged_record_is_not_a_migration() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = LocalFsClientStore::new(dir.path().to_path_buf());
        let client_id = ClientId::try_new("ev1_interrupted")?;
        let at = Utc::now();

        let markers = dir.path().join("signature-migration");
        std::fs::create_dir_all(&markers)?;
        std::fs::write(
            markers.join(format!(
                "{client_id}.00000000-0000-4000-8000-000000000000.staged"
            )),
            serde_json::to_vec(&pipette_core::client::SignatureMigration { first_seen: at })?,
        )?;

        assert!(!store.has_signature_migration(&client_id).await?);
        assert!(store.list_signature_migrations().await?.is_empty());
        assert_eq!(
            store.record_signature_migration(&client_id, at).await?,
            pipette_core::client::MigrationRecord::First
        );
        assert_eq!(store.list_signature_migrations().await?.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn test_localfs_signature_migrations_absent_tree_is_empty() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = LocalFsClientStore::new(dir.path().to_path_buf());
        assert!(store.list_signature_migrations().await?.is_empty());
        Ok(())
    }

    /// Repairs every drift the non-atomic double-write can leave, and a second
    /// run is a no-op. Records and markers sit under two roots, as with
    /// `[auth_storage]` and `[storage]`.
    #[tokio::test]
    async fn test_reindex_tags_repairs_drift() -> anyhow::Result<()> {
        use pipette_plan_types::Tag;
        use pipette_store::TagStore;

        let auth = tempfile::tempdir()?;
        let data = tempfile::tempdir()?;
        std::fs::create_dir_all(auth.path().join("clients"))?;
        let store = LocalFsClientStore::new(auth.path().to_path_buf());
        let tags = crate::LocalFsTagStore::new(data.path().to_path_buf());

        let a = auth_test_client("ev1_a", "00", "acme")?;
        store.put_client(&a).await?;
        let team = Tag::try_new("team-mobile")?;
        let east = Tag::try_new("us-east")?;
        tags.add_client_tag(&a.client_id, &team).await?; // consistent in both trees

        let by_client = data.path().join("tags-index/by-client");
        let by_tag = data.path().join("tags-index/by-tag");
        let touch = |p: std::path::PathBuf| -> anyhow::Result<()> {
            std::fs::create_dir_all(p.parent().context("a marker path has a parent")?)?;
            std::fs::write(p, [])?;
            Ok(())
        };

        // Drift 1: forward marker with no reverse (crash after forward write).
        touch(by_client.join(a.client_id.as_str()).join(east.as_str()))?;
        // Drift 2: orphan reverse (crash after forward delete, before reverse).
        touch(by_tag.join("ghost").join(a.client_id.as_str()))?;
        // Drift 3: both markers for a client whose record is gone.
        touch(by_client.join("ev1_ghostclient").join(team.as_str()))?;
        touch(by_tag.join(team.as_str()).join("ev1_ghostclient"))?;

        let report = pipette_store::reindex_tags(&store, &tags).await?;
        assert_eq!(report.added, 1, "east's reverse marker created");
        assert_eq!(report.removed, 2, "orphan reverse + deleted-client pair");

        assert_eq!(
            tags.get_client_tags(&a.client_id).await?,
            std::collections::BTreeSet::from([team.clone(), east.clone()])
        );
        assert_eq!(
            tags.list_client_ids_by_tag(&east).await?,
            vec![a.client_id.clone()]
        );
        assert!(
            tags.list_client_ids_by_tag(&Tag::try_new("ghost")?)
                .await?
                .is_empty()
        );
        assert!(
            tags.get_client_tags(&ClientId::try_new("ev1_ghostclient")?)
                .await?
                .is_empty()
        );

        let again = pipette_store::reindex_tags(&store, &tags).await?;
        assert_eq!((again.added, again.removed), (0, 0));
        Ok(())
    }

    /// An empty roster beside live markers is refused, and drops nothing.
    #[tokio::test]
    async fn test_reindex_tags_refuses_an_empty_roster() -> anyhow::Result<()> {
        use pipette_plan_types::Tag;
        use pipette_store::TagStore;

        let (auth, data) = (tempfile::tempdir()?, tempfile::tempdir()?);
        let store = client_store(&auth)?;
        let tags = crate::LocalFsTagStore::new(data.path().to_path_buf());
        let a = ClientId::try_new("ev1_a")?;
        let team = Tag::try_new("team-mobile")?;
        tags.add_client_tag(&a, &team).await?;

        let refused = pipette_store::reindex_tags(&store, &tags)
            .await
            .err()
            .context("an empty roster must be refused")?;
        assert!(
            refused.to_string().contains("[auth_storage]"),
            "got: {refused}"
        );
        assert_eq!(
            tags.get_client_tags(&a).await?,
            std::collections::BTreeSet::from([team.clone()])
        );
        assert_eq!(tags.list_client_ids_by_tag(&team).await?, vec![a]);
        Ok(())
    }

    /// Every pre-auth path comes from one field, so a typo is self-consistent;
    /// only an assertion on the literal path catches a prefix that moved.
    #[tokio::test]
    async fn test_localfs_client_store_preauth_paths() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = LocalFsClientStore::new(dir.path().to_path_buf());

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
            Utc::now(),
        )?;
        let key_id = minted.key.key_id.clone();
        store.put_client_preauth_key(&minted.key).await?;
        assert!(
            dir.path()
                .join(format!("client-preauth/{key_id}.json"))
                .exists(),
            "the record must land under client-preauth/"
        );

        let (parsed_id, secret) = minted
            .token
            .parse_parts()
            .context("the minted token must parse")?;
        assert_eq!(parsed_id, key_id);
        assert!(matches!(
            store.consume_client_preauth_key(&key_id, &secret).await?,
            pipette_core::client_preauth::ClientPreauthConsumeOutcome::Granted(_)
        ));
        assert!(
            dir.path()
                .join(format!("client-preauth/{key_id}.spent"))
                .exists(),
            "spending must leave the marker beside the record"
        );
        assert!(
            !dir.path()
                .join(format!("client-preauth/{key_id}.json"))
                .exists()
        );
        assert_eq!(store.list_spent_client_preauth_markers().await?, [key_id]);
        Ok(())
    }

    #[tokio::test]
    async fn test_localfs_client_store_preauth_contract() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = LocalFsClientStore::new(dir.path().to_path_buf());

        pipette_store_testkit::assert_client_preauth_store_roundtrip(&store).await
    }
}
