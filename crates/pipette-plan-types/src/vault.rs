//! `/api/v1/vault`: the records of stored files, the upload that registers
//! them, and the variants an upload takes.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::owners::PrincipalOwner;
use crate::{
    FileName, GitOid, HfOrg, HfRepoName, Model, ModelKey, ModelParams, NonEmptyTrimmedString,
    RepoSubpath, Sha256, VaultArtifact, VaultEntry, VaultFileFormat, VaultFileRole,
    VaultResourceId, VaultStoredFile, empty_as_none,
};

/// The public source a stored file is a copy of.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum VaultOrigin {
    /// A HuggingFace repository at one commit, or one file in it.
    #[serde(rename = "huggingface")]
    HuggingFace {
        #[cfg_attr(feature = "schemars", schemars(with = "String"))]
        org: HfOrg,
        #[cfg_attr(feature = "schemars", schemars(with = "String"))]
        repo_name: HfRepoName,
        #[cfg_attr(feature = "schemars", schemars(with = "String"))]
        commit: GitOid,
        /// The file in the repository; absent for the whole repository at that
        /// commit, packed into one archive.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "schemars", schemars(with = "Option<String>"))]
        path: Option<RepoSubpath>,
    },
    /// One asset of a GitHub release.
    GithubRelease {
        #[cfg_attr(feature = "schemars", schemars(with = "String"))]
        owner: GithubOwner,
        #[cfg_attr(feature = "schemars", schemars(with = "String"))]
        repo: GithubRepoName,
        #[cfg_attr(feature = "schemars", schemars(with = "String"))]
        commit: GitOid,
        #[cfg_attr(feature = "schemars", schemars(with = "String"))]
        asset: FileName,
    },
}

impl std::fmt::Display for VaultOrigin {
    /// For HuggingFace, the display form is `org/repo@commit[:path]`, as
    /// [`HfRepo::reference`](crate::HfRepo::reference) writes it. For GitHub, it is
    /// `github.com/owner/repo@commit:asset`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VaultOrigin::HuggingFace {
                org,
                repo_name,
                commit,
                path,
            } => {
                write!(f, "{org}/{repo_name}@{commit}")?;
                path.as_ref().map_or(Ok(()), |path| write!(f, ":{path}"))
            }
            VaultOrigin::GithubRelease {
                owner,
                repo,
                commit,
                asset,
            } => write!(f, "github.com/{owner}/{repo}@{commit}:{asset}"),
        }
    }
}

/// GitHub's own rule for an account or organization name: 1 to 39 letters,
/// digits or single hyphens, with no hyphen at either end.
fn is_github_owner(name: &str) -> bool {
    (1..=39).contains(&name.len())
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--")
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// A GitHub account or organization name.
#[nutype::nutype(
    validate(predicate = is_github_owner),
    derive(
        Debug,
        Clone,
        PartialEq,
        Eq,
        Hash,
        Serialize,
        Deserialize,
        AsRef,
        Display
    )
)]
pub struct GithubOwner(String);

/// GitHub's own rule for a repository name: 1 to 100 letters, digits, `.`,
/// `_` or `-`, and never `.` or `..`.
fn is_github_repo_name(name: &str) -> bool {
    (1..=100).contains(&name.len())
        && name != "."
        && name != ".."
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// A GitHub repository name, without its owner.
#[nutype::nutype(
    validate(predicate = is_github_repo_name),
    derive(
        Debug,
        Clone,
        PartialEq,
        Eq,
        Hash,
        Serialize,
        Deserialize,
        AsRef,
        Display
    )
)]
pub struct GithubRepoName(String);

/// A record as every answer carries it, without where its bytes are stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct VaultResource {
    #[cfg_attr(feature = "schemars", schemars(with = "String"))]
    pub resource_id: VaultResourceId,
    pub name: NonEmptyTrimmedString,
    pub owner: PrincipalOwner,
    /// The file's digest, checked when it was uploaded.
    #[cfg_attr(feature = "schemars", schemars(with = "String"))]
    pub sha256: Sha256,
    pub bytes: u64,
    /// The variant the file was uploaded as, every file of it; absent on a
    /// record registered before uploads went through mgmt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schemars", schemars(with = "Option<schema::VaultArtifact>"))]
    pub artifact: Option<VaultArtifact<VaultStoredFile>>,
    /// The public source the file is a copy of; absent for an upload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<VaultOrigin>,
    pub created_at: DateTime<Utc>,
}

