//! Vault resources: the private artifacts mgmt serves to one principal. A record
//! is an index entry plus a gate, never the bytes, and is immutable: a plan
//! carries its id in a URL, so it must never come to mean something else.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use pipette_plan_types::{
    FileName, NonEmptyTrimmedString, PrincipalOwner, Sha256, Sha256Error, VaultArtifact,
    VaultEntry, VaultFileFormat, VaultFileRole, VaultOrigin, VaultResourceId, VaultStoredFile,
    VaultUploadFile,
};

use crate::types::Mint;

/// Longest object key this accepts. S3 allows 1024 bytes for a whole key, and
/// the store prepends a prefix to everything here.
const OBJECT_KEY_MAX_BYTES: usize = 900;

/// Who a vault resource belongs to, and therefore which clients may fetch it.
pub type VaultOwner = PrincipalOwner;

/// The one key prefix an owner's artifacts may sit under, trailing slash
/// included. **This binds a record to the bytes it may name**: any principal can
/// see that the prefix holds objects, so the owner's id as first segment is the
/// entitlement. The slash keeps `org_0195ab` from prefixing `org_0195abcdef/`.
pub fn owner_key_prefix(owner: &VaultOwner) -> String {
    format!("{}/", owner.label())
}

/// Where `owner`'s blob with `sha256` is stored: `{principal_id}/blobs/{sha256}`,
/// under the owner's own segment and named by the digest its bytes were checked
/// against when written.
pub fn blob_key(
    owner: &VaultOwner,
    sha256: &Sha256,
) -> Result<VaultObjectKey, VaultObjectKeyError> {
    VaultObjectKey::try_new(format!("{}{sha256}", blob_dir(owner)))
}

/// The directory of `owner`'s blobs, `{principal_id}/blobs/`, trailing slash
/// included. Every [`blob_key`] of `owner` is in it.
pub fn blob_dir(owner: &VaultOwner) -> String {
    format!("{}blobs/", owner_key_prefix(owner))
}

/// Whether `key` is a [`blob_key`]: `{principal_id}/blobs/{sha256}`. A record
/// from before uploads went through mgmt may name a file of another shape,
/// which only its record accounts for.
pub fn is_blob_key(key: &VaultObjectKey) -> bool {
    let mut segments = key.as_ref().split('/');
    matches!(
        (segments.next(), segments.next(), segments.next(), segments.next()),
        (Some(principal), Some("blobs"), Some(sha256), None)
            if principal.parse::<pipette_plan_types::VaultPrincipalId>().is_ok()
                && Sha256::try_new(sha256.to_string()).is_ok()
    )
}

/// A running sha256 and length over a blob's bytes as they stream past, so a
/// store can decide whether to complete a write before the blob exists.
#[derive(Default)]
pub struct BlobDigest {
    hasher: sha2::Sha256,
    bytes: u64,
}

impl BlobDigest {
    pub fn update(&mut self, chunk: &[u8]) {
        use sha2::Digest as _;
        self.hasher.update(chunk);
        self.bytes += chunk.len() as u64;
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// The digest and the length of everything seen.
    pub fn finish(self) -> Result<(Sha256, u64), Sha256Error> {
        use sha2::Digest as _;
        let digest = Sha256::try_new(hex::encode(self.hasher.finalize()))?;
        Ok((digest, self.bytes))
    }
}

/// How many leading bytes [`looks_like`] needs: a tar header's `ustar` magic
/// sits at offset 257.
pub const SNIFF_BYTES: usize = 512;

/// Whether a file's first bytes are the `format` its role requires: a gguf, or
/// a gzip, zip or tar archive. What an archive holds is taken on trust.
pub fn looks_like(format: VaultFileFormat, head: &[u8]) -> bool {
    match format {
        VaultFileFormat::Gguf => head.starts_with(b"GGUF"),
        VaultFileFormat::Archive => {
            head.starts_with(&[0x1f, 0x8b])
                || head.starts_with(b"PK\x03\x04")
                || head.get(257..262) == Some(b"ustar".as_slice())
        }
    }
}

/// Where the bytes sit, **relative to the vault prefix** the store owns, and
/// traversal-free, so no record can name an object outside it.
#[nutype::nutype(
    sanitize(trim),
    validate(with = validate_object_key, error = VaultObjectKeyError),
    derive(
        Debug,
        Clone,
        PartialEq,
        Eq,
        PartialOrd,
        Ord,
        Hash,
        AsRef,
        Display,
        Into,
        Serialize,
        Deserialize,
        TryFrom,
    ),
)]
pub struct VaultObjectKey(String);

