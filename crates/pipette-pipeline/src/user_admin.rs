//! The account teardown behind `pipette-mgmt users delete` (nothing over HTTP
//! deletes an account), the one-key revocation the CLI and both revoke routes
//! share, and the account a Clerk sign-in creates.

use anyhow::Context;
use chrono::Utc;

use pipette_core::user::User;
use pipette_core::user_permission::UserPermissionSet;
use pipette_plan_types::{ContactEmail, OrgId, UserId, UserKeyId, UserPermission};
use pipette_store::{EmailClaim, KeyIndexStore, OrgStore, UserStore};

/// What a delete found and freed, for the caller's report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deleted {
    /// The address on the record, or `None` when no record was there.
    pub email: Option<ContactEmail>,
    /// How many registered keys were retired.
    pub keys: usize,
    /// Whether the address claim was released. `false` with an `email` means
    /// the claim names another account and was left alone.
    pub released_claim: bool,
}

/// Delete an account, its keys, claims, permissions and membership. Idempotent:
/// a re-run finishes a partial teardown.
///
/// The record goes first, so every key is inert at once and the rest is
/// best-effort; [`revoke_key`] takes the opposite order on a live account. The
/// address claim is released only while it still names this account.
pub async fn delete(
    users: &dyn UserStore,
    key_index: &dyn KeyIndexStore,
    orgs: &dyn OrgStore,
    user_id: &UserId,
) -> anyhow::Result<Deleted> {
    let email = users
        .get_user(user_id)
        .await?
        .map(|user| user.contact_email);
    // Read before anything is removed: the claims below are addressed by them.
    let key_ids: Vec<_> = users
        .list_user_keys(user_id)
        .await?
        .into_iter()
        .map(|key| key.key_id)
        .collect();

    users.delete_user(user_id).await?;

    futures::future::join_all(key_ids.iter().map(|key_id| async move {
        if let Err(e) = users.delete_user_key(user_id, key_id).await {
            tracing::warn!(%user_id, %key_id, error = %e,
                "failed to delete a user key during account deletion");
        }
        if let Some(digest) = pipette_core::key_index::digest_of_key_id(key_id)
            && let Err(e) = key_index.release_public_key(&digest).await
        {
            tracing::warn!(%user_id, %key_id, error = %e,
                "failed to release a public-key claim during account deletion");
        }
    }))
    .await;

    let released_claim = match &email {
        Some(email)
            if users.lookup_user_by_email(email.as_str()).await? == Some(user_id.clone()) =>
        {
            users.release_user_email(email.as_str()).await?;
            true
        }
        _ => false,
    };
    users.delete_user_permissions(user_id).await?;
    orgs.delete_membership(user_id).await?;
    users.clear_user_awaiting_sign_in(user_id).await?;
    users.delete_user_first_clerk_sign_in(user_id).await?;

    tracing::info!(%user_id, keys = key_ids.len(), released_claim, "user deleted");
    Ok(Deleted {
        email,
        keys: key_ids.len(),
        released_claim,
    })
}

/// Retire one of `user_id`'s keys, or `false` when it holds none. Keys are
/// stored under their account, so this lookup is the binding check.
pub async fn revoke_key(
    users: &dyn UserStore,
    key_index: &dyn KeyIndexStore,
    user_id: &UserId,
    key_id: &UserKeyId,
) -> anyhow::Result<bool> {
    if users
        .get_user_key(user_id, key_id)
        .await
        .context("failed to load the user key")?
        .is_none()
    {
        return Ok(false);
    }

    // Registration first, then the claim: authentication reads only the
    // registration, so the other order leaves a revoked key that still signs.
    users
        .delete_user_key(user_id, key_id)
        .await
        .context("failed to delete the user key")?;

    // Best-effort, once the key is inert: a stranded claim retires one keypair,
    // which is not worth failing a revocation that already took effect.
    if let Some(digest) = pipette_core::key_index::digest_of_key_id(key_id)
        && let Err(e) = key_index.release_public_key(&digest).await
    {
        tracing::warn!(%user_id, %key_id, error = %e,
            "failed to release the public-key claim after revoking a key");
    }
    Ok(true)
}

/// What [`provision`] found at the address.
#[derive(Debug)]
pub enum Provisioned {
    Created(User),
    /// The address was already claimed, by this account.
    Taken(UserId),
}

