//! The vault reads and resolutions `pipette-mgmt vault` and the API share,
//! above all [`store_blob`] and [`upload`], which bind bytes to an owner. No
//! authorization: the CLI runs past every gate, and the routes scope each
//! request through `pipette_httpapi::vault_access`. A second copy of
//! `store_blob` or `upload` could hand one owner's bytes to another.

use std::collections::{BTreeSet, HashMap};

use chrono::{DateTime, Utc};
use futures::{StreamExt, TryStreamExt};

use pipette_core::vault::{
    FoundBlob, ImportedBlob, SNIFF_BYTES, VaultObjectKey, VaultOwner, VaultResource, blob_key,
    is_blob_key, looks_like,
};
use pipette_plan_types::{
    NonEmptyTrimmedString, Sha256, UserId, VaultArtifact, VaultFileFormat, VaultFileRole,
    VaultResourceId, VaultUploadFile,
};
use pipette_store::{BlobBody, BlobWrite, Stores, VaultBlob, VaultStore, drain_vault_resources};

use crate::org;
use crate::org::OrgRef;

/// Why a vault write could not run.
#[derive(Debug, thiserror::Error)]
pub enum VaultAdminError {
    #[error("no vault resource {0}")]
    NotFound(VaultResourceId),
    #[error("no account {0}")]
    NoSuchUser(UserId),
    #[error("no organization {0}")]
    NoSuchOrg(OrgRef),
    /// No owner named; each surface names its own way of taking one.
    #[error("name an owner: an account or an organization")]
    NoOwnerNamed,
    /// The uploaded bytes hash to something other than the digest they were
    /// sent under, so nothing was kept.
    #[error("the bytes hash to {actual}, not {expected}")]
    DigestMismatch { expected: Sha256, actual: Sha256 },
    /// An upload names a file nobody uploaded for its owner. Names only what
    /// the caller sent, never where a file would be stored.
    #[error("no file {sha256} is uploaded for {owner}; upload it first")]
    NoBlob { owner: String, sha256: Sha256 },
    /// A file's first bytes are not what its role needs.
    #[error("the `{role}` file is not {}", format_name(.role.format()))]
    NotA { role: VaultFileRole },
    /// The file is past the configured ceiling, so nothing was kept.
    #[error("the file is over the {limit}-byte limit")]
    TooLarge { limit: u64 },
    #[error(transparent)]
    Store(#[from] anyhow::Error),
}

fn format_name(format: VaultFileFormat) -> &'static str {
    match format {
        VaultFileFormat::Gguf => "a gguf",
        VaultFileFormat::Archive => "a tar, tar.gz or zip archive",
    }
}

/// The record `resource_id` names, or [`VaultAdminError::NotFound`].
pub async fn require(
    vault: &dyn VaultStore,
    resource_id: &VaultResourceId,
) -> Result<VaultResource, VaultAdminError> {
    vault
        .get_resource(resource_id)
        .await?
        .ok_or_else(|| VaultAdminError::NotFound(resource_id.clone()))
}

/// Resolve the `--user` / `--org` pair into the one owner a resource must have,
/// against its roster, so an artifact is never owned by a missing record.
pub async fn owner(
    stores: &Stores,
    user: Option<UserId>,
    org: Option<OrgRef>,
) -> Result<VaultOwner, VaultAdminError> {
    match (user, org) {
        (Some(user_id), None) => {
            if stores.users.get_user(&user_id).await?.is_none() {
                return Err(VaultAdminError::NoSuchUser(user_id));
            }
            Ok(VaultOwner::User { user_id })
        }
        (None, Some(reference)) => {
            let found = org::resolve(&*stores.orgs, &reference).await?;
            let org = found.ok_or(VaultAdminError::NoSuchOrg(reference))?;
            Ok(VaultOwner::Org { org_id: org.org_id })
        }
        _ => Err(VaultAdminError::NoOwnerNamed),
    }
}

/// Where `owner`'s blob with `sha256` sits. Always a valid key, since both
/// halves are ids and hex; a refusal would be a bug, so it is a store fault.
fn key_of(owner: &VaultOwner, sha256: &Sha256) -> Result<VaultObjectKey, VaultAdminError> {
    blob_key(owner, sha256).map_err(|e| VaultAdminError::Store(e.into()))
}