/// Construction error for [`VaultObjectKey`].
#[derive(Debug, thiserror::Error)]
pub enum VaultObjectKeyError {
    #[error("must not be empty")]
    Empty,
    #[error("must be at most {OBJECT_KEY_MAX_BYTES} bytes")]
    TooLong,
    #[error("must not contain whitespace, control characters or a backslash")]
    BadCharacter,
    #[error("must be a relative key with no empty, `.` or `..` segment")]
    NotRelative,
    #[error("must not repeat the `{0}` prefix the store adds")]
    Prefixed(&'static str),
}

/// The prefix the store prepends. Spelled here so the validator can refuse a
/// key that already carries it, which would otherwise silently nest.
pub const VAULT_OBJECT_PREFIX: &str = "vault-resources/";

/// The prefix of the import entries ([`VaultImport`]), a sibling of
/// [`VAULT_OBJECT_PREFIX`].
pub const VAULT_IMPORT_PREFIX: &str = "vault-imports/";

fn validate_object_key(s: &str) -> Result<(), VaultObjectKeyError> {
    if s.is_empty() {
        return Err(VaultObjectKeyError::Empty);
    }
    if s.len() > OBJECT_KEY_MAX_BYTES {
        return Err(VaultObjectKeyError::TooLong);
    }
    if s.chars()
        .any(|c| c.is_whitespace() || c.is_control() || c == '\\')
    {
        return Err(VaultObjectKeyError::BadCharacter);
    }
    if s.starts_with('/')
        || s.split('/')
            .any(|seg| seg.is_empty() || seg == "." || seg == "..")
    {
        return Err(VaultObjectKeyError::NotRelative);
    }
    if s.starts_with(VAULT_OBJECT_PREFIX) {
        return Err(VaultObjectKeyError::Prefixed(VAULT_OBJECT_PREFIX));
    }
    Ok(())
}

/// A vault resource — the body of `vault/{resource_id}.json` in the `[storage]`
/// backend. One per uploaded file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultResource {
    pub resource_id: VaultResourceId,
    /// What to print for a human. Free-text; nothing keys on it.
    pub name: NonEmptyTrimmedString,
    /// Who owns this, and therefore which devices may fetch the bytes. Fixed at
    /// registration and read as it stands, like a campaign's owner.
    pub owner: VaultOwner,
    pub object_key: VaultObjectKey,
    /// What the bytes hash to, checked as mgmt wrote the blob. A client still
    /// checks what it downloads: other bytes mean the object was replaced
    /// behind the record.
    pub sha256: Sha256,
    /// The blob's size when uploaded, so a listing costs no object read.
    /// The served `Content-Length` comes from the store.
    pub bytes: u64,
    /// The artifact this file was uploaded as, naming every file of it; this
    /// record's own file is the one with its id. `None` on records from before
    /// uploads went through mgmt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<VaultArtifact<VaultStoredFile>>,
    /// The public source that this file is a copy of. The value is `None` for a
    /// file that a user uploaded. mgmt cannot check where uploaded bytes came
    /// from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<VaultOrigin>,
    pub created_at: DateTime<Utc>,
}

