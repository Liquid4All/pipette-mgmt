//! Import a HuggingFace model into the vault, and give the model as its vault
//! arm. [`ImportPlan::resolve`] calls only the Hub, so a dry run downloads
//! nothing; [`ImportPlan::fetch`] downloads and stores what the owner lacks.

mod archive;
mod hub;
#[cfg(test)]
mod tests;

pub use archive::{BodyMismatchError, UnsupportedPathError};
pub use hub::{HubClient, HubError, HubFile, HubGate, HubRefusal, HubSetupError, ResolvedRepo};

use std::collections::{HashMap, HashSet};
use std::num::{NonZeroU64, NonZeroUsize};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use futures::{StreamExt, TryStreamExt};
use reqwest::Url;
use serde::Serialize;
use tokio::sync::{Semaphore, mpsc};
use tracing::Instrument;

use pipette_core::config::Config;
use pipette_core::vault::{BlobDigest, ImportedBlob, VaultImport, VaultOwner, VaultResource};
use pipette_plan_types::{
    FileName, FileNameError, GgufText, GgufTextSource, GgufVision, GgufVisionSource,
    GgufVisionSpeculative, GgufVisionSpeculativeSource, GitOid, HfOrg, HfRepo, HfRepoName,
    ImportAction, Model, ModelSource, PlannedImport, RepoSubpath, Sha256, VaultArtifact,
    VaultEntry, VaultFileRole, VaultModel, VaultOrigin,
};
use pipette_store::{BlobBody, StagedWrite, VaultStore};

use self::archive::TarEntry;
use crate::vault_admin;
use crate::vault_admin::{StoredBlob, VaultAdminError};

/// Why an import could not run. Each message names the model or the file by
/// its HuggingFace reference, and never holds the token.
#[derive(Debug, thiserror::Error)]
pub enum HfImportError {
    #[error(transparent)]
    Hub(#[from] HubError),
    #[error(
        "{model} has a source that is not on HuggingFace. An import needs every source on HuggingFace"
    )]
    NotAllHuggingFace { model: String },
    #[error("{hf_ref} is not a file at commit {commit}")]
    NoSuchFile { hf_ref: String, commit: GitOid },
    #[error("{hf_ref} has no LFS digest. A gguf file must be in LFS storage")]
    NoLfsDigest { hf_ref: String },
    #[error("HuggingFace lists {hf_ref} with no size")]
    MissingSize { hf_ref: String },
    #[error("{hf_ref} gives a name that the vault cannot store a file under")]
    BadFileName {
        hf_ref: String,
        #[source]
        source: FileNameError,
    },
    #[error("{hf_ref} hashes to {hub} on HuggingFace, not to the pinned {pinned}")]
    PinMismatch {
        hf_ref: String,
        pinned: Sha256,
        hub: Sha256,
    },
    #[error("{hf_ref} has no files at commit {commit}")]
    EmptySnapshot { hf_ref: String, commit: GitOid },
    #[error(transparent)]
    UnsupportedPathError(#[from] UnsupportedPathError),
    #[error("the files of {hf_ref} are over the {limit}-byte limit")]
    TooLarge { hf_ref: String, limit: NonZeroU64 },
    #[error(transparent)]
    Body(#[from] BodyMismatchError),
    #[error(transparent)]
    Vault(#[from] VaultAdminError),
    /// The import wrote these records, so this is a bug.
    #[error("the records of {hf_ref} name no model")]
    RecordsNameNoModel { hf_ref: String },
    #[error(transparent)]
    Store(#[from] anyhow::Error),
}

/// Who must fix the cause of an [`HfImportError`]. A transport chooses its
/// status from this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportFault {
    /// The named model cannot import: a refused token, a missing file, a
    /// mismatched pin, or a file unfit for its role.
    Caller,
    /// HuggingFace failed or served bytes unlike its listing; a retry can succeed.
    Upstream,
    /// A retry after HuggingFace's wait can succeed.
    RateLimited,
    /// The vault store failed, or its records break what the import relies on.
    Server,
}

impl HfImportError {
    pub fn fault(&self) -> ImportFault {
        match self {
            Self::Hub(HubError::Refused { .. })
            | Self::NotAllHuggingFace { .. }
            | Self::NoSuchFile { .. }
            | Self::NoLfsDigest { .. }
            | Self::BadFileName { .. }
            | Self::PinMismatch { .. }
            | Self::EmptySnapshot { .. }
            | Self::UnsupportedPathError(_)
            | Self::TooLarge { .. }
            | Self::Vault(VaultAdminError::NotA { .. } | VaultAdminError::TooLarge { .. }) => {
                ImportFault::Caller
            }
            Self::Hub(HubError::RateLimited { .. }) => ImportFault::RateLimited,
            Self::Hub(HubError::Upstream { .. })
            | Self::MissingSize { .. }
            | Self::Body(_)
            | Self::Vault(VaultAdminError::DigestMismatch { .. }) => ImportFault::Upstream,
            Self::Vault(_) | Self::RecordsNameNoModel { .. } | Self::Store(_) => {
                ImportFault::Server
            }
        }
    }
}

/// An [`HfImportError`], and the model whose import failed.
#[derive(Debug, thiserror::Error)]
#[error("{error}")]
pub struct ModelImportError {
    pub model: Box<Model>,
    pub error: HfImportError,
}

/// The import settings for the whole process. A clone shares the permits.
#[derive(Debug, Clone)]
pub struct HfImports {
    pub hub: HubClient,
    /// One permit for each ingest that imports.
    pub permits: Arc<Semaphore>,
    pub blob_limit: NonZeroU64,
    pub progress: Progress,
}

impl HfImports {
    /// Build the imports for `endpoint`, reporting no progress.
    pub fn new(
        endpoint: &str,
        max_concurrent: NonZeroUsize,
        max_concurrent_requests: NonZeroUsize,
        blob_limit: NonZeroU64,
    ) -> Result<Self, HubSetupError> {
        let endpoint = Url::parse(endpoint).map_err(|e| HubSetupError::BadEndpoint {
            endpoint: endpoint.to_owned(),
            reason: e.to_string(),
        })?;
        Ok(Self {
            hub: HubClient::new(endpoint, max_concurrent_requests)?,
            permits: Arc::new(Semaphore::new(max_concurrent.get())),
            blob_limit,
            progress: Progress::Silent,
        })
    }

    pub fn from_config(config: &Config) -> Result<Self, HubSetupError> {
        Self::new(
            &config.hf_import_endpoint,
            config.hf_import_max_concurrent,
            config.hf_max_concurrent_requests,
            config.vault_blob_limit_bytes,
        )
    }

    pub fn with_progress(self, progress: Progress) -> Self {
        Self { progress, ..self }
    }

    pub fn importer<'a>(&'a self, vault: &'a dyn VaultStore) -> Importer<'a> {
        Importer {
            hub: &self.hub,
            vault,
            blob_limit: self.blob_limit,
        }
    }
}

/// What an import calls: the Hub, the vault, and the largest blob that the
/// vault keeps.
#[derive(Clone, Copy)]
pub struct Importer<'a> {
    pub hub: &'a HubClient,
    pub vault: &'a dyn VaultStore,
    pub blob_limit: NonZeroU64,
}

/// Where an import reports the bytes that it downloads.
#[derive(Debug, Clone, Default)]
pub enum Progress {
    #[default]
    Silent,
    /// One event for each chunk. A full or closed channel drops the event,
    /// so a slow reader never stops the download.
    Channel(mpsc::Sender<ImportProgress>),
}

/// The bytes of one file that an import downloaded so far.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportProgress {
    /// The source, as `org/repo_name@commit`.
    pub hf_ref: String,
    /// The path in the repository.
    pub file: String,
    pub bytes_done: u64,
    pub bytes_total: u64,
}

