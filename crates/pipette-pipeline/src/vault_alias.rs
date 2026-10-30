//! The public descriptor a result records for a model run from the vault: the
//! HuggingFace form, from the records' origins, so vault and Hub runs share a
//! `model_descriptor`. All or nothing: a partial alias describes neither.

use std::collections::HashMap;

use pipette_plan_types::{
    GgufTextSource, GgufVisionHfFiles, GgufVisionSource, GgufVisionSpeculativeSource,
    GgufVisionVaultUrlFiles, HfRepo, HfRevision, Model, ModelSource, RepoSubpath, VaultOrigin,
    VaultPath, VaultResourceId,
};
use pipette_store::VaultStore;

use crate::vault_check::VaultScope;

/// The alias of the model `descriptor` reports, or `None` if the row keeps it.
/// A failed record read is an error, or the row would not join its HuggingFace
/// runs. Only a record outside `known` is read.
pub async fn model_alias(
    vault: &dyn VaultStore,
    descriptor: Option<&str>,
    known: &KnownOrigins,
) -> anyhow::Result<Option<String>> {
    let Some(model) = descriptor.and_then(|d| serde_json::from_str::<Model>(d).ok()) else {
        return Ok(None);
    };
    alias_of(vault, model, known)
        .await?
        .map(|alias| serde_json::to_string(&alias))
        .transpose()
        .map_err(Into::into)
}

/// [`model_alias`] as a filer may see it: a record outside `scope` reads as
/// one without an origin, so nothing derived from the alias, such as a
/// refusal, can disclose what another principal's model was imported from.
/// Each record is read once, to learn its owner.
pub async fn model_alias_within(
    vault: &dyn VaultStore,
    descriptor: Option<&str>,
    scope: &VaultScope,
) -> anyhow::Result<Option<String>> {
    let Some(model) = descriptor.and_then(|d| serde_json::from_str::<Model>(d).ok()) else {
        return Ok(None);
    };
    let mut known = KnownOrigins::new();
    for id in vault_resource_ids(&model) {
        let origin = vault
            .get_resource(id)
            .await?
            .filter(|record| scope.admits(&record.owner))
            .and_then(|record| record.origin);
        known.insert(id.clone(), origin);
    }
    alias_of(vault, model, &known)
        .await?
        .map(|alias| serde_json::to_string(&alias))
        .transpose()
        .map_err(Into::into)
}

/// The origin of each record a caller already holds, `None` for a record
/// without one.
pub type KnownOrigins = HashMap<VaultResourceId, Option<VaultOrigin>>;

/// [`model_alias`] of a parsed model: its alias, or `None` if it keeps its
/// own descriptor. Only a record outside `known` is read.
pub async fn alias_of(
    vault: &dyn VaultStore,
    model: Model,
    known: &KnownOrigins,
) -> anyhow::Result<Option<Model>> {
    let mut ids = vault_resource_ids(&model);
    if ids.is_empty() {
        return Ok(None);
    }
    ids.sort_unstable();
    ids.dedup();
    let records = futures::future::try_join_all(ids.into_iter().map(|id| async move {
        let origin = match known.get(id) {
            Some(origin) => origin.clone(),
            None => vault.get_resource(id).await?.and_then(|r| r.origin),
        };
        anyhow::Ok(origin.map(|origin| (id.clone(), origin)))
    }))
    .await?;
    let origins: HashMap<VaultResourceId, VaultOrigin> = records.into_iter().flatten().collect();
    Ok(aliased(model, &origins))
}

