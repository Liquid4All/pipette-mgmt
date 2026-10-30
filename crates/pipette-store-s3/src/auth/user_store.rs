//! [`S3UserStore`] — the user keyspaces on an object store: operator accounts,
//! the address claims that keep one mailbox to one account, their permissions
//! and the pre-auth keys that register a machine against them.

use std::sync::Arc;

use async_trait::async_trait;
use object_store::ObjectStore;

use pipette_core::user::{User, UserKey};
use pipette_core::user_permission::UserPermissionSet;
use pipette_core::user_preauth::{
    UserPreauthConsumeOutcome, UserPreauthKey, UserPreauthRejectionError, UserPreauthSecret,
};
use pipette_plan_types::{FirstClerkSignIn, UserId, UserKeyId, UserPreauthKeyId, UserPreauthUsage};
use pipette_store::UserStore;

use super::{preauth_layout, user_layout, user_permission_layout};

/// The user keyspaces under one bucket and prefix. Addresses no client
/// keyspace, so `list_users` cannot return a device by construction.
#[derive(Clone)]
pub struct S3UserStore {
    store: Arc<dyn ObjectStore>,
    prefix: String,
}

impl S3UserStore {
    pub fn new(store: Arc<dyn ObjectStore>, prefix: String) -> Self {
        Self { store, prefix }
    }
}

#[async_trait]
impl UserStore for S3UserStore {
    async fn get_user(&self, user_id: &UserId) -> anyhow::Result<Option<User>> {
        user_layout::load_user(&self.store, &self.prefix, user_id).await
    }

    async fn put_user(&self, user: &User) -> anyhow::Result<()> {
        user_layout::save_user(&self.store, &self.prefix, user).await
    }

    async fn delete_user(&self, user_id: &UserId) -> anyhow::Result<()> {
        user_layout::delete_user(&self.store, &self.prefix, user_id).await
    }

    async fn list_users(&self) -> anyhow::Result<Vec<User>> {
        user_layout::list_users(&self.store, &self.prefix).await
    }

    async fn claim_user_email(
        &self,
        email: &str,
        user_id: &UserId,
    ) -> anyhow::Result<pipette_store::EmailClaim> {
        Ok(
            match user_layout::claim_email(&self.store, &self.prefix, email, user_id).await? {
                user_layout::Claim::Claimed => pipette_store::EmailClaim::Claimed,
                user_layout::Claim::Taken(holder) => pipette_store::EmailClaim::Taken(holder),
            },
        )
    }

    async fn lookup_user_by_email(&self, email: &str) -> anyhow::Result<Option<UserId>> {
        user_layout::lookup_email(&self.store, &self.prefix, email).await
    }

    async fn release_user_email(&self, email: &str) -> anyhow::Result<()> {
        user_layout::release_email(&self.store, &self.prefix, email).await
    }

    async fn put_user_key(&self, user_id: &UserId, key: &UserKey) -> anyhow::Result<()> {
        user_layout::save_key(&self.store, &self.prefix, user_id, key).await
    }

    async fn get_user_key(
        &self,
        user_id: &UserId,
        key_id: &UserKeyId,
    ) -> anyhow::Result<Option<UserKey>> {
        user_layout::load_key(&self.store, &self.prefix, user_id, key_id).await
    }

    async fn list_user_keys(&self, user_id: &UserId) -> anyhow::Result<Vec<UserKey>> {
        user_layout::list_keys(&self.store, &self.prefix, user_id).await
    }

    async fn delete_user_key(&self, user_id: &UserId, key_id: &UserKeyId) -> anyhow::Result<()> {
        user_layout::delete_key(&self.store, &self.prefix, user_id, key_id).await
    }

    async fn get_user_permissions(
        &self,
        user_id: &UserId,
    ) -> anyhow::Result<Option<UserPermissionSet>> {
        user_permission_layout::load(&self.store, &self.prefix, user_id.as_str()).await
    }

    async fn put_user_permissions(
        &self,
        user_id: &UserId,
        set: &UserPermissionSet,
    ) -> anyhow::Result<()> {
        user_permission_layout::save(&self.store, &self.prefix, user_id.as_str(), set).await
    }

    async fn delete_user_permissions(&self, user_id: &UserId) -> anyhow::Result<()> {
        user_permission_layout::delete(&self.store, &self.prefix, user_id.as_str()).await
    }

