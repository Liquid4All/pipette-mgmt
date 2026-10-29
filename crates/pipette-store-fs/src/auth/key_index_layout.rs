//! Filesystem layout for `public-key-index/{digest}`: one claim per keypair.
//! The claim is an exclusive create ([`create_complete`]), not a
//! read-then-write, so of two registrations of one keypair exactly one wins.

use std::path::Path;

use pipette_core::key_index::{KeyClaim, KeyHolderPrincipalId, PublicKeyDigest};
use pipette_store::CreateOutcome;

use crate::storage::create_complete;

/// Claim `digest` for `holder`, or report the principal already holding it.
/// `digest` is 64 lowercase hex characters, so it is one path segment with no
/// traversal available to it.
pub(super) fn claim(
    index_dir: &Path,
    digest: &PublicKeyDigest,
    holder: &KeyHolderPrincipalId,
) -> anyhow::Result<KeyClaim> {
    let path = index_dir.join(digest.as_str());
    if create_complete(index_dir, &path, holder.as_str().as_bytes())? == CreateOutcome::Created {
        return Ok(KeyClaim::Claimed);
    }
    Ok(KeyClaim::Taken(read_holder(&path)?))
}

/// The principal holding `digest`, or `None` when nothing claims it.
pub(super) fn holder(
    index_dir: &Path,
    digest: &PublicKeyDigest,
) -> anyhow::Result<Option<KeyHolderPrincipalId>> {
    match std::fs::read_to_string(index_dir.join(digest.as_str())) {
        Ok(body) => Ok(Some(body.trim().parse()?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Release a claim. Idempotent. Runs after the record it protects is gone, so a
/// crash between the two strands the claim rather than freeing a keypair that
/// still authenticates.
pub(super) fn release(index_dir: &Path, digest: &PublicKeyDigest) -> anyhow::Result<()> {
    match std::fs::remove_file(index_dir.join(digest.as_str())) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// A corrupt body is an error rather than a `None`: treating an unreadable
/// owner as "unclaimed" hands the keypair to the next caller who asks.
fn read_holder(path: &Path) -> anyhow::Result<KeyHolderPrincipalId> {
    Ok(std::fs::read_to_string(path)?.trim().parse()?)
}