/// The ids of every vault file that `model` names.
fn vault_resource_ids(model: &Model) -> Vec<&VaultResourceId> {
    let paths: Vec<&VaultPath> = match model {
        Model::GgufText(m) => match &m.source {
            GgufTextSource::VaultUrl { url, .. } => vec![url],
            GgufTextSource::HuggingFace { .. }
            | GgufTextSource::RelativeFile { .. }
            | GgufTextSource::AbsoluteFile { .. }
            | GgufTextSource::Url { .. } => vec![],
        },
        Model::GgufVision(m) => match &m.source {
            GgufVisionSource::VaultUrl(f) => vec![&f.model, &f.mmproj],
            GgufVisionSource::HuggingFace(_)
            | GgufVisionSource::RelativeFiles(_)
            | GgufVisionSource::AbsoluteFiles(_)
            | GgufVisionSource::Url(_) => vec![],
        },
        Model::GgufVisionSpeculative(m) => match &m.source {
            GgufVisionSpeculativeSource::VaultUrl { files, draft, .. } => {
                [&files.model, &files.mmproj]
                    .into_iter()
                    .chain(draft.as_ref())
                    .collect()
            }
            GgufVisionSpeculativeSource::HuggingFace { .. }
            | GgufVisionSpeculativeSource::RelativeFiles { .. }
            | GgufVisionSpeculativeSource::AbsoluteFiles { .. }
            | GgufVisionSpeculativeSource::Url { .. } => vec![],
        },
        Model::Mlx(m) => archive_path(&m.source).into_iter().collect(),
        Model::Torch(m) => archive_path(&m.source).into_iter().collect(),
        Model::Openvino(m) => archive_path(&m.source).into_iter().collect(),
        Model::OnnxGenai(m) => archive_path(&m.source).into_iter().collect(),
        Model::MlxSpeculative(pair) | Model::TorchSpeculative(pair) => pair
            .sources()
            .into_iter()
            .filter_map(archive_path)
            .collect(),
        Model::AppleFoundationText => vec![],
    };
    paths.into_iter().map(VaultPath::resource_id).collect()
}

fn archive_path(source: &ModelSource) -> Option<&VaultPath> {
    match source {
        ModelSource::VaultArchive { url, .. } => Some(url),
        ModelSource::HuggingFace { .. }
        | ModelSource::RelativeDir { .. }
        | ModelSource::AbsoluteDir { .. } => None,
    }
}

/// Replace each vault source of `model` with its HuggingFace arm, or `None` if
/// one has none.
fn aliased(model: Model, origins: &HashMap<VaultResourceId, VaultOrigin>) -> Option<Model> {
    let origin = |url: &VaultPath| hf_origin(origins, url);
    Some(match model {
        Model::GgufText(mut m) => {
            if let GgufTextSource::VaultUrl { url, .. } = &m.source {
                let (repo, path) = origin(url)?;
                m.source = GgufTextSource::HuggingFace {
                    repo,
                    path: path?,
                    sha256: None,
                };
            }
            Model::GgufText(m)
        }
        Model::GgufVision(mut m) => {
            if let GgufVisionSource::VaultUrl(files) = &m.source {
                m.source = GgufVisionSource::HuggingFace(hf_files(origins, files)?.0);
            }
            Model::GgufVision(m)
        }
        Model::GgufVisionSpeculative(mut m) => {
            if let GgufVisionSpeculativeSource::VaultUrl { files, draft, .. } = &m.source {
                let (files, repo) = hf_files(origins, files)?;
                let draft = draft
                    .as_ref()
                    .map(|url| same_repo_file(origin(url)?, &repo))
                    .map_or(Some(None), |d| d.map(Some))?;
                m.source = GgufVisionSpeculativeSource::HuggingFace {
                    files,
                    draft,
                    draft_sha256: None,
                };
            }
            Model::GgufVisionSpeculative(m)
        }
        Model::Mlx(mut m) => {
            m.source = aliased_source(origins, &m.source)?;
            Model::Mlx(m)
        }
        Model::Torch(mut m) => {
            m.source = aliased_source(origins, &m.source)?;
            Model::Torch(m)
        }
        Model::Openvino(mut m) => {
            m.source = aliased_source(origins, &m.source)?;
            Model::Openvino(m)
        }
        Model::OnnxGenai(mut m) => {
            m.source = aliased_source(origins, &m.source)?;
            Model::OnnxGenai(m)
        }
        Model::MlxSpeculative(pair) => Model::MlxSpeculative(aliased_pair(origins, &pair)?),
        Model::TorchSpeculative(pair) => Model::TorchSpeculative(aliased_pair(origins, &pair)?),
        Model::AppleFoundationText => Model::AppleFoundationText,
    })
}

