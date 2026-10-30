//! Object-store layout for the user accounts [`super::S3UserStore`] serves —
//! `users/{user_id}.json` for the record and `user-email-index/{email}` for the
//! address claim. Apart from [`super::client_layout`] because listing
//! `clients/` must never return an account.

use std::sync::Arc;

use futures::TryStreamExt;
use futures::stream;
use futures::stream::StreamExt;
use object_store::ObjectStore;
use object_store::path::Path as ObjPath;

use pipette_core::user::{User, UserKey};
use pipette_plan_types::{FirstClerkSignIn, UserId, UserKeyId};
use pipette_store::CreateOutcome;

use crate::{delete_idempotent, get_bytes, obj_path, put_bytes, put_create};

pub(super) const USERS_KEYSPACE: &str = "users";

pub(super) const EMAIL_INDEX_KEYSPACE: &str = "user-email-index";

pub(super) const AWAITING_SIGN_IN_KEYSPACE: &str = "user-awaiting-sign-in";

pub(super) const FIRST_CLERK_SIGN_IN_KEYSPACE: &str = "user-first-clerk-sign-in";

/// What an address claim did; `Taken` names the holder for the operator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Claim {
    Claimed,
    Taken(UserId),
}

fn user_path(prefix: &str, user_id: &UserId) -> ObjPath {
    obj_path(prefix, &format!("{USERS_KEYSPACE}/{user_id}.json"))
}

/// `email` is a [`ContactEmail`], which refuses a quoted local part and a bare
/// `/`, so it is one key segment; [`User::try_new`] folds case, so one mailbox
/// is one key.
///
/// [`ContactEmail`]: pipette_plan_types::ContactEmail
fn email_path(prefix: &str, email: &str) -> ObjPath {
    obj_path(prefix, &format!("{EMAIL_INDEX_KEYSPACE}/{email}"))
}

pub(super) async fn load_user(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    user_id: &UserId,
) -> anyhow::Result<Option<User>> {
    match get_bytes(store, &user_path(prefix, user_id)).await? {
        Some(data) => Ok(Some(serde_json::from_slice(&data)?)),
        None => Ok(None),
    }
}

/// Unconditional: after creation only `status` changes, written by an
/// administrator. The caller claims the address first, so a record only ever
/// exists for an address this account holds.
pub(super) async fn save_user(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    user: &User,
) -> anyhow::Result<()> {
    put_bytes(
        store,
        &user_path(prefix, &user.user_id),
        serde_json::to_vec_pretty(user)?,
    )
    .await
}

/// The record is the existence authority, so this is what stops the account
/// authenticating; every other key it owns is inert once this is gone.
pub(super) async fn delete_user(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    user_id: &UserId,
) -> anyhow::Result<()> {
    delete_idempotent(store, &user_path(prefix, user_id)).await
}

/// Every account, sorted by id (whose UUIDv7 prefix sorts by creation time). A
/// record that will not parse is logged and skipped, so one corrupt record does
/// not take the whole roster with it.
pub(super) async fn list_users(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
) -> anyhow::Result<Vec<User>> {
    let listing = obj_path(prefix, &format!("{USERS_KEYSPACE}/"));
    let entries: Vec<_> = store.list(Some(&listing)).try_collect().await?;
    // Owned keys: a closure borrowing `entries` cannot satisfy the higher-ranked
    // bound `buffer_unordered` needs.
    let keys: Vec<ObjPath> = entries
        .into_iter()
        .map(|meta| meta.location)
        .filter(|key| key.filename().is_some_and(crate::is_json_key))
        .collect();
    let records = keys.into_iter().map(|key| async move {
        // Deleted between the listing and the read.
        let Some(data) = get_bytes(store, &key).await? else {
            return Ok::<Option<User>, anyhow::Error>(None);
        };
        match serde_json::from_slice(&data) {
            Ok(user) => Ok(Some(user)),
            Err(e) => {
                tracing::warn!(key = %key, error = %e, "skipping malformed user record");
                Ok(None)
            }
        }
    });
    let mut users: Vec<User> = stream::iter(records)
        .buffer_unordered(pipette_store::STORAGE_CONCURRENCY)
        .try_collect::<Vec<_>>()
        .await?
        .into_iter()
        .flatten()
        .collect();
    users.sort_by(|a, b| a.user_id.as_str().cmp(b.user_id.as_str()));
    Ok(users)
}

/// Of any number of concurrent creates for one address exactly one succeeds
/// ([`put_create`], `If-None-Match: *`) and the rest read the winner back. The
/// id is the body, not a key segment, so two racers contend for one key.
pub(super) async fn claim_email(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    email: &str,
    user_id: &UserId,
) -> anyhow::Result<Claim> {
    let path = email_path(prefix, email);
    match put_create(store, &path, user_id.as_str().as_bytes().to_vec()).await? {
        CreateOutcome::Created => Ok(Claim::Claimed),
        CreateOutcome::AlreadyExists => {
            let holder = get_bytes(store, &path)
                .await?
                .ok_or_else(|| anyhow::anyhow!("address claim for {email} vanished mid-create"))?;
            Ok(Claim::Taken(UserId::try_new(
                String::from_utf8(holder.to_vec())?.trim(),
            )?))
        }
    }
}

pub(super) async fn lookup_email(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    email: &str,
) -> anyhow::Result<Option<UserId>> {
    match get_bytes(store, &email_path(prefix, email)).await? {
        Some(holder) => Ok(Some(UserId::try_new(
            String::from_utf8(holder.to_vec())?.trim(),
        )?)),
        None => Ok(None),
    }
}

