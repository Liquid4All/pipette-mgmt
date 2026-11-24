//! A revision's `replace_model`: whether two models name the same bytes. A
//! checksum the server cannot read is taken on the filer's word.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::Value;

use pipette_plan_types::{
    GgufTextSource, GgufVisionSource, GgufVisionSpeculativeSource, GitOid, HfRepo, Model,
    ModelReplacement, ModelSource, ModelType, ReplacedModel, Sha256, VaultPath,
};
use pipette_store::VaultStore;

use crate::hf_import::{HubClient, HubError, HubFile};
use crate::vault_check::VaultScope;

#[derive(Debug, thiserror::Error)]
pub enum ReplaceModelError {
    #[error("replace_model names {0} twice")]
    Repeated(String),
    #[error("replace_model replaces {0} with itself")]
    Same(String),
    #[error("replace_model: no job of the head or of a plan on offer names {0}")]
    NoOld(String),
    #[error("replace_model: no cell of the revision names {0}")]
    NoNew(String),
    #[error("replace_model: {0} names more than one model of the revision")]
    Ambiguous(String),
    #[error(
        "replace_model: {0} carries an auth_token, so the filing imports it into the vault; \
         name the vault copy instead"
    )]
    Token(String),
    #[error("replace_model: {from} and {to} are models of different types")]
    OtherType { from: String, to: String },
    #[error("replace_model: {from} and {to} have different numbers of files")]
    OtherFiles { from: String, to: String },
    #[error(
        "replace_model: {from} and {to} are not the same bytes: the checksums of {file} differ"
    )]
    Differ {
        from: String,
        to: String,
        file: String,
    },
    #[error(transparent)]
    Hub(#[from] HubError),
    #[error(transparent)]
    Store(#[from] anyhow::Error),
}

/// Each old `spec.model` with its new one, and each pair as resolved.
#[derive(Debug, Clone, Default)]
pub struct Replacements {
    pub models: Vec<(Value, Value)>,
    pub replaced: Vec<ReplacedModel>,
}

/// A vault record outside `scope` counts as unread.
pub async fn resolve<'a>(
    requested: &[ModelReplacement],
    offered: impl Iterator<Item = &'a Value>,
    revised: &[Value],
    vault: &dyn VaultStore,
    scope: &VaultScope,
    hub: &HubClient,
) -> Result<Replacements, ReplaceModelError> {
    if requested.is_empty() {
        return Ok(Replacements::default());
    }
    let mut seen = HashSet::new();
    if let Some(pair) = requested.iter().find(|pair| !seen.insert(&pair.from)) {
        return Err(ReplaceModelError::Repeated(pair.from.clone()));
    }
    if let Some(pair) = requested.iter().find(|pair| pair.from == pair.to) {
        return Err(ReplaceModelError::Same(pair.from.clone()));
    }
    let on_offer = distinct_models(offered);
    let in_revision = distinct_models(revised.iter());
    let mut reader = Reader {
        vault,
        scope,
        hub,
        repos: HashMap::new(),
    };
    let mut resolved = Replacements::default();
    // A loop: the pairs await reads and share the Hub listings.
    for pair in requested {
        let olds: Vec<&(Value, Model)> = on_offer
            .iter()
            .filter(|(_, model)| model.to_string() == pair.from)
            .collect();
        let Some((_, old)) = olds.first() else {
            return Err(ReplaceModelError::NoOld(pair.from.clone()));
        };
        let (new_value, new) = match in_revision
            .iter()
            .filter(|(_, model)| model.to_string() == pair.to)
            .collect::<Vec<_>>()
            .as_slice()
        {
            [] => return Err(ReplaceModelError::NoNew(pair.to.clone())),
            [one] => (&one.0, &one.1),
            _ => return Err(ReplaceModelError::Ambiguous(pair.to.clone())),
        };
        if new.auth_token().is_some() {
            return Err(ReplaceModelError::Token(pair.to.clone()));
        }
        if ModelType::of(old) != ModelType::of(new) {
            return Err(ReplaceModelError::OtherType {
                from: pair.from.clone(),
                to: pair.to.clone(),
            });
        }
        let verified = reader.compare(pair, old, new).await?;
        resolved.models.extend(
            olds.into_iter()
                .map(|(old_value, _)| (old_value.clone(), new_value.clone())),
        );
        resolved.replaced.push(ReplacedModel {
            from: pair.from.clone(),
            to: pair.to.clone(),
            verified,
        });
    }
    Ok(resolved)
}