/// The least time between two progress lines for one file in the log.
const LOG_EVERY: std::time::Duration = std::time::Duration::from_secs(5);

impl Progress {
    /// A channel whose events a task logs, at most once per `LOG_EVERY` for a file
    /// plus once when it completes. Call this on a Tokio runtime.
    pub fn logged() -> Self {
        let (sender, mut receiver) = mpsc::channel::<ImportProgress>(64);
        tokio::spawn(async move {
            let mut last: HashMap<(String, String), tokio::time::Instant> = HashMap::new();
            while let Some(event) = receiver.recv().await {
                let key = (event.hf_ref.clone(), event.file.clone());
                let now = tokio::time::Instant::now();
                let due = last
                    .get(&key)
                    .is_none_or(|at| now.duration_since(*at) >= LOG_EVERY);
                if due || event.bytes_done >= event.bytes_total {
                    tracing::info!(
                        hf_ref = %event.hf_ref,
                        file = %event.file,
                        bytes_done = event.bytes_done,
                        bytes_total = event.bytes_total,
                        "hf import progress"
                    );
                    last.insert(key, now);
                }
            }
        });
        Self::Channel(sender)
    }

    fn track(&self, body: BlobBody, hf_ref: &str, file: &str, bytes_total: u64) -> BlobBody {
        let Self::Channel(sender) = self else {
            return body;
        };
        let sender = sender.clone();
        let (hf_ref, file) = (hf_ref.to_owned(), file.to_owned());
        let mut bytes_done = 0;
        body.and_then(move |chunk| {
            bytes_done += chunk.len() as u64;
            let event = ImportProgress {
                hf_ref: hf_ref.clone(),
                file: file.clone(),
                bytes_done,
                bytes_total,
            };
            // The completing event waits for room so the reader sees each file finish;
            // an earlier event drops when the channel is full.
            let completing = if bytes_done >= bytes_total {
                Some((sender.clone(), event))
            } else {
                let _dropped = sender.try_send(event);
                None
            };
            async move {
                if let Some((sender, event)) = completing {
                    let _closed = sender.send(event).await;
                }
                Ok(chunk)
            }
        })
        .boxed()
    }
}

