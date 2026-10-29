//! Object-store layout for `public-key-index/{digest}`: one claim per keypair.
//! The claim is `PutMode::Create` (`If-None-Match: *`), so of two registrations
//! of one keypair exactly one wins, across replicas too.

use std::sync::Arc;

use object_store::ObjectStore;
use object_store::path::Path as ObjPath;

use pipette_core::key_index::{KeyClaim, KeyHolderPrincipalId, PublicKeyDigest};
use pipette_store::CreateOutcome;

use crate::{delete_idempotent, get_bytes, obj_path, put_create};

pub(super) const KEY_INDEX_KEYSPACE: &str = "public-key-index";

/// `digest` is 64 lowercase hex characters, so it is one key segment with no
/// traversal available to it.
fn claim_path(prefix: &str, digest: &PublicKeyDigest) -> ObjPath {
    obj_path(prefix, &format!("{KEY_INDEX_KEYSPACE}/{digest}"))
}

/// Claim `digest` for `holder`, or report the principal already holding it.
pub(super) async fn claim(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    digest: &PublicKeyDigest,
    holder: &KeyHolderPrincipalId,
) -> anyhow::Result<KeyClaim> {
    let path = claim_path(prefix, digest);
    match put_create(store, &path, holder.as_str().as_bytes().to_vec()).await? {
        CreateOutcome::Created => Ok(KeyClaim::Claimed),
        CreateOutcome::AlreadyExists => {
            let incumbent = get_bytes(store, &path).await?.ok_or_else(|| {
                anyhow::anyhow!("public-key claim for {digest} vanished mid-create")
            })?;
            Ok(KeyClaim::Taken(parse_holder(&incumbent)?))
        }
    }
}

/// The principal holding `digest`, or `None` when nothing claims it.
pub(super) async fn holder(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    digest: &PublicKeyDigest,
) -> anyhow::Result<Option<KeyHolderPrincipalId>> {
    get_bytes(store, &claim_path(prefix, digest))
        .await?
        .map(|body| parse_holder(&body))
        .transpose()
}

/// Release a claim. Idempotent. Runs after the record it protects is gone, so a
/// crash between the two strands the claim rather than freeing a keypair that
/// still authenticates.
pub(super) async fn release(
    store: &Arc<dyn ObjectStore>,
    prefix: &str,
    digest: &PublicKeyDigest,
) -> anyhow::Result<()> {
    delete_idempotent(store, &claim_path(prefix, digest)).await
}

/// A corrupt body is an error rather than a `None`: treating an unreadable
/// owner as "unclaimed" hands the keypair to the next caller who asks.
fn parse_holder(body: &[u8]) -> anyhow::Result<KeyHolderPrincipalId> {
    Ok(std::str::from_utf8(body)?.trim().parse()?)
}
