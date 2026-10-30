//! Filesystem layout for the user accounts [`super::LocalFsUserStore`] serves —
//! `users/{user_id}.json` for the record and `user-email-index/{email}` for the
//! address claim. Apart from [`super::client_layout`] because listing
//! `clients/` must never return an account.

use std::path::Path;

use pipette_core::user::{User, UserKey};
use pipette_plan_types::{FirstClerkSignIn, UserId, UserKeyId};
use pipette_store::CreateOutcome;

use crate::storage::create_complete;

/// What an address claim did; `Taken` names the holder for the operator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Claim {
    Claimed,
    Taken(UserId),
}

pub(super) fn load_user(users_dir: &Path, user_id: &UserId) -> anyhow::Result<Option<User>> {
    match std::fs::read(users_dir.join(format!("{user_id}.json"))) {
        Ok(data) => Ok(Some(serde_json::from_slice(&data)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Unconditional: after creation only `status` changes, written by an
/// administrator. The caller claims the address first, so a record only ever
/// exists for an address this account holds.
pub(super) fn save_user(users_dir: &Path, user: &User) -> anyhow::Result<()> {
    std::fs::create_dir_all(users_dir)?;
    let path = users_dir.join(format!("{}.json", user.user_id));
    std::fs::write(&path, serde_json::to_vec_pretty(user)?)?;
    Ok(())
}

/// The record is the existence authority, so this is what stops the account
/// authenticating; every other key it owns is inert once this is gone.
pub(super) fn delete_user(users_dir: &Path, user_id: &UserId) -> anyhow::Result<()> {
    match std::fs::remove_file(users_dir.join(format!("{user_id}.json"))) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Every account, in creation order (the id's UUIDv7 prefix sorts by time). A
/// record that will not parse is logged and skipped, so one corrupt record does
/// not take the whole roster with it.
pub(super) fn list_users(users_dir: &Path) -> anyhow::Result<Vec<User>> {
    if !users_dir.exists() {
        return Ok(Vec::new());
    }
    let mut users: Vec<User> = std::fs::read_dir(users_dir)?
        .filter_map(|entry| crate::readable_entry(users_dir, entry))
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .filter_map(|entry| {
            let path = entry.path();
            match std::fs::read(&path) {
                Ok(data) => match serde_json::from_slice::<User>(&data) {
                    Ok(user) => Some(Ok(user)),
                    Err(e) => {
                        tracing::warn!(path = %path.display(), error = %e, "skipping malformed user record");
                        None
                    }
                },
                // Deleted between the listing and the read.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                // Any other failure is not evidence the account is absent.
                Err(e) => Some(Err(e.into())),
            }
        })
        .collect::<anyhow::Result<_>>()?;
    users.sort_by(|a, b| a.user_id.as_str().cmp(b.user_id.as_str()));
    Ok(users)
}

/// Of any number of concurrent creates for one address exactly one makes the
/// file and the rest read the winner back ([`create_complete`]). The id is the
/// body, not a path segment, so two racers contend for one key.
///
/// `email` is a [`ContactEmail`], which refuses a quoted local part and a bare
/// `/`, so it is one path segment; [`User::try_new`] folds case, so one mailbox
/// is one key.
///
/// [`ContactEmail`]: pipette_plan_types::ContactEmail
pub(super) fn claim_email(
    index_dir: &Path,
    email: &str,
    user_id: &UserId,
) -> anyhow::Result<Claim> {
    let path = index_dir.join(email);
    if create_complete(index_dir, &path, user_id.as_str().as_bytes())? == CreateOutcome::Created {
        return Ok(Claim::Claimed);
    }
    let holder = std::fs::read_to_string(&path)?;
    Ok(Claim::Taken(UserId::try_new(holder.trim())?))
}

pub(super) fn lookup_email(index_dir: &Path, email: &str) -> anyhow::Result<Option<UserId>> {
    match std::fs::read_to_string(index_dir.join(email)) {
        Ok(holder) => Ok(Some(UserId::try_new(holder.trim())?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Runs after the record is gone, so a crash between the two strands the claim
/// rather than freeing an address whose account still authenticates.
pub(super) fn release_email(index_dir: &Path, email: &str) -> anyhow::Result<()> {
    match std::fs::remove_file(index_dir.join(email)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

// ── Registered keys: `user-keys/{user_id}/{key_id}.json` ────────────────────
//
// The account segment makes the authentication lookup a binding check too: a
// request naming another account addresses a directory without the key. Both
// segments are validated newtypes excluding `/` and `.`.

/// Unconditional: a key id is the key's digest, so the only write landing on an
/// existing object is a re-registration of that key.
pub(super) fn save_key(keys_dir: &Path, user_id: &UserId, key: &UserKey) -> anyhow::Result<()> {
    let dir = keys_dir.join(user_id.as_str());
    std::fs::create_dir_all(&dir)?;
    std::fs::write(
        dir.join(format!("{}.json", key.key_id)),
        serde_json::to_vec_pretty(key)?,
    )?;
    Ok(())
}

pub(super) fn load_key(
    keys_dir: &Path,
    user_id: &UserId,
    key_id: &UserKeyId,
) -> anyhow::Result<Option<UserKey>> {
    match std::fs::read(
        keys_dir
            .join(user_id.as_str())
            .join(format!("{key_id}.json")),
    ) {
        Ok(data) => Ok(Some(serde_json::from_slice(&data)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// An unparseable record is logged and skipped: this is the listing someone
/// reads to find a machine they did not add, so one bad row must not hide the
/// rest.
pub(super) fn list_keys(keys_dir: &Path, user_id: &UserId) -> anyhow::Result<Vec<UserKey>> {
    let dir = keys_dir.join(user_id.as_str());
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut keys: Vec<UserKey> = std::fs::read_dir(&dir)?
        .filter_map(|entry| crate::readable_entry(&dir, entry))
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .filter_map(|entry| {
            let path = entry.path();
            match std::fs::read(&path) {
                Ok(data) => match serde_json::from_slice::<UserKey>(&data) {
                    Ok(key) => Some(Ok(key)),
                    Err(e) => {
                        tracing::warn!(path = %path.display(), error = %e, "skipping malformed user key record");
                        None
                    }
                },
                // Revoked between the listing and the read.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => Some(Err(e.into())),
            }
        })
        .collect::<anyhow::Result<_>>()?;
    keys.sort_by_key(|key| key.added_at);
    Ok(keys)
}

/// Runs before the claim is released: authentication reads only this object, so
/// dropping the claim first would leave a revoked key that still verifies.
pub(super) fn delete_key(
    keys_dir: &Path,
    user_id: &UserId,
    key_id: &UserKeyId,
) -> anyhow::Result<()> {
    match std::fs::remove_file(
        keys_dir
            .join(user_id.as_str())
            .join(format!("{key_id}.json")),
    ) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Mark `user_id` as awaiting its first sign-in: an empty file, since its
/// presence is the fact. Idempotent.
pub(super) fn mark_awaiting(dir: &Path, user_id: &UserId) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join(user_id.as_str()), [])?;
    Ok(())
}

pub(super) fn awaits(dir: &Path, user_id: &UserId) -> anyhow::Result<bool> {
    Ok(dir.join(user_id.as_str()).try_exists()?)
}

/// Idempotent.
pub(super) fn clear_awaiting(dir: &Path, user_id: &UserId) -> anyhow::Result<()> {
    match std::fs::remove_file(dir.join(user_id.as_str())) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// `{dir}/{user_id}.json`, made by [`create_complete`] so one first sign-in wins.
pub(super) fn record_first_clerk_sign_in(
    dir: &Path,
    user_id: &UserId,
    first: &FirstClerkSignIn,
) -> anyhow::Result<bool> {
    let path = dir.join(format!("{user_id}.json"));
    Ok(create_complete(dir, &path, &serde_json::to_vec_pretty(first)?)? == CreateOutcome::Created)
}

pub(super) fn load_first_clerk_sign_in(
    dir: &Path,
    user_id: &UserId,
) -> anyhow::Result<Option<FirstClerkSignIn>> {
    match std::fs::read(dir.join(format!("{user_id}.json"))) {
        Ok(data) => Ok(Some(serde_json::from_slice(&data)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Idempotent.
pub(super) fn delete_first_clerk_sign_in(dir: &Path, user_id: &UserId) -> anyhow::Result<()> {
    match std::fs::remove_file(dir.join(format!("{user_id}.json"))) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}