    async fn mutate_user_permissions(
        &self,
        user_id: &UserId,
        f: &(dyn for<'s> Fn(&'s mut UserPermissionSet) -> bool + Send + Sync),
    ) -> anyhow::Result<pipette_store::UserPermissionMutation> {
        let (changed, resulting) =
            user_permission_layout::mutate(&self.store, &self.prefix, user_id.as_str(), f)
                .await?
                .ok_or_else(|| user_permission_layout::contention_error(user_id.as_str()))?;
        Ok(pipette_store::UserPermissionMutation { changed, resulting })
    }

    async fn put_user_preauth_key(&self, key: &UserPreauthKey) -> anyhow::Result<()> {
        preauth_layout::save(
            &self.store,
            &self.prefix,
            preauth_layout::USER_KEYSPACE,
            key.key_id.as_str(),
            key,
        )
        .await
    }

    async fn consume_user_preauth_key(
        &self,
        key_id: &UserPreauthKeyId,
        secret: &UserPreauthSecret,
    ) -> anyhow::Result<UserPreauthConsumeOutcome> {
        let keyspace = preauth_layout::USER_KEYSPACE;
        let Some(key): Option<UserPreauthKey> =
            preauth_layout::load(&self.store, &self.prefix, keyspace, key_id.as_str()).await?
        else {
            return Ok(UserPreauthConsumeOutcome::Rejected(
                UserPreauthRejectionError::NotFound,
            ));
        };
        let now = chrono::Utc::now();
        let grant = match pipette_core::user_preauth::validate(&key, secret, now) {
            Ok(grant) => grant,
            Err(rejection) => return Ok(UserPreauthConsumeOutcome::Rejected(rejection)),
        };
        if matches!(key.usage, UserPreauthUsage::SingleUse) {
            match preauth_layout::claim_spent(
                &self.store,
                &self.prefix,
                keyspace,
                key_id.as_str(),
                now,
            )
            .await?
            {
                preauth_layout::Spend::AlreadySpent => {
                    return Ok(UserPreauthConsumeOutcome::Rejected(
                        UserPreauthRejectionError::NotFound,
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
        Ok(UserPreauthConsumeOutcome::Granted(grant))
    }
    async fn list_user_preauth_keys(&self) -> anyhow::Result<Vec<UserPreauthKey>> {
        preauth_layout::list(&self.store, &self.prefix, preauth_layout::USER_KEYSPACE).await
    }

    async fn delete_user_preauth_key(&self, key_id: &UserPreauthKeyId) -> anyhow::Result<()> {
        preauth_layout::delete_record(
            &self.store,
            &self.prefix,
            preauth_layout::USER_KEYSPACE,
            key_id.as_str(),
        )
        .await
    }

    async fn list_spent_user_preauth_markers(&self) -> anyhow::Result<Vec<UserPreauthKeyId>> {
        preauth_layout::list_spent(&self.store, &self.prefix, preauth_layout::USER_KEYSPACE).await
    }

    async fn delete_spent_user_preauth_marker(
        &self,
        key_id: &UserPreauthKeyId,
    ) -> anyhow::Result<()> {
        preauth_layout::delete_spent(
            &self.store,
            &self.prefix,
            preauth_layout::USER_KEYSPACE,
            key_id.as_str(),
        )
        .await
    }

    async fn mark_user_awaiting_sign_in(&self, user_id: &UserId) -> anyhow::Result<()> {
        user_layout::mark_awaiting(&self.store, &self.prefix, user_id).await
    }

    async fn user_awaits_sign_in(&self, user_id: &UserId) -> anyhow::Result<bool> {
        user_layout::awaits(&self.store, &self.prefix, user_id).await
    }

    async fn clear_user_awaiting_sign_in(&self, user_id: &UserId) -> anyhow::Result<()> {
        user_layout::clear_awaiting(&self.store, &self.prefix, user_id).await
    }

    async fn record_user_first_clerk_sign_in(
        &self,
        user_id: &UserId,
        first: &FirstClerkSignIn,
    ) -> anyhow::Result<bool> {
        user_layout::record_first_clerk_sign_in(&self.store, &self.prefix, user_id, first).await
    }

    async fn get_user_first_clerk_sign_in(
        &self,
        user_id: &UserId,
    ) -> anyhow::Result<Option<FirstClerkSignIn>> {
        user_layout::load_first_clerk_sign_in(&self.store, &self.prefix, user_id).await
    }

    async fn delete_user_first_clerk_sign_in(&self, user_id: &UserId) -> anyhow::Result<()> {
        user_layout::delete_first_clerk_sign_in(&self.store, &self.prefix, user_id).await
    }
}

#[cfg(test)]
mod tests {
    //! The user store, over an in-memory object store.

    use std::sync::Arc;

    use anyhow::Context;
    use object_store::memory::InMemory;
    use object_store::path::Path as ObjPath;
    use object_store::{ObjectStore, ObjectStoreExt};

    use pipette_core::types::Mint;
    use pipette_plan_types::UserId;
    use pipette_store::UserStore;

    use crate::auth::S3UserStore;
    use crate::auth::tests::user_store;
    use crate::get_bytes;

    #[tokio::test]
    async fn s3_user_store_permissions() -> anyhow::Result<()> {
        let (_store, users) = user_store();

        pipette_store_testkit::assert_user_permission_store_roundtrip(&users, &UserId::mint()).await
    }

    #[tokio::test]
    async fn s3_user_account_roundtrip() -> anyhow::Result<()> {
        let (_store, users) = user_store();

        pipette_store_testkit::assert_user_account_store_roundtrip(&users).await
    }

    #[tokio::test]
    async fn s3_user_awaiting_sign_in() -> anyhow::Result<()> {
        let (_store, users) = user_store();

        pipette_store_testkit::assert_user_awaiting_sign_in_roundtrip(&users).await
    }

    #[tokio::test]
    async fn s3_user_first_clerk_sign_in() -> anyhow::Result<()> {
        let (_store, users) = user_store();

        pipette_store_testkit::assert_user_first_clerk_sign_in_roundtrip(&users).await
    }

    /// Pins the keys an account lands at, and that the claim's body is a bare
    /// id rather than JSON.
    #[tokio::test]
    async fn s3_user_account_keys() -> anyhow::Result<()> {
        let (store, users) = user_store();

        let email = pipette_plan_types::ContactEmail::try_new("Operator@Example.com")?;
        let user = pipette_core::user::User::try_new(email, chrono::Utc::now())?;
        let user_id = user.user_id.clone();
        users
            .claim_user_email(user.contact_email.as_str(), &user_id)
            .await?;
        users.put_user(&user).await?;

        assert!(
            store
                .get(&ObjPath::from(format!("test-prefix/users/{user_id}.json")))
                .await
                .is_ok(),
            "the record must land at users/{user_id}.json"
        );
        // Folded by `User::try_new`, so one mailbox is one key.
        let claim = get_bytes(
            &store,
            &ObjPath::from("test-prefix/user-email-index/operator@example.com"),
        )
        .await?
        .context("the claim must be keyed by the folded address")?;
        assert_eq!(
            String::from_utf8(claim.to_vec())?,
            user_id.as_str(),
            "the claim body is the bare id"
        );
        Ok(())
    }

    /// No seeding, unlike the client race: a user's absent object grants
    /// nothing, so both mutations start from a set neither name is in.
    #[tokio::test]
    async fn s3_concurrent_user_permission_grants_all_land() -> anyhow::Result<()> {
        use pipette_plan_types::UserPermission;

        let (_store, users) = user_store();
        let user_id = UserId::mint();

        let grants = [UserPermission::SubmitPlans, UserPermission::CancelPlans];
        let outcomes = futures::future::join_all(grants.map(|permission| {
            let users = users.clone();
            let user_id = user_id.clone();
            async move {
                users
                    .mutate_user_permissions(&user_id, &move |set| set.insert_all([permission]))
                    .await
            }
        }))
        .await;
        outcomes
            .into_iter()
            .try_for_each(|outcome| -> anyhow::Result<()> {
                assert!(outcome?.changed);
                Ok(())
            })?;

        let resulting = users
            .get_user_permissions(&user_id)
            .await?
            .context("expected the granted set")?;
        let stored: Vec<&str> = resulting.iter().map(|member| member.wire_name()).collect();
        grants.iter().for_each(|permission| {
            assert!(
                stored.contains(&permission.as_ref()),
                "a grant that reported success must be in the stored set: {permission}"
            );
        });
        Ok(())
    }

    /// A user's record carries the account, so the grant is checked for it:
    /// binding a machine to the wrong account is what this key prevents.
    #[tokio::test]
    async fn s3_user_store_preauth_keys_use_the_user_preauth_prefix() -> anyhow::Result<()> {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let users = S3UserStore::new(store.clone(), "test-prefix".to_string());

        let user_id = UserId::mint();
        let minted = pipette_core::user_preauth::mint(
            pipette_core::user_preauth::UserPreauthParams {
                user_id: user_id.clone(),
                usage: pipette_plan_types::UserPreauthUsage::SingleUse,
                expires_at: None,
                note: None,
            },
            chrono::Utc::now(),
        )?;
        let key_id = minted.key.key_id.clone();
        users.put_user_preauth_key(&minted.key).await?;

        let record = ObjPath::from(format!("test-prefix/user-preauth/{key_id}.json"));
        assert!(
            store.get(&record).await.is_ok(),
            "the record must land at user-preauth/{key_id}.json"
        );
        assert_eq!(users.list_user_preauth_keys().await?.len(), 1);

        let (_, secret) = minted
            .token
            .parse_parts()
            .context("the minted token must parse")?;
        let outcome = users.consume_user_preauth_key(&key_id, &secret).await?;
        let pipette_core::user_preauth::UserPreauthConsumeOutcome::Granted(grant) = outcome else {
            anyhow::bail!("a valid key must grant: {outcome:?}");
        };
        assert_eq!(
            grant.user_id, user_id,
            "the grant must name the account the key was minted against"
        );

        let marker = ObjPath::from(format!("test-prefix/user-preauth/{key_id}.spent"));
        assert!(
            store.get(&marker).await.is_ok(),
            "spending must leave the marker beside the record"
        );
        assert_eq!(
            users.list_spent_user_preauth_markers().await?,
            std::slice::from_ref(&key_id)
        );

        // The spend deleted the record, so a re-use reads as `NotFound`.
        assert!(matches!(
            users.consume_user_preauth_key(&key_id, &secret).await?,
            pipette_core::user_preauth::UserPreauthConsumeOutcome::Rejected(_)
        ));

        users.delete_spent_user_preauth_marker(&key_id).await?;
        assert!(users.list_spent_user_preauth_markers().await?.is_empty());
        Ok(())
    }
}