/// The commit-pinned HuggingFace repository and file the vault file at `url`
/// copies, or `None` without a HuggingFace origin.
fn hf_origin(
    origins: &HashMap<VaultResourceId, VaultOrigin>,
    url: &VaultPath,
) -> Option<(HfRepo, Option<RepoSubpath>)> {
    match origins.get(url.resource_id())? {
        VaultOrigin::HuggingFace {
            org,
            repo_name,
            commit,
            path,
        } => Some((
            HfRepo {
                org: org.clone(),
                repo_name: repo_name.clone(),
                revision: Some(HfRevision::try_new(commit.to_string()).ok()?),
                auth_token: None,
            },
            path.clone(),
        )),
        VaultOrigin::GithubRelease { .. } => None,
    }
}

/// The path of a file that the HuggingFace arm names inside `repo`. The file
/// must have a path. Its repository and commit must be the same as `repo`.
fn same_repo_file(
    (repo, path): (HfRepo, Option<RepoSubpath>),
    expected: &HfRepo,
) -> Option<RepoSubpath> {
    (repo == *expected).then_some(path).flatten()
}

/// The weights and projector of a vision model as the HuggingFace arm names
/// them, with the one repository that holds both.
fn hf_files(
    origins: &HashMap<VaultResourceId, VaultOrigin>,
    files: &GgufVisionVaultUrlFiles,
) -> Option<(GgufVisionHfFiles, HfRepo)> {
    let (repo, model) = hf_origin(origins, &files.model)?;
    let mmproj = same_repo_file(hf_origin(origins, &files.mmproj)?, &repo)?;
    Some((
        GgufVisionHfFiles {
            repo: repo.clone(),
            model: model?,
            model_sha256: None,
            mmproj,
            mmproj_sha256: None,
        },
        repo,
    ))
}

/// A directory source with a vault archive replaced by its HuggingFace arm.
/// The origin's path is the `prefix` subdirectory of the repository.
fn aliased_source(
    origins: &HashMap<VaultResourceId, VaultOrigin>,
    source: &ModelSource,
) -> Option<ModelSource> {
    match source {
        ModelSource::VaultArchive { url, .. } => {
            let (repo, prefix) = hf_origin(origins, url)?;
            Some(ModelSource::HuggingFace { repo, prefix })
        }
        ModelSource::HuggingFace { .. }
        | ModelSource::RelativeDir { .. }
        | ModelSource::AbsoluteDir { .. } => Some(source.clone()),
    }
}

fn aliased_pair(
    origins: &HashMap<VaultResourceId, VaultOrigin>,
    pair: &pipette_plan_types::SpeculativePair,
) -> Option<pipette_plan_types::SpeculativePair> {
    Some(pipette_plan_types::SpeculativePair {
        target: aliased_source(origins, &pair.target)?,
        draft: aliased_source(origins, &pair.draft)?,
    })
}

#[cfg(test)]
mod tests {
    use anyhow::Context;
    use async_trait::async_trait;
    use rstest::rstest;
    use serde_json::{Value, json};

    use pipette_core::vault::{VaultObjectKey, VaultResource};
    use pipette_plan_types::Sha256;
    use pipette_store::{ArtifactBody, BlobBody, BlobWrite, VaultResourcePage};

    use super::*;

    const V1: &str = "vault_0199aaaa-bbbb-7ccc-8ddd-000000000001";
    const V2: &str = "vault_0199aaaa-bbbb-7ccc-8ddd-000000000002";
    const V3: &str = "vault_0199aaaa-bbbb-7ccc-8ddd-000000000003";

