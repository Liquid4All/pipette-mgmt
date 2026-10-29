//! [`LocalFsKeyIndexStore`] — `public-key-index/` on a filesystem, its own
//! store because both the client and the user store write this keyspace.

use std::path::PathBuf;

use async_trait::async_trait;

use pipette_core::key_index::{KeyClaim, KeyHolderPrincipalId, PublicKeyDigest};
use pipette_store::KeyIndexStore;

use super::{key_index_layout, run_blocking};

/// One directory, one file per claimed keypair, each holding its owner's id.
#[derive(Clone)]
pub(crate) struct LocalFsKeyIndexStore {
    index_dir: PathBuf,
}

impl LocalFsKeyIndexStore {
    pub(crate) fn new(root: PathBuf) -> Self {
        Self {
            index_dir: root.join("public-key-index"),
        }
    }
}

#[async_trait]
impl KeyIndexStore for LocalFsKeyIndexStore {
    async fn claim_public_key(
        &self,
        digest: &PublicKeyDigest,
        holder: &KeyHolderPrincipalId,
    ) -> anyhow::Result<KeyClaim> {
        let dir = self.index_dir.clone();
        let digest = digest.clone();
        let holder = holder.clone();
        run_blocking(move || key_index_layout::claim(&dir, &digest, &holder)).await
    }

    async fn holder_of_public_key(
        &self,
        digest: &PublicKeyDigest,
    ) -> anyhow::Result<Option<KeyHolderPrincipalId>> {
        let dir = self.index_dir.clone();
        let digest = digest.clone();
        run_blocking(move || key_index_layout::holder(&dir, &digest)).await
    }

    async fn release_public_key(&self, digest: &PublicKeyDigest) -> anyhow::Result<()> {
        let dir = self.index_dir.clone();
        let digest = digest.clone();
        run_blocking(move || key_index_layout::release(&dir, &digest)).await
    }
}