impl From<VaultResource> for pipette_plan_types::VaultResource {
    /// The record without its storage key: destructured, so a field added to
    /// the record is a decision here rather than a leak.
    fn from(resource: VaultResource) -> Self {
        let VaultResource {
            resource_id,
            name,
            owner,
            object_key: _,
            sha256,
            bytes,
            artifact,
            origin,
            created_at,
        } = resource;
        Self {
            resource_id,
            name,
            owner,
            sha256,
            bytes,
            artifact,
            origin,
            created_at,
        }
    }
}

/// One uploaded file found in the store: what the upload named it, and its
/// size there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundBlob {
    pub file: VaultUploadFile,
    pub bytes: u64,
}

/// The records one import of a public model wrote, at
/// `vault-imports/{principal_id}/{sha256}.json`; a retry reuses them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultImport {
    pub resource_ids: Vec<VaultResourceId>,
}

/// One file the server fetched from a public source and stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedBlob {
    pub filename: FileName,
    pub sha256: Sha256,
    pub bytes: u64,
    pub origin: VaultOrigin,
}

/// Why [`VaultResource::from_import`] could not make a record: always a caller
/// bug.
#[derive(Debug, thiserror::Error)]
pub enum ImportRecordError {
    #[error("the record name {0}")]
    Name(#[from] pipette_plan_types::EmptyStringError),
    #[error("the object key {0}")]
    Key(#[from] VaultObjectKeyError),
}

/// One file of a new record set, before its id is minted.
struct NewFile {
    filename: FileName,
    sha256: Sha256,
    bytes: u64,
    name: NonEmptyTrimmedString,
    origin: Option<VaultOrigin>,
}

impl VaultResource {
    /// The records one upload makes, one per file, each keeping the whole
    /// artifact. Every record has the upload's `name`.
    pub fn from_upload(
        name: NonEmptyTrimmedString,
        owner: VaultOwner,
        artifact: VaultArtifact<FoundBlob>,
        created_at: DateTime<Utc>,
    ) -> Result<Vec<Self>, VaultObjectKeyError> {
        let files = artifact.map(|_, blob| NewFile {
            filename: blob.file.filename,
            sha256: blob.file.sha256,
            bytes: blob.bytes,
            name: name.clone(),
            origin: None,
        });
        Self::mint(owner, files, created_at)
    }

    /// The records that one import makes, one for each file. Each record
    /// keeps the whole artifact and the origin of its own file. The display
    /// form of that origin is the record's name.
    pub fn from_import(
        owner: VaultOwner,
        artifact: VaultArtifact<ImportedBlob>,
        created_at: DateTime<Utc>,
    ) -> Result<Vec<Self>, ImportRecordError> {
        let files = artifact.try_map(|_, blob| {
            Ok::<_, ImportRecordError>(NewFile {
                name: NonEmptyTrimmedString::try_new(blob.origin.to_string())?,
                filename: blob.filename,
                sha256: blob.sha256,
                bytes: blob.bytes,
                origin: Some(blob.origin),
            })
        })?;
        Ok(Self::mint(owner, files, created_at)?)
    }

    /// The records of `artifact`, one per file, each keeping the whole
    /// artifact. This function mints the ids, so no caller can choose one
    /// and collide ([`crate::campaign::NewCampaign::new`]).
    fn mint(
        owner: VaultOwner,
        artifact: VaultArtifact<NewFile>,
        created_at: DateTime<Utc>,
    ) -> Result<Vec<Self>, VaultObjectKeyError> {
        let minted = artifact.map(|_, new| {
            let file = VaultStoredFile {
                resource_id: VaultResourceId::mint(),
                filename: new.filename,
                sha256: new.sha256,
            };
            (file, new.bytes, new.name, new.origin)
        });
        let stored = minted.clone().map(|_, (file, ..)| file);
        minted
            .files()
            .into_iter()
            .map(|(_, (file, bytes, name, origin))| {
                Ok(Self {
                    resource_id: file.resource_id.clone(),
                    name: name.clone(),
                    owner: owner.clone(),
                    object_key: blob_key(&owner, &file.sha256)?,
                    sha256: file.sha256.clone(),
                    bytes: *bytes,
                    artifact: Some(stored.clone()),
                    origin: origin.clone(),
                    created_at,
                })
            })
            .collect()
    }

    /// The plan entry that fetches this record's artifact, or `None` for a
    /// record registered before uploads went through mgmt.
    pub fn entry(&self) -> Option<VaultEntry> {
        let artifact = self.artifact.clone()?;
        // Every record of an upload has its name, so this one speaks for all.
        let custom_label = custom_label(&[self]).ok().cloned();
        Some(artifact.into_entry(self.owner.clone().into(), custom_label))
    }

    /// Which file of its artifact this record is, and its role there.
    pub fn own_file(&self) -> Option<(VaultFileRole, &VaultStoredFile)> {
        self.artifact
            .as_ref()?
            .files()
            .into_iter()
            .find(|(_, file)| file.resource_id == self.resource_id)
    }
}

/// Why a vault source's records give it no [`custom_label`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoCustomLabel {
    NoRecords,
    /// An import names each file after its own source.
    Imported,
    NamesDiffer,
}

/// The `custom_label` of a vault source whose files are `records`: their one
/// name, when every one is an upload's.
pub fn custom_label<'a>(
    records: &[&'a VaultResource],
) -> Result<&'a NonEmptyTrimmedString, NoCustomLabel> {
    if records.iter().any(|record| record.origin.is_some()) {
        return Err(NoCustomLabel::Imported);
    }
    let (first, rest) = records.split_first().ok_or(NoCustomLabel::NoRecords)?;
    if rest.iter().any(|record| record.name != first.name) {
        return Err(NoCustomLabel::NamesDiffer);
    }
    Ok(&first.name)
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use serde_json::json;