/// The models of one import, resolved against the Hub.
#[derive(Debug)]
pub struct ImportPlan {
    owner: VaultOwner,
    imports: Vec<ResolvedImport>,
}

#[derive(Debug)]
struct ResolvedImport {
    model: Model,
    digest: Sha256,
    sources: VaultModel<Source>,
    step: Step,
}

#[derive(Debug)]
enum Step {
    Reuse(Vec<VaultResource>),
    /// The import downloads this many bytes.
    Fetch {
        bytes: u64,
    },
}

/// One source of a model, as the job body names it.
#[derive(Debug, Clone)]
enum Wanted {
    File {
        repo: HfRepo,
        path: RepoSubpath,
        pin: Option<Sha256>,
    },
    Dir {
        repo: HfRepo,
        prefix: Option<RepoSubpath>,
    },
}

impl Wanted {
    fn repo(&self) -> &HfRepo {
        match self {
            Self::File { repo, .. } | Self::Dir { repo, .. } => repo,
        }
    }
}

/// One source of a model at its resolved commit, with the files to fetch.
#[derive(Debug, Clone)]
struct Source {
    repo: HfRepo,
    commit: GitOid,
    /// The name of the record: the last segment of a gguf path, or
    /// `{repo_name}.tar` for an archive.
    filename: FileName,
    files: SourceFiles,
}

#[derive(Debug, Clone)]
enum SourceFiles {
    Gguf {
        path: RepoSubpath,
        size: u64,
        sha256: Sha256,
    },
    Dir {
        /// `None` for a whole repository.
        prefix: Option<RepoSubpath>,
        entries: Vec<TarEntry>,
    },
}

impl Source {
    fn origin(&self) -> VaultOrigin {
        VaultOrigin::HuggingFace {
            org: self.repo.org.clone(),
            repo_name: self.repo.repo_name.clone(),
            commit: self.commit.clone(),
            path: self.path().cloned(),
        }
    }

    /// The source as `org/repo_name@commit`, the form of `ImportProgress::hf_ref`.
    fn reference(&self) -> String {
        format!("{}@{}", self.repo, self.commit)
    }

    /// The gguf file, or the prefix of a directory. `None` for a whole
    /// repository.
    fn path(&self) -> Option<&RepoSubpath> {
        match &self.files {
            SourceFiles::Gguf { path, .. } => Some(path),
            SourceFiles::Dir { prefix, .. } => prefix.as_ref(),
        }
    }

    fn bytes(&self) -> u64 {
        match &self.files {
            SourceFiles::Gguf { size, .. } => *size,
            SourceFiles::Dir { entries, .. } => archive::archive_size(entries),
        }
    }
}

impl ImportPlan {
    /// Resolve each distinct model of `models` for `owner`, downloading nothing.
    /// Each resolves with the caller's token before the index is read, so a reuse
    /// still needs access to list the repository at that commit, though not to
    /// download a file.
    pub async fn resolve(
        importer: &Importer<'_>,
        owner: &VaultOwner,
        models: &[Model],
    ) -> Result<Self, ModelImportError> {
        let mut seen = HashSet::new();
        let mut listings = HashMap::new();
        let mut counted = HashSet::new();
        let mut imports = Vec::new();
        for model in models.iter().filter(|model| seen.insert(*model)) {
            let planned = plan_one(importer, owner, model, &mut listings, &mut counted)
                .instrument(span(owner, model))
                .await
                .map_err(|error| ModelImportError {
                    model: Box::new(model.clone()),
                    error,
                })?;
            imports.push(planned);
        }
        Ok(Self {
            owner: owner.clone(),
            imports,
        })
    }

    /// Whether [`Self::fetch`] downloads a model, rather than only reusing records.
    pub fn downloads(&self) -> bool {
        self.imports
            .iter()
            .any(|planned| matches!(planned.step, Step::Fetch { .. }))
    }

    /// What the import does for each distinct model. Models that differ only
    /// in the token give one entry.
    pub fn entries(&self) -> Vec<PlannedImport> {
        let mut seen = HashSet::new();
        self.imports
            .iter()
            .filter(|planned| seen.insert(planned.model.without_auth_token()))
            .map(|planned| PlannedImport {
                hf_ref: planned.model.to_string(),
                action: match &planned.step {
                    Step::Reuse(records) => ImportAction::Reuse {
                        resource_ids: records.iter().map(|r| r.resource_id.clone()).collect(),
                    },
                    Step::Fetch { bytes } => ImportAction::Fetch { bytes: *bytes },
                },
            })
            .collect()
    }