/// Whether a blob of `owner`'s was stored by this call or already there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoredBlob {
    Stored { bytes: u64 },
    AlreadyThere { bytes: u64 },
}

/// The size of `owner`'s blob with `sha256`, or `None` when nobody uploaded it.
pub async fn blob_size(
    vault: &dyn VaultStore,
    owner: &VaultOwner,
    sha256: &Sha256,
) -> Result<Option<u64>, VaultAdminError> {
    Ok(vault.stat_blob(&key_of(owner, sha256)?).await?)
}

/// Store `body` as `owner`'s blob with `sha256`, kept only if it hashes to that
/// digest within `limit` bytes; a blob already there is kept unread. The caller
/// has already decided it may upload for `owner`.
pub async fn store_blob(
    vault: &dyn VaultStore,
    owner: &VaultOwner,
    sha256: &Sha256,
    limit: u64,
    body: BlobBody,
) -> Result<StoredBlob, VaultAdminError> {
    let key = key_of(owner, sha256)?;
    if let Some(bytes) = vault.stat_blob(&key).await? {
        return Ok(StoredBlob::AlreadyThere { bytes });
    }
    let mut seen: u64 = 0;
    let capped: BlobBody = body
        .map(move |chunk| {
            let chunk = chunk?;
            seen += chunk.len() as u64;
            if seen > limit {
                return Err(anyhow::Error::new(OverLimitError));
            }
            Ok(chunk)
        })
        .boxed();
    match vault.put_blob(&key, sha256, capped).await {
        Ok(BlobWrite::Stored { bytes }) => {
            tracing::info!(owner = %owner.label(), %sha256, bytes, "vault blob stored");
            Ok(StoredBlob::Stored { bytes })
        }
        Ok(BlobWrite::Mismatch { actual, .. }) => Err(VaultAdminError::DigestMismatch {
            expected: sha256.clone(),
            actual,
        }),
        // A body that fails midway writes nothing (`VaultStore::put_blob`).
        Err(e) if e.is::<OverLimitError>() => Err(VaultAdminError::TooLarge { limit }),
        Err(e) => Err(e.into()),
    }
}

/// What a capped body yields past its limit, told apart from a store fault.
#[derive(Debug, thiserror::Error)]
#[error("the body ran past its limit")]
struct OverLimitError;

/// The first [`SNIFF_BYTES`] of a blob, or fewer if it is shorter.
async fn head_of(vault: &dyn VaultStore, key: &VaultObjectKey) -> Result<Vec<u8>, VaultAdminError> {
    let Some(body) = vault.open_blob(key, 0).await? else {
        return Ok(Vec::new());
    };
    let mut stream = body.stream;
    let mut head = Vec::with_capacity(SNIFF_BYTES);
    while head.len() < SNIFF_BYTES {
        let Some(chunk) = stream.next().await else {
            break;
        };
        head.extend_from_slice(&chunk?);
    }
    head.truncate(SNIFF_BYTES);
    Ok(head)
}

/// Record `artifact` as `owner`'s: every file it names must already be
/// uploaded for `owner` and start as its role requires. Writes one record per
/// file and returns them, in the artifact's file order.
pub async fn upload(
    vault: &dyn VaultStore,
    name: NonEmptyTrimmedString,
    owner: VaultOwner,
    artifact: VaultArtifact<VaultUploadFile>,
    now: DateTime<Utc>,
) -> Result<Vec<VaultResource>, VaultAdminError> {
    let owner_ref = &owner;
    let sizes: HashMap<Sha256, u64> = futures::stream::iter(artifact.files())
        .then(|(role, file)| async move {
            let key = key_of(owner_ref, &file.sha256)?;
            let bytes = vault
                .stat_blob(&key)
                .await?
                .ok_or_else(|| VaultAdminError::NoBlob {
                    owner: owner_ref.label(),
                    sha256: file.sha256.clone(),
                })?;
            if !looks_like(role.format(), &head_of(vault, &key).await?) {
                return Err(VaultAdminError::NotA { role });
            }
            Ok((file.sha256.clone(), bytes))
        })
        .try_collect()
        .await?;
    let found = artifact.try_map(|role, file| {
        let bytes = sizes.get(&file.sha256).copied().ok_or_else(|| {
            VaultAdminError::Store(anyhow::anyhow!("the `{role}` file was never sized"))
        })?;
        Ok::<_, VaultAdminError>(FoundBlob { file, bytes })
    })?;
    let records = VaultResource::from_upload(name, owner, found, now)
        .map_err(|e| VaultAdminError::Store(e.into()))?;
    put_together(vault, &records).await?;
    Ok(records)
}

