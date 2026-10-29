//! [`VaultStore`] on an object store: records under `vault/`, blobs under
//! `vault-resources/`, imports under `vault-imports/`, staged writes under
//! `vault-staging/`.

use std::num::{NonZeroU64, NonZeroUsize};
use std::sync::Arc;

use anyhow::Context;
use async_trait::async_trait;
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
use futures::{StreamExt, TryStreamExt};
use object_store::path::Path as ObjPath;
use object_store::{GetOptions, GetRange, ObjectStore, ObjectStoreExt, WriteMultipart};

use pipette_core::vault::{
    BlobDigest, VAULT_IMPORT_PREFIX, VAULT_OBJECT_PREFIX, VaultImport, VaultObjectKey, VaultOwner,
    VaultResource, blob_key, owner_key_prefix,
};
use pipette_plan_types::{Sha256, VaultResourceId};
use pipette_store::{
    ArtifactBody, BlobBody, BlobWrite, StagedWrite, VaultBlob, VaultResourcePage, VaultStore,
};

use super::{get_bytes, obj_path, page_flat_keyspace, put_bytes};

const RECORDS_PREFIX: &str = "vault/";

/// Part size: S3's 10,000-part cap puts the largest blob at ~312 GiB. With
/// [`PARTS_IN_FLIGHT`], one upload buffers at most ~190 MiB.
const PART: usize = 32 * 1024 * 1024;
const PARTS_IN_FLIGHT: usize = 4;

/// The staging keyspace root. It is outside [`VAULT_OBJECT_PREFIX`], so no
/// record can name a staged object.
const STAGING_PREFIX: &str = "vault-staging/";

/// The largest object that one `CopyObject` copies, and the largest part of
/// an `UploadPartCopy`: 5 GiB, the S3 limit for both.
const COPY_PART: u64 = 5 * 1024 * 1024 * 1024;

#[derive(Clone)]
pub(crate) struct S3VaultStore {
    store: Arc<dyn ObjectStore>,
    prefix: String,
    copier: PartCopier,
}

impl S3VaultStore {
    pub(crate) fn new(store: Arc<dyn ObjectStore>, prefix: String, copier: PartCopier) -> Self {
        Self {
            store,
            prefix,
            copier,
        }
    }

    /// The UUID keeps two concurrent writes apart.
    fn staging_path(&self, owner: &VaultOwner) -> ObjPath {
        obj_path(
            &self.prefix,
            &format!(
                "{STAGING_PREFIX}{}{}",
                owner_key_prefix(owner),
                uuid::Uuid::new_v4()
            ),
        )
    }

    fn import_path(&self, owner: &VaultOwner, import: &Sha256) -> ObjPath {
        obj_path(
            &self.prefix,
            &format!(
                "{VAULT_IMPORT_PREFIX}{}{import}.json",
                owner_key_prefix(owner)
            ),
        )
    }