    use pipette_plan_types::{OrgId, UserId, VaultPrincipalId};

    use super::*;

    fn digest(c: char) -> anyhow::Result<Sha256> {
        Ok(Sha256::try_new(c.to_string().repeat(64))?)
    }

    fn a_user() -> VaultOwner {
        VaultOwner::User {
            user_id: UserId::mint(),
        }
    }

    /// Stands in for mgmt finding each named blob in the store.
    fn found(raw: serde_json::Value) -> anyhow::Result<VaultArtifact<FoundBlob>> {
        let artifact: VaultArtifact<VaultUploadFile> = serde_json::from_value(raw)?;
        Ok(artifact.map(|_, file| FoundBlob { file, bytes: 4096 }))
    }

    fn a_vision_upload(owner: VaultOwner) -> anyhow::Result<Vec<VaultResource>> {
        let artifact = found(json!({
            "type": "gguf_vision",
            "model": {"filename": "LFM2-VL-3B-Q4_K_M.gguf", "sha256": "a".repeat(64)},
            "mmproj": {"filename": "mmproj-LFM2-VL-3B-F16.gguf", "sha256": "b".repeat(64)},
        }))?;
        Ok(VaultResource::from_upload(
            NonEmptyTrimmedString::try_new("acme LFM2-VL-3B q4")?,
            owner,
            artifact,
            Utc::now(),
        )?)
    }

    /// A key may not climb out of the prefix that bounds this keyspace, and may
    /// not carry that prefix itself. This is the containment the gate rests on.
    #[rstest]
    #[case::plain("acme/lfm2.gguf", true)]
    #[case::nested("acme/2026-09/lfm2.gguf", true)]
    #[case::traversal("acme/../../etc/passwd", false)]
    #[case::parent_only("../secrets", false)]
    #[case::absolute("/acme/lfm2.gguf", false)]
    #[case::double_slash("acme//lfm2.gguf", false)]
    #[case::current_dir("acme/./lfm2.gguf", false)]
    #[case::backslash("acme\\lfm2.gguf", false)]
    #[case::whitespace("acme/lfm2 final.gguf", false)]
    #[case::already_prefixed("vault-resources/acme/lfm2.gguf", false)]
    #[case::empty("", false)]
    fn an_object_key_stays_inside_the_prefix(#[case] raw: &str, #[case] accepted: bool) {
        assert_eq!(VaultObjectKey::try_new(raw).is_ok(), accepted, "{raw:?}");
    }