    /// The vault arm of each model the owner already holds, keyed by the model
    /// without its token. A model still to download has no arm yet.
    pub fn reused_arms(&self) -> Result<HashMap<Model, Model>, ModelImportError> {
        self.imports
            .iter()
            .filter_map(|planned| match &planned.step {
                Step::Reuse(records) => Some((planned, records)),
                Step::Fetch { .. } => None,
            })
            .map(|(planned, records)| {
                let arm = arm_of(records, &planned.model).map_err(|error| ModelImportError {
                    model: Box::new(planned.model.clone()),
                    error,
                })?;
                Ok((planned.model.without_auth_token(), arm))
            })
            .collect()
    }

    /// Download and store what the owner does not hold, and give each model of
    /// the plan as its vault arm.
    pub async fn fetch(
        self,
        importer: &Importer<'_>,
        progress: &Progress,
        now: DateTime<Utc>,
    ) -> Result<HashMap<Model, Model>, ModelImportError> {
        let owner = &self.owner;
        let mut done: HashMap<Sha256, Model> = HashMap::new();
        let mut rewrites = HashMap::new();
        for planned in self.imports {
            let rewritten = match done.get(&planned.digest) {
                Some(model) => model.clone(),
                None => {
                    let model = fetch_one(importer, owner, &planned, progress, now)
                        .instrument(span(owner, &planned.model))
                        .await
                        .map_err(|error| ModelImportError {
                            model: Box::new(planned.model.clone()),
                            error,
                        })?;
                    done.insert(planned.digest, model.clone());
                    model
                }
            };
            rewrites.insert(planned.model, rewritten);
        }
        Ok(rewrites)
    }
}

/// The span of one model's import. The model's display form names its
/// sources and never its token.
fn span(owner: &VaultOwner, model: &Model) -> tracing::Span {
    tracing::info_span!("hf_import", owner = %owner.label(), hf_ref = %model)
}

async fn plan_one(
    importer: &Importer<'_>,
    owner: &VaultOwner,
    model: &Model,
    listings: &mut HashMap<HfRepo, ResolvedRepo>,
    counted: &mut HashSet<Sha256>,
) -> Result<ResolvedImport, HfImportError> {
    let wanted = wanted(model).ok_or_else(|| HfImportError::NotAllHuggingFace {
        model: model.to_string(),
    })?;
    for (_, want) in wanted.files() {
        let repo = want.repo();
        if !listings.contains_key(repo) {
            let listing = importer.hub.resolve(repo).await.map_err(hub_failure)?;
            listings.insert(repo.clone(), listing);
        }
    }
    let sources = wanted.try_map(|_, want| {
        let listing = listings
            .get(want.repo())
            .ok_or_else(|| anyhow::anyhow!("{} has no listing", want.repo().reference()))?;
        select(want, listing)
    })?;
    let digest = import_digest(&sources)?;
    let step = match reusable(importer.vault, owner, &digest).await? {
        Some(records) => Step::Reuse(records),
        None => {
            let limit = importer.blob_limit;
            if sources.files().iter().any(|(_, s)| s.bytes() > limit.get()) {
                return Err(HfImportError::TooLarge {
                    hf_ref: model.to_string(),
                    limit,
                });
            }
            let bytes = download_bytes(importer, owner, &sources, counted).await?;
            Step::Fetch { bytes }
        }
    };
    Ok(ResolvedImport {
        model: model.clone(),
        digest,
        sources,
        step,
    })
}

/// Each Hugging Face repository of `model` at its resolved commit. Each file
/// that `model` names must be at that commit, in the form that an import
/// needs.
pub(crate) async fn listings_for(
    hub: &HubClient,
    model: &Model,
) -> Result<HashMap<HfRepo, ResolvedRepo>, HfImportError> {
    let wanted = wanted(model).ok_or_else(|| HfImportError::NotAllHuggingFace {
        model: model.to_string(),
    })?;
    let mut listings = HashMap::new();
    for (_, want) in wanted.files() {
        let repo = want.repo();
        if !listings.contains_key(repo) {
            listings.insert(repo.clone(), hub.resolve(repo).await?);
        }
    }
    wanted.try_map(|_, want| {
        let listing = listings
            .get(want.repo())
            .ok_or_else(|| anyhow::anyhow!("{} has no listing", want.repo().reference()))?;
        select(want, listing)
    })?;
    Ok(listings)
}

/// The bytes that a fetch of `sources` downloads. The function counts no bytes
/// for a source that `owner` holds, or for a source with an id ([`source_id`])
/// in `counted`. An earlier model of the plan downloads that source. The
/// function adds the id of each source to `counted`.
async fn download_bytes(
    importer: &Importer<'_>,
    owner: &VaultOwner,
    sources: &VaultModel<Source>,
    counted: &mut HashSet<Sha256>,
) -> Result<u64, HfImportError> {
    let mut total: u64 = 0;
    for (_, source) in sources.files() {
        let held = !counted.insert(source_id(source)?)
            || match &source.files {
                SourceFiles::Gguf { sha256, .. } => {
                    vault_admin::blob_size(importer.vault, owner, sha256)
                        .await?
                        .is_some()
                }
                SourceFiles::Dir { .. } => {
                    held_archive(importer.vault, owner, source).await?.is_some()
                }
            };
        let bytes = if held { 0 } else { source.bytes() };
        total = total.saturating_add(bytes);
    }
    Ok(total)
}

