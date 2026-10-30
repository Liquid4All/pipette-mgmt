//! [`S3KeyIndexStore`] — `public-key-index/` on an object store, its own store
//! because both the client and the user store write this keyspace.

use std::sync::Arc;

use async_trait::async_trait;
use object_store::ObjectStore;

use pipette_core::key_index::{KeyClaim, KeyHolderPrincipalId, PublicKeyDigest};
use pipette_store::KeyIndexStore;

use super::key_index_layout;

/// One object per claimed keypair, each holding its owner's id.
#[derive(Clone)]
pub(crate) struct S3KeyIndexStore {
    store: Arc<dyn ObjectStore>,
    prefix: String,
}

impl S3KeyIndexStore {
    pub(crate) fn new(store: Arc<dyn ObjectStore>, prefix: String) -> Self {
        Self { store, prefix }
    }
}

#[async_trait]
impl KeyIndexStore for S3KeyIndexStore {
    async fn claim_public_key(
        &self,
        digest: &PublicKeyDigest,
        holder: &KeyHolderPrincipalId,
    ) -> anyhow::Result<KeyClaim> {
        key_index_layout::claim(&self.store, &self.prefix, digest, holder).await
    }

    async fn holder_of_public_key(
        &self,
        digest: &PublicKeyDigest,
    ) -> anyhow::Result<Option<KeyHolderPrincipalId>> {
        key_index_layout::holder(&self.store, &self.prefix, digest).await
    }

    async fn release_public_key(&self, digest: &PublicKeyDigest) -> anyhow::Result<()> {
        key_index_layout::release(&self.store, &self.prefix, digest).await
    }
}