    fn url(id: &str) -> String {
        format!("/vault/resources/{id}/content")
    }

    fn commit(c: char) -> String {
        c.to_string().repeat(40)
    }

    fn hf(repo: &str, c: char, path: Option<&str>) -> Value {
        let mut origin = json!({"source": "huggingface", "org": "LiquidAI", "repo_name": repo, "commit": commit(c)});
        if let Some(path) = path {
            origin["path"] = json!(path);
        }
        origin
    }

    fn github() -> Value {
        json!({"source": "github_release", "owner": "ggml-org", "repo": "llama.cpp", "commit": commit('c'), "asset": "llama-cli.tar.gz"})
    }

    fn origins(raw: &[(&str, Value)]) -> anyhow::Result<HashMap<VaultResourceId, VaultOrigin>> {
        raw.iter()
            .map(|(id, origin)| Ok((id.parse()?, serde_json::from_value(origin.clone())?)))
            .collect()
    }

    /// The arm pins the origin's commit and has no digest, the form a run from
    /// HuggingFace names.
    #[rstest]
    #[case::gguf_text(
        json!({"type": "gguf_text", "source": "vault_url", "url": url(V1), "sha256": "e".repeat(64), "principal_id": "org_1"}),
        vec![(V1, hf("LFM2-700M-GGUF", 'a', Some("LFM2-700M-Q4_0.gguf")))],
        json!({"type": "gguf_text", "source": "huggingface", "org": "LiquidAI", "repo_name": "LFM2-700M-GGUF", "revision": commit('a'), "path": "LFM2-700M-Q4_0.gguf"})
    )]
    #[case::gguf_vision(
        json!({"type": "gguf_vision", "source": "vault_url", "model": url(V1), "mmproj": url(V2), "principal_id": "org_1"}),
        vec![(V1, hf("LFM2-VL-3B-GGUF", 'a', Some("m.gguf"))), (V2, hf("LFM2-VL-3B-GGUF", 'a', Some("p.gguf")))],
        json!({"type": "gguf_vision", "source": "huggingface", "org": "LiquidAI", "repo_name": "LFM2-VL-3B-GGUF", "revision": commit('a'), "model": "m.gguf", "mmproj": "p.gguf"})
    )]
    #[case::gguf_vision_speculative(
        json!({"type": "gguf_vision_speculative", "source": "vault_url", "model": url(V1), "mmproj": url(V2), "draft": url(V3), "principal_id": "org_1"}),
        vec![(V1, hf("LFM2-VL-3B-GGUF", 'a', Some("m.gguf"))), (V2, hf("LFM2-VL-3B-GGUF", 'a', Some("p.gguf"))), (V3, hf("LFM2-VL-3B-GGUF", 'a', Some("d.gguf")))],
        json!({"type": "gguf_vision_speculative", "source": "huggingface", "org": "LiquidAI", "repo_name": "LFM2-VL-3B-GGUF", "revision": commit('a'), "model": "m.gguf", "mmproj": "p.gguf", "draft": "d.gguf"})
    )]
    #[case::gguf_vision_speculative_merged_draft(
        json!({"type": "gguf_vision_speculative", "source": "vault_url", "model": url(V1), "mmproj": url(V2), "principal_id": "org_1"}),
        vec![(V1, hf("LFM2-VL-3B-GGUF", 'a', Some("m.gguf"))), (V2, hf("LFM2-VL-3B-GGUF", 'a', Some("p.gguf")))],
        json!({"type": "gguf_vision_speculative", "source": "huggingface", "org": "LiquidAI", "repo_name": "LFM2-VL-3B-GGUF", "revision": commit('a'), "model": "m.gguf", "mmproj": "p.gguf"})
    )]
    #[case::mlx_whole_repo(
        json!({"type": "mlx", "source": "vault_archive", "url": url(V1), "principal_id": "org_1"}),
        vec![(V1, hf("LFM2.5-350M-MLX-4bit", 'b', None))],
        json!({"type": "mlx", "source": "huggingface", "org": "LiquidAI", "repo_name": "LFM2.5-350M-MLX-4bit", "revision": commit('b')})
    )]
    #[case::openvino_prefix(
        json!({"type": "openvino", "source": "vault_archive", "url": url(V1), "principal_id": "org_1"}),
        vec![(V1, hf("LFM2.5-350M-ov", 'b', Some("int4-sym-cw")))],
        json!({"type": "openvino", "source": "huggingface", "org": "LiquidAI", "repo_name": "LFM2.5-350M-ov", "revision": commit('b'), "prefix": "int4-sym-cw"})
    )]
    #[case::torch_speculative_vault_target_hf_draft(
        json!({"type": "torch_speculative", "target": {"source": "vault_archive", "url": url(V1), "principal_id": "org_1"}, "draft": {"source": "huggingface", "org": "LiquidAI", "repo_name": "drafter"}}),
        vec![(V1, hf("LFM2.5-VL-3B", 'a', None))],
        json!({"type": "torch_speculative", "target": {"source": "huggingface", "org": "LiquidAI", "repo_name": "LFM2.5-VL-3B", "revision": commit('a')}, "draft": {"source": "huggingface", "org": "LiquidAI", "repo_name": "drafter"}})
    )]
    fn a_vault_model_aliases_as_its_huggingface_model(
        #[case] reported: Value,
        #[case] origin: Vec<(&str, Value)>,
        #[case] expected: Value,
    ) -> anyhow::Result<()> {
        let model: Model = serde_json::from_value(reported)?;
        let alias = aliased(model, &origins(&origin)?).context("expected an alias")?;
        assert_eq!(serde_json::to_value(&alias)?, expected);
        Ok(())
    }

    /// A model gets no alias unless every vault file has a HuggingFace origin
    /// that its source's HuggingFace arm can name.
    #[rstest]
    #[case::uploaded(
        json!({"type": "gguf_text", "source": "vault_url", "url": url(V1), "principal_id": "org_1"}),
        vec![]
    )]
    #[case::github_origin(
        json!({"type": "gguf_text", "source": "vault_url", "url": url(V1), "principal_id": "org_1"}),
        vec![(V1, github())]
    )]
    #[case::gguf_whole_repo(
        json!({"type": "gguf_text", "source": "vault_url", "url": url(V1), "principal_id": "org_1"}),
        vec![(V1, hf("LFM2-700M-GGUF", 'a', None))]
    )]
    #[case::vision_projector_uploaded(
        json!({"type": "gguf_vision", "source": "vault_url", "model": url(V1), "mmproj": url(V2), "principal_id": "org_1"}),
        vec![(V1, hf("LFM2-VL-3B-GGUF", 'a', Some("m.gguf")))]
    )]
    #[case::vision_files_from_two_repos(
        json!({"type": "gguf_vision", "source": "vault_url", "model": url(V1), "mmproj": url(V2), "principal_id": "org_1"}),
        vec![(V1, hf("LFM2-VL-3B-GGUF", 'a', Some("m.gguf"))), (V2, hf("other", 'a', Some("p.gguf")))]
    )]
    #[case::vision_files_at_two_commits(
        json!({"type": "gguf_vision", "source": "vault_url", "model": url(V1), "mmproj": url(V2), "principal_id": "org_1"}),
        vec![(V1, hf("LFM2-VL-3B-GGUF", 'a', Some("m.gguf"))), (V2, hf("LFM2-VL-3B-GGUF", 'b', Some("p.gguf")))]
    )]
    #[case::speculative_draft_uploaded(
        json!({"type": "gguf_vision_speculative", "source": "vault_url", "model": url(V1), "mmproj": url(V2), "draft": url(V3), "principal_id": "org_1"}),
        vec![(V1, hf("LFM2-VL-3B-GGUF", 'a', Some("m.gguf"))), (V2, hf("LFM2-VL-3B-GGUF", 'a', Some("p.gguf")))]
    )]
    #[case::pair_draft_uploaded(
        json!({"type": "mlx_speculative", "target": {"source": "vault_archive", "url": url(V1), "principal_id": "org_1"}, "draft": {"source": "vault_archive", "url": url(V2), "principal_id": "org_1"}}),
        vec![(V1, hf("LFM2.5-350M-MLX-4bit", 'a', None))]
    )]
    fn a_model_without_a_full_origin_has_no_alias(
        #[case] reported: Value,
        #[case] origin: Vec<(&str, Value)>,
    ) -> anyhow::Result<()> {
        let model: Model = serde_json::from_value(reported)?;
        assert_eq!(aliased(model, &origins(&origin)?), None);
        Ok(())
    }

    /// A vault store that answers from a map, or fails every read.
    struct Records(Option<HashMap<VaultResourceId, VaultResource>>);

    #[async_trait]
    impl VaultStore for Records {
        async fn get_resource(
            &self,
            resource_id: &VaultResourceId,
        ) -> anyhow::Result<Option<VaultResource>> {
            self.0
                .as_ref()
                .map(|records| records.get(resource_id).cloned())
                .context("the vault store is unreachable")
        }
        async fn put_resource(&self, _: &VaultResource) -> anyhow::Result<()> {
            anyhow::bail!("not used")
        }
        async fn list_resources(
            &self,
            _: Option<&VaultResourceId>,
            _: std::num::NonZeroUsize,
        ) -> anyhow::Result<VaultResourcePage> {
            anyhow::bail!("not used")
        }
        async fn delete_resource(&self, _: &VaultResourceId) -> anyhow::Result<()> {
            anyhow::bail!("not used")
        }
        async fn put_blob(
            &self,
            _: &VaultObjectKey,
            _: &Sha256,
            _: BlobBody,
        ) -> anyhow::Result<BlobWrite> {
            anyhow::bail!("not used")
        }
        async fn stat_blob(&self, _: &VaultObjectKey) -> anyhow::Result<Option<u64>> {
            anyhow::bail!("not used")
        }
        async fn delete_blob(&self, _key: &VaultObjectKey) -> anyhow::Result<()> {
            anyhow::bail!("not used")
        }
        async fn open_blob(
            &self,
            _: &VaultObjectKey,
            _: u64,
        ) -> anyhow::Result<Option<ArtifactBody>> {
            anyhow::bail!("not used")
        }
        async fn put_staged_blob(
            &self,
            _: &pipette_core::vault::VaultOwner,
            _: BlobBody,
            _: std::num::NonZeroU64,
        ) -> anyhow::Result<pipette_store::StagedWrite> {
            anyhow::bail!("not used")
        }
        async fn get_import(
            &self,
            _: &pipette_core::vault::VaultOwner,
            _: &Sha256,
        ) -> anyhow::Result<Option<pipette_core::vault::VaultImport>> {
            anyhow::bail!("not used")
        }
        async fn put_import(
            &self,
            _: &pipette_core::vault::VaultOwner,
            _: &Sha256,
            _: &pipette_core::vault::VaultImport,
        ) -> anyhow::Result<()> {
            anyhow::bail!("not used")
        }
    }

    fn record(id: &str, origin: Option<Value>) -> anyhow::Result<(VaultResourceId, VaultResource)> {
        let resource = VaultResource {
            resource_id: id.parse()?,
            origin: origin.map(serde_json::from_value).transpose()?,
            ..pipette_store_testkit::a_vault_resource()?
        };
        Ok((resource.resource_id.clone(), resource))
    }

    /// The store that the alias tests read: one imported record and one
    /// upload.
    fn records() -> anyhow::Result<Records> {
        Ok(Records(Some(
            [
                record(V1, Some(hf("LFM2-700M-GGUF", 'a', Some("m.gguf"))))?,
                record(V2, None)?,
            ]
            .into_iter()
            .collect(),
        )))
    }

    /// A `gguf_text` descriptor that names the vault file `id`.
    fn vault(id: &str) -> String {
        json!({"type": "gguf_text", "source": "vault_url", "url": url(id), "principal_id": "org_1"})
            .to_string()
    }

    #[tokio::test]
    async fn the_alias_reads_the_records_the_descriptor_names() -> anyhow::Result<()> {
        let alias = model_alias(&records()?, Some(&vault(V1)), &KnownOrigins::new())
            .await?
            .context("expected an alias")?;
        assert_eq!(
            serde_json::from_str::<Value>(&alias)?,
            json!({"type": "gguf_text", "source": "huggingface", "org": "LiquidAI", "repo_name": "LFM2-700M-GGUF", "revision": commit('a'), "path": "m.gguf"})
        );
        Ok(())
    }

    /// A record that is gone has no origin.
    #[rstest]
    #[case::upload(Some(vault(V2)))]
    #[case::deleted_record(Some(vault(V3)))]
    #[case::no_descriptor(None)]
    #[case::unparseable(Some("not json".to_string()))]
    #[tokio::test]
    async fn a_descriptor_without_an_imported_record_has_no_alias(
        #[case] descriptor: Option<String>,
    ) -> anyhow::Result<()> {
        let alias = model_alias(&records()?, descriptor.as_deref(), &KnownOrigins::new()).await?;
        assert_eq!(alias, None);
        Ok(())
    }

    /// A failed read is an error, not an absent origin.
    #[tokio::test]
    async fn only_a_vault_file_is_read() -> anyhow::Result<()> {
        let unreachable = Records(None);
        let hf_model =
            json!({"type": "mlx", "source": "huggingface", "org": "LiquidAI", "repo_name": "m"})
                .to_string();
        let known = KnownOrigins::new();
        assert_eq!(
            model_alias(&unreachable, Some(&hf_model), &known).await?,
            None,
            "no vault file, no read"
        );
        assert!(
            model_alias(&unreachable, Some(&vault(V1)), &known)
                .await
                .is_err(),
            "a failed read is an error, not an absent origin"
        );
        Ok(())
    }

    /// A filer sees the origin of its own records only: another principal's
    /// record reads as one without an origin.
    #[tokio::test]
    async fn an_alias_within_a_scope_reads_only_its_records_origins() -> anyhow::Result<()> {
        let (id, resource) = record(V1, Some(hf("LFM2-700M-GGUF", 'a', Some("m.gguf"))))?;
        let pipette_core::vault::VaultOwner::User { user_id } = resource.owner.clone() else {
            anyhow::bail!("the fixture is a user's upload");
        };
        let store = Records(Some([(id, resource)].into_iter().collect()));
        let owner = VaultScope::Own {
            user: user_id,
            org: None,
        };
        let stranger = VaultScope::Own {
            user: pipette_plan_types::UserId::try_new("user_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e")?,
            org: None,
        };
        for (scope, aliased) in [(VaultScope::Every, true), (owner, true), (stranger, false)] {
            let alias = model_alias_within(&store, Some(&vault(V1)), &scope).await?;
            assert_eq!(alias.is_some(), aliased, "{scope:?}");
        }
        Ok(())
    }

    #[rstest]
    #[case::imported(Some(hf("LFM2-700M-GGUF", 'a', Some("m.gguf"))), true)]
    #[case::upload(None, false)]
    #[tokio::test]
    async fn a_known_record_is_not_read(
        #[case] origin: Option<Value>,
        #[case] aliased: bool,
    ) -> anyhow::Result<()> {
        let unreachable = Records(None);
        let known =
            KnownOrigins::from([(V1.parse()?, origin.map(serde_json::from_value).transpose()?)]);
        let alias = alias_of(&unreachable, serde_json::from_str(&vault(V1))?, &known).await?;
        assert_eq!(alias.is_some(), aliased);
        Ok(())
    }
}