/// `model` with each source as a [`Wanted`], or `None` if a source is not on
/// HuggingFace.
fn wanted(model: &Model) -> Option<VaultModel<Wanted>> {
    let file = |repo: &HfRepo, path: &RepoSubpath, pin: &Option<Sha256>| Wanted::File {
        repo: repo.clone(),
        path: path.clone(),
        pin: pin.clone(),
    };
    let dir = |source: &ModelSource| match source {
        ModelSource::HuggingFace { repo, prefix } => Some(Wanted::Dir {
            repo: repo.clone(),
            prefix: prefix.clone(),
        }),
        ModelSource::VaultArchive { .. }
        | ModelSource::RelativeDir { .. }
        | ModelSource::AbsoluteDir { .. } => None,
    };
    Some(match model {
        Model::GgufText(GgufText {
            source: GgufTextSource::HuggingFace { repo, path, sha256 },
        }) => VaultModel::GgufText {
            model: file(repo, path, sha256),
        },
        Model::GgufVision(GgufVision {
            source: GgufVisionSource::HuggingFace(files),
        }) => VaultModel::GgufVision {
            model: file(&files.repo, &files.model, &files.model_sha256),
            mmproj: file(&files.repo, &files.mmproj, &files.mmproj_sha256),
        },
        Model::GgufVisionSpeculative(GgufVisionSpeculative {
            source:
                GgufVisionSpeculativeSource::HuggingFace {
                    files,
                    draft,
                    draft_sha256,
                },
        }) => VaultModel::GgufVisionSpeculative {
            model: file(&files.repo, &files.model, &files.model_sha256),
            mmproj: file(&files.repo, &files.mmproj, &files.mmproj_sha256),
            draft: draft.as_ref().map(|d| file(&files.repo, d, draft_sha256)),
        },
        Model::Mlx(m) => VaultModel::Mlx {
            dir_archive: dir(&m.source)?,
        },
        Model::Torch(m) => VaultModel::Torch {
            dir_archive: dir(&m.source)?,
        },
        Model::Openvino(m) => VaultModel::Openvino {
            dir_archive: dir(&m.source)?,
        },
        Model::OnnxGenai(m) => VaultModel::OnnxGenai {
            dir_archive: dir(&m.source)?,
        },
        Model::MlxSpeculative(pair) => VaultModel::MlxSpeculative {
            target_dir_archive: dir(&pair.target)?,
            draft_dir_archive: dir(&pair.draft)?,
        },
        Model::TorchSpeculative(pair) => VaultModel::TorchSpeculative {
            target_dir_archive: dir(&pair.target)?,
            draft_dir_archive: dir(&pair.draft)?,
        },
        Model::GgufText(_)
        | Model::GgufVision(_)
        | Model::GgufVisionSpeculative(_)
        | Model::AppleFoundationText => return None,
    })
}

/// The files of `want` in `listing`. A gguf file must be in LFS storage, and
/// must match the pin that the body sets. A directory is every file under
/// its prefix, with the prefix removed.
fn select(want: Wanted, listing: &ResolvedRepo) -> Result<Source, HfImportError> {
    let commit = listing.commit.clone();
    match want {
        Wanted::File { repo, path, pin } => {
            let hf_ref = format!("{}:{path}", repo.reference());
            let file = listing
                .files
                .iter()
                .find(|file| file.path == path.as_ref())
                .ok_or_else(|| HfImportError::NoSuchFile {
                    hf_ref: hf_ref.clone(),
                    commit: commit.clone(),
                })?;
            let sha256 = file
                .lfs_sha256
                .clone()
                .ok_or_else(|| HfImportError::NoLfsDigest {
                    hf_ref: hf_ref.clone(),
                })?;
            let size = file.size.ok_or_else(|| HfImportError::MissingSize {
                hf_ref: hf_ref.clone(),
            })?;
            if let Some(pinned) = pin.filter(|pinned| *pinned != sha256) {
                return Err(HfImportError::PinMismatch {
                    hf_ref,
                    pinned,
                    hub: sha256,
                });
            }
            let last = file.path.rsplit('/').next().unwrap_or(&file.path);
            let filename = file_name(&hf_ref, last)?;
            Ok(Source {
                repo,
                commit,
                filename,
                files: SourceFiles::Gguf { path, size, sha256 },
            })
        }
        Wanted::Dir { repo, prefix } => {
            let under = prefix.as_ref().map(|prefix| format!("{prefix}/"));
            let entries = listing
                .files
                .iter()
                .filter_map(|file| {
                    let name = match &under {
                        Some(under) => file.path.strip_prefix(under.as_str())?,
                        None => file.path.as_str(),
                    };
                    Some((name, file))
                })
                .map(|(name, file)| {
                    let size = file.size.ok_or_else(|| HfImportError::MissingSize {
                        hf_ref: format!("{}:{}", repo.reference(), file.path),
                    })?;
                    Ok(TarEntry {
                        name: name.to_owned(),
                        repo_path: file.path.clone(),
                        size,
                        sha256: file.lfs_sha256.clone(),
                    })
                })
                .collect::<Result<Vec<_>, HfImportError>>()?;
            if entries.is_empty() {
                let source = ModelSource::HuggingFace { repo, prefix };
                return Err(HfImportError::EmptySnapshot {
                    hf_ref: source.reference(),
                    commit,
                });
            }
            archive::check(&entries)?;
            let filename = file_name(&repo.reference(), &format!("{}.tar", repo.repo_name))?;
            Ok(Source {
                repo,
                commit,
                filename,
                files: SourceFiles::Dir { prefix, entries },
            })
        }
    }
}