/// Runs after the record is gone, so a crash between the two strands the claim
/// rather than freeing an address whose account still authenticates.
pub(super) async fn release_email(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    email: &str,
) -> anyhow::Result<()> {
    delete_idempotent(store, &email_path(prefix, email)).await
}

// ── Registered keys: `user-keys/{user_id}/{key_id}.json` ────────────────────
//
// The account segment makes the authentication lookup a binding check too: a
// request naming another account addresses a key that holds nothing.

pub(super) const USER_KEYS_KEYSPACE: &str = "user-keys";

fn user_key_path(prefix: &str, user_id: &UserId, key_id: &UserKeyId) -> ObjPath {
    obj_path(
        prefix,
        &format!("{USER_KEYS_KEYSPACE}/{user_id}/{key_id}.json"),
    )
}

/// Unconditional: a key id is the key's digest, so the only put landing on an
/// existing object is a re-registration of that key.
pub(super) async fn save_key(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    user_id: &UserId,
    key: &UserKey,
) -> anyhow::Result<()> {
    put_bytes(
        store,
        &user_key_path(prefix, user_id, &key.key_id),
        serde_json::to_vec_pretty(key)?,
    )
    .await
}

pub(super) async fn load_key(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    user_id: &UserId,
    key_id: &UserKeyId,
) -> anyhow::Result<Option<UserKey>> {
    match get_bytes(store, &user_key_path(prefix, user_id, key_id)).await? {
        Some(data) => Ok(Some(serde_json::from_slice(&data)?)),
        None => Ok(None),
    }
}

/// An unparseable record is logged and skipped: this is the listing someone
/// reads to find a machine they did not add, so one bad row must not hide the
/// rest.
pub(super) async fn list_keys(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    user_id: &UserId,
) -> anyhow::Result<Vec<UserKey>> {
    let listing = obj_path(prefix, &format!("{USER_KEYS_KEYSPACE}/{user_id}/"));
    let entries: Vec<_> = store.list(Some(&listing)).try_collect().await?;
    let keys: Vec<ObjPath> = entries
        .into_iter()
        .map(|meta| meta.location)
        .filter(|key| key.filename().is_some_and(crate::is_json_key))
        .collect();
    let records = keys.into_iter().map(|key| async move {
        // Revoked between the listing and the read.
        let Some(data) = get_bytes(store, &key).await? else {
            return Ok::<Option<UserKey>, anyhow::Error>(None);
        };
        match serde_json::from_slice(&data) {
            Ok(registration) => Ok(Some(registration)),
            Err(e) => {
                tracing::warn!(key = %key, error = %e, "skipping malformed user key record");
                Ok(None)
            }
        }
    });
    let mut registrations: Vec<UserKey> = stream::iter(records)
        .buffer_unordered(pipette_store::STORAGE_CONCURRENCY)
        .try_collect::<Vec<_>>()
        .await?
        .into_iter()
        .flatten()
        .collect();
    registrations.sort_by_key(|key| key.added_at);
    Ok(registrations)
}

/// Runs before the claim is released: authentication reads only this object, so
/// dropping the claim first would leave a revoked key that still verifies.
pub(super) async fn delete_key(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    user_id: &UserId,
    key_id: &UserKeyId,
) -> anyhow::Result<()> {
    delete_idempotent(store, &user_key_path(prefix, user_id, key_id)).await
}

fn awaiting_path(prefix: &str, user_id: &UserId) -> ObjPath {
    obj_path(prefix, &format!("{AWAITING_SIGN_IN_KEYSPACE}/{user_id}"))
}

/// Mark `user_id` as awaiting its first sign-in: an empty object, since its
/// presence is the fact. Idempotent.
pub(super) async fn mark_awaiting(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    user_id: &UserId,
) -> anyhow::Result<()> {
    put_bytes(store, &awaiting_path(prefix, user_id), Vec::new()).await
}

pub(super) async fn awaits(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    user_id: &UserId,
) -> anyhow::Result<bool> {
    Ok(get_bytes(store, &awaiting_path(prefix, user_id))
        .await?
        .is_some())
}

/// Idempotent.
pub(super) async fn clear_awaiting(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    user_id: &UserId,
) -> anyhow::Result<()> {
    delete_idempotent(store, &awaiting_path(prefix, user_id)).await
}

fn first_clerk_sign_in_path(prefix: &str, user_id: &UserId) -> ObjPath {
    obj_path(
        prefix,
        &format!("{FIRST_CLERK_SIGN_IN_KEYSPACE}/{user_id}.json"),
    )
}

/// [`put_create`], `If-None-Match: *`, so one first sign-in wins.
pub(super) async fn record_first_clerk_sign_in(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    user_id: &UserId,
    first: &FirstClerkSignIn,
) -> anyhow::Result<bool> {
    let path = first_clerk_sign_in_path(prefix, user_id);
    Ok(
        put_create(store, &path, serde_json::to_vec_pretty(first)?).await?
            == CreateOutcome::Created,
    )
}

pub(super) async fn load_first_clerk_sign_in(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    user_id: &UserId,
) -> anyhow::Result<Option<FirstClerkSignIn>> {
    match get_bytes(store, &first_clerk_sign_in_path(prefix, user_id)).await? {
        Some(data) => Ok(Some(serde_json::from_slice(&data)?)),
        None => Ok(None),
    }
}

/// Idempotent.
pub(super) async fn delete_first_clerk_sign_in(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    user_id: &UserId,
) -> anyhow::Result<()> {
    delete_idempotent(store, &first_clerk_sign_in_path(prefix, user_id)).await
}