    /// A blob sits under its owner's own segment, named by its digest, and the
    /// trailing slash keeps one id from prefixing a longer one.
    #[test]
    fn a_blob_key_is_its_owner_s_and_its_digest_s() -> anyhow::Result<()> {
        let org = VaultOwner::Org {
            org_id: OrgId::try_new("org_0195ab")?,
        };
        let key = blob_key(&org, &digest('a')?)?;
        assert_eq!(key.as_ref(), format!("org_0195ab/blobs/{}", "a".repeat(64)));
        assert!(!key.as_ref().starts_with("org_0195abcdef"));
        Ok(())
    }

    /// The first bytes of each format, and of files that are neither.
    #[rstest]
    #[case::gguf(VaultFileFormat::Gguf, b"GGUF\x03\x00\x00\x00".to_vec(), true)]
    #[case::gguf_is_not_an_archive(VaultFileFormat::Archive, b"GGUF\x03\x00\x00\x00".to_vec(), false)]
    #[case::gzip(VaultFileFormat::Archive, vec![0x1f, 0x8b, 0x08, 0x00], true)]
    #[case::zip(VaultFileFormat::Archive, b"PK\x03\x04\x14\x00".to_vec(), true)]
    #[case::tar(VaultFileFormat::Archive, tar_head(), true)]
    #[case::tar_is_not_a_gguf(VaultFileFormat::Gguf, tar_head(), false)]
    #[case::digest_sidecar(VaultFileFormat::Archive, b"9f86d081884c7d65  llama.zip\n".to_vec(), false)]
    #[case::empty(VaultFileFormat::Gguf, vec![], false)]
    #[case::short_of_the_tar_magic(VaultFileFormat::Archive, vec![0; 260], false)]
    fn a_file_is_recognized_by_its_first_bytes(
        #[case] format: VaultFileFormat,
        #[case] head: Vec<u8>,
        #[case] expected: bool,
    ) {
        assert_eq!(looks_like(format, &head), expected);
    }

    fn tar_head() -> Vec<u8> {
        let mut head = vec![0; SNIFF_BYTES];
        head.splice(257..262, *b"ustar");
        head
    }

    /// An upload makes one record per file, each under its owner's blob key,
    /// each keeping the whole artifact, and each knowing which file it is.
    #[test]
    fn an_upload_makes_one_record_per_file() -> anyhow::Result<()> {
        let owner = a_user();
        let records = a_vision_upload(owner.clone())?;
        let [model, mmproj] = records.as_slice() else {
            anyhow::bail!("expected two records, got {records:?}");
        };
        assert_ne!(model.resource_id, mmproj.resource_id);
        assert_eq!(
            model.own_file().map(|(role, _)| role),
            Some(VaultFileRole::Model)
        );
        assert_eq!(
            mmproj
                .own_file()
                .map(|(role, file)| (role, file.filename.as_ref())),
            Some((VaultFileRole::Mmproj, "mmproj-LFM2-VL-3B-F16.gguf"))
        );
        assert_eq!(model.object_key, blob_key(&owner, &digest('a')?)?);
        assert_eq!(mmproj.sha256, digest('b')?);
        assert_eq!(model.bytes, 4096);
        assert_eq!(model.artifact, mmproj.artifact);
        Ok(())
    }