/// Record `artifact` as an import of `owner`: each file must be `owner`'s blob
/// and start as its role requires. One record per file, in artifact order.
pub async fn record_import(
    vault: &dyn VaultStore,
    owner: VaultOwner,
    artifact: VaultArtifact<ImportedBlob>,
    now: DateTime<Utc>,
) -> Result<Vec<VaultResource>, VaultAdminError> {
    let owner_ref = &owner;
    futures::stream::iter(artifact.files())
        .map(Ok)
        .try_for_each(|(role, blob)| async move {
            let key = key_of(owner_ref, &blob.sha256)?;
            if looks_like(role.format(), &head_of(vault, &key).await?) {
                Ok(())
            } else {
                Err(VaultAdminError::NotA { role })
            }
        })
        .await?;
    let records = VaultResource::from_import(owner, artifact, now)
        .map_err(|e| VaultAdminError::Store(e.into()))?;
    put_together(vault, &records).await?;
    Ok(records)
}

/// Write every record of one upload or import, or none: each names the others'
/// files.
async fn put_together(vault: &dyn VaultStore, records: &[VaultResource]) -> anyhow::Result<()> {
    let written = futures::stream::iter(records)
        .map(Ok)
        .try_for_each(|record| vault.put_resource(record))
        .await;
    if let Err(e) = written {
        let ids: Vec<_> = records.iter().map(|r| r.resource_id.clone()).collect();
        if let Err(undo) = delete_each(vault, &ids).await {
            tracing::error!(
                resource_ids = ?ids,
                error = %format_args!("{undo:#}"),
                "records of a failed vault upload or import are left behind"
            );
        }
        return Err(e);
    }
    Ok(())
}

/// Delete `resource` and every other record of its upload, which make one plan
/// entry together, then the files no remaining record names. `resource` goes
/// last, so a retry after a partial failure still finds it.
pub async fn delete(
    vault: &dyn VaultStore,
    resource: &VaultResource,
) -> Result<Vec<VaultResourceId>, VaultAdminError> {
    let mut ids: Vec<VaultResourceId> = resource
        .artifact
        .iter()
        .flat_map(VaultArtifact::files)
        .map(|(_, file)| file.resource_id.clone())
        .filter(|id| *id != resource.resource_id)
        .collect();
    ids.push(resource.resource_id.clone());
    delete_each(vault, &ids).await?;
    release_files(vault, resource).await;
    Ok(ids)
}

/// Delete the upload's files no remaining record names. Best effort: the
/// records are gone, and a file left behind only costs storage. A record
/// written between the listing and the delete outlives its bytes until the file
/// is uploaded again.
async fn release_files(vault: &dyn VaultStore, resource: &VaultResource) {
    let mut keys: BTreeSet<VaultObjectKey> = resource
        .artifact
        .iter()
        .flat_map(VaultArtifact::files)
        .filter_map(|(_, file)| blob_key(&resource.owner, &file.sha256).ok())
        .collect();
    keys.insert(resource.object_key.clone());
    let released = async {
        // A key sits under its owner's prefix, so only the owner's records name it.
        let remaining = vault.list_resources_of(&resource.owner).await?;
        // An unreadable record may name one of them.
        anyhow::ensure!(
            remaining.unreadable.is_empty(),
            "{} vault records are unreadable",
            remaining.unreadable.len()
        );
        remaining.items.iter().for_each(|record| {
            keys.remove(&record.object_key);
        });
        futures::stream::iter(&keys)
            .map(anyhow::Ok)
            .try_for_each(|key| async move {
                vault.delete_blob(key).await?;
                tracing::info!(object_key = %key, "vault file deleted");
                Ok(())
            })
            .await
    };
    if let Err(e) = released.await {
        tracing::warn!(
            resource_id = %resource.resource_id,
            error = %format_args!("{e:#}"),
            "the files of a deleted vault upload are left stored"
        );
    }
}