/// `name` as the name of a vault record, or the refusal that names `hf_ref`.
fn file_name(hf_ref: &str, name: &str) -> Result<FileName, HfImportError> {
    FileName::try_new(name).map_err(|source| HfImportError::BadFileName {
        hf_ref: hf_ref.to_owned(),
        source,
    })
}

/// The input of the import digest, which keys the import index. Raise `v` on
/// any change to its fields, their order or the encoding; the structs hold no
/// maps, so `serde_json` keeps declaration order. Raise `v` also on a change to
/// the archive layout in `archive.rs`.
#[derive(Serialize)]
struct ImportKey<'a> {
    v: u8,
    #[serde(rename = "type")]
    model_type: &'static str,
    /// In the role order of [`VaultModel::files`].
    sources: Vec<KeySource<'a>>,
}

#[derive(Serialize)]
struct KeySource<'a> {
    role: VaultFileRole,
    org: &'a HfOrg,
    repo_name: &'a HfRepoName,
    commit: &'a GitOid,
    path: Option<&'a RepoSubpath>,
    /// In the byte order of their paths.
    files: Vec<KeyFile<'a>>,
}

#[derive(Serialize)]
struct KeyFile<'a> {
    /// The path from the repository root.
    path: &'a str,
    size: u64,
    lfs_sha256: Option<&'a Sha256>,
}

/// The input of the source digest, which keys the source entry of one
/// directory source. That entry names the archive record of the source, so an
/// import of another model with the source reuses the archive. Raise `v` on any
/// change to its fields, their order or the encoding. Raise `v` also on a change
/// to the archive layout in `archive.rs`.
#[derive(Serialize)]
struct SourceKey<'a> {
    v: u8,
    /// `dir_source`. The source digest and the import digest share the
    /// `vault-imports/` keyspace, and this tag keeps a source key apart from a
    /// key of any other kind.
    kind: &'static str,
    org: &'a HfOrg,
    repo_name: &'a HfRepoName,
    commit: &'a GitOid,
    path: Option<&'a RepoSubpath>,
    /// In the byte order of their paths.
    files: Vec<KeyFile<'a>>,
}

/// The files of `source` in the byte order of their paths.
fn key_files(source: &Source) -> Vec<KeyFile<'_>> {
    let mut files: Vec<KeyFile<'_>> = match &source.files {
        SourceFiles::Gguf { path, size, sha256 } => vec![KeyFile {
            path: path.as_ref(),
            size: *size,
            lfs_sha256: Some(sha256),
        }],
        SourceFiles::Dir { entries, .. } => entries
            .iter()
            .map(|entry| KeyFile {
                path: &entry.repo_path,
                size: entry.size,
                lfs_sha256: entry.sha256.as_ref(),
            })
            .collect(),
    };
    files.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
    files
}

/// The SHA-256 of the JSON form of `key`.
fn digest_of(key: &impl Serialize) -> Result<Sha256, HfImportError> {
    let mut digest = BlobDigest::default();
    digest.update(&serde_json::to_vec(key).map_err(anyhow::Error::from)?);
    Ok(digest.finish().map_err(anyhow::Error::from)?.0)
}

/// The SHA-256 of the JSON form of the [`ImportKey`] of `model`.
fn import_digest(model: &VaultModel<Source>) -> Result<Sha256, HfImportError> {
    let sources = model
        .files()
        .into_iter()
        .map(|(role, source)| KeySource {
            role,
            org: &source.repo.org,
            repo_name: &source.repo.repo_name,
            commit: &source.commit,
            path: source.path(),
            files: key_files(source),
        })
        .collect();
    digest_of(&ImportKey {
        v: 1,
        model_type: model.type_name(),
        sources,
    })
}