fn distinct_models<'a>(bodies: impl Iterator<Item = &'a Value>) -> Vec<(Value, Model)> {
    bodies
        .filter_map(|body| body.pointer("/spec/model"))
        .fold(Vec::<&Value>::new(), |mut seen, model| {
            if !seen.contains(&model) {
                seen.push(model);
            }
            seen
        })
        .into_iter()
        .filter_map(|value| {
            let model = serde_json::from_value(value.clone()).ok()?;
            Some((value.clone(), model))
        })
        .collect()
}

/// One file or directory of a model.
enum Piece<'a> {
    HubFile(&'a HfRepo, String),
    /// `None` for the repository root.
    HubDir(&'a HfRepo, Option<String>),
    VaultFile(&'a VaultPath),
    /// A directory packed as one archive.
    VaultArchive(&'a VaultPath),
    /// A local path or a URL.
    Unread(String),
}

impl Piece<'_> {
    fn label(&self) -> String {
        match self {
            Self::HubFile(repo, path) => format!("{}:{path}", repo.reference()),
            Self::HubDir(repo, Some(prefix)) => format!("{}:{prefix}", repo.reference()),
            Self::HubDir(repo, None) => repo.reference(),
            Self::VaultFile(url) | Self::VaultArchive(url) => url.to_string(),
            Self::Unread(label) => label.clone(),
        }
    }
}

/// The files of `model` in the order of its type's fields.
fn pieces(model: &Model) -> Vec<Piece<'_>> {
    match model {
        Model::GgufText(m) => vec![match &m.source {
            GgufTextSource::HuggingFace { repo, path, .. } => {
                Piece::HubFile(repo, path.to_string())
            }
            GgufTextSource::VaultUrl { url, .. } => Piece::VaultFile(url),
            other @ (GgufTextSource::RelativeFile { .. }
            | GgufTextSource::AbsoluteFile { .. }
            | GgufTextSource::Url { .. }) => Piece::Unread(other.reference()),
        }],
        Model::GgufVision(m) => vision_pieces(&m.source),
        Model::GgufVisionSpeculative(m) => {
            let (vision, draft) = match &m.source {
                GgufVisionSpeculativeSource::HuggingFace { files, draft, .. } => (
                    vec![
                        Piece::HubFile(&files.repo, files.model.to_string()),
                        Piece::HubFile(&files.repo, files.mmproj.to_string()),
                    ],
                    draft
                        .as_ref()
                        .map(|draft| Piece::HubFile(&files.repo, draft.to_string())),
                ),
                GgufVisionSpeculativeSource::VaultUrl { files, draft, .. } => (
                    vec![
                        Piece::VaultFile(&files.model),
                        Piece::VaultFile(&files.mmproj),
                    ],
                    draft.as_ref().map(Piece::VaultFile),
                ),
                other @ (GgufVisionSpeculativeSource::RelativeFiles { .. }
                | GgufVisionSpeculativeSource::AbsoluteFiles { .. }
                | GgufVisionSpeculativeSource::Url { .. }) => (
                    vec![
                        Piece::Unread(model.to_string()),
                        Piece::Unread(model.to_string()),
                    ],
                    other.draft_reference().map(Piece::Unread),
                ),
            };
            vision.into_iter().chain(draft).collect()
        }
        Model::Mlx(m) => vec![dir(&m.source)],
        Model::AppleCoreAi(m) => vec![dir(&m.source)],
        Model::Torch(m) => vec![dir(&m.source)],
        Model::Openvino(m) => vec![dir(&m.source)],
        Model::OnnxGenai(m) => vec![dir(&m.source)],
        Model::MlxSpeculative(m) | Model::TorchSpeculative(m) => {
            m.sources().into_iter().map(dir).collect()
        }
        Model::AppleFoundationText => Vec::new(),
    }
}

fn dir(source: &ModelSource) -> Piece<'_> {
    match source {
        ModelSource::HuggingFace { repo, prefix } => {
            Piece::HubDir(repo, prefix.as_ref().map(ToString::to_string))
        }
        ModelSource::VaultArchive { url, .. } => Piece::VaultArchive(url),
        ModelSource::RelativeDir { .. } | ModelSource::AbsoluteDir { .. } => {
            Piece::Unread(source.reference())
        }
    }
}