/// Create an active account for `email` in `org_id`, for a Clerk sign-in. The
/// record is written last, so an existing account is complete.
pub async fn provision(
    users: &dyn UserStore,
    orgs: &dyn OrgStore,
    email: ContactEmail,
    org_id: &OrgId,
    permissions: &[UserPermission],
) -> anyhow::Result<Provisioned> {
    let user = User::try_new(email, Utc::now())?;
    if let EmailClaim::Taken(holder) = users
        .claim_user_email(user.contact_email.as_str(), &user.user_id)
        .await?
    {
        return Ok(Provisioned::Taken(holder));
    }

    let set = UserPermissionSet::from_iter(permissions.iter().copied()).normalized();
    let written = async {
        orgs.set_membership(&user.user_id, Some(org_id.clone()))
            .await?;
        users.put_user_permissions(&user.user_id, &set).await?;
        users.put_user(&user).await
    }
    .await;
    if let Err(e) = written {
        // Only a record known absent is undone: one that landed despite the
        // error, or that cannot be read, may be a complete account.
        if matches!(users.get_user(&user.user_id).await, Ok(None)) {
            undo_provision(users, orgs, &user).await;
        }
        return Err(e.context("failed to create the account"));
    }

    tracing::info!(user_id = %user.user_id, %org_id, granted = ?set.names(), "user provisioned");
    Ok(Provisioned::Created(user))
}

