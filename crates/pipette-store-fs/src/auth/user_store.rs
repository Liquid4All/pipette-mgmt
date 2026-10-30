//! [`LocalFsUserStore`] — the user trees on a filesystem: operator accounts,
//! the address claims that keep one mailbox to one account, their permissions
//! and the pre-auth keys that register a machine against them.

use std::path::PathBuf;

use async_trait::async_trait;
use chrono::Utc;

use pipette_core::user::{User, UserKey};
use pipette_core::user_permission::UserPermissionSet;
use pipette_core::user_preauth::{
    UserPreauthConsumeOutcome, UserPreauthKey, UserPreauthRejectionError, UserPreauthSecret,
};
use pipette_plan_types::{FirstClerkSignIn, UserId, UserKeyId, UserPreauthKeyId, UserPreauthUsage};
use pipette_store::UserStore;

use super::{preauth_layout, run_blocking, user_layout, user_permission_layout};

/// The user trees under the `[auth_storage]` root. Holds no client directory,
/// so `list_users` cannot return a device by construction.
#[derive(Clone)]
pub struct LocalFsUserStore {
    users_dir: PathBuf,
    user_email_index_dir: PathBuf,
    user_keys_dir: PathBuf,
    user_preauth_dir: PathBuf,
    /// Not the client's directory, so a client's delete cannot reach an
    /// account's grants.
    user_permissions_dir: PathBuf,
    user_awaiting_sign_in_dir: PathBuf,
    user_first_clerk_sign_in_dir: PathBuf,
}

impl LocalFsUserStore {
    pub fn new(root: PathBuf) -> Self {
        Self {
            users_dir: root.join("users"),
            user_email_index_dir: root.join("user-email-index"),
            user_keys_dir: root.join("user-keys"),
            user_preauth_dir: root.join("user-preauth"),
            user_permissions_dir: root.join("user-permissions"),
            user_awaiting_sign_in_dir: root.join("user-awaiting-sign-in"),
            user_first_clerk_sign_in_dir: root.join("user-first-clerk-sign-in"),
        }
    }
}

#[async_trait]
impl UserStore for LocalFsUserStore {
    async fn get_user(&self, user_id: &UserId) -> anyhow::Result<Option<User>> {
        let dir = self.users_dir.clone();
        let user_id = user_id.clone();
        run_blocking(move || user_layout::load_user(&dir, &user_id)).await
    }

    async fn put_user(&self, user: &User) -> anyhow::Result<()> {
        let dir = self.users_dir.clone();
        let user = user.clone();
        run_blocking(move || user_layout::save_user(&dir, &user)).await
    }

    async fn delete_user(&self, user_id: &UserId) -> anyhow::Result<()> {
        let dir = self.users_dir.clone();
        let user_id = user_id.clone();
        run_blocking(move || user_layout::delete_user(&dir, &user_id)).await
    }

    async fn list_users(&self) -> anyhow::Result<Vec<User>> {
        let dir = self.users_dir.clone();
        run_blocking(move || user_layout::list_users(&dir)).await
    }

    async fn claim_user_email(
        &self,
        email: &str,
        user_id: &UserId,
    ) -> anyhow::Result<pipette_store::EmailClaim> {
        let dir = self.user_email_index_dir.clone();
        let email = email.to_string();
        let user_id = user_id.clone();
        let claim = run_blocking(move || user_layout::claim_email(&dir, &email, &user_id)).await?;
        Ok(match claim {
            user_layout::Claim::Claimed => pipette_store::EmailClaim::Claimed,
            user_layout::Claim::Taken(holder) => pipette_store::EmailClaim::Taken(holder),
        })
    }

    async fn lookup_user_by_email(&self, email: &str) -> anyhow::Result<Option<UserId>> {
        let dir = self.user_email_index_dir.clone();
        let email = email.to_string();
        run_blocking(move || user_layout::lookup_email(&dir, &email)).await
    }

    async fn release_user_email(&self, email: &str) -> anyhow::Result<()> {
        let dir = self.user_email_index_dir.clone();
        let email = email.to_string();
        run_blocking(move || user_layout::release_email(&dir, &email)).await
    }