fn vision_pieces(source: &GgufVisionSource) -> Vec<Piece<'_>> {
    match source {
        GgufVisionSource::HuggingFace(f) => vec![
            Piece::HubFile(&f.repo, f.model.to_string()),
            Piece::HubFile(&f.repo, f.mmproj.to_string()),
        ],
        GgufVisionSource::VaultUrl(f) => {
            vec![Piece::VaultFile(&f.model), Piece::VaultFile(&f.mmproj)]
        }
        GgufVisionSource::RelativeFiles(f) => vec![
            Piece::Unread(f.model.to_string()),
            Piece::Unread(f.mmproj.to_string()),
        ],
        GgufVisionSource::AbsoluteFiles(f) => vec![
            Piece::Unread(f.model.to_string()),
            Piece::Unread(f.mmproj.to_string()),
        ],
        GgufVisionSource::Url(f) => vec![
            Piece::Unread(f.model.to_string()),
            Piece::Unread(f.mmproj.to_string()),
        ],
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Print {
    File(Sha256),
    /// LFS files by path; a file outside LFS has no published checksum.
    Dir(BTreeMap<String, Sha256>),
    Archive(Sha256),
}

/// `None` when the prints cannot tell: two packings of one directory can hash
/// differently.
fn same(old: Option<Print>, new: Option<Print>) -> Option<bool> {
    match (old?, new?) {
        (Print::File(a), Print::File(b)) | (Print::Archive(a), Print::Archive(b)) if a == b => {
            Some(true)
        }
        (Print::File(_), Print::File(_)) => Some(false),
        (Print::Dir(a), Print::Dir(b)) => Some(a == b),
        _ => None,
    }
}

struct Reader<'a> {
    vault: &'a dyn VaultStore,
    scope: &'a VaultScope,
    hub: &'a HubClient,
    /// Each pinned repository's files, `None` where the Hub refused it.
    repos: HashMap<String, Option<Vec<HubFile>>>,
}

