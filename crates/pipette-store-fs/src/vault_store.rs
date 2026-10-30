//! [`VaultStore`] on a filesystem: records under `vault/`, blobs under
//! `vault-resources/`, imports under `vault-imports/`.

use std::num::{NonZeroU64, NonZeroUsize};
use std::path::{Path, PathBuf};

use anyhow::Context;
use async_trait::async_trait;
use futures::StreamExt;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use pipette_core::tmp_file::TmpFileGuard;
use pipette_core::vault::{
    BlobDigest, VAULT_IMPORT_PREFIX, VAULT_OBJECT_PREFIX, VaultImport, VaultObjectKey, VaultOwner,
    VaultResource, blob_dir, blob_key, owner_key_prefix,
};
use pipette_plan_types::{Sha256, VaultResourceId};
use pipette_store::{
    ArtifactBody, BlobBody, BlobWrite, StagedWrite, VaultBlob, VaultResourcePage, VaultStore,
};

use super::{page_flat_keyspace, remove_file_idempotent, run_blocking};

/// A multi-gigabyte checkpoint is many reads, little of it in memory at once.
const CHUNK: usize = 256 * 1024;

#[derive(Clone)]
pub(crate) struct LocalFsVaultStore {
    records_dir: PathBuf,
    blobs_dir: PathBuf,
    imports_dir: PathBuf,
}

impl LocalFsVaultStore {
    pub(crate) fn new(data_dir: &std::path::Path) -> Self {
        Self {
            records_dir: data_dir.join("vault"),
            blobs_dir: data_dir.join(VAULT_OBJECT_PREFIX.trim_end_matches('/')),
            imports_dir: data_dir.join(VAULT_IMPORT_PREFIX.trim_end_matches('/')),
        }
    }

    fn record_path(&self, resource_id: &VaultResourceId) -> PathBuf {
        self.records_dir
            .join(format!("{}.json", resource_id.as_str()))
    }

    /// The key is traversal-free by construction, so the join stays under the
    /// blob directory.
    fn blob_path(&self, key: &VaultObjectKey) -> PathBuf {
        self.blobs_dir.join(key.as_ref())
    }

    fn import_dir(&self, owner: &VaultOwner) -> PathBuf {
        self.imports_dir.join(owner_key_prefix(owner))
    }

    fn import_path(&self, owner: &VaultOwner, import: &Sha256) -> PathBuf {
        self.import_dir(owner).join(format!("{import}.json"))
    }
}

/// Stream and hash `body` into a `.tmp-` file in `dir`, stopping before the
/// chunk that passes `limit`. No key names a `.tmp-` file.
async fn write_tmp(
    dir: &Path,
    mut body: BlobBody,
    limit: Option<NonZeroU64>,
) -> anyhow::Result<(PathBuf, TmpFileGuard, BlobDigest)> {
    tokio::fs::create_dir_all(dir).await?;
    let tmp = dir.join(format!(".tmp-{}", uuid::Uuid::new_v4()));
    let guard = TmpFileGuard::new(tmp.clone());
    let mut file = tokio::fs::File::create(&tmp).await?;
    let mut digest = BlobDigest::default();
    while let Some(chunk) = body.next().await {
        let chunk = chunk?;
        digest.update(&chunk);
        if limit.is_some_and(|limit| digest.bytes() > limit.get()) {
            break;
        }
        file.write_all(&chunk).await?;
    }
    file.sync_all().await?;
    Ok((tmp, guard, digest))
}

/// The rename is an atomic replace, so a racing reader never sees a truncated
/// file. The `.tmp-` name has no `.json` extension, so a listing skips it.
fn write_atomic(dir: &Path, path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".tmp-{}", uuid::Uuid::new_v4()));
    let guard = TmpFileGuard::new(tmp.clone());
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    guard.disarm();
    Ok(())
}