    async fn put_user_key(&self, user_id: &UserId, key: &UserKey) -> anyhow::Result<()> {
        let dir = self.user_keys_dir.clone();
        let user_id = user_id.clone();
        let key = key.clone();
        run_blocking(move || user_layout::save_key(&dir, &user_id, &key)).await
    }

    async fn get_user_key(
        &self,
        user_id: &UserId,
        key_id: &UserKeyId,
    ) -> anyhow::Result<Option<UserKey>> {
        let dir = self.user_keys_dir.clone();
        let user_id = user_id.clone();
        let key_id = key_id.clone();
        run_blocking(move || user_layout::load_key(&dir, &user_id, &key_id)).await
    }

    async fn list_user_keys(&self, user_id: &UserId) -> anyhow::Result<Vec<UserKey>> {
        let dir = self.user_keys_dir.clone();
        let user_id = user_id.clone();
        run_blocking(move || user_layout::list_keys(&dir, &user_id)).await
    }

    async fn delete_user_key(&self, user_id: &UserId, key_id: &UserKeyId) -> anyhow::Result<()> {
        let dir = self.user_keys_dir.clone();
        let user_id = user_id.clone();
        let key_id = key_id.clone();
        run_blocking(move || user_layout::delete_key(&dir, &user_id, &key_id)).await
    }

    async fn get_user_permissions(
        &self,
        user_id: &UserId,
    ) -> anyhow::Result<Option<UserPermissionSet>> {
        let dir = self.user_permissions_dir.clone();
        let user_id = user_id.clone();
        run_blocking(move || user_permission_layout::load(&dir, user_id.as_str())).await
    }

    async fn put_user_permissions(
        &self,
        user_id: &UserId,
        set: &UserPermissionSet,
    ) -> anyhow::Result<()> {
        let dir = self.user_permissions_dir.clone();
        let user_id = user_id.clone();
        let set = set.clone();
        run_blocking(move || user_permission_layout::save(&dir, user_id.as_str(), set)).await
    }

    async fn delete_user_permissions(&self, user_id: &UserId) -> anyhow::Result<()> {
        let dir = self.user_permissions_dir.clone();
        let user_id = user_id.clone();
        run_blocking(move || user_permission_layout::delete(&dir, user_id.as_str())).await
    }