impl Reader<'_> {
    /// `Ok(false)` when some file was taken on trust.
    async fn compare(
        &mut self,
        pair: &ModelReplacement,
        old: &Model,
        new: &Model,
    ) -> Result<bool, ReplaceModelError> {
        let (old_pieces, new_pieces) = (pieces(old), pieces(new));
        if old_pieces.len() != new_pieces.len() {
            return Err(ReplaceModelError::OtherFiles {
                from: pair.from.clone(),
                to: pair.to.clone(),
            });
        }
        let mut verified = true;
        for (old_piece, new_piece) in old_pieces.iter().zip(&new_pieces) {
            let old_print = self.print(old_piece).await?;
            let new_print = self.print(new_piece).await?;
            match same(old_print, new_print) {
                Some(true) => {}
                Some(false) => {
                    return Err(ReplaceModelError::Differ {
                        from: pair.from.clone(),
                        to: pair.to.clone(),
                        file: new_piece.label(),
                    });
                }
                None => verified = false,
            }
        }
        tracing::info!(from = %pair.from, to = %pair.to, verified, "replace_model compared");
        Ok(verified)
    }

    async fn print(&mut self, piece: &Piece<'_>) -> Result<Option<Print>, ReplaceModelError> {
        Ok(match piece {
            Piece::HubFile(repo, path) => self.files(repo).await?.and_then(|files| {
                files
                    .iter()
                    .find(|file| &file.path == path)
                    .and_then(|file| file.lfs_sha256.clone())
                    .map(Print::File)
            }),
            Piece::HubDir(repo, prefix) => self.files(repo).await?.and_then(|files| {
                let inside = prefix.as_ref().map(|prefix| format!("{prefix}/"));
                let lfs: BTreeMap<String, Sha256> = files
                    .iter()
                    .filter_map(|file| {
                        let path = match &inside {
                            Some(inside) => file.path.strip_prefix(inside.as_str())?,
                            None => file.path.as_str(),
                        };
                        Some((path.to_owned(), file.lfs_sha256.clone()?))
                    })
                    .collect();
                (!lfs.is_empty()).then_some(Print::Dir(lfs))
            }),
            Piece::VaultFile(url) => self.vault_sha256(url).await?.map(Print::File),
            Piece::VaultArchive(url) => self.vault_sha256(url).await?.map(Print::Archive),
            Piece::Unread(_) => None,
        })
    }

    async fn vault_sha256(&self, url: &VaultPath) -> anyhow::Result<Option<Sha256>> {
        Ok(self
            .vault
            .get_resource(url.resource_id())
            .await?
            .filter(|record| self.scope.admits(&record.owner))
            .map(|record| record.sha256))
    }

    /// `None` for a revision that is not a commit, or one the Hub refuses,
    /// such as a squashed commit.
    async fn files(&mut self, repo: &HfRepo) -> Result<Option<&[HubFile]>, HubError> {
        let pinned = repo
            .revision
            .as_ref()
            .is_some_and(|revision| GitOid::try_new(revision.as_ref().to_owned()).is_ok());
        if !pinned {
            return Ok(None);
        }
        let key = repo.reference();
        if !self.repos.contains_key(&key) {
            let files = match self.hub.resolve(repo).await {
                Ok(resolved) => Some(resolved.files),
                Err(HubError::Refused { repo, status }) => {
                    tracing::info!(%repo, %status, "replace_model: the Hub refused a revision; trusting the filer");
                    None
                }
                Err(e) => return Err(e),
            };
            self.repos.insert(key.clone(), files);
        }
        Ok(self.repos.get(&key).and_then(Option::as_deref))
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use anyhow::Context;
    use serde_json::json;

    use pipette_core::config::{Config, StorageConfig};
    use pipette_core::vault::VaultOwner;
    use pipette_hub_testkit::{MockFile, MockGate, MockHub, MockRepo};
    use pipette_plan_types::OrgId;
    use pipette_store::Stores;

    use super::*;

    fn sha(c: char) -> anyhow::Result<Sha256> {
        Ok(Sha256::try_new(c.to_string().repeat(64))?)
    }

    const WEIGHTS: &[u8] = b"GGUF weights";
    const ORG: &str = "org_0195ab";

    fn commit(c: char) -> String {
        c.to_string().repeat(40)
    }

    /// The new model is `LiquidAI/LFM@c:m.gguf`, or `LiquidAI/Dir@c` for a
    /// directory.
    #[derive(Debug, Clone, Copy)]
    enum Old {
        VaultSame,
        VaultOther,
        VaultOtherOutOfReach,
        /// The Hub answers `404`.
        Squashed,
        Unpinned,
        AbsoluteFile,
        /// The Hub answers `429`, an error rather than trust.
        RateLimited,
        DirSame,
        DirOther,
    }

    /// `Ok(verified)` or a part of the refusal's message.
    #[rstest::rstest]
    #[case::same_upload(Old::VaultSame, Ok(true))]
    #[case::other_upload(
        Old::VaultOther,
        Err(
            "the checksums of LiquidAI/LFM@cccccccccccccccccccccccccccccccccccccccc:m.gguf differ"
        )
    )]
    #[case::other_upload_out_of_reach(Old::VaultOtherOutOfReach, Ok(false))]
    #[case::squashed(Old::Squashed, Ok(false))]
    #[case::unpinned(Old::Unpinned, Ok(false))]
    #[case::absolute_file(Old::AbsoluteFile, Ok(false))]
    #[case::rate_limited(Old::RateLimited, Err("rate-limited"))]
    #[case::same_directory(Old::DirSame, Ok(true))]
    #[case::other_directory(
        Old::DirOther,
        Err("the checksums of LiquidAI/Dir@cccccccccccccccccccccccccccccccccccccccc differ")
    )]
    #[tokio::test]
    async fn a_pair_is_refused_only_on_a_known_difference(
        #[case] case: Old,
        #[case] expected: Result<bool, &str>,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores(dir.path())?;
        let repo = |commit: String, rate_limited: bool, files: Vec<MockFile>| MockRepo {
            commit,
            public: true,
            rate_limited,
            gate: MockGate::Open,
            files,
        };
        let hub = MockHub::start(HashMap::from([
            (
                "LiquidAI/LFM".to_owned(),
                repo(commit('c'), false, vec![MockFile::lfs("m.gguf", WEIGHTS)]),
            ),
            (
                "LiquidAI/Busy".to_owned(),
                repo(commit('c'), true, vec![MockFile::lfs("m.gguf", WEIGHTS)]),
            ),
            (
                "LiquidAI/Dir".to_owned(),
                repo(
                    commit('c'),
                    false,
                    vec![
                        MockFile::lfs("w.safetensors", WEIGHTS),
                        MockFile::git("config.json", b"{}"),
                    ],
                ),
            ),
            (
                "LiquidAI/Dirs".to_owned(),
                repo(
                    commit('d'),
                    false,
                    vec![
                        MockFile::lfs("4bit/w.safetensors", WEIGHTS),
                        MockFile::git("4bit/config.json", br#"{"bits":4}"#),
                        MockFile::lfs("8bit/w.safetensors", b"other weights"),
                    ],
                ),
            ),
        ]))
        .await?;
        let client = HubClient::new(hub.endpoint.clone(), NonZeroUsize::MIN)?;
        let hub_file = |repo_name: &str, revision: &str| {
            json!({
                "type": "gguf_text", "source": "huggingface", "org": "LiquidAI",
                "repo_name": repo_name, "revision": revision, "path": "m.gguf",
            })
        };
        let mlx = |repo_name: &str, revision: String, prefix: Option<&str>| {
            let mut model = json!({
                "type": "mlx", "source": "huggingface", "org": "LiquidAI",
                "repo_name": repo_name, "revision": revision,
            });
            if let Some(prefix) = prefix {
                model["prefix"] = json!(prefix);
            }
            model
        };
        let old = match case {
            Old::VaultSame => {
                let same = Sha256::try_new(MockFile::lfs("m.gguf", WEIGHTS).sha256())?;
                upload(&stores, same).await?
            }
            Old::VaultOther | Old::VaultOtherOutOfReach => upload(&stores, sha('b')?).await?,
            Old::Squashed => hub_file("LFM", &commit('d')),
            Old::Unpinned => hub_file("LFM", "main"),
            Old::AbsoluteFile => {
                json!({"type": "gguf_text", "source": "absolute_file", "path": "/m/m.gguf"})
            }
            Old::RateLimited => hub_file("Busy", &commit('c')),
            Old::DirSame => mlx("Dirs", commit('d'), Some("4bit")),
            Old::DirOther => mlx("Dirs", commit('d'), Some("8bit")),
        };
        let new = match case {
            Old::DirSame | Old::DirOther => mlx("Dir", commit('c'), None),
            Old::VaultSame
            | Old::VaultOther
            | Old::VaultOtherOutOfReach
            | Old::Squashed
            | Old::Unpinned
            | Old::AbsoluteFile
            | Old::RateLimited => hub_file("LFM", &commit('c')),
        };
        let pair = ModelReplacement {
            from: serde_json::from_value::<Model>(old.clone())?.to_string(),
            to: serde_json::from_value::<Model>(new.clone())?.to_string(),
        };
        let offered = [json!({"spec": {"model": old.clone()}})];
        let revised = [json!({"spec": {"model": new.clone()}})];
        let scope = match case {
            Old::VaultOtherOutOfReach => VaultScope::Org(OrgId::try_new("org_0195ff")?),
            Old::VaultSame
            | Old::VaultOther
            | Old::Squashed
            | Old::Unpinned
            | Old::AbsoluteFile
            | Old::RateLimited
            | Old::DirSame
            | Old::DirOther => VaultScope::Every,
        };

        let resolved = resolve(
            std::slice::from_ref(&pair),
            offered.iter(),
            &revised,
            &*stores.vault,
            &scope,
            &client,
        )
        .await;

        match (resolved, expected) {
            (Ok(resolved), Ok(verified)) => {
                assert_eq!(resolved.models, vec![(old, new)]);
                assert_eq!(
                    resolved.replaced.first().map(|r| r.verified),
                    Some(verified)
                );
            }
            (Err(e), Err(message)) => assert!(e.to_string().contains(message), "{e}"),
            (got, expected) => anyhow::bail!("got {got:?}, expected {expected:?}"),
        }
        Ok(())
    }

    /// Refused before any read.
    #[rstest::rstest]
    #[case::repeated("/o.gguf=/n.gguf,/o.gguf=/x.gguf", "names /o.gguf twice")]
    #[case::no_old(
        "x/y@cccccccccccccccccccccccccccccccccccccccc:m.gguf=/n.gguf",
        "no job of the head"
    )]
    #[case::no_new("/o.gguf=/x.gguf", "no cell of the revision")]
    #[case::itself("/o.gguf=/o.gguf", "with itself")]
    #[case::other_type("/o.gguf=/v.gguf+/p.gguf", "different types")]
    #[tokio::test]
    async fn a_pair_that_names_nothing_is_refused(
        #[case] pairs: &str,
        #[case] message: &str,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores(dir.path())?;
        let client = HubClient::new("http://127.0.0.1:9".parse()?, NonZeroUsize::MIN)?;
        let file = |path: &str| json!({"spec": {"model": {"type": "gguf_text", "source": "absolute_file", "path": path}}});
        let offered = [file("/o.gguf")];
        let revised = [
            file("/n.gguf"),
            json!({"spec": {"model": {"type": "gguf_vision", "source": "absolute_files", "model": "/v.gguf", "mmproj": "/p.gguf"}}}),
        ];
        let pairs = pairs
            .split(',')
            .map(str::parse)
            .collect::<Result<Vec<ModelReplacement>, _>>()?;

        let refused = resolve(
            &pairs,
            offered.iter(),
            &revised,
            &*stores.vault,
            &VaultScope::Every,
            &client,
        )
        .await
        .err()
        .context("refused")?;

        assert!(refused.to_string().contains(message), "{refused}");
        Ok(())
    }

    async fn upload(stores: &Stores, sha256: Sha256) -> anyhow::Result<Value> {
        let record = pipette_store_testkit::vault_resource_for(
            VaultOwner::Org {
                org_id: OrgId::try_new(ORG)?,
            },
            sha256,
            1,
        )?;
        stores.vault.put_resource(&record).await?;
        Ok(json!({
            "type": "gguf_text", "source": "vault_url", "principal_id": ORG,
            "url": format!("/vault/resources/{}/content", record.resource_id),
        }))
    }

    fn stores(dir: &std::path::Path) -> anyhow::Result<Stores> {
        pipette_store_fs::build_local_fs_stores(
            &Config {
                storage: StorageConfig::local_fs(dir.to_path_buf()),
                auth_storage: StorageConfig::local_fs(dir.to_path_buf()),
                ..Config::default()
            },
            pipette_store_testkit::no_queue(),
        )
    }

    #[rstest::rstest]
    #[case::same_file(Some('a'), Some('a'), Some(true))]
    #[case::other_file(Some('a'), Some('b'), Some(false))]
    #[case::unread_side(None, Some('a'), None)]
    fn files_compare_by_checksum(
        #[case] old: Option<char>,
        #[case] new: Option<char>,
        #[case] expected: Option<bool>,
    ) -> anyhow::Result<()> {
        let print = |c: Option<char>| c.map(sha).transpose().map(|s| s.map(Print::File));
        assert_eq!(same(print(old)?, print(new)?), expected);
        Ok(())
    }

    /// Two packings of one directory can hash differently.
    #[test]
    fn archives_only_vouch_when_equal() -> anyhow::Result<()> {
        assert_eq!(
            same(
                Some(Print::Archive(sha('a')?)),
                Some(Print::Archive(sha('a')?))
            ),
            Some(true)
        );
        assert_eq!(
            same(
                Some(Print::Archive(sha('a')?)),
                Some(Print::Archive(sha('b')?))
            ),
            None
        );
        let dir = Print::Dir(BTreeMap::from([("w.safetensors".to_owned(), sha('a')?)]));
        assert_eq!(same(Some(Print::Archive(sha('a')?)), Some(dir)), None);
        Ok(())
    }

    /// One piece per file field, so a pair of one type lines its files up.
    #[rstest::rstest]
    #[case::gguf_text(
        r#"{"type":"gguf_text","source":"huggingface","org":"o","repo_name":"r","path":"m.gguf"}"#,
        1
    )]
    #[case::gguf_vision(r#"{"type":"gguf_vision","source":"vault_url","model":"/vault/resources/vault_01a13395-e4ed-719a-bdcf-ec8ef8cc151b/content","mmproj":"/vault/resources/vault_01a13395-e4ed-719a-bdcf-ec8ef8cc151c/content","principal_id":"org_1"}"#, 2)]
    #[case::gguf_vision_speculative(r#"{"type":"gguf_vision_speculative","source":"huggingface","org":"o","repo_name":"r","model":"m.gguf","mmproj":"p.gguf","draft":"d.gguf"}"#, 3)]
    #[case::merged_drafter(r#"{"type":"gguf_vision_speculative","source":"absolute_files","model":"/m.gguf","mmproj":"/p.gguf"}"#, 2)]
    #[case::mlx(
        r#"{"type":"mlx","source":"huggingface","org":"o","repo_name":"r"}"#,
        1
    )]
    #[case::torch_speculative(r#"{"type":"torch_speculative","target":{"source":"huggingface","org":"o","repo_name":"t"},"draft":{"source":"absolute_dir","dir":"/d"}}"#, 2)]
    #[case::foundation(r#"{"type":"apple_foundation_text"}"#, 0)]
    fn each_type_lists_its_files(#[case] json: &str, #[case] count: usize) -> anyhow::Result<()> {
        let model: Model = serde_json::from_str(json)?;
        assert_eq!(pieces(&model).len(), count);
        Ok(())
    }
}