/// What [`prune`] found: the files no record names, oldest first.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Pruned {
    /// Old enough to delete, and deleted unless it was a dry run.
    pub orphans: Vec<VaultBlob>,
    /// No record names these either, but they were written too recently: an
    /// upload stores its file before it describes it.
    pub recent: usize,
}

/// Delete each stored file no record names, last written over `older_than`
/// ago. Refuses while a record is unreadable, since it may name one; only a
/// file at a [`blob_key`] is judged.
pub async fn prune(
    vault: &dyn VaultStore,
    older_than: std::time::Duration,
    now: DateTime<Utc>,
    dry_run: bool,
) -> anyhow::Result<Pruned> {
    // Files first, so a record written while this runs is read after them.
    let stored = vault.list_blobs().await?;
    let records = drain_vault_resources(vault).await?;
    anyhow::ensure!(
        records.unreadable.is_empty(),
        "{} vault record(s) are unreadable and may name a file: fix or delete them first",
        records.unreadable.len()
    );
    let named: BTreeSet<&VaultObjectKey> = records
        .items
        .iter()
        .map(|record| &record.object_key)
        .collect();
    let cutoff = now - chrono::Duration::from_std(older_than)?;
    let (mut orphans, recent): (Vec<VaultBlob>, Vec<VaultBlob>) = stored
        .into_iter()
        .filter(|blob| is_blob_key(&blob.key) && !named.contains(&blob.key))
        .partition(|blob| blob.modified <= cutoff);
    orphans.sort_by(|a, b| (a.modified, &a.key).cmp(&(b.modified, &b.key)));
    if !dry_run {
        futures::stream::iter(&orphans)
            .map(anyhow::Ok)
            .try_for_each(|blob| async move {
                vault.delete_blob(&blob.key).await?;
                tracing::info!(object_key = %blob.key, bytes = blob.bytes, "vault file pruned");
                Ok(())
            })
            .await?;
    }
    Ok(Pruned {
        orphans,
        recent: recent.len(),
    })
}