    /// Copy the staged object to its digest's key. `object_store` copies with
    /// one `CopyObject`, capped at 5 GiB, so a larger one uses [`PartCopier`].
    async fn promote_staged(
        &self,
        staging: &ObjPath,
        owner: &VaultOwner,
        sha256: Sha256,
        bytes: u64,
    ) -> anyhow::Result<StagedWrite> {
        let key = blob_key(owner, &sha256)?;
        // A blob already at the key holds these bytes, because the key is
        // their digest.
        let already_stored = self.stat_blob(&key).await?.is_some();
        if !already_stored {
            let dest = self.blob_path(&key);
            if bytes <= COPY_PART {
                self.store.copy(staging, &dest).await?;
            } else {
                self.copier
                    .copy(staging.as_ref(), dest.as_ref(), bytes)
                    .await?;
            }
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

    /// Stream and hash `body` into a multipart upload, stopping before the
    /// chunk that passes `limit`. On success the caller finishes or aborts it.
    async fn stream_to(
        &self,
        path: &ObjPath,
        mut body: BlobBody,
        limit: Option<NonZeroU64>,
    ) -> anyhow::Result<(WriteMultipart, BlobDigest)> {
        let upload = self.store.put_multipart(path).await?;
        let mut writer = WriteMultipart::new_with_chunk_size(upload, PART);
        let mut digest = BlobDigest::default();
        let streamed = async {
            while let Some(chunk) = body.next().await {
                let chunk = chunk?;
                digest.update(&chunk);
                if limit.is_some_and(|limit| digest.bytes() > limit.get()) {
                    break;
                }
                writer.wait_for_capacity(PARTS_IN_FLIGHT).await?;
                writer.write(&chunk);
            }
            anyhow::Ok(())
        }
        .await;
        // `finish` aborts only when its own `complete` fails, so an earlier
        // failure must abort here. A dead process leaves the upload to the
        // bucket's incomplete-upload lifecycle rule (`docs/operations.md`).
        if let Err(e) = streamed {
            abort(writer).await;
            return Err(e);
        }
        Ok((writer, digest))
    }

    fn record_path(&self, resource_id: &VaultResourceId) -> ObjPath {
        obj_path(
            &self.prefix,
            &format!("{RECORDS_PREFIX}{}.json", resource_id.as_str()),
        )
    }

    /// The key is traversal-free by construction, so the join stays under the
    /// blob prefix.
    fn blob_path(&self, key: &VaultObjectKey) -> ObjPath {
        obj_path(
            &self.prefix,
            &format!("{VAULT_OBJECT_PREFIX}{}", key.as_ref()),
        )
    }
}

#[async_trait]
impl VaultStore for S3VaultStore {
    async fn get_resource(
        &self,
        resource_id: &VaultResourceId,
    ) -> anyhow::Result<Option<VaultResource>> {
        match get_bytes(&self.store, &self.record_path(resource_id)).await? {
            Some(data) => Ok(Some(serde_json::from_slice(&data)?)),
            None => Ok(None),
        }
    }

    async fn put_resource(&self, resource: &VaultResource) -> anyhow::Result<()> {
        put_bytes(
            &self.store,
            &self.record_path(&resource.resource_id),
            serde_json::to_vec_pretty(resource)?,
        )
        .await
    }

    async fn list_resources(
        &self,
        after: Option<&VaultResourceId>,
        limit: NonZeroUsize,
    ) -> anyhow::Result<VaultResourcePage> {
        page_flat_keyspace(
            &self.store,
            &obj_path(&self.prefix, RECORDS_PREFIX),
            |resource_id| self.record_path(resource_id),
            after,
            limit,
            VaultResourceId::try_new,
            // No filter: ownership is a caller's scope, not a store-side
            // selection.
            |_: &VaultResource| true,
        )
        .await
    }

    async fn delete_resource(&self, resource_id: &VaultResourceId) -> anyhow::Result<()> {
        match self.store.delete(&self.record_path(resource_id)).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    async fn delete_blob(&self, key: &VaultObjectKey) -> anyhow::Result<()> {
        match self.store.delete(&self.blob_path(key)).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    async fn list_blobs(&self) -> anyhow::Result<Vec<VaultBlob>> {
        let root = obj_path(&self.prefix, VAULT_OBJECT_PREFIX.trim_end_matches('/'));
        let under = format!("{root}/");
        self.store
            .list(Some(&root))
            .map_err(anyhow::Error::from)
            .try_filter_map(|meta| {
                let key = meta
                    .location
                    .as_ref()
                    .strip_prefix(under.as_str())
                    .and_then(|key| VaultObjectKey::try_new(key.to_string()).ok());
                futures::future::ready(Ok(key.map(|key| VaultBlob {
                    key,
                    bytes: meta.size,
                    modified: meta.last_modified,
                })))
            })
            .try_collect()
            .await
    }

    async fn stat_blob(&self, key: &VaultObjectKey) -> anyhow::Result<Option<u64>> {
        match self.store.head(&self.blob_path(key)).await {
            Ok(meta) => Ok(Some(meta.size)),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn put_blob(
        &self,
        key: &VaultObjectKey,
        sha256: &Sha256,
        body: BlobBody,
    ) -> anyhow::Result<BlobWrite> {
        // A multipart upload is visible only once completed, so deciding on
        // the digest before `finish` is what keeps a wrong blob from existing.
        let (writer, digest) = self.stream_to(&self.blob_path(key), body, None).await?;
        let (actual, bytes) = digest.finish()?;
        if actual != *sha256 {
            abort(writer).await;
            return Ok(BlobWrite::Mismatch { actual, bytes });
        }
        writer.finish().await?;
        Ok(BlobWrite::Stored { bytes })
    }

    async fn open_blob(
        &self,
        key: &VaultObjectKey,
        from: u64,
    ) -> anyhow::Result<Option<ArtifactBody>> {
        // An empty blob has no byte `0` to start a range at, so the whole read
        // asks for no range.
        let options = GetOptions::new().with_range((from > 0).then_some(GetRange::Offset(from)));
        let result = match self.store.get_opts(&self.blob_path(key), options).await {
            Ok(result) => result,
            Err(object_store::Error::NotFound { .. }) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let bytes = result.range.end - result.range.start;
        let stream = result.into_stream().map_err(anyhow::Error::from).boxed();
        Ok(Some(ArtifactBody { bytes, stream }))
    }

    async fn put_staged_blob(
        &self,
        owner: &VaultOwner,
        body: BlobBody,
        limit: NonZeroU64,
    ) -> anyhow::Result<StagedWrite> {
        let staging = self.staging_path(owner);
        let (writer, digest) = self.stream_to(&staging, body, Some(limit)).await?;
        if digest.bytes() > limit.get() {
            abort(writer).await;
            return Ok(StagedWrite::TooLarge { limit });
        }
        writer.finish().await?;

        let (sha256, bytes) = digest.finish()?;
        let promoted = self.promote_staged(&staging, owner, sha256, bytes).await;
        // Delete the staged object whatever the promote did; a failed delete is
        // left to the lifecycle rule for `vault-staging/`.
        match self.store.delete(&staging).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => {}
            Err(e) => tracing::warn!(
                error = %format_args!("{e:#}"),
                key = %staging,
                "deleting a vault staging object failed"
            ),
        }
        promoted
    }

    async fn get_import(
        &self,
        owner: &VaultOwner,
        import: &Sha256,
    ) -> anyhow::Result<Option<VaultImport>> {
        match get_bytes(&self.store, &self.import_path(owner, import)).await? {
            Some(data) => Ok(Some(serde_json::from_slice(&data)?)),
            None => Ok(None),
        }
    }

    async fn put_import(
        &self,
        owner: &VaultOwner,
        import: &Sha256,
        entry: &VaultImport,
    ) -> anyhow::Result<()> {
        put_bytes(
            &self.store,
            &self.import_path(owner, import),
            serde_json::to_vec_pretty(entry)?,
        )
        .await?;
        tracing::info!(
            owner = %owner.label(),
            %import,
            resources = entry.resource_ids.len(),
            "vault import indexed"
        );
        Ok(())
    }
}

/// A server-side copy in ranged `UploadPartCopy` parts, for an object over
/// [`COPY_PART`] that one `CopyObject` cannot copy.
#[derive(Clone)]
pub(crate) struct PartCopier {
    client: aws_sdk_s3::Client,
    bucket: String,
}

impl PartCopier {
    pub(crate) fn new(client: aws_sdk_s3::Client, bucket: impl Into<String>) -> Self {
        Self {
            client,
            bucket: bucket.into(),
        }
    }

    /// `src` and `dst` are keys in this bucket. A part that fails aborts the
    /// upload.
    async fn copy(&self, src: &str, dst: &str, size: u64) -> anyhow::Result<()> {
        let source = format!("{}/{}", self.bucket, percent_encode(src));
        let upload_id = self
            .client
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(dst)
            .send()
            .await
            .context("starting the copy of a staged vault blob")?
            .upload_id
            .context("S3 started a multipart copy with no upload id")?;
        let copied = self.copy_parts(&source, dst, &upload_id, size).await;
        if copied.is_err()
            && let Err(e) = self
                .client
                .abort_multipart_upload()
                .bucket(&self.bucket)
                .key(dst)
                .upload_id(&upload_id)
                .send()
                .await
        {
            // The incomplete-upload lifecycle rule reclaims the parts.
            tracing::warn!(
                error = %format_args!("{e:#}"),
                key = dst,
                "aborting a failed vault blob copy failed"
            );
        }
        copied
    }

    async fn copy_parts(
        &self,
        source: &str,
        dst: &str,
        upload_id: &str,
        size: u64,
    ) -> anyhow::Result<()> {
        // `then` sends the parts one after another, in order.
        let parts: Vec<CompletedPart> = futures::stream::iter((1..).zip(copy_ranges(size)))
            .then(|(part_number, range)| async move {
                let e_tag = self
                    .client
                    .upload_part_copy()
                    .bucket(&self.bucket)
                    .key(dst)
                    .upload_id(upload_id)
                    .part_number(part_number)
                    .copy_source(source)
                    .copy_source_range(range)
                    .send()
                    .await
                    .context("copying a part of a staged vault blob")?
                    .copy_part_result
                    .and_then(|result| result.e_tag)
                    .context("S3 copied a part with no ETag")?;
                anyhow::Ok(
                    CompletedPart::builder()
                        .part_number(part_number)
                        .e_tag(e_tag)
                        .build(),
                )
            })
            .try_collect()
            .await?;
        self.client
            .complete_multipart_upload()
            .bucket(&self.bucket)
            .key(dst)
            .upload_id(upload_id)
            .multipart_upload(
                CompletedMultipartUpload::builder()
                    .set_parts(Some(parts))
                    .build(),
            )
            .send()
            .await
            .context("completing the copy of a staged vault blob")?;
        Ok(())
    }
}

/// The `CopySourceRange` values that cover `size` bytes in parts of at most
/// [`COPY_PART`] bytes, in order. Each range is inclusive at both ends.
fn copy_ranges(size: u64) -> impl Iterator<Item = String> {
    (0..size.div_ceil(COPY_PART)).map(move |n| {
        let start = n * COPY_PART;
        let end = (start + COPY_PART).min(size) - 1;
        format!("bytes={start}-{end}")
    })
}

/// Percent-encode a key for `CopySource`, which S3 reads as a URL path. The
/// unreserved characters and `/` stay as they are.
fn percent_encode(key: &str) -> String {
    key.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Abort a refused upload, keeping the refusal as the outcome: a failed abort
/// is left to the bucket's lifecycle rule.
async fn abort(writer: WriteMultipart) {
    if let Err(e) = writer.abort().await {
        tracing::warn!(
            error = %format_args!("{e:#}"),
            "aborting a refused vault upload failed"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use aws_sdk_s3::operation::abort_multipart_upload::AbortMultipartUploadOutput;
    use aws_sdk_s3::operation::complete_multipart_upload::CompleteMultipartUploadOutput;
    use aws_sdk_s3::operation::create_multipart_upload::CreateMultipartUploadOutput;
    use aws_sdk_s3::operation::upload_part_copy::{UploadPartCopyError, UploadPartCopyOutput};
    use aws_sdk_s3::types::CopyPartResult;
    use aws_smithy_mocks::{Rule, RuleMode, mock};
    use rstest::rstest;

    use pipette_core::types::Mint;

    use super::*;

    const GIB: u64 = 1024 * 1024 * 1024;

    /// A vault store over `InMemory`. Its copier has no mock rules, so a
    /// test that reaches the SDK copy fails.
    fn vault_store() -> (Arc<dyn ObjectStore>, S3VaultStore) {
        vault_store_with(copier(&[]))
    }

    fn vault_store_with(copier: PartCopier) -> (Arc<dyn ObjectStore>, S3VaultStore) {
        let backend: Arc<dyn ObjectStore> = Arc::new(object_store::memory::InMemory::new());
        let store = S3VaultStore::new(backend.clone(), "root".to_string(), copier);
        (backend, store)
    }

    #[tokio::test]
    async fn vault_store_meets_the_shared_contract() -> anyhow::Result<()> {
        let (_backend, store) = vault_store();
        pipette_store_testkit::assert_vault_store_roundtrip(&store).await
    }

    #[tokio::test]
    async fn vault_staged_write_meets_the_shared_contract() -> anyhow::Result<()> {
        let (_backend, store) = vault_store();
        pipette_store_testkit::assert_vault_staged_write(&store).await
    }

    #[tokio::test]
    async fn vault_import_meets_the_shared_contract() -> anyhow::Result<()> {
        let (_backend, store) = vault_store();
        pipette_store_testkit::assert_vault_import_roundtrip(&store).await
    }

    async fn listed(backend: &Arc<dyn ObjectStore>, prefix: &str) -> anyhow::Result<Vec<String>> {
        Ok(backend
            .list(Some(&ObjPath::from(prefix)))
            .map_ok(|meta| meta.location.to_string())
            .try_collect()
            .await?)
    }

    /// Refused, cut-off and promoted writes all leave `vault-staging/` empty,
    /// and only the promoted one leaves a blob.
    #[tokio::test]
    async fn a_staged_write_leaves_nothing_in_staging() -> anyhow::Result<()> {
        let (backend, store) = vault_store();
        let owner = pipette_store_testkit::a_vault_resource()?.owner;
        let blob = pipette_store_testkit::VAULT_BLOB;
        let limit = pipette_store_testkit::vault_blob_limit()?;

        let refused = store
            .put_staged_blob(
                &owner,
                pipette_store_testkit::blob_body(blob),
                NonZeroU64::MIN,
            )
            .await?;
        assert!(
            matches!(refused, StagedWrite::TooLarge { .. }),
            "{refused:?}"
        );
        assert!(
            store
                .put_staged_blob(&owner, pipette_store_testkit::cut_off_blob_body(), limit)
                .await
                .is_err()
        );
        assert!(listed(&backend, "root/vault-resources").await?.is_empty());

        store
            .put_staged_blob(&owner, pipette_store_testkit::blob_body(blob), limit)
            .await?;
        assert!(listed(&backend, "root/vault-staging").await?.is_empty());
        assert_eq!(
            listed(&backend, "root/vault-resources").await?,
            vec![format!(
                "root/vault-resources/{}/blobs/{}",
                owner.label(),
                pipette_store_testkit::vault_blob_sha256()?
            )]
        );
        Ok(())
    }

    /// Both sit under the owner's segment, outside the blob prefix.
    #[tokio::test]
    async fn staging_and_import_keys_are_pinned() -> anyhow::Result<()> {
        let (backend, store) = vault_store();
        let owner = pipette_store_testkit::a_vault_resource()?.owner;
        let staging = store.staging_path(&owner).to_string();
        let staging_dir = format!("root/vault-staging/{}/", owner.label());
        assert!(
            staging
                .strip_prefix(&staging_dir)
                .is_some_and(|rest| uuid::Uuid::parse_str(rest).is_ok()),
            "{staging}"
        );

        let import = pipette_store_testkit::vault_blob_sha256()?;
        let entry = VaultImport {
            resource_ids: vec![VaultResourceId::mint()],
        };
        store.put_import(&owner, &import, &entry).await?;
        assert_eq!(
            listed(&backend, "root/vault-imports").await?,
            vec![format!(
                "root/vault-imports/{}/{import}.json",
                owner.label()
            )]
        );
        Ok(())
    }

    #[rstest]
    #[case::one_byte(1, &["bytes=0-0"])]
    #[case::one_whole_part(5 * GIB, &["bytes=0-5368709119"])]
    #[case::one_byte_over(5 * GIB + 1, &["bytes=0-5368709119", "bytes=5368709120-5368709120"])]
    #[case::twelve_gib(
        12 * GIB,
        &["bytes=0-5368709119", "bytes=5368709120-10737418239", "bytes=10737418240-12884901887"]
    )]
    fn copy_ranges_cover_the_object(#[case] size: u64, #[case] expected: &[&str]) {
        assert_eq!(copy_ranges(size).collect::<Vec<_>>(), expected);
    }

    fn copier(rules: &[&Rule]) -> PartCopier {
        PartCopier::new(
            aws_smithy_mocks::mock_client!(aws_sdk_s3, RuleMode::MatchAny, rules),
            "test-bucket",
        )
    }

    fn create_rule() -> Rule {
        mock!(aws_sdk_s3::Client::create_multipart_upload).then_output(|| {
            CreateMultipartUploadOutput::builder()
                .upload_id("upload-1")
                .build()
        })
    }

    fn part_rule(seen: Arc<Mutex<Vec<(i32, String, String)>>>) -> Rule {
        mock!(aws_sdk_s3::Client::upload_part_copy).then_compute_output(move |req| {
            let part_number = req.part_number().unwrap_or_default();
            if let Ok(mut seen) = seen.lock() {
                seen.push((
                    part_number,
                    req.copy_source_range().unwrap_or_default().to_string(),
                    req.copy_source().unwrap_or_default().to_string(),
                ));
            }
            UploadPartCopyOutput::builder()
                .copy_part_result(
                    CopyPartResult::builder()
                        .e_tag(format!("etag-{part_number}"))
                        .build(),
                )
                .build()
        })
    }

    fn abort_rule() -> Rule {
        mock!(aws_sdk_s3::Client::abort_multipart_upload)
            .then_output(|| AbortMultipartUploadOutput::builder().build())
    }

    /// 12 GiB goes in three ordered parts and completes with their ETags.
    #[tokio::test]
    async fn a_large_copy_goes_in_ranged_parts() -> anyhow::Result<()> {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let completed = Arc::new(Mutex::new(Vec::new()));
        let recorded = completed.clone();
        let complete =
            mock!(aws_sdk_s3::Client::complete_multipart_upload).then_compute_output(move |req| {
                if let Ok(mut recorded) = recorded.lock() {
                    recorded.extend(
                        req.multipart_upload()
                            .map(|upload| upload.parts())
                            .into_iter()
                            .flatten()
                            .map(|part| {
                                (
                                    part.part_number().unwrap_or_default(),
                                    part.e_tag().unwrap_or_default().to_string(),
                                )
                            }),
                    );
                }
                CompleteMultipartUploadOutput::builder().build()
            });
        let (create, part, abort) = (create_rule(), part_rule(seen.clone()), abort_rule());
        copier(&[&create, &part, &complete, &abort])
            .copy(
                "root/vault-staging/user_1/a",
                "root/vault-resources/user_1/blobs/b",
                12 * GIB,
            )
            .await?;

        let source = "test-bucket/root/vault-staging/user_1/a".to_string();
        let seen = seen
            .lock()
            .map_err(|_| anyhow::anyhow!("poisoned"))?
            .clone();
        assert_eq!(
            seen,
            vec![
                (1, "bytes=0-5368709119".to_string(), source.clone()),
                (
                    2,
                    "bytes=5368709120-10737418239".to_string(),
                    source.clone()
                ),
                (3, "bytes=10737418240-12884901887".to_string(), source),
            ]
        );
        let completed = completed
            .lock()
            .map_err(|_| anyhow::anyhow!("poisoned"))?
            .clone();
        assert_eq!(
            completed,
            vec![
                (1, "etag-1".to_string()),
                (2, "etag-2".to_string()),
                (3, "etag-3".to_string()),
            ]
        );
        assert_eq!(abort.num_calls(), 0);
        Ok(())
    }

    #[tokio::test]
    async fn a_failed_part_aborts_the_copy() -> anyhow::Result<()> {
        let fails = mock!(aws_sdk_s3::Client::upload_part_copy)
            .match_requests(|req| req.part_number() == Some(2))
            .then_error(|| {
                UploadPartCopyError::generic(
                    aws_sdk_s3::error::ErrorMetadata::builder()
                        .code("InternalError")
                        .build(),
                )
            });
        let complete = mock!(aws_sdk_s3::Client::complete_multipart_upload)
            .then_output(|| CompleteMultipartUploadOutput::builder().build());
        let (create, part, abort) = (
            create_rule(),
            part_rule(Arc::new(Mutex::new(Vec::new()))),
            abort_rule(),
        );
        let copied = copier(&[&create, &fails, &part, &complete, &abort])
            .copy("src", "dst", 12 * GIB)
            .await;
        assert!(copied.is_err());
        assert_eq!(abort.num_calls(), 1);
        assert_eq!(complete.num_calls(), 0);
        Ok(())
    }

    #[rstest]
    #[case::unreserved(
        "root/vault-staging/user_1/a-b.c~d",
        "root/vault-staging/user_1/a-b.c~d"
    )]
    #[case::space("v 1/src", "v%201/src")]
    #[case::non_ascii("é", "%C3%A9")]
    fn percent_encode_keeps_the_unreserved_characters(#[case] key: &str, #[case] expected: &str) {
        assert_eq!(percent_encode(key), expected);
    }

    #[test]
    fn each_staged_write_logs_one_promote_event() -> anyhow::Result<()> {
        let (_backend, store) = vault_store();
        pipette_store_testkit::assert_vault_staged_write_logs_once(&store)
    }

    /// Over [`COPY_PART`], a promote goes through [`PartCopier`]. Each mock
    /// matches only the blob key, so a request for another key fails.
    #[tokio::test]
    async fn a_large_promote_copies_from_staging_to_the_blob_key() -> anyhow::Result<()> {
        let owner = pipette_store_testkit::a_vault_resource()?.owner;
        let sha256 = pipette_store_testkit::vault_blob_sha256()?;
        let dest = format!("root/vault-resources/{}/blobs/{sha256}", owner.label());
        let (to_create, to_part, to_complete) = (dest.clone(), dest.clone(), dest);
        let create = mock!(aws_sdk_s3::Client::create_multipart_upload)
            .match_requests(move |req| req.key() == Some(to_create.as_str()))
            .then_output(|| {
                CreateMultipartUploadOutput::builder()
                    .upload_id("upload-1")
                    .build()
            });
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorded = seen.clone();
        let part = mock!(aws_sdk_s3::Client::upload_part_copy)
            .match_requests(move |req| req.key() == Some(to_part.as_str()))
            .then_compute_output(move |req| {
                if let Ok(mut recorded) = recorded.lock() {
                    recorded.push(req.copy_source().unwrap_or_default().to_string());
                }
                UploadPartCopyOutput::builder()
                    .copy_part_result(CopyPartResult::builder().e_tag("etag").build())
                    .build()
            });
        let complete = mock!(aws_sdk_s3::Client::complete_multipart_upload)
            .match_requests(move |req| req.key() == Some(to_complete.as_str()))
            .then_output(|| CompleteMultipartUploadOutput::builder().build());
        let (_backend, store) = vault_store_with(copier(&[&create, &part, &complete]));

        let staging = store.staging_path(&owner);
        let written = store
            .promote_staged(&staging, &owner, sha256.clone(), COPY_PART + 1)
            .await?;
        assert_eq!(
            written,
            StagedWrite::Stored {
                sha256: sha256.clone(),
                bytes: COPY_PART + 1,
                key: blob_key(&owner, &sha256)?,
            }
        );
        let source = format!("test-bucket/{staging}");
        let seen = seen
            .lock()
            .map_err(|_| anyhow::anyhow!("poisoned"))?
            .clone();
        assert_eq!(seen, vec![source.clone(), source]);
        assert_eq!(complete.num_calls(), 1);
        Ok(())
    }

    /// The record sits outside the blob prefix and the blob inside it: that
    /// split is the security boundary (`pipette_store::VaultStore`).
    #[tokio::test]
    async fn vault_keys_are_pinned() -> anyhow::Result<()> {
        let (backend, store) = vault_store();
        let resource = pipette_store_testkit::a_vault_resource()?;
        store.put_resource(&resource).await?;
        assert!(
            backend
                .get(&ObjPath::from(format!(
                    "root/vault/{}.json",
                    resource.resource_id
                )))
                .await
                .is_ok(),
            "the record must land under vault/, outside the blob prefix"
        );
        assert_eq!(
            store.blob_path(&resource.object_key),
            ObjPath::from(format!(
                "root/vault-resources/{}/blobs/{}",
                resource.owner.label(),
                resource.sha256
            )),
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_refused_blob_leaves_no_object() -> anyhow::Result<()> {
        let (backend, store) = vault_store();
        let resource = pipette_store_testkit::a_vault_resource()?;
        let written = store
            .put_blob(
                &resource.object_key,
                &resource.sha256,
                pipette_store_testkit::blob_body(b"other weights"),
            )
            .await?;
        assert!(matches!(written, BlobWrite::Mismatch { .. }), "{written:?}");
        let listed: Vec<_> = backend
            .list(Some(&ObjPath::from("root/vault-resources")))
            .try_collect()
            .await?;
        assert!(listed.is_empty(), "{listed:?}");
        Ok(())
    }
}