/// The identity of `source` in the vault. For a gguf file, it is the digest of
/// the blob. For a directory, it is the SHA-256 of the JSON form of the
/// [`SourceKey`].
fn source_id(source: &Source) -> Result<Sha256, HfImportError> {
    match &source.files {
        SourceFiles::Gguf { sha256, .. } => Ok(sha256.clone()),
        SourceFiles::Dir { .. } => digest_of(&SourceKey {
            v: 1,
            kind: "dir_source",
            org: &source.repo.org,
            repo_name: &source.repo.repo_name,
            commit: &source.commit,
            path: source.path(),
            files: key_files(source),
        }),
    }
}

/// The records of `owner`'s import under `digest`, if every record and its
/// blob still exist.
async fn reusable(
    vault: &dyn VaultStore,
    owner: &VaultOwner,
    digest: &Sha256,
) -> Result<Option<Vec<VaultResource>>, HfImportError> {
    let Some(entry) = vault.get_import(owner, digest).await? else {
        return Ok(None);
    };
    let mut records = Vec::with_capacity(entry.resource_ids.len());
    for resource_id in &entry.resource_ids {
        let Some(record) = vault.get_resource(resource_id).await? else {
            return Ok(None);
        };
        if vault.stat_blob(&record.object_key).await?.is_none() {
            return Ok(None);
        }
        records.push(record);
    }
    Ok((!records.is_empty()).then_some(records))
}

async fn fetch_one(
    importer: &Importer<'_>,
    owner: &VaultOwner,
    planned: &ResolvedImport,
    progress: &Progress,
    now: DateTime<Utc>,
) -> Result<Model, HfImportError> {
    // A record can appear or go between the two phases, so the fetch checks
    // again.
    let records = match reusable(importer.vault, owner, &planned.digest).await? {
        Some(records) => {
            records.iter().for_each(|record| {
                tracing::info!(
                    resource_id = %record.resource_id,
                    origin = %record.name,
                    "hf import reused record"
                );
            });
            records
        }
        None => {
            let sources = &planned.sources;
            tracing::info!("hf import started");
            let hf_ref = &planned.model.to_string();
            // One after another. The permits of `HfImports` bound the imports
            // of the whole process. Two roles with one source store it one
            // time: the source entry does not exist until after
            // `record_import`.
            let (_, mut blobs) = futures::stream::iter(sources.files())
                .map(Ok)
                .try_fold(
                    (HashMap::<Sha256, ImportedBlob>::new(), HashMap::new()),
                    |(mut stored, mut blobs), (role, source)| async move {
                        let id = source_id(source)?;
                        let blob = match stored.get(&id) {
                            Some(blob) => blob.clone(),
                            None => store(importer, owner, hf_ref, source, progress).await?,
                        };
                        stored.insert(id, blob.clone());
                        blobs.insert(role, blob);
                        Ok::<_, HfImportError>((stored, blobs))
                    },
                )
                .await?;
            let artifact = sources.clone().try_map(|role, _| {
                blobs
                    .remove(&role)
                    .ok_or_else(|| anyhow::anyhow!("the `{role}` file was never stored"))
            })?;
            let records = vault_admin::record_import(
                importer.vault,
                owner.clone(),
                VaultArtifact::Model(artifact),
                now,
            )
            .await?;
            records.iter().for_each(|record| {
                tracing::info!(
                    resource_id = %record.resource_id,
                    origin = %record.name,
                    "hf import stored record"
                );
            });
            let entry = VaultImport {
                resource_ids: records.iter().map(|r| r.resource_id.clone()).collect(),
            };
            // The records are correct without the entry. A later import of
            // the model writes new records.
            if let Err(e) = importer
                .vault
                .put_import(owner, &planned.digest, &entry)
                .await
            {
                tracing::warn!(error = %format_args!("{e:#}"), "hf import index write failed");
            }
            index_archives(importer.vault, owner, sources, &records).await;
            records
        }
    };
    arm_of(&records, &planned.model)
}

/// The archive record of the directory `source`, if an earlier import of
/// `owner` stored it, and the record and its blob still exist.
async fn held_archive(
    vault: &dyn VaultStore,
    owner: &VaultOwner,
    source: &Source,
) -> Result<Option<VaultResource>, HfImportError> {
    let records = reusable(vault, owner, &source_id(source)?).await?;
    // The entry is stored data. An entry with more or fewer than one record is
    // a miss.
    Ok(records
        .filter(|records| records.len() == 1)
        .and_then(|records| records.into_iter().next()))
}

/// Write a source entry for each directory source of `sources`. The entry
/// names the archive record of the source in `records`. The records are correct
/// without the entries. Without an entry, a later import of another model with
/// the source downloads the source again.
async fn index_archives(
    vault: &dyn VaultStore,
    owner: &VaultOwner,
    sources: &VaultModel<Source>,
    records: &[VaultResource],
) {
    let archives = sources
        .files()
        .into_iter()
        .filter(|(_, source)| matches!(source.files, SourceFiles::Dir { .. }))
        .filter_map(|(role, source)| {
            let record = records
                .iter()
                .find(|record| record.own_file().is_some_and(|(own, _)| own == role))?;
            Some((source, record))
        });
    for (source, record) in archives {
        let entry = VaultImport {
            resource_ids: vec![record.resource_id.clone()],
        };
        let written = match source_id(source) {
            Ok(id) => vault
                .put_import(owner, &id, &entry)
                .await
                .map_err(HfImportError::from),
            Err(e) => Err(e),
        };
        if let Err(e) = written {
            tracing::warn!(
                resource_id = %record.resource_id,
                source = %source.reference(),
                error = %format_args!("{e:#}"),
                "hf import source index write failed"
            );
        }
    }
}