async fn delete_each(vault: &dyn VaultStore, ids: &[VaultResourceId]) -> anyhow::Result<()> {
    futures::stream::iter(ids)
        .map(Ok)
        .try_for_each(|id| vault.delete_resource(id))
        .await
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::json;

    use pipette_core::types::Mint;
    use pipette_core::vault::BlobDigest;
    use pipette_store::{ArtifactBody, VaultResourcePage, drain_vault_resources};

    use super::*;

    /// A working store whose record writes fail after the first.
    struct FailsAfterOneWrite {
        inner: Arc<dyn VaultStore>,
        writes: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl VaultStore for FailsAfterOneWrite {
        async fn get_resource(
            &self,
            resource_id: &VaultResourceId,
        ) -> anyhow::Result<Option<VaultResource>> {
            self.inner.get_resource(resource_id).await
        }
        async fn put_resource(&self, resource: &VaultResource) -> anyhow::Result<()> {
            if self.writes.fetch_add(1, Ordering::SeqCst) > 0 {
                anyhow::bail!("down");
            }
            self.inner.put_resource(resource).await
        }
        async fn list_resources(
            &self,
            after: Option<&VaultResourceId>,
            limit: NonZeroUsize,
        ) -> anyhow::Result<VaultResourcePage> {
            self.inner.list_resources(after, limit).await
        }
        async fn delete_resource(&self, resource_id: &VaultResourceId) -> anyhow::Result<()> {
            self.inner.delete_resource(resource_id).await
        }
        async fn stat_blob(&self, key: &VaultObjectKey) -> anyhow::Result<Option<u64>> {
            self.inner.stat_blob(key).await
        }
        async fn delete_blob(&self, key: &VaultObjectKey) -> anyhow::Result<()> {
            self.inner.delete_blob(key).await
        }
        async fn put_blob(
            &self,
            key: &VaultObjectKey,
            sha256: &Sha256,
            body: BlobBody,
        ) -> anyhow::Result<BlobWrite> {
            self.inner.put_blob(key, sha256, body).await
        }
        async fn open_blob(
            &self,
            key: &VaultObjectKey,
            from: u64,
        ) -> anyhow::Result<Option<ArtifactBody>> {
            self.inner.open_blob(key, from).await
        }
        async fn put_staged_blob(
            &self,
            owner: &VaultOwner,
            body: BlobBody,
            limit: std::num::NonZeroU64,
        ) -> anyhow::Result<pipette_store::StagedWrite> {
            self.inner.put_staged_blob(owner, body, limit).await
        }
        async fn get_import(
            &self,
            owner: &VaultOwner,
            import: &Sha256,
        ) -> anyhow::Result<Option<pipette_core::vault::VaultImport>> {
            self.inner.get_import(owner, import).await
        }
        async fn put_import(
            &self,
            owner: &VaultOwner,
            import: &Sha256,
            entry: &pipette_core::vault::VaultImport,
        ) -> anyhow::Result<()> {
            self.inner.put_import(owner, import, entry).await
        }
    }

    fn fs_vault() -> anyhow::Result<(tempfile::TempDir, Arc<dyn VaultStore>)> {
        let dir = tempfile::tempdir()?;
        let config = pipette_core::config::Config {
            storage: pipette_core::config::StorageConfig::local_fs(dir.path().to_path_buf()),
            auth_storage: pipette_core::config::StorageConfig::local_fs(dir.path().to_path_buf()),
            ..Default::default()
        };
        let stores = pipette_store_fs::build_local_fs_stores(&config)?;
        Ok((dir, stores.vault))
    }

    fn sha256_of(bytes: &[u8]) -> anyhow::Result<Sha256> {
        let mut digest = BlobDigest::default();
        digest.update(bytes);
        Ok(digest.finish()?.0)
    }

    async fn stored(
        vault: &dyn VaultStore,
        owner: &VaultOwner,
        bytes: &'static [u8],
    ) -> anyhow::Result<Sha256> {
        let sha256 = sha256_of(bytes)?;
        store_blob(
            vault,
            owner,
            &sha256,
            u64::MAX,
            pipette_store_testkit::blob_body(bytes),
        )
        .await?;
        Ok(sha256)
    }

    /// Uploads a `gguf_vision` model's two files and describes them.
    async fn vision_upload(
        vault: &dyn VaultStore,
    ) -> anyhow::Result<Result<Vec<VaultResource>, VaultAdminError>> {
        let owner = VaultOwner::User {
            user_id: UserId::mint(),
        };
        let model = stored(vault, &owner, b"GGUF weights").await?;
        let mmproj = stored(vault, &owner, b"GGUF projector").await?;
        let artifact = serde_json::from_value(json!({
            "type": "gguf_vision",
            "model": {"filename": "model.gguf", "sha256": model},
            "mmproj": {"filename": "mmproj.gguf", "sha256": mmproj},
        }))?;
        let name = NonEmptyTrimmedString::try_new("vision")?;
        Ok(upload(vault, name, owner, artifact, Utc::now()).await)
    }

    /// A record already written would name a file nothing else records.
    #[tokio::test]
    async fn an_upload_s_records_are_written_together_or_not_at_all() -> anyhow::Result<()> {
        let (_dir, inner) = fs_vault()?;
        let vault = FailsAfterOneWrite {
            inner,
            writes: AtomicUsize::new(0),
        };
        let refused = vision_upload(&vault).await?;
        assert!(
            matches!(refused, Err(VaultAdminError::Store(_))),
            "{refused:?}"
        );
        assert!(drain_vault_resources(&vault).await?.items.is_empty());
        Ok(())
    }

    /// No length is declared up front to refuse by; a body at the limit is stored.
    #[tokio::test]
    async fn a_file_past_the_limit_keeps_nothing() -> anyhow::Result<()> {
        const BYTES: &[u8] = b"GGUF weights";
        let (_dir, vault) = fs_vault()?;
        let owner = VaultOwner::User {
            user_id: UserId::mint(),
        };
        let sha256 = sha256_of(BYTES)?;
        let len = BYTES.len() as u64;

        let body = pipette_store_testkit::blob_body(BYTES);
        let refused = store_blob(vault.as_ref(), &owner, &sha256, len - 1, body).await;
        assert!(
            matches!(refused, Err(VaultAdminError::TooLarge { limit }) if limit == len - 1),
            "{refused:?}"
        );
        assert_eq!(blob_size(vault.as_ref(), &owner, &sha256).await?, None);

        let body = pipette_store_testkit::blob_body(BYTES);
        let stored = store_blob(vault.as_ref(), &owner, &sha256, len, body).await?;
        assert_eq!(stored, StoredBlob::Stored { bytes: len });
        Ok(())
    }

    /// The named file goes last.
    #[tokio::test]
    async fn deleting_a_file_deletes_its_upload() -> anyhow::Result<()> {
        let (_dir, vault) = fs_vault()?;
        let records = vision_upload(vault.as_ref()).await??;
        let [model, mmproj] = records.as_slice() else {
            anyhow::bail!("one record per file: {records:?}");
        };
        let deleted = delete(vault.as_ref(), mmproj).await?;
        assert_eq!(
            deleted,
            [model.resource_id.clone(), mmproj.resource_id.clone()]
        );
        assert!(
            drain_vault_resources(vault.as_ref())
                .await?
                .items
                .is_empty()
        );
        Ok(())
    }

    /// A described upload's files stay, a recent one waits, and a dry run deletes
    /// nothing.
    #[tokio::test]
    async fn a_prune_takes_only_old_files_no_record_names() -> anyhow::Result<()> {
        let (_dir, vault) = fs_vault()?;
        let vault = vault.as_ref();
        let records = vision_upload(vault).await??;
        let owner = VaultOwner::User {
            user_id: UserId::mint(),
        };
        let orphan = blob_key(&owner, &stored(vault, &owner, b"GGUF left behind").await?)?;
        let now = Utc::now();
        let hour = std::time::Duration::from_secs(3600);

        // Just written: an upload stores its file before it describes it.
        let young = prune(vault, hour, now, false).await?;
        assert_eq!((young.orphans.len(), young.recent), (0, 1));
        assert!(vault.stat_blob(&orphan).await?.is_some());

        let later = now + chrono::Duration::hours(2);
        let dry = prune(vault, hour, later, true).await?;
        let keys: Vec<&VaultObjectKey> = dry.orphans.iter().map(|blob| &blob.key).collect();
        assert_eq!(keys, [&orphan]);
        assert!(vault.stat_blob(&orphan).await?.is_some(), "a dry run");

        let pruned = prune(vault, hour, later, false).await?;
        assert_eq!(pruned, dry);
        assert_eq!(vault.stat_blob(&orphan).await?, None);
        for record in &records {
            assert!(
                vault.stat_blob(&record.object_key).await?.is_some(),
                "a file its record names stays"
            );
        }
        assert_eq!(prune(vault, hour, later, false).await?, Pruned::default());
        Ok(())
    }

    /// An unreadable record may name any file, so nothing is pruned beside one.
    #[tokio::test]
    async fn a_prune_refuses_beside_an_unreadable_record() -> anyhow::Result<()> {
        let (dir, vault) = fs_vault()?;
        let vault = vault.as_ref();
        let owner = VaultOwner::User {
            user_id: UserId::mint(),
        };
        let orphan = blob_key(&owner, &stored(vault, &owner, b"GGUF left behind").await?)?;
        let record = dir
            .path()
            .join("vault")
            .join(format!("{}.json", VaultResourceId::mint()));
        std::fs::create_dir_all(dir.path().join("vault"))?;
        std::fs::write(record, "{")?;

        let later = Utc::now() + chrono::Duration::hours(2);
        let refused = prune(vault, std::time::Duration::ZERO, later, false).await;
        assert!(
            refused
                .as_ref()
                .is_err_and(|e| e.to_string().contains("unreadable")),
            "{refused:?}"
        );
        assert!(vault.stat_blob(&orphan).await?.is_some());
        Ok(())
    }
}