/// The plan entry an upload makes, as JSON and as the inline table a plan's
/// `models` or `runtimes` lists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct VaultEntryWithToml {
    pub entry: VaultEntry,
    pub toml: String,
}

impl TryFrom<VaultEntry> for VaultEntryWithToml {
    type Error = toml::ser::Error;

    /// The entry with the inline table a plan lists it as.
    fn try_from(entry: VaultEntry) -> Result<Self, Self::Error> {
        let toml = entry.to_toml()?;
        Ok(Self { entry, toml })
    }
}

/// Whether the bytes a record names are still stored, as the store answers
/// now: a file can go missing without mgmt hearing of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ObjectState {
    Present,
    /// There, but not the size the record was written at: replaced behind the
    /// record, so the digest no longer describes it.
    Resized {
        bytes: u64,
    },
    Missing,
    /// The store would not answer. Only a listing's row says so, where one
    /// file costs one cell rather than the page.
    Unknown,
}

impl ObjectState {
    /// The store's answer against the size the record pinned.
    pub fn of(recorded: u64, found: Option<u64>) -> Self {
        match found {
            Some(bytes) if bytes == recorded => Self::Present,
            Some(bytes) => Self::Resized { bytes },
            None => Self::Missing,
        }
    }
}

/// `POST /api/v1/vault/resources`: the entry, and one record per file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct VaultResourcesUploaded {
    #[serde(flatten)]
    pub entry: VaultEntryWithToml,
    pub resources: Vec<VaultResource>,
}

/// A vault resource as its reads answer it: the record, for an uploaded one the
/// entry it belongs to, and what the reader may do.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct VaultResourceDetail {
    #[serde(flatten)]
    pub resource: VaultResource,
    #[serde(flatten, default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<VaultEntryWithToml>,
    /// The public descriptor a result from the entry's model is recorded
    /// under, where every file of it is a copy of a HuggingFace file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<Model>,
    /// Asked with `object=true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object: Option<ObjectState>,
    /// Whether the reader may delete it; present on every user's read, absent on a device's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub may_administer: Option<bool>,
    /// Absent on a device's read and for a runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_params: Option<ModelParamsDetail>,
}

/// The parameter counts a vault model's results take.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct ModelParamsDetail {
    /// The model key the counts are held under: the resource id of the
    /// model's main file.
    pub model_key: ModelKey,
    /// `null` while the key has no row.
    pub row: Option<ModelParams>,
    /// Whether the reader may write the row.
    pub may_set: bool,
}

/// What a user's read of the vault may add. A device's read ignores both.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct VaultReadQuery {
    /// The owners' names.
    #[serde(
        default,
        deserialize_with = "empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub names: Option<bool>,
    /// Whether each file is still stored, from a stat of its object.
    #[serde(
        default,
        deserialize_with = "empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub object: Option<bool>,
}

/// `PUT /api/v1/vault/blobs/…`: what was kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct VaultBlobStored {
    pub sha256: Sha256,
    pub bytes: u64,
}

/// One file field of a type: the role it fills in the upload body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct VaultTypeFile {
    pub role: VaultFileRole,
    pub optional: bool,
    pub format: VaultFileFormat,
}

/// One model or runtime type an upload takes, with the known flavors a runtime
/// suggests (empty for a model).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct VaultType {
    /// The type's plan tag, e.g. `gguf_vision`.
    #[serde(rename = "type")]
    pub type_name: String,
    pub description: String,
    pub files: Vec<VaultTypeFile>,
    pub flavors: Vec<String>,
}

/// The `GET /api/v1/vault/types` body: every model and runtime type an upload
/// takes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct VaultTypes {
    pub models: Vec<VaultType>,
    pub runtimes: Vec<VaultType>,
}