/// The vault arm of `model`, read from `records`, the records of its import.
fn arm_of(records: &[VaultResource], model: &Model) -> Result<Model, HfImportError> {
    match records.first().and_then(VaultResource::entry) {
        Some(VaultEntry::Model(arm)) => Ok(arm),
        Some(VaultEntry::Runtime(_)) | None => Err(HfImportError::RecordsNameNoModel {
            hf_ref: model.to_string(),
        }),
    }
}

/// Download one source of a model and store it as one of `owner`'s blobs. The
/// function does not download a gguf blob that `owner` holds. It also does not
/// download an archive that an earlier import stored.
async fn store(
    importer: &Importer<'_>,
    owner: &VaultOwner,
    hf_ref: &str,
    source: &Source,
    progress: &Progress,
) -> Result<ImportedBlob, HfImportError> {
    let limit = importer.blob_limit;
    let too_large = || HfImportError::TooLarge {
        hf_ref: hf_ref.to_owned(),
        limit,
    };
    if source.bytes() > limit.get() {
        return Err(too_large());
    }
    let (sha256, bytes) = match &source.files {
        SourceFiles::Gguf { path, size, sha256 } => {
            let bytes = match vault_admin::blob_size(importer.vault, owner, sha256).await? {
                Some(bytes) => bytes,
                None => {
                    let body = importer
                        .hub
                        .download(&source.repo, &source.commit, path.as_ref())
                        .await
                        .map_err(hub_failure)?;
                    let body = progress.track(body, &source.reference(), path.as_ref(), *size);
                    let stored =
                        vault_admin::store_blob(importer.vault, owner, sha256, limit.get(), body)
                            .await;
                    match stored {
                        Ok(StoredBlob::Stored { bytes } | StoredBlob::AlreadyThere { bytes }) => {
                            bytes
                        }
                        Err(VaultAdminError::TooLarge { .. }) => return Err(too_large()),
                        Err(VaultAdminError::Store(e)) => return Err(from_body(e)),
                        Err(e) => return Err(e.into()),
                    }
                }
            };
            (sha256.clone(), bytes)
        }
        SourceFiles::Dir { entries, .. } => {
            match held_archive(importer.vault, owner, source).await? {
                Some(record) => {
                    tracing::info!(
                        resource_id = %record.resource_id,
                        source = %source.reference(),
                        "hf import reused archive"
                    );
                    (record.sha256, record.bytes)
                }
                None => {
                    let hub = importer.hub.clone();
                    let (repo, commit) = (source.repo.clone(), source.commit.clone());
                    let (progress, reference) = (progress.clone(), source.reference());
                    let body = archive::archive(entries.clone(), move |entry| {
                        let (hub, repo, commit) = (hub.clone(), repo.clone(), commit.clone());
                        let (progress, reference) = (progress.clone(), reference.clone());
                        async move {
                            let body = hub.download(&repo, &commit, &entry.repo_path).await?;
                            Ok(progress.track(body, &reference, &entry.repo_path, entry.size))
                        }
                    })?;
                    match importer.vault.put_staged_blob(owner, body, limit).await {
                        Ok(StagedWrite::Stored { sha256, bytes, .. }) => (sha256, bytes),
                        Ok(StagedWrite::TooLarge { .. }) => return Err(too_large()),
                        Err(e) => return Err(from_body(e)),
                    }
                }
            }
        }
    };
    Ok(ImportedBlob {
        filename: source.filename.clone(),
        sha256,
        bytes,
        origin: source.origin(),
    })
}

/// The import error for a body that failed while it streamed into the store.
fn from_body(error: anyhow::Error) -> HfImportError {
    let error = match error.downcast::<HubError>() {
        Ok(hub) => return hub_failure(hub),
        Err(error) => error,
    };
    match error.downcast::<BodyMismatchError>() {
        Ok(mismatch) => mismatch.into(),
        Err(error) => error.into(),
    }
}

/// The import error for a failed Hub call. This function logs each refusal
/// and each rate limit once.
fn hub_failure(error: HubError) -> HfImportError {
    match &error {
        HubError::Refused { status, .. } => {
            tracing::warn!(status = status.code(), "hf import refused by hub");
        }
        HubError::RateLimited {
            retry_after_secs, ..
        } => {
            tracing::warn!(retry_after_secs, "hf request rate-limited by hub");
        }
        HubError::Upstream { .. } => {}
    }
    error.into()
}