/// Best-effort, so one failure does not strand the rest.
async fn undo_provision(users: &dyn UserStore, orgs: &dyn OrgStore, user: &User) {
    let user_id = &user.user_id;
    if let Err(e) = users.delete_user_permissions(user_id).await {
        tracing::warn!(%user_id, error = %e, "failed to delete the grant of an account never created");
    }
    if let Err(e) = orgs.delete_membership(user_id).await {
        tracing::warn!(%user_id, error = %e, "failed to delete the membership of an account never created");
    }
    if let Err(e) = users.release_user_email(user.contact_email.as_str()).await {
        tracing::warn!(%user_id, error = %e,
            "failed to release the address claim of an account never created; \
             `users release-claim <email>` will clear it");
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use pipette_core::types::Mint;
    use pipette_core::user_permission::UserPermissionGroup;
    use pipette_plan_types::{FirstClerkSignIn, UserStatus};
    use pipette_store::Stores;
    use pipette_store_fs::build_local_fs_stores;

    use super::*;

    fn stores() -> anyhow::Result<(tempfile::TempDir, Stores)> {
        let dir = tempfile::tempdir()?;
        let config = pipette_core::config::Config {
            storage: pipette_core::config::StorageConfig::local_fs(dir.path().to_path_buf()),
            auth_storage: pipette_core::config::StorageConfig::local_fs(dir.path().to_path_buf()),
            ..Default::default()
        };
        let stores = build_local_fs_stores(&config)?;
        Ok((dir, stores))
    }

    /// Otherwise one address would end up on two accounts.
    #[tokio::test]
    async fn a_claim_naming_another_account_is_left_alone() -> anyhow::Result<()> {
        let (_dir, stores) = stores()?;
        let stale = User::try_new(ContactEmail::try_new("op@example.test")?, Utc::now())?;
        let current = User::try_new(ContactEmail::try_new("op@example.test")?, Utc::now())?;
        stores.users.put_user(&stale).await?;
        stores.users.put_user(&current).await?;
        assert_eq!(
            stores
                .users
                .claim_user_email("op@example.test", &current.user_id)
                .await?,
            EmailClaim::Claimed
        );

        let deleted = delete(
            &*stores.users,
            &*stores.key_index,
            &*stores.orgs,
            &stale.user_id,
        )
        .await?;
        assert!(!deleted.released_claim);
        assert_eq!(
            stores.users.lookup_user_by_email("op@example.test").await?,
            Some(current.user_id.clone()),
            "the claim still names the account that holds it"
        );

        let deleted = delete(
            &*stores.users,
            &*stores.key_index,
            &*stores.orgs,
            &current.user_id,
        )
        .await?;
        assert!(deleted.released_claim);
        assert!(
            stores
                .users
                .lookup_user_by_email("op@example.test")
                .await?
                .is_none()
        );

        // Idempotent: nothing there is not an error.
        let again = delete(
            &*stores.users,
            &*stores.key_index,
            &*stores.orgs,
            &current.user_id,
        )
        .await?;
        assert_eq!(again.email, None);
        Ok(())
    }

    /// A key revokes only through the account holding it and frees only its own
    /// keypair, and a second revocation finds nothing.
    #[tokio::test]
    async fn revoking_a_key_retires_it_and_frees_only_its_keypair() -> anyhow::Result<()> {
        use pipette_core::key_index::{KeyHolderPrincipalId, PublicKeyDigest, digest_of};
        use pipette_core::user::UserKey;
        use pipette_plan_types::NonEmptyTrimmedString;
        use pipette_plan_types::PublicKeyHex;

        let (_dir, stores) = stores()?;
        let (a, b) = (UserId::mint(), UserId::mint());
        let mut held: Vec<(UserKeyId, PublicKeyDigest)> = Vec::new();
        for (holder, byte) in [(&a, 0x11u8), (&a, 0x22), (&b, 0x33)] {
            let public_key = PublicKeyHex::try_new(hex::encode([byte; PublicKeyHex::BYTE_LEN]))?;
            let key = UserKey::new(
                public_key.clone(),
                NonEmptyTrimmedString::try_new("laptop")?,
                Utc::now(),
            );
            stores.users.put_user_key(holder, &key).await?;
            let digest = digest_of(&public_key);
            stores
                .key_index
                .claim_public_key(&digest, &KeyHolderPrincipalId::User(holder.clone()))
                .await?;
            held.push((key.key_id, digest));
        }
        let [(a1, a1_digest), (a2, a2_digest), (b1, b1_digest)] = held.as_slice() else {
            anyhow::bail!("three keys were registered");
        };
        let (users, index) = (&*stores.users, &*stores.key_index);

        assert!(
            !revoke_key(users, index, &a, b1).await?,
            "b's key is not a's to revoke"
        );
        assert!(stores.users.get_user_key(&b, b1).await?.is_some());
        assert_eq!(
            stores.key_index.holder_of_public_key(b1_digest).await?,
            Some(KeyHolderPrincipalId::User(b.clone()))
        );

        assert!(revoke_key(users, index, &a, a1).await?);
        assert!(stores.users.get_user_key(&a, a1).await?.is_none());
        assert!(stores.users.get_user_key(&a, a2).await?.is_some());
        assert_eq!(
            stores.key_index.holder_of_public_key(a1_digest).await?,
            None,
            "the revoked keypair is free to register again"
        );
        assert_eq!(
            stores.key_index.holder_of_public_key(a2_digest).await?,
            Some(KeyHolderPrincipalId::User(a.clone()))
        );

        assert!(
            !revoke_key(users, index, &a, a1).await?,
            "a revocation is reported once"
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_provisioned_account_is_complete() -> anyhow::Result<()> {
        let (_dir, stores) = stores()?;
        let org_id = OrgId::mint();
        let member = UserPermissionGroup::OrgMember.permissions();
        let Provisioned::Created(user) = provision(
            &*stores.users,
            &*stores.orgs,
            ContactEmail::try_new("Op@Example.test")?,
            &org_id,
            member,
        )
        .await?
        else {
            anyhow::bail!("the address was free");
        };

        assert_eq!(user.contact_email.as_str(), "op@example.test");
        assert_eq!(user.status, UserStatus::Active);
        assert_eq!(
            stores.users.get_user(&user.user_id).await?,
            Some(user.clone())
        );
        assert_eq!(
            stores.users.lookup_user_by_email("op@example.test").await?,
            Some(user.user_id.clone())
        );
        assert_eq!(
            stores.orgs.get_membership(&user.user_id).await?,
            Some(org_id)
        );
        assert_eq!(
            stores.users.get_user_permissions(&user.user_id).await?,
            Some(UserPermissionSet::from_iter(member.iter().copied()).normalized())
        );
        Ok(())
    }

    /// A claimed address is somebody's account, whatever state it is in.
    #[tokio::test]
    async fn provisioning_a_claimed_address_writes_nothing() -> anyhow::Result<()> {
        let (_dir, stores) = stores()?;
        let holder = UserId::mint();
        stores
            .users
            .claim_user_email("op@example.test", &holder)
            .await?;

        let provisioned = provision(
            &*stores.users,
            &*stores.orgs,
            ContactEmail::try_new("op@example.test")?,
            &OrgId::mint(),
            UserPermissionGroup::OrgMember.permissions(),
        )
        .await?;
        assert!(matches!(provisioned, Provisioned::Taken(id) if id == holder));
        assert!(stores.users.list_users().await?.is_empty());
        assert!(stores.orgs.list_memberships().await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn the_membership_and_the_sign_in_mark_go_with_the_account() -> anyhow::Result<()> {
        let (_dir, stores) = stores()?;
        let user = User::try_new(ContactEmail::try_new("op@example.test")?, Utc::now())?;
        stores.users.put_user(&user).await?;
        stores
            .orgs
            .set_membership(&user.user_id, Some(OrgId::mint()))
            .await?;
        stores
            .users
            .mark_user_awaiting_sign_in(&user.user_id)
            .await?;
        let first = FirstClerkSignIn {
            at: Utc::now(),
            prior_keys: Vec::new(),
        };
        stores
            .users
            .record_user_first_clerk_sign_in(&user.user_id, &first)
            .await?;

        delete(
            &*stores.users,
            &*stores.key_index,
            &*stores.orgs,
            &user.user_id,
        )
        .await?;
        assert_eq!(stores.orgs.get_membership(&user.user_id).await?, None);
        assert!(!stores.users.user_awaits_sign_in(&user.user_id).await?);
        assert_eq!(
            stores
                .users
                .get_user_first_clerk_sign_in(&user.user_id)
                .await?,
            None
        );
        Ok(())
    }
}