    /// Either record of an upload rebuilds the same plan entry, which names
    /// both files for the owner under the upload's name.
    #[test]
    fn every_record_of_an_upload_rebuilds_its_entry() -> anyhow::Result<()> {
        let owner = VaultOwner::Org {
            org_id: OrgId::mint(),
        };
        let records = a_vision_upload(owner.clone())?;
        let entries: Vec<_> = records.iter().map(VaultResource::entry).collect();
        let Some(Some(VaultEntry::Model(model))) = entries.first() else {
            anyhow::bail!("expected a model entry, got {entries:?}");
        };
        assert!(
            entries
                .iter()
                .all(|e| e == entries.first().unwrap_or(&None))
        );
        let model = serde_json::to_value(model)?;
        assert_eq!(model["principal_id"], owner.label());
        assert_eq!(model["custom_label"], "acme LFM2-VL-3B q4");
        let model = model.to_string();
        let missing: Vec<String> = records
            .iter()
            .map(|record| format!("/vault/resources/{}/content", record.resource_id))
            .filter(|path| !model.contains(path.as_str()))
            .collect();
        assert!(missing.is_empty(), "{missing:?} missing from {model}");
        Ok(())
    }

    /// A record written before uploads went through mgmt still reads, and says
    /// nothing about what it is.
    #[test]
    fn a_record_without_an_artifact_still_reads() -> anyhow::Result<()> {
        let mut value = serde_json::to_value(a_vision_upload(a_user())?.first())?;
        value
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("a record is an object"))?
            .remove("artifact");
        let record: VaultResource = serde_json::from_value(value)?;
        assert_eq!(record.artifact, None);
        assert_eq!(record.entry(), None);
        assert_eq!(record.own_file(), None);
        Ok(())
    }

    /// An owner and its bare plan-dialect id convert both ways.
    #[rstest]
    #[case::user(a_user())]
    #[case::org(VaultOwner::Org { org_id: OrgId::mint() })]
    fn an_owner_is_its_principal_id(#[case] owner: VaultOwner) {
        let principal = VaultPrincipalId::from(owner.clone());
        assert_eq!(principal.to_string(), owner.label());
        assert_eq!(VaultOwner::from(principal), owner);
    }

    /// The stored shape, pinned in both directions: a renamed tag or field
    /// would be invisible to a round trip and would strand every record written.
    #[test]
    fn the_owner_shape_is_pinned() -> anyhow::Result<()> {
        let records = a_vision_upload(a_user())?;
        let resource = records
            .first()
            .ok_or_else(|| anyhow::anyhow!("no record"))?;
        let value = serde_json::to_value(resource)?;
        assert_eq!(value["owner"]["type"], "user");
        assert_eq!(
            value["owner"]["user_id"],
            resource.owner.label(),
            "the id sits beside the tag"
        );
        assert!(value["owner"].get("kind").is_none());

        let org_id = OrgId::mint();
        let mut value = value;
        value["owner"] = json!({ "type": "org", "org_id": org_id.as_str() });
        let read: VaultResource = serde_json::from_value(value)?;
        assert_eq!(read.owner.org_id(), Some(&org_id));
        Ok(())
    }

    #[test]
    fn a_resource_round_trips() -> anyhow::Result<()> {
        a_vision_upload(a_user())?
            .into_iter()
            .try_for_each(|resource| {
                let read: VaultResource = serde_json::from_value(serde_json::to_value(&resource)?)?;
                assert_eq!(read, resource);
                anyhow::Ok(())
            })
    }

    /// An upload records no origin, and a record without one stores no
    /// `origin` key.
    #[test]
    fn an_upload_has_no_origin() -> anyhow::Result<()> {
        a_vision_upload(a_user())?
            .into_iter()
            .try_for_each(|resource| {
                assert_eq!(resource.origin, None);
                assert!(serde_json::to_value(&resource)?.get("origin").is_none());
                anyhow::Ok(())
            })
    }

    /// An import makes one record per file. Each record carries the origin of
    /// its own file, is named by that origin, and keeps the whole artifact.
    /// Its files share no name, so its entry carries no label.
    #[test]
    fn an_import_names_each_record_by_its_own_origin() -> anyhow::Result<()> {
        let owner = a_user();
        let origin = |path: &str| -> anyhow::Result<VaultOrigin> {
            Ok(serde_json::from_value(json!({
                "source": "huggingface", "org": "LiquidAI", "repo_name": "LFM2-VL-3B-GGUF",
                "commit": "a".repeat(40), "path": path,
            }))?)
        };
        let blob = |name: &str, c: char| -> anyhow::Result<ImportedBlob> {
            Ok(ImportedBlob {
                filename: FileName::try_new(name)?,
                sha256: digest(c)?,
                bytes: 4096,
                origin: origin(name)?,
            })
        };
        let artifact = VaultArtifact::Model(pipette_plan_types::VaultModel::GgufVision {
            model: blob("m.gguf", 'a')?,
            mmproj: blob("p.gguf", 'b')?,
        });
        let records = VaultResource::from_import(owner.clone(), artifact, Utc::now())?;
        let [model, mmproj] = records.as_slice() else {
            anyhow::bail!("expected two records, got {records:?}");
        };
        assert_eq!(model.origin, Some(origin("m.gguf")?));
        assert_eq!(mmproj.origin, Some(origin("p.gguf")?));
        assert_eq!(
            mmproj.name.as_ref(),
            format!("LiquidAI/LFM2-VL-3B-GGUF@{}:p.gguf", "a".repeat(40))
        );
        assert_eq!(model.object_key, blob_key(&owner, &digest('a')?)?);
        assert_eq!(model.artifact, mmproj.artifact);
        assert_eq!(
            mmproj.own_file().map(|(role, _)| role),
            Some(VaultFileRole::Mmproj)
        );
        let entry = serde_json::to_value(model.entry())?;
        assert_eq!(entry.pointer("/model/custom_label"), None, "{entry}");
        Ok(())
    }

    /// A source takes the name its uploads share, or says why it has none.
    #[test]
    fn a_source_takes_the_name_its_uploads_share() -> anyhow::Result<()> {
        let upload = a_vision_upload(a_user())?;
        let [model, mmproj] = upload.as_slice() else {
            anyhow::bail!("expected two records, got {upload:?}");
        };
        assert_eq!(
            custom_label(&[model, mmproj]).map(ToString::to_string),
            Ok("acme LFM2-VL-3B q4".to_string())
        );
        let renamed = VaultResource {
            name: NonEmptyTrimmedString::try_new("another upload")?,
            ..mmproj.clone()
        };
        assert_eq!(
            custom_label(&[model, &renamed]),
            Err(NoCustomLabel::NamesDiffer)
        );
        let imported = VaultResource {
            origin: Some(serde_json::from_value(json!({
                "source": "huggingface", "org": "LiquidAI", "repo_name": "LFM2-VL-3B-GGUF",
                "commit": "a".repeat(40), "path": "p.gguf",
            }))?),
            ..mmproj.clone()
        };
        assert_eq!(
            custom_label(&[model, &imported]),
            Err(NoCustomLabel::Imported)
        );
        assert_eq!(custom_label(&[]), Err(NoCustomLabel::NoRecords));
        Ok(())
    }

    /// The wire record is the stored one minus its key.
    #[test]
    fn a_vault_record_serializes_without_its_key() -> anyhow::Result<()> {
        let owner = PrincipalOwner::User {
            user_id: UserId::try_new("user_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e")?,
        };
        let sha256 = Sha256::try_new("a".repeat(64))?;
        let resource = VaultResource {
            resource_id: VaultResourceId::try_new("vault_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e")?,
            name: NonEmptyTrimmedString::try_new("LFM2")?,
            object_key: blob_key(&owner, &sha256)?,
            owner,
            sha256,
            bytes: 9,
            artifact: None,
            origin: None,
            created_at: Utc::now(),
        };
        let mut stored = serde_json::to_value(&resource)?;
        if let Some(fields) = stored.as_object_mut() {
            fields.remove("object_key");
        }
        assert_eq!(
            serde_json::to_value(pipette_plan_types::VaultResource::from(resource))?,
            stored
        );
        Ok(())
    }
}