    async fn mutate_user_permissions(
        &self,
        user_id: &UserId,
        f: &(dyn for<'s> Fn(&'s mut UserPermissionSet) -> bool + Send + Sync),
    ) -> anyhow::Result<pipette_store::UserPermissionMutation> {
        let (changed, resulting) =
            user_permission_layout::mutate(&self.user_permissions_dir, user_id.as_str(), f).await?;
        Ok(pipette_store::UserPermissionMutation { changed, resulting })
    }

    async fn put_user_preauth_key(&self, key: &UserPreauthKey) -> anyhow::Result<()> {
        let dir = self.user_preauth_dir.clone();
        let key = key.clone();
        run_blocking(move || preauth_layout::save(&dir, key.key_id.as_str(), &key)).await
    }

    async fn consume_user_preauth_key(
        &self,
        key_id: &UserPreauthKeyId,
        secret: &UserPreauthSecret,
    ) -> anyhow::Result<UserPreauthConsumeOutcome> {
        let dir = self.user_preauth_dir.clone();
        let key_id = key_id.clone();
        let secret = secret.clone();
        run_blocking(move || {
            let Some(key): Option<UserPreauthKey> = preauth_layout::load(&dir, key_id.as_str())?
            else {
                return Ok(UserPreauthConsumeOutcome::Rejected(
                    UserPreauthRejectionError::NotFound,
                ));
            };
            let now = Utc::now();
            let grant = match pipette_core::user_preauth::validate(&key, &secret, now) {
                Ok(grant) => grant,
                Err(rejection) => return Ok(UserPreauthConsumeOutcome::Rejected(rejection)),
            };
            if matches!(key.usage, UserPreauthUsage::SingleUse) {
                match preauth_layout::claim_spent(&dir, key_id.as_str(), now)? {
                    preauth_layout::Spend::AlreadySpent => {
                        return Ok(UserPreauthConsumeOutcome::Rejected(
                            UserPreauthRejectionError::NotFound,
                        ));
                    }
                    preauth_layout::Spend::Claimed => {
                        preauth_layout::delete_record(&dir, key_id.as_str())?;
                    }
                }
            }
            Ok(UserPreauthConsumeOutcome::Granted(grant))
        })
        .await
    }

    async fn list_user_preauth_keys(&self) -> anyhow::Result<Vec<UserPreauthKey>> {
        let dir = self.user_preauth_dir.clone();
        run_blocking(move || preauth_layout::list(&dir)).await
    }

    async fn delete_user_preauth_key(&self, key_id: &UserPreauthKeyId) -> anyhow::Result<()> {
        let dir = self.user_preauth_dir.clone();
        let key_id = key_id.clone();
        run_blocking(move || preauth_layout::delete_record(&dir, key_id.as_str())).await
    }

    async fn list_spent_user_preauth_markers(&self) -> anyhow::Result<Vec<UserPreauthKeyId>> {
        let dir = self.user_preauth_dir.clone();
        run_blocking(move || preauth_layout::list_spent(&dir)).await
    }

    async fn delete_spent_user_preauth_marker(
        &self,
        key_id: &UserPreauthKeyId,
    ) -> anyhow::Result<()> {
        let dir = self.user_preauth_dir.clone();
        let key_id = key_id.clone();
        run_blocking(move || preauth_layout::delete_spent(&dir, key_id.as_str())).await
    }

    async fn mark_user_awaiting_sign_in(&self, user_id: &UserId) -> anyhow::Result<()> {
        let dir = self.user_awaiting_sign_in_dir.clone();
        let user_id = user_id.clone();
        run_blocking(move || user_layout::mark_awaiting(&dir, &user_id)).await
    }

    async fn user_awaits_sign_in(&self, user_id: &UserId) -> anyhow::Result<bool> {
        let dir = self.user_awaiting_sign_in_dir.clone();
        let user_id = user_id.clone();
        run_blocking(move || user_layout::awaits(&dir, &user_id)).await
    }

    async fn clear_user_awaiting_sign_in(&self, user_id: &UserId) -> anyhow::Result<()> {
        let dir = self.user_awaiting_sign_in_dir.clone();
        let user_id = user_id.clone();
        run_blocking(move || user_layout::clear_awaiting(&dir, &user_id)).await
    }

    async fn record_user_first_clerk_sign_in(
        &self,
        user_id: &UserId,
        first: &FirstClerkSignIn,
    ) -> anyhow::Result<bool> {
        let dir = self.user_first_clerk_sign_in_dir.clone();
        let (user_id, first) = (user_id.clone(), first.clone());
        run_blocking(move || user_layout::record_first_clerk_sign_in(&dir, &user_id, &first)).await
    }

    async fn get_user_first_clerk_sign_in(
        &self,
        user_id: &UserId,
    ) -> anyhow::Result<Option<FirstClerkSignIn>> {
        let dir = self.user_first_clerk_sign_in_dir.clone();
        let user_id = user_id.clone();
        run_blocking(move || user_layout::load_first_clerk_sign_in(&dir, &user_id)).await
    }

    async fn delete_user_first_clerk_sign_in(&self, user_id: &UserId) -> anyhow::Result<()> {
        let dir = self.user_first_clerk_sign_in_dir.clone();
        let user_id = user_id.clone();
        run_blocking(move || user_layout::delete_first_clerk_sign_in(&dir, &user_id)).await
    }
}

#[cfg(test)]
mod tests {
    //! The user store, over a tempdir.

    use anyhow::Context;
    use chrono::Utc;

    use pipette_core::types::Mint;
    use pipette_plan_types::UserId;
    use pipette_store::UserStore;

    use super::LocalFsUserStore;
    use crate::auth::tests::user_store;

    #[tokio::test]
    async fn test_localfs_user_store_permissions() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = user_store(&dir);