/// Mirrors of this crate's vault shapes for the app's schema bundle, which
/// their own types (generic, and keyed by file role) cannot derive one for.
#[cfg(feature = "schemars")]
pub(crate) mod schema {
    #![expect(
        dead_code,
        reason = "schema mirrors: only their schemas are read, so none is built"
    )]

    use schemars::JsonSchema;

    /// One stored file of a record's artifact, as [`crate::VaultStoredFile`]
    /// serializes.
    #[derive(JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub(crate) struct VaultStoredFile {
        pub(crate) resource_id: String,
        pub(crate) filename: String,
        pub(crate) sha256: String,
    }

    /// A record's artifact, as [`crate::VaultArtifact`] serializes: one field
    /// per file role.
    #[derive(JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub(crate) struct VaultArtifact {
        #[serde(rename = "type")]
        pub(crate) type_name: String,
        /// A runtime's.
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(with = "String")]
        pub(crate) flavor: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(with = "VaultStoredFile")]
        pub(crate) model: Option<VaultStoredFile>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(with = "VaultStoredFile")]
        pub(crate) mmproj: Option<VaultStoredFile>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(with = "VaultStoredFile")]
        pub(crate) draft: Option<VaultStoredFile>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(with = "VaultStoredFile")]
        pub(crate) dir_archive: Option<VaultStoredFile>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(with = "VaultStoredFile")]
        pub(crate) target_dir_archive: Option<VaultStoredFile>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(with = "VaultStoredFile")]
        pub(crate) draft_dir_archive: Option<VaultStoredFile>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(with = "VaultStoredFile")]
        pub(crate) archive: Option<VaultStoredFile>,
    }

    /// One file of an upload, as [`crate::VaultUploadFile`] reads it.
    #[derive(JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub(crate) struct VaultUploadFile {
        pub(crate) filename: String,
        pub(crate) sha256: String,
    }

    /// `POST /api/v1/vault/resources` request body,
    /// [`crate::UploadVaultResourcesRequest`]'s shape. `type` decides which
    /// roles it needs; `owner` absent is the uploader.
    #[derive(JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub(crate) struct UploadVaultResourcesRequest {
        /// Trimmed, non-empty, at most 4096 characters.
        pub(crate) name: String,
        #[serde(default)]
        #[schemars(with = "String")]
        pub(crate) owner: Option<String>,
        #[serde(rename = "type")]
        pub(crate) type_name: String,
        #[serde(default)]
        #[schemars(with = "String")]
        pub(crate) flavor: Option<String>,
        #[serde(default)]
        #[schemars(with = "VaultUploadFile")]
        pub(crate) model: Option<VaultUploadFile>,
        #[serde(default)]
        #[schemars(with = "VaultUploadFile")]
        pub(crate) mmproj: Option<VaultUploadFile>,
        #[serde(default)]
        #[schemars(with = "VaultUploadFile")]
        pub(crate) draft: Option<VaultUploadFile>,
        #[serde(default)]
        #[schemars(with = "VaultUploadFile")]
        pub(crate) dir_archive: Option<VaultUploadFile>,
        #[serde(default)]
        #[schemars(with = "VaultUploadFile")]
        pub(crate) target_dir_archive: Option<VaultUploadFile>,
        #[serde(default)]
        #[schemars(with = "VaultUploadFile")]
        pub(crate) draft_dir_archive: Option<VaultUploadFile>,
        #[serde(default)]
        #[schemars(with = "VaultUploadFile")]
        pub(crate) archive: Option<VaultUploadFile>,
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use serde_json::json;

    use super::*;

    /// Each origin kind keeps its stored shape and its display form.
    #[rstest]
    #[case::huggingface_file(
        json!({"source": "huggingface", "org": "LiquidAI", "repo_name": "LFM2-700M-GGUF", "commit": "a".repeat(40), "path": "LFM2-700M-Q4_0.gguf"}),
        format!("LiquidAI/LFM2-700M-GGUF@{}:LFM2-700M-Q4_0.gguf", "a".repeat(40))
    )]
    #[case::huggingface_repo(
        json!({"source": "huggingface", "org": "LiquidAI", "repo_name": "LFM2.5-350M-MLX-4bit", "commit": "b".repeat(40)}),
        format!("LiquidAI/LFM2.5-350M-MLX-4bit@{}", "b".repeat(40))
    )]
    #[case::github_asset(
        json!({"source": "github_release", "owner": "ggml-org", "repo": "llama.cpp", "commit": "c".repeat(40), "asset": "llama-b6000-bin-ubuntu-x64.zip"}),
        format!("github.com/ggml-org/llama.cpp@{}:llama-b6000-bin-ubuntu-x64.zip", "c".repeat(40))
    )]
    fn an_origin_keeps_its_shape(
        #[case] stored: serde_json::Value,
        #[case] display: String,
    ) -> anyhow::Result<()> {
        let origin: VaultOrigin = serde_json::from_value(stored.clone())?;
        assert_eq!(serde_json::to_value(&origin)?, stored);
        assert_eq!(origin.to_string(), display);
        Ok(())
    }

    /// A valid HuggingFace origin with `field` set to `value`. A null `value`
    /// removes the field.
    fn hf_with(field: &str, value: serde_json::Value) -> serde_json::Value {
        with(
            json!({"source": "huggingface", "org": "LiquidAI", "repo_name": "LFM2", "commit": "a".repeat(40)}),
            field,
            value,
        )
    }

    /// A valid GitHub release origin with `field` set to `value`. A null
    /// `value` removes the field.
    fn github_with(field: &str, value: serde_json::Value) -> serde_json::Value {
        with(
            json!({"source": "github_release", "owner": "acme", "repo": "llama.cpp", "commit": "a".repeat(40), "asset": "llama-cli.tar.gz"}),
            field,
            value,
        )
    }

    fn with(
        mut origin: serde_json::Value,
        field: &str,
        value: serde_json::Value,
    ) -> serde_json::Value {
        if let Some(fields) = origin.as_object_mut() {
            if value.is_null() {
                fields.remove(field);
            } else {
                fields.insert(field.to_string(), value);
            }
        }
        origin
    }

    /// The bases of [`hf_with`] and [`github_with`] parse, so each refusal
    /// below comes from the one field that its case changes.
    #[rstest]
    #[case::huggingface(hf_with("org", json!("LiquidAI")))]
    #[case::github(github_with("owner", json!("acme")))]
    fn a_refusal_base_parses(#[case] stored: serde_json::Value) -> anyhow::Result<()> {
        serde_json::from_value::<VaultOrigin>(stored)?;
        Ok(())
    }

    /// Malformed: no full commit, or a name in a form its source refuses.
    #[rstest]
    #[case::no_source(json!({"org": "LiquidAI", "repo_name": "LFM2", "commit": "a".repeat(40)}))]
    #[case::unknown_source(json!({"source": "url", "url": "https://example.com/m.gguf"}))]
    #[case::hf_without_commit(hf_with("commit", json!(null)))]
    #[case::hf_branch_not_commit(hf_with("commit", json!("main")))]
    #[case::hf_short_commit(hf_with("commit", json!("abc1234")))]
    #[case::hf_upper_case_commit(hf_with("commit", json!("A".repeat(40))))]
    #[case::hf_path_escapes(hf_with("path", json!("../m.gguf")))]
    #[case::github_without_asset(github_with("asset", json!(null)))]
    #[case::github_asset_with_slash(github_with("asset", json!("build/llama-cli.tar.gz")))]
    #[case::github_owner_leading_hyphen(github_with("owner", json!("-acme")))]
    #[case::github_owner_double_hyphen(github_with("owner", json!("ac--me")))]
    #[case::github_owner_too_long(github_with("owner", json!("a".repeat(40))))]
    #[case::github_owner_hyphenated_too_long(github_with("owner", json!("a-".repeat(20) + "a")))]
    #[case::github_owner_dot(github_with("owner", json!("ac.me")))]
    #[case::github_repo_dot_dot(github_with("repo", json!("..")))]
    #[case::github_repo_slash(github_with("repo", json!("llama/cpp")))]
    #[case::github_repo_too_long(github_with("repo", json!("a".repeat(101))))]
    fn a_malformed_origin_is_refused(#[case] stored: serde_json::Value) {
        assert!(
            serde_json::from_value::<VaultOrigin>(stored.clone()).is_err(),
            "{stored}"
        );
    }
}