#[async_trait]
impl VaultStore for LocalFsVaultStore {
    async fn get_resource(
        &self,
        resource_id: &VaultResourceId,
    ) -> anyhow::Result<Option<VaultResource>> {
        let path = self.record_path(resource_id);
        run_blocking(move || match std::fs::read(&path) {
            Ok(data) => Ok(Some(serde_json::from_slice(&data)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        })
        .await
    }

    async fn put_resource(&self, resource: &VaultResource) -> anyhow::Result<()> {
        let dir = self.records_dir.clone();
        let path = self.record_path(&resource.resource_id);
        let bytes = serde_json::to_vec_pretty(resource)?;
        run_blocking(move || write_atomic(&dir, &path, &bytes)).await
    }

    async fn list_resources(
        &self,
        after: Option<&VaultResourceId>,
        limit: NonZeroUsize,
    ) -> anyhow::Result<VaultResourcePage> {
        let dir = self.records_dir.clone();
        let after = after.map(|id| id.as_str().to_string());
        run_blocking(move || {
            // No filter: ownership is a caller's scope, not a store-side
            // selection.
            page_flat_keyspace(
                &dir,
                after.as_deref(),
                limit,
                VaultResourceId::try_new,
                |_| true,
            )
        })
        .await
    }

    async fn delete_resource(&self, resource_id: &VaultResourceId) -> anyhow::Result<()> {
        remove_file_idempotent(self.record_path(resource_id)).await
    }

    async fn delete_blob(&self, key: &VaultObjectKey) -> anyhow::Result<()> {
        remove_file_idempotent(self.blob_path(key)).await
    }

    async fn list_blobs(&self) -> anyhow::Result<Vec<VaultBlob>> {
        let root = self.blobs_dir.clone();
        run_blocking(move || {
            let mut blobs = Vec::new();
            let mut pending = vec![root.clone()];
            while let Some(dir) = pending.pop() {
                let entries = match std::fs::read_dir(&dir) {
                    Ok(entries) => entries,
                    // No upload yet.
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(e).with_context(|| format!("listing {}", dir.display())),
                };
                for entry in entries {
                    let entry = entry?;
                    let meta = entry.metadata()?;
                    if meta.is_dir() {
                        pending.push(entry.path());
                        continue;
                    }
                    let relative = entry.path().strip_prefix(&root)?.to_path_buf();
                    let Some(key) = relative
                        .to_str()
                        .and_then(|key| VaultObjectKey::try_new(key.to_string()).ok())
                    else {
                        continue;
                    };
                    blobs.push(VaultBlob {
                        key,
                        bytes: meta.len(),
                        modified: meta.modified()?.into(),
                    });
                }
            }
            Ok(blobs)
        })
        .await
    }

    async fn stat_blob(&self, key: &VaultObjectKey) -> anyhow::Result<Option<u64>> {
        match tokio::fs::metadata(self.blob_path(key)).await {
            Ok(meta) if meta.is_file() => Ok(Some(meta.len())),
            // A directory at the key is not an artifact, and reads as absent.
            Ok(_) => Ok(None),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn put_blob(
        &self,
        key: &VaultObjectKey,
        sha256: &Sha256,
        body: BlobBody,
    ) -> anyhow::Result<BlobWrite> {
        let dest = self.blob_path(key);
        let dir = dest
            .parent()
            .ok_or_else(|| anyhow::anyhow!("a blob key has a parent"))?;
        // Beside the blob, so the rename that publishes it stays on one
        // filesystem.
        let (tmp, guard, digest) = write_tmp(dir, body, None).await?;
        let (actual, bytes) = digest.finish()?;
        if actual != *sha256 {
            return Ok(BlobWrite::Mismatch { actual, bytes });
        }
        tokio::fs::rename(&tmp, &dest).await?;
        guard.disarm();
        Ok(BlobWrite::Stored { bytes })
    }

    async fn open_blob(
        &self,
        key: &VaultObjectKey,
        from: u64,
    ) -> anyhow::Result<Option<ArtifactBody>> {
        let mut file = match tokio::fs::File::open(self.blob_path(key)).await {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let meta = file.metadata().await?;
        if !meta.is_file() {
            return Ok(None);
        }
        let bytes = meta.len().saturating_sub(from);
        file.seek(std::io::SeekFrom::Start(from)).await?;
        let stream = futures::stream::try_unfold(file, |mut file| async move {
            let mut buf = vec![0_u8; CHUNK];
            let n = file.read(&mut buf).await?;
            if n == 0 {
                return Ok(None);
            }
            buf.truncate(n);
            Ok(Some((bytes::Bytes::from(buf), file)))
        })
        .boxed();
        Ok(Some(ArtifactBody { bytes, stream }))
    }

    async fn put_staged_blob(
        &self,
        owner: &VaultOwner,
        body: BlobBody,
        limit: NonZeroU64,
    ) -> anyhow::Result<StagedWrite> {
        // Stage in the owner's blob directory, known before the digest, so the
        // publishing rename stays on one filesystem.
        let dir = self.blobs_dir.join(blob_dir(owner));
        let (tmp, guard, digest) = write_tmp(&dir, body, Some(limit)).await?;
        // The guard removes the temporary file.
        if digest.bytes() > limit.get() {
            return Ok(StagedWrite::TooLarge { limit });
        }

        let (sha256, bytes) = digest.finish()?;
        let key = blob_key(owner, &sha256)?;
        let dest = self.blob_path(&key);
        // A blob already at the key holds these bytes, because the key is
        // their digest. The guard then removes the temporary file.
        let already_stored = self.stat_blob(&key).await?.is_some();
        if !already_stored {
            tokio::fs::rename(&tmp, &dest).await?;
            guard.disarm();
        }
        tracing::info!(
            owner = %owner.label(),
            %sha256,
            bytes,
            already_stored,
            "vault staged blob promoted"
        );
        Ok(StagedWrite::Stored { sha256, bytes, key })
    }

    async fn get_import(
        &self,
        owner: &VaultOwner,
        import: &Sha256,
    ) -> anyhow::Result<Option<VaultImport>> {
        let path = self.import_path(owner, import);
        run_blocking(move || match std::fs::read(&path) {
            Ok(data) => Ok(Some(serde_json::from_slice(&data)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        })
        .await
    }

    async fn put_import(
        &self,
        owner: &VaultOwner,
        import: &Sha256,
        entry: &VaultImport,
    ) -> anyhow::Result<()> {
        let dir = self.import_dir(owner);
        let path = self.import_path(owner, import);
        let bytes = serde_json::to_vec_pretty(entry)?;
        run_blocking(move || write_atomic(&dir, &path, &bytes)).await?;
        tracing::info!(
            owner = %owner.label(),
            %import,
            resources = entry.resource_ids.len(),
            "vault import indexed"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use pipette_core::types::Mint;

    use super::*;

    #[tokio::test]
    async fn test_localfs_vault_store_meets_the_shared_contract() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        pipette_store_testkit::assert_vault_store_roundtrip(&LocalFsVaultStore::new(dir.path()))
            .await
    }

    #[tokio::test]
    async fn test_localfs_vault_staged_write_meets_the_shared_contract() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        pipette_store_testkit::assert_vault_staged_write(&LocalFsVaultStore::new(dir.path())).await
    }

    #[tokio::test]
    async fn test_localfs_vault_import_meets_the_shared_contract() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        pipette_store_testkit::assert_vault_import_roundtrip(&LocalFsVaultStore::new(dir.path()))
            .await
    }

    /// One entry per owner and import digest, outside the blob prefix.
    #[tokio::test]
    async fn test_localfs_vault_import_path() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = LocalFsVaultStore::new(dir.path());
        let owner = pipette_store_testkit::a_vault_resource()?.owner;
        let import = pipette_store_testkit::vault_blob_sha256()?;
        let entry = VaultImport {
            resource_ids: vec![VaultResourceId::mint()],
        };
        store.put_import(&owner, &import, &entry).await?;
        assert!(
            dir.path()
                .join(format!("vault-imports/{}/{import}.json", owner.label()))
                .is_file(),
            "the entry must land under vault-imports/"
        );
        Ok(())
    }

    /// Refused and cut-off staged writes leave no temporary file beside the
    /// blobs.
    #[tokio::test]
    async fn test_localfs_staged_write_leaves_only_the_blob() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = LocalFsVaultStore::new(dir.path());
        let owner = pipette_store_testkit::a_vault_resource()?.owner;
        let blobs = dir
            .path()
            .join(format!("vault-resources/{}", blob_dir(&owner)));
        let limit = pipette_store_testkit::vault_blob_limit()?;

        let tight = NonZeroU64::MIN;
        let refused = store
            .put_staged_blob(
                &owner,
                pipette_store_testkit::blob_body(pipette_store_testkit::VAULT_BLOB),
                tight,
            )
            .await?;
        assert_eq!(refused, StagedWrite::TooLarge { limit: tight });
        assert_eq!(std::fs::read_dir(&blobs)?.count(), 0);

        assert!(
            store
                .put_staged_blob(&owner, pipette_store_testkit::cut_off_blob_body(), limit)
                .await
                .is_err()
        );
        assert_eq!(std::fs::read_dir(&blobs)?.count(), 0);

        for _ in 0..2 {
            store
                .put_staged_blob(
                    &owner,
                    pipette_store_testkit::blob_body(pipette_store_testkit::VAULT_BLOB),
                    limit,
                )
                .await?;
        }
        let names: Vec<_> = std::fs::read_dir(&blobs)?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name())
            .collect();
        assert_eq!(
            names,
            vec![std::ffi::OsString::from(
                pipette_store_testkit::vault_blob_sha256()?.to_string()
            )]
        );
        Ok(())
    }

    #[test]
    fn test_localfs_each_staged_write_logs_one_promote_event() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        pipette_store_testkit::assert_vault_staged_write_logs_once(&LocalFsVaultStore::new(
            dir.path(),
        ))
    }

    /// A directory at the digest path is not a blob: the rename onto it fails,
    /// so the write errs rather than return a `Stored` that no read can serve.
    #[tokio::test]
    async fn test_localfs_staged_write_over_a_directory_is_an_error() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = LocalFsVaultStore::new(dir.path());
        let owner = pipette_store_testkit::a_vault_resource()?.owner;
        let key = blob_key(&owner, &pipette_store_testkit::vault_blob_sha256()?)?;
        std::fs::create_dir_all(store.blob_path(&key).join("not-a-blob"))?;

        let written = store
            .put_staged_blob(
                &owner,
                pipette_store_testkit::blob_body(pipette_store_testkit::VAULT_BLOB),
                pipette_store_testkit::vault_blob_limit()?,
            )
            .await;
        assert!(written.is_err(), "{written:?}");
        assert_eq!(store.stat_blob(&key).await?, None);
        Ok(())
    }

    /// Pins both layouts. The paths are built from one field each, so a
    /// directory that moved is self-consistent and invisible to the round trip.
    #[tokio::test]
    async fn test_localfs_vault_paths() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = LocalFsVaultStore::new(dir.path());
        let resource = pipette_store_testkit::a_vault_resource()?;
        store.put_resource(&resource).await?;
        assert!(
            dir.path()
                .join(format!("vault/{}.json", resource.resource_id))
                .exists(),
            "the record must land under vault/, outside the blob prefix"
        );
        assert_eq!(
            store.blob_path(&resource.object_key),
            dir.path().join(format!(
                "vault-resources/{}/blobs/{}",
                resource.owner.label(),
                resource.sha256
            )),
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_localfs_refused_blob_leaves_nothing() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = LocalFsVaultStore::new(dir.path());
        let resource = pipette_store_testkit::a_vault_resource()?;
        let written = store
            .put_blob(
                &resource.object_key,
                &resource.sha256,
                pipette_store_testkit::blob_body(b"other weights"),
            )
            .await?;
        assert!(matches!(written, BlobWrite::Mismatch { .. }), "{written:?}");
        let blobs = dir
            .path()
            .join(format!("vault-resources/{}/blobs", resource.owner.label()));
        assert_eq!(std::fs::read_dir(blobs)?.count(), 0);
        Ok(())
    }
}