        pipette_store_testkit::assert_user_permission_store_roundtrip(&store, &UserId::mint()).await
    }

    /// Asserted over the bytes on disk, not a round trip, which would pass just
    /// as well if both ends dropped the `{"permissions": [...]}` key together.
    #[tokio::test]
    async fn test_localfs_user_permissions_keep_the_stored_envelope() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = user_store(&dir);
        let user_id = UserId::mint();
        let path = dir
            .path()
            .join("user-permissions")
            .join(format!("{user_id}.json"));

        store
            .put_user_permissions(
                &user_id,
                &pipette_core::user_permission::UserPermissionSet::from_iter([
                    pipette_plan_types::UserPermission::SubmitPlans,
                ]),
            )
            .await?;
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&std::fs::read(&path)?)?,
            serde_json::json!({ "permissions": ["submit_plans"] }),
            "a write must produce the envelope an older binary reads"
        );

        // A hand-written envelope reads back as the set it names.
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({ "permissions": ["read_plans"] }))?,
        )?;
        assert_eq!(
            store.get_user_permissions(&user_id).await?,
            Some(pipette_core::user_permission::UserPermissionSet::from_iter(
                [pipette_plan_types::UserPermission::ReadPlans]
            )),
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_localfs_user_account_roundtrip() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = user_store(&dir);

        pipette_store_testkit::assert_user_account_store_roundtrip(&store).await
    }

    #[tokio::test]
    async fn test_localfs_user_awaiting_sign_in() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = user_store(&dir);

        pipette_store_testkit::assert_user_awaiting_sign_in_roundtrip(&store).await
    }

    #[tokio::test]
    async fn test_localfs_user_first_clerk_sign_in() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = user_store(&dir);

        pipette_store_testkit::assert_user_first_clerk_sign_in_roundtrip(&store).await
    }

    /// Each path comes from one field, so a moved directory is self-consistent;
    /// only an assertion on the literal path catches it.
    #[tokio::test]
    async fn test_localfs_user_account_paths() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = user_store(&dir);

        let email = pipette_plan_types::ContactEmail::try_new("Operator@Example.com")?;
        let user = pipette_core::user::User::try_new(email, Utc::now())?;
        let user_id = user.user_id.clone();
        store
            .claim_user_email(user.contact_email.as_str(), &user_id)
            .await?;
        store.put_user(&user).await?;

        assert!(
            dir.path().join(format!("users/{user_id}.json")).exists(),
            "the record must land under users/"
        );
        // Folded by `User::try_new`, so one mailbox is one key.
        let claim = dir.path().join("user-email-index/operator@example.com");
        assert!(
            claim.exists(),
            "the claim must be keyed by the folded address"
        );
        assert_eq!(
            std::fs::read_to_string(&claim)?,
            user_id.as_str(),
            "the claim body is the bare id"
        );
        Ok(())
    }

    /// A user's record carries the account, so the grant is checked for it:
    /// binding a machine to the wrong account is what this key prevents.
    #[tokio::test]
    async fn test_localfs_user_store_preauth_paths() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = LocalFsUserStore::new(dir.path().to_path_buf());

        let user_id = UserId::mint();
        let minted = pipette_core::user_preauth::mint(
            pipette_core::user_preauth::UserPreauthParams {
                user_id: user_id.clone(),
                usage: pipette_plan_types::UserPreauthUsage::SingleUse,
                expires_at: None,
                note: None,
            },
            Utc::now(),
        )?;
        let key_id = minted.key.key_id.clone();
        store.put_user_preauth_key(&minted.key).await?;
        assert!(
            dir.path()
                .join(format!("user-preauth/{key_id}.json"))
                .exists(),
            "the record must land under user-preauth/"
        );
        assert_eq!(store.list_user_preauth_keys().await?.len(), 1);

        let (parsed_id, secret) = minted
            .token
            .parse_parts()
            .context("the minted token must parse")?;
        assert_eq!(parsed_id, key_id);
        let outcome = store.consume_user_preauth_key(&key_id, &secret).await?;
        let pipette_core::user_preauth::UserPreauthConsumeOutcome::Granted(grant) = outcome else {
            anyhow::bail!("a valid key must grant: {outcome:?}");
        };
        assert_eq!(
            grant.user_id, user_id,
            "the grant must name the account the key was minted against"
        );
        assert!(
            dir.path()
                .join(format!("user-preauth/{key_id}.spent"))
                .exists(),
            "spending must leave the marker beside the record"
        );
        assert!(
            !dir.path()
                .join(format!("user-preauth/{key_id}.json"))
                .exists()
        );
        assert_eq!(store.list_spent_user_preauth_markers().await?, [key_id]);
        Ok(())
    }

    /// No marker and the record intact: the branch that would silently spend a
    /// fleet's key if the layout claimed unconditionally.
    #[tokio::test]
    async fn test_localfs_multi_use_user_preauth_key_survives_consumption() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = LocalFsUserStore::new(dir.path().to_path_buf());

        let minted = pipette_core::user_preauth::mint(
            pipette_core::user_preauth::UserPreauthParams {
                user_id: UserId::mint(),
                usage: pipette_plan_types::UserPreauthUsage::MultiUse,
                expires_at: None,
                note: None,
            },
            Utc::now(),
        )?;
        let key_id = minted.key.key_id.clone();
        store.put_user_preauth_key(&minted.key).await?;
        let (_, secret) = minted
            .token
            .parse_parts()
            .context("the minted token must parse")?;

        for attempt in 1..=2 {
            assert!(
                matches!(
                    store.consume_user_preauth_key(&key_id, &secret).await?,
                    pipette_core::user_preauth::UserPreauthConsumeOutcome::Granted(_)
                ),
                "a multi-use key must grant on attempt {attempt}"
            );
        }
        assert!(
            store.list_spent_user_preauth_markers().await?.is_empty(),
            "a multi-use key is never marked spent"
        );
        assert_eq!(store.list_user_preauth_keys().await?.len(), 1);
        Ok(())
    }

    /// The secret is checked before the key's status, so an expired key with the
    /// wrong secret reads as invalid; the store is where that order must hold.
    #[tokio::test]
    async fn test_localfs_user_preauth_rejections() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = LocalFsUserStore::new(dir.path().to_path_buf());

        let mut minted = pipette_core::user_preauth::mint(
            pipette_core::user_preauth::UserPreauthParams {
                user_id: UserId::mint(),
                usage: pipette_plan_types::UserPreauthUsage::SingleUse,
                expires_at: Some(Utc::now() - chrono::Duration::seconds(1)),
                note: None,
            },
            Utc::now(),
        )?;
        let key_id = minted.key.key_id.clone();
        store.put_user_preauth_key(&minted.key).await?;

        assert_eq!(
            store
                .consume_user_preauth_key(
                    &key_id,
                    &pipette_core::user_preauth::UserPreauthSecret::new("wrong-secret")
                )
                .await?,
            pipette_core::user_preauth::UserPreauthConsumeOutcome::Rejected(
                pipette_core::user_preauth::UserPreauthRejectionError::BadSecret
            ),
            "the secret is checked before the expiry, so status never leaks"
        );

        let (_, secret) = minted
            .token
            .parse_parts()
            .context("the minted token must parse")?;
        assert_eq!(
            store.consume_user_preauth_key(&key_id, &secret).await?,
            pipette_core::user_preauth::UserPreauthConsumeOutcome::Rejected(
                pipette_core::user_preauth::UserPreauthRejectionError::Expired
            ),
            "the holder of the secret is told the key expired"
        );
        assert!(
            store.list_spent_user_preauth_markers().await?.is_empty(),
            "a rejected consume spends nothing"
        );

        // A revoked key reads as `NotFound`, like an unknown or pruned one.
        minted.key.expires_at = None;
        store.delete_user_preauth_key(&key_id).await?;
        assert_eq!(
            store.consume_user_preauth_key(&key_id, &secret).await?,
            pipette_core::user_preauth::UserPreauthConsumeOutcome::Rejected(
                pipette_core::user_preauth::UserPreauthRejectionError::NotFound
            )
        );
        assert!(store.list_user_preauth_keys().await?.is_empty());
        Ok(())
    }
}
