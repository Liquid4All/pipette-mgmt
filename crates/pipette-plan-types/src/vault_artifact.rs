//! What an upload to the vault names, and the plan entry it becomes: mgmt
//! stores one resource per file of an [`UploadVaultResourcesRequest`] and
//! answers with the [`Model`] or [`Runtime`] that [`VaultArtifact::into_entry`]
//! builds.

use std::convert::Infallible;

use serde::de::{self, DeserializeOwned};
use serde::{Deserialize, Deserializer, Serialize};

use crate::{
    FileName, GgufText, GgufTextSource, GgufVision, GgufVisionSource, GgufVisionSpeculative,
    GgufVisionSpeculativeSource, GgufVisionVaultUrlFiles, LlamaCppFlavor, LlamacppCliStockTools,
    LlamacppCliStockToolsSource, Mlx, Model, ModelSource, NonEmptyTrimmedString, OnnxGenai,
    Openvino, Ovms, OvmsFlavor, OvmsSource, Runtime, Sha256, SpeculativePair, Torch, TrtEdgellm,
    TrtEdgellmFlavor, TrtEdgellmSource, VaultPath, VaultPrincipalId, VaultResourceId,
};

/// A model a device can download, with its files and without its source.
/// Tagged with [`Model`]'s own `type`. `F` is a file at each stage: a
/// [`VaultUploadFile`] as an upload names it, a [`VaultStoredFile`] as a record
/// keeps it.
///
/// A gguf is uploaded as itself. A model directory is uploaded as one
/// `*dir_archive`: the directory packed as a tar, tar.gz or zip whose root is
/// the directory itself, not a folder holding it.
#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    Hash,
    Deserialize,
    Serialize,
    strum::IntoStaticStr,
    strum::VariantNames,
)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
#[strum(serialize_all = "snake_case")]
pub enum VaultModel<F> {
    /// One `.gguf`.
    GgufText { model: F },
    /// The weights and the projector, a `.gguf` each.
    GgufVision { model: F, mmproj: F },
    /// As [`Self::GgufVision`], plus the drafter's `.gguf` unless it is merged
    /// into `model`.
    GgufVisionSpeculative {
        model: F,
        mmproj: F,
        // No `default`: it would bound `F: Default`, and a missing `Option` reads as `None` anyway.
        #[serde(skip_serializing_if = "Option::is_none")]
        draft: Option<F>,
    },
    /// The MLX bundle directory (safetensors and config), packed.
    Mlx { dir_archive: F },
    /// The target's and the drafter's directories, each packed.
    MlxSpeculative {
        target_dir_archive: F,
        draft_dir_archive: F,
    },
    /// The safetensors checkpoint directory, packed.
    Torch { dir_archive: F },
    /// The target's and the drafter's checkpoint directories, each packed.
    TorchSpeculative {
        target_dir_archive: F,
        draft_dir_archive: F,
    },
    /// The OpenVINO IR directory, packed, with its tokenizer and detokenizer
    /// IRs: without them it loads and cannot generate.
    Openvino { dir_archive: F },
    /// The ONNX Runtime GenAI directory, packed: graph, weights,
    /// `genai_config.json` and tokenizer files.
    OnnxGenai { dir_archive: F },
}

/// A prebuilt runtime a device can download, tagged with [`Runtime`]'s `type`:
/// `archive` is what a `remote_archive` URL for `flavor` would serve (zip on
/// Windows, tar.gz elsewhere), uploaded as is. No uv wheelhouse: it also
/// carries install choices (`contents`, `install_flags`).
#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    Hash,
    Deserialize,
    Serialize,
    strum::IntoStaticStr,
    strum::VariantNames,
)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
#[strum(serialize_all = "snake_case")]
pub enum VaultRuntime<F> {
    LlamacppCliStockTools {
        flavor: LlamaCppFlavor,
        archive: F,
    },
    Ovms {
        flavor: OvmsFlavor,
        archive: F,
    },
    TrtEdgellm {
        flavor: TrtEdgellmFlavor,
        archive: F,
    },
}

/// Either family, spelled as the variant alone: model and runtime `type` tags
/// never collide, so the tag says which family it is.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(untagged)]
pub enum VaultArtifact<F> {
    Model(VaultModel<F>),
    Runtime(VaultRuntime<F>),
}

/// Dispatched on `type` rather than `untagged`, so a refusal names what was
/// wrong with the variant it chose instead of that no variant matched.
impl<'de, F: DeserializeOwned> Deserialize<'de> for VaultArtifact<F> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use strum::VariantNames as _;
        let value = serde_json::Value::deserialize(deserializer)?;
        let tag = value
            .get("type")
            .ok_or_else(|| de::Error::missing_field("type"))?
            .as_str()
            .ok_or_else(|| de::Error::custom("`type` is not a string"))?;
        let models = VaultModel::<F>::VARIANTS;
        let runtimes = VaultRuntime::<F>::VARIANTS;
        let parsed = if models.contains(&tag) {
            VaultModel::deserialize(value).map(Self::Model)
        } else if runtimes.contains(&tag) {
            VaultRuntime::deserialize(value).map(Self::Runtime)
        } else {
            let expected: Vec<&str> = models.iter().chain(runtimes).copied().collect();
            return Err(de::Error::custom(format!(
                "unknown type `{tag}`, expected one of `{}`",
                expected.join("`, `")
            )));
        };
        parsed.map_err(de::Error::custom)
    }
}

/// The field a file fills in a variant, so mgmt can key what it stores per
/// file.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize, strum::Display,
)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum VaultFileRole {
    Model,
    Mmproj,
    Draft,
    DirArchive,
    TargetDirArchive,
    DraftDirArchive,
    Archive,
}

impl VaultFileRole {
    pub const fn format(self) -> VaultFileFormat {
        match self {
            Self::Model | Self::Mmproj | Self::Draft => VaultFileFormat::Gguf,
            Self::DirArchive | Self::TargetDirArchive | Self::DraftDirArchive | Self::Archive => {
                VaultFileFormat::Archive
            }
        }
    }
}

/// What a file's bytes must be, as far as their first bytes can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum VaultFileFormat {
    Gguf,
    Archive,
}

impl<F> VaultModel<F> {
    /// The wire tag, for messages.
    pub fn type_name(&self) -> &'static str {
        self.into()
    }

    pub fn files(&self) -> Vec<(VaultFileRole, &F)> {
        match self {
            Self::GgufText { model } => vec![(VaultFileRole::Model, model)],
            Self::GgufVision { model, mmproj } => vec![
                (VaultFileRole::Model, model),
                (VaultFileRole::Mmproj, mmproj),
            ],
            Self::GgufVisionSpeculative {
                model,
                mmproj,
                draft,
            } => [
                Some((VaultFileRole::Model, model)),
                Some((VaultFileRole::Mmproj, mmproj)),
                draft.as_ref().map(|d| (VaultFileRole::Draft, d)),
            ]
            .into_iter()
            .flatten()
            .collect(),
            Self::Mlx { dir_archive }
            | Self::Torch { dir_archive }
            | Self::Openvino { dir_archive }
            | Self::OnnxGenai { dir_archive } => vec![(VaultFileRole::DirArchive, dir_archive)],
            Self::MlxSpeculative {
                target_dir_archive,
                draft_dir_archive,
            }
            | Self::TorchSpeculative {
                target_dir_archive,
                draft_dir_archive,
            } => vec![
                (VaultFileRole::TargetDirArchive, target_dir_archive),
                (VaultFileRole::DraftDirArchive, draft_dir_archive),
            ],
        }
    }

    /// The same model with every file replaced, stopping at the first error.
    pub fn try_map<G, E>(
        self,
        mut f: impl FnMut(VaultFileRole, F) -> Result<G, E>,
    ) -> Result<VaultModel<G>, E> {
        Ok(match self {
            Self::GgufText { model } => VaultModel::GgufText {
                model: f(VaultFileRole::Model, model)?,
            },
            Self::GgufVision { model, mmproj } => VaultModel::GgufVision {
                model: f(VaultFileRole::Model, model)?,
                mmproj: f(VaultFileRole::Mmproj, mmproj)?,
            },
            Self::GgufVisionSpeculative {
                model,
                mmproj,
                draft,
            } => VaultModel::GgufVisionSpeculative {
                model: f(VaultFileRole::Model, model)?,
                mmproj: f(VaultFileRole::Mmproj, mmproj)?,
                draft: draft.map(|d| f(VaultFileRole::Draft, d)).transpose()?,
            },
            Self::Mlx { dir_archive } => VaultModel::Mlx {
                dir_archive: f(VaultFileRole::DirArchive, dir_archive)?,
            },
            Self::MlxSpeculative {
                target_dir_archive,
                draft_dir_archive,
            } => VaultModel::MlxSpeculative {
                target_dir_archive: f(VaultFileRole::TargetDirArchive, target_dir_archive)?,
                draft_dir_archive: f(VaultFileRole::DraftDirArchive, draft_dir_archive)?,
            },
            Self::Torch { dir_archive } => VaultModel::Torch {
                dir_archive: f(VaultFileRole::DirArchive, dir_archive)?,
            },
            Self::TorchSpeculative {
                target_dir_archive,
                draft_dir_archive,
            } => VaultModel::TorchSpeculative {
                target_dir_archive: f(VaultFileRole::TargetDirArchive, target_dir_archive)?,
                draft_dir_archive: f(VaultFileRole::DraftDirArchive, draft_dir_archive)?,
            },
            Self::Openvino { dir_archive } => VaultModel::Openvino {
                dir_archive: f(VaultFileRole::DirArchive, dir_archive)?,
            },
            Self::OnnxGenai { dir_archive } => VaultModel::OnnxGenai {
                dir_archive: f(VaultFileRole::DirArchive, dir_archive)?,
            },
        })
    }
}

impl<F> VaultRuntime<F> {
    /// The wire tag, for messages.
    pub fn type_name(&self) -> &'static str {
        self.into()
    }

    pub fn files(&self) -> Vec<(VaultFileRole, &F)> {
        match self {
            Self::LlamacppCliStockTools { archive, .. }
            | Self::Ovms { archive, .. }
            | Self::TrtEdgellm { archive, .. } => vec![(VaultFileRole::Archive, archive)],
        }
    }

    /// The same runtime with its archive replaced.
    pub fn try_map<G, E>(
        self,
        mut f: impl FnMut(VaultFileRole, F) -> Result<G, E>,
    ) -> Result<VaultRuntime<G>, E> {
        Ok(match self {
            Self::LlamacppCliStockTools { flavor, archive } => {
                VaultRuntime::LlamacppCliStockTools {
                    flavor,
                    archive: f(VaultFileRole::Archive, archive)?,
                }
            }
            Self::Ovms { flavor, archive } => VaultRuntime::Ovms {
                flavor,
                archive: f(VaultFileRole::Archive, archive)?,
            },
            Self::TrtEdgellm { flavor, archive } => VaultRuntime::TrtEdgellm {
                flavor,
                archive: f(VaultFileRole::Archive, archive)?,
            },
        })
    }
}

impl<F> VaultArtifact<F> {
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Model(m) => m.type_name(),
            Self::Runtime(r) => r.type_name(),
        }
    }

    pub fn files(&self) -> Vec<(VaultFileRole, &F)> {
        match self {
            Self::Model(m) => m.files(),
            Self::Runtime(r) => r.files(),
        }
    }

    pub fn map<G>(self, mut f: impl FnMut(VaultFileRole, F) -> G) -> VaultArtifact<G> {
        let Ok(mapped) = self.try_map(|role, file| Ok::<_, Infallible>(f(role, file)));
        mapped
    }

    pub fn try_map<G, E>(
        self,
        f: impl FnMut(VaultFileRole, F) -> Result<G, E>,
    ) -> Result<VaultArtifact<G>, E> {
        Ok(match self {
            Self::Model(m) => VaultArtifact::Model(m.try_map(f)?),
            Self::Runtime(r) => VaultArtifact::Runtime(r.try_map(f)?),
        })
    }
}

/// A vault upload's description: what to call the files, whose they are,
/// and beside those the variant they make up. Each file's bytes are already
/// uploaded under the owner, by digest.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct UploadVaultResourcesRequest {
    pub name: NonEmptyTrimmedString,
    /// `None` is the uploader itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<VaultPrincipalId>,
    /// Flattened, and a stray key is still refused: the variant's own
    /// `deny_unknown_fields` sees every key but `name` and `owner`.
    #[serde(flatten)]
    pub artifact: VaultArtifact<VaultUploadFile>,
}

/// One file as an upload names it: its name, and the digest its bytes were
/// uploaded under.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VaultUploadFile {
    pub filename: FileName,
    pub sha256: Sha256,
}

/// One stored file, as a resource record keeps the artifact it was uploaded
/// as: the resource that serves it, its name, and what its bytes hash to.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VaultStoredFile {
    pub resource_id: VaultResourceId,
    pub filename: FileName,
    pub sha256: Sha256,
}

impl VaultStoredFile {
    fn url(&self) -> VaultPath {
        VaultPath::of(self.resource_id.clone())
    }
}

/// The plan entry an upload answers with: the `models` or `runtimes` element
/// whose every source is the vault.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum VaultEntry {
    Model(Model),
    Runtime(Runtime),
}

impl VaultEntry {
    /// The entry as a TOML inline table, the form a plan's `models` or
    /// `runtimes` array lists it in.
    pub fn to_toml(&self) -> Result<String, toml::ser::Error> {
        let mut out = String::new();
        let serializer = toml::ser::ValueSerializer::new(&mut out);
        match self {
            Self::Model(model) => model.serialize(serializer)?,
            Self::Runtime(runtime) => runtime.serialize(serializer)?,
        };
        Ok(out)
    }
}

fn vault_archive(
    file: VaultStoredFile,
    principal_id: VaultPrincipalId,
    custom_label: Option<NonEmptyTrimmedString>,
) -> ModelSource {
    ModelSource::VaultArchive {
        url: file.url(),
        sha256: Some(file.sha256),
        principal_id,
        custom_label,
    }
}

fn vision_vault_files(
    model: VaultStoredFile,
    mmproj: VaultStoredFile,
    principal_id: VaultPrincipalId,
    custom_label: Option<NonEmptyTrimmedString>,
) -> GgufVisionVaultUrlFiles {
    GgufVisionVaultUrlFiles {
        model: model.url(),
        model_sha256: Some(model.sha256),
        mmproj: mmproj.url(),
        mmproj_sha256: Some(mmproj.sha256),
        principal_id,
        custom_label,
    }
}

fn vault_pair(
    target: VaultStoredFile,
    draft: VaultStoredFile,
    principal_id: VaultPrincipalId,
    custom_label: Option<NonEmptyTrimmedString>,
) -> SpeculativePair {
    SpeculativePair {
        target: vault_archive(target, principal_id.clone(), custom_label.clone()),
        draft: vault_archive(draft, principal_id, custom_label),
    }
}

/// Struct literals throughout, here and for [`Runtime`], so a field added to a
/// plan-types variant fails to compile until the vault side carries it too.
impl VaultModel<VaultStoredFile> {
    /// The plan entry that fetches these files for `principal_id`, each pinned
    /// to its digest, under `custom_label` where their records share that name.
    pub fn into_model(
        self,
        principal_id: VaultPrincipalId,
        custom_label: Option<NonEmptyTrimmedString>,
    ) -> Model {
        match self {
            Self::GgufText { model } => Model::GgufText(GgufText {
                source: GgufTextSource::VaultUrl {
                    url: model.url(),
                    sha256: Some(model.sha256),
                    principal_id,
                    custom_label,
                },
            }),
            Self::GgufVision { model, mmproj } => Model::GgufVision(GgufVision {
                source: GgufVisionSource::VaultUrl(vision_vault_files(
                    model,
                    mmproj,
                    principal_id,
                    custom_label,
                )),
            }),
            Self::GgufVisionSpeculative {
                model,
                mmproj,
                draft,
            } => Model::GgufVisionSpeculative(GgufVisionSpeculative {
                source: GgufVisionSpeculativeSource::VaultUrl {
                    files: vision_vault_files(model, mmproj, principal_id, custom_label),
                    draft: draft.as_ref().map(VaultStoredFile::url),
                    draft_sha256: draft.map(|d| d.sha256),
                },
            }),
            Self::Mlx { dir_archive } => Model::Mlx(Mlx {
                source: vault_archive(dir_archive, principal_id, custom_label),
            }),
            Self::Torch { dir_archive } => Model::Torch(Torch {
                source: vault_archive(dir_archive, principal_id, custom_label),
            }),
            Self::Openvino { dir_archive } => Model::Openvino(Openvino {
                source: vault_archive(dir_archive, principal_id, custom_label),
            }),
            Self::OnnxGenai { dir_archive } => Model::OnnxGenai(OnnxGenai {
                source: vault_archive(dir_archive, principal_id, custom_label),
            }),
            Self::MlxSpeculative {
                target_dir_archive,
                draft_dir_archive,
            } => Model::MlxSpeculative(vault_pair(
                target_dir_archive,
                draft_dir_archive,
                principal_id,
                custom_label,
            )),
            Self::TorchSpeculative {
                target_dir_archive,
                draft_dir_archive,
            } => Model::TorchSpeculative(vault_pair(
                target_dir_archive,
                draft_dir_archive,
                principal_id,
                custom_label,
            )),
        }
    }
}

impl VaultRuntime<VaultStoredFile> {
    /// The plan entry that fetches this archive for `principal_id`, under
    /// `custom_label` where its record has that name. The runtime vault arms
    /// carry no pin, so the digest stays on the record.
    pub fn into_runtime(
        self,
        principal_id: VaultPrincipalId,
        custom_label: Option<NonEmptyTrimmedString>,
    ) -> Runtime {
        match self {
            Self::LlamacppCliStockTools { flavor, archive } => {
                Runtime::LlamacppCliStockTools(LlamacppCliStockTools {
                    source: LlamacppCliStockToolsSource::VaultRemoteArchive {
                        url: archive.url(),
                        principal_id,
                        custom_label,
                    },
                    flavor,
                })
            }
            Self::Ovms { flavor, archive } => Runtime::Ovms(Ovms {
                source: OvmsSource::VaultRemoteArchive {
                    url: archive.url(),
                    principal_id,
                    custom_label,
                },
                flavor,
            }),
            Self::TrtEdgellm { flavor, archive } => Runtime::TrtEdgellm(TrtEdgellm {
                source: TrtEdgellmSource::VaultRemoteArchive {
                    url: archive.url(),
                    principal_id,
                    custom_label,
                },
                flavor,
            }),
        }
    }
}

impl VaultArtifact<VaultStoredFile> {
    pub fn into_entry(
        self,
        principal_id: VaultPrincipalId,
        custom_label: Option<NonEmptyTrimmedString>,
    ) -> VaultEntry {
        match self {
            Self::Model(model) => VaultEntry::Model(model.into_model(principal_id, custom_label)),
            Self::Runtime(runtime) => {
                VaultEntry::Runtime(runtime.into_runtime(principal_id, custom_label))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Context;
    use rstest::rstest;
    use serde::de::DeserializeOwned;
    use serde_json::{Value, json};

    use super::*;

    fn digest(c: char) -> String {
        c.to_string().repeat(64)
    }

    /// A file as an upload names it: `filename`, and its digest spelled with `c`.
    fn file(filename: &str, c: char) -> Value {
        json!({"filename": filename, "sha256": digest(c)})
    }

    fn principal() -> anyhow::Result<VaultPrincipalId> {
        Ok("org_01936d".parse()?)
    }

    fn name() -> anyhow::Result<NonEmptyTrimmedString> {
        Ok(NonEmptyTrimmedString::try_new("acme checkpoint")?)
    }

    /// Mints one resource per file, as mgmt does once it finds each blob.
    fn store(role: VaultFileRole, file: VaultUploadFile) -> anyhow::Result<VaultStoredFile> {
        Ok(VaultStoredFile {
            resource_id: VaultResourceId::try_new(format!("vault_0195c3e2-{role}"))?,
            filename: file.filename,
            sha256: file.sha256,
        })
    }

    /// The two arrays of a plan an entry is pasted into.
    #[derive(Deserialize)]
    struct PlanLists {
        #[serde(default)]
        models: Vec<Model>,
        #[serde(default)]
        runtimes: Vec<Runtime>,
    }

    fn parse<T: DeserializeOwned>(raw: &Value) -> anyhow::Result<T> {
        serde_json::from_value(raw.clone()).with_context(|| format!("{raw}"))
    }

    /// Every string anywhere in `value`.
    fn strings(value: &Value) -> Vec<&str> {
        match value {
            Value::String(s) => vec![s],
            Value::Array(items) => items.iter().flat_map(strings).collect(),
            Value::Object(fields) => fields.values().flat_map(strings).collect(),
            Value::Null | Value::Bool(_) | Value::Number(_) => vec![],
        }
    }

    /// Stores an uploaded artifact and turns it into its plan entry, checking
    /// on the way that a record keeps it losslessly, that the entry fetches
    /// every stored file, and that its TOML pastes into its family's plan list.
    fn entry_of(upload: VaultArtifact<VaultUploadFile>) -> anyhow::Result<Value> {
        let stored = upload.try_map(store)?;
        let kept: VaultArtifact<VaultStoredFile> = parse(&serde_json::to_value(&stored)?)?;
        assert_eq!(kept, stored, "a record keeps the stored artifact as is");

        let paths: Vec<String> = stored
            .files()
            .into_iter()
            .map(|(_, file)| file.url().to_string())
            .collect();
        let entry = stored.into_entry(principal()?, Some(name()?));
        let json = serde_json::to_value(&entry)?;
        let fetched = strings(&json);
        let missing: Vec<&String> = paths
            .iter()
            .filter(|path| !fetched.contains(&path.as_str()))
            .collect();
        assert!(missing.is_empty(), "{missing:?} missing from {json}");

        let list = match &entry {
            VaultEntry::Model(_) => "models",
            VaultEntry::Runtime(_) => "runtimes",
        };
        let pasted = format!("{list} = [ {} ]", entry.to_toml()?);
        let listed: PlanLists = toml::from_str(&pasted).context(pasted)?;
        let read_back: Vec<VaultEntry> = listed
            .models
            .into_iter()
            .map(VaultEntry::Model)
            .chain(listed.runtimes.into_iter().map(VaultEntry::Runtime))
            .collect();
        assert_eq!(read_back, std::slice::from_ref(&entry));
        Ok(json)
    }

    /// Every model becomes a vault-sourced plan entry of its own type.
    #[rstest]
    #[case::gguf_text(json!({"type": "gguf_text", "model": file("LFM2-1.2B-Q4_K_M.gguf", 'a')}))]
    #[case::gguf_vision(json!({"type": "gguf_vision", "model": file("LFM2-VL-3B-Q4_K_M.gguf", 'a'), "mmproj": file("mmproj-LFM2-VL-3B-F16.gguf", 'b')}))]
    #[case::gguf_vision_speculative_merged(json!({"type": "gguf_vision_speculative", "model": file("LFM2.5-VL-3B-DSpark-merged-F16.gguf", 'a'), "mmproj": file("mmproj-LFM2.5-VL-3B-F16.gguf", 'b')}))]
    #[case::gguf_vision_speculative_drafter(json!({"type": "gguf_vision_speculative", "model": file("LFM2.5-VL-3B-F16.gguf", 'a'), "mmproj": file("mmproj-LFM2.5-VL-3B-F16.gguf", 'b'), "draft": file("LFM2.5-VL-3B-DSpark-drafter-F16.gguf", 'c')}))]
    #[case::mlx(json!({"type": "mlx", "dir_archive": file("LFM2.5-350M-MLX-4bit.tar", 'a')}))]
    #[case::torch(json!({"type": "torch", "dir_archive": file("LFM2.5-VL-3B.tar.gz", 'a')}))]
    #[case::openvino(json!({"type": "openvino", "dir_archive": file("LFM2.5-350M-ov-int4-sym-cw.tar", 'a')}))]
    #[case::onnx_genai(json!({"type": "onnx_genai", "dir_archive": file("Phi-3-mini-4k-instruct-onnx-cpu-int4.zip", 'a')}))]
    #[case::mlx_speculative(json!({"type": "mlx_speculative", "target_dir_archive": file("LFM2.5-VL-3B-MLX-8bit.tar", 'a'), "draft_dir_archive": file("LFM2.5-VL-3B-DSpark-MLX.tar", 'b')}))]
    #[case::torch_speculative(json!({"type": "torch_speculative", "target_dir_archive": file("LFM2.5-VL-3B.tar.gz", 'a'), "draft_dir_archive": file("LFM2.5-VL-3B-DSpark-4L.tar.gz", 'b')}))]
    fn every_model_becomes_its_plan_entry(#[case] raw: Value) -> anyhow::Result<()> {
        let upload: VaultModel<VaultUploadFile> = parse(&raw)?;
        assert_eq!(serde_json::to_value(&upload)?, raw, "an upload round-trips");
        assert_eq!(raw["type"], upload.type_name());

        let entry = entry_of(VaultArtifact::Model(upload))?;
        let model: Model = parse(&entry["model"])?;
        assert_eq!(serde_json::to_value(&model)?["type"], raw["type"]);
        Ok(())
    }

    /// The same for a prebuilt runtime, which keeps its flavor.
    #[rstest]
    #[case::llamacpp(json!({"type": "llamacpp_cli_stock_tools", "flavor": "macos-arm64", "archive": file("llama-b1234-bin-macos-arm64.tar.gz", 'a')}))]
    #[case::ovms(json!({"type": "ovms", "flavor": "windows-x64", "archive": file("ovms-2025.3-windows-x64.zip", 'a')}))]
    #[case::trt_edgellm(json!({"type": "trt_edgellm", "flavor": "jetson-orin-cuda-13", "archive": file("trt-edgellm-server-0.11.0-jetson-orin-cuda-13-4e3f9d23f052.tar.gz", 'a')}))]
    fn every_runtime_becomes_its_plan_entry(#[case] raw: Value) -> anyhow::Result<()> {
        let upload: VaultRuntime<VaultUploadFile> = parse(&raw)?;
        assert_eq!(serde_json::to_value(&upload)?, raw, "an upload round-trips");
        assert_eq!(raw["type"], upload.type_name());

        let entry = entry_of(VaultArtifact::Runtime(upload))?;
        let runtime: Runtime = parse(&entry["runtime"])?;
        let runtime = serde_json::to_value(&runtime)?;
        assert_eq!(runtime["type"], raw["type"]);
        assert_eq!(runtime["flavor"], raw["flavor"]);
        Ok(())
    }

    /// The answer is keyed by family, so a reader knows which list it goes in,
    /// and pins each file to its digest.
    #[test]
    fn the_entry_shape_is_pinned() -> anyhow::Result<()> {
        let upload: UploadVaultResourcesRequest = parse(&json!({
            "name": "acme LFM2-VL-3B q4",
            "type": "gguf_vision",
            "model": file("LFM2-VL-3B-Q4_K_M.gguf", 'a'),
            "mmproj": file("mmproj-LFM2-VL-3B-F16.gguf", 'b'),
        }))?;
        let entry = upload
            .artifact
            .try_map(store)?
            .into_entry(principal()?, Some(upload.name));
        assert_eq!(
            serde_json::to_value(&entry)?,
            json!({"model": {
                "type": "gguf_vision",
                "source": "vault_url",
                "model": "/vault/resources/vault_0195c3e2-model/content",
                "model_sha256": digest('a'),
                "mmproj": "/vault/resources/vault_0195c3e2-mmproj/content",
                "mmproj_sha256": digest('b'),
                "principal_id": "org_01936d",
                "custom_label": "acme LFM2-VL-3B q4",
            }})
        );
        Ok(())
    }

    /// What a record keeps, pinned: records are immutable, so a renamed field
    /// would strand every one already written.
    #[test]
    fn the_stored_artifact_shape_is_pinned() -> anyhow::Result<()> {
        let upload: VaultArtifact<VaultUploadFile> = parse(&json!({
            "type": "ovms",
            "flavor": "windows-x64",
            "archive": file("ovms-2025.3-windows-x64.zip", 'a'),
        }))?;
        assert_eq!(
            serde_json::to_value(upload.try_map(store)?)?,
            json!({
                "type": "ovms",
                "flavor": "windows-x64",
                "archive": {
                    "resource_id": "vault_0195c3e2-archive",
                    "filename": "ovms-2025.3-windows-x64.zip",
                    "sha256": digest('a'),
                },
            })
        );
        Ok(())
    }

    /// A body is the variant itself, its `type` beside the name and an
    /// optional owner, and the tag alone says which family it is.
    #[rstest]
    #[case::model(json!({"name": "LFM2 1.2B q4", "type": "gguf_text", "model": file("LFM2-1.2B-Q4_K_M.gguf", 'a')}), "gguf_text", false)]
    #[case::runtime(json!({"name": "OVMS 2025.3 Windows", "type": "ovms", "flavor": "windows-x64", "archive": file("ovms-2025.3-windows-x64.zip", 'a')}), "ovms", true)]
    #[case::owned(json!({"name": "LFM2.5 350M MLX", "owner": "org_01936d", "type": "mlx", "dir_archive": file("LFM2.5-350M-MLX-4bit.tar", 'a')}), "mlx", false)]
    fn an_upload_body_round_trips(
        #[case] raw: Value,
        #[case] type_name: &str,
        #[case] runtime: bool,
    ) -> anyhow::Result<()> {
        let upload: UploadVaultResourcesRequest = parse(&raw)?;
        assert_eq!(serde_json::to_value(&upload)?, raw);
        assert_eq!(upload.artifact.type_name(), type_name);
        assert_eq!(
            matches!(upload.artifact, VaultArtifact::Runtime(_)),
            runtime
        );
        Ok(())
    }

    #[rstest]
    #[case::no_type(json!({"name": "q4"}))]
    #[case::under_a_family_key(json!({"name": "q4", "model": {"type": "gguf_text", "model": file("LFM2-1.2B-Q4_K_M.gguf", 'a')}}))]
    #[case::unknown_type(json!({"name": "q4", "type": "docker_vllm", "model": file("LFM2-1.2B-Q4_K_M.gguf", 'a')}))]
    #[case::type_not_a_string(json!({"name": "q4", "type": 1, "model": file("LFM2-1.2B-Q4_K_M.gguf", 'a')}))]
    #[case::no_name(json!({"type": "gguf_text", "model": file("LFM2-1.2B-Q4_K_M.gguf", 'a')}))]
    #[case::blank_name(json!({"name": " ", "type": "gguf_text", "model": file("LFM2-1.2B-Q4_K_M.gguf", 'a')}))]
    #[case::owner_not_a_principal(json!({"name": "q4", "owner": "acme", "type": "gguf_text", "model": file("LFM2-1.2B-Q4_K_M.gguf", 'a')}))]
    #[case::stray_field(json!({"name": "q4", "object_key": "org_01936d/q4.gguf", "type": "gguf_text", "model": file("LFM2-1.2B-Q4_K_M.gguf", 'a')}))]
    #[case::another_variant_s_file(json!({"name": "q4", "type": "gguf_text", "model": file("LFM2-1.2B-Q4_K_M.gguf", 'a'), "archive": file("llama-b1234-bin-macos-arm64.tar.gz", 'b')}))]
    fn an_upload_body_is_one_variant_beside_its_name(#[case] raw: Value) {
        assert!(
            serde_json::from_value::<UploadVaultResourcesRequest>(raw.clone()).is_err(),
            "{raw}"
        );
    }

    /// Only an uploadable model parses, with exactly its own files, each named
    /// and pinned.
    #[rstest]
    #[case::apple_foundation_text(json!({"type": "apple_foundation_text"}))]
    #[case::unknown(json!({"type": "bogus", "model": file("LFM2-1.2B-Q4_K_M.gguf", 'a')}))]
    #[case::no_type(json!({"model": file("LFM2-1.2B-Q4_K_M.gguf", 'a')}))]
    #[case::missing_projector(json!({"type": "gguf_vision", "model": file("LFM2-VL-3B-Q4_K_M.gguf", 'a')}))]
    #[case::drafter_on_plain_vision(json!({"type": "gguf_vision", "model": file("LFM2-VL-3B-Q4_K_M.gguf", 'a'), "mmproj": file("mmproj-LFM2-VL-3B-F16.gguf", 'b'), "draft": file("LFM2.5-VL-3B-DSpark-drafter-F16.gguf", 'c')}))]
    #[case::pair_without_drafter(json!({"type": "mlx_speculative", "target_dir_archive": file("LFM2.5-VL-3B-MLX-8bit.tar", 'a')}))]
    #[case::with_a_runtime_field(json!({"type": "gguf_text", "flavor": "macos-arm64", "model": file("LFM2-1.2B-Q4_K_M.gguf", 'a')}))]
    #[case::with_a_source(json!({"type": "mlx", "source": "vault_archive", "dir_archive": file("LFM2.5-350M-MLX-4bit.tar", 'a')}))]
    #[case::directory_as_a_model_file(json!({"type": "mlx", "model": file("model.safetensors", 'a')}))]
    #[case::gguf_as_a_dir_archive(json!({"type": "gguf_text", "dir_archive": file("LFM2-1.2B-Q4_K_M.gguf", 'a')}))]
    #[case::pair_under_the_gguf_drafter_s_name(json!({"type": "torch_speculative", "target_dir_archive": file("LFM2.5-VL-3B.tar.gz", 'a'), "draft": file("LFM2.5-VL-3B-DSpark-4L.tar.gz", 'b')}))]
    #[case::file_as_a_bare_digest(json!({"type": "gguf_text", "model": digest('a')}))]
    #[case::file_without_a_name(json!({"type": "gguf_text", "model": {"sha256": digest('a')}}))]
    #[case::file_without_a_digest(json!({"type": "gguf_text", "model": {"filename": "LFM2-1.2B-Q4_K_M.gguf"}}))]
    #[case::file_naming_its_own_url(json!({"type": "gguf_text", "model": {"filename": "LFM2-1.2B-Q4_K_M.gguf", "sha256": digest('a'), "url": "/vault/resources/vault_1/content"}}))]
    #[case::uppercase_digest(json!({"type": "gguf_text", "model": {"filename": "LFM2-1.2B-Q4_K_M.gguf", "sha256": "A".repeat(64)}}))]
    fn a_model_upload_is_an_uploadable_variant_with_exactly_its_files(#[case] raw: Value) {
        assert!(
            serde_json::from_value::<VaultModel<VaultUploadFile>>(raw.clone()).is_err(),
            "{raw}"
        );
    }

    /// Only a prebuilt archive parses, with its flavor and nothing else.
    #[rstest]
    #[case::docker_vllm(json!({"type": "docker_vllm", "image_name": "vllm", "image_tag": "latest", "flavor": "nvidia_gpu"}))]
    #[case::uv_wheelhouse(json!({"type": "uv_vllm", "server_version": "0.11.0", "build": "cu128", "python_version": "3.12", "archive": file("vllm-0.11.0-cu128-wheels.tar", 'a')}))]
    #[case::without_flavor(json!({"type": "llamacpp_cli_stock_tools", "archive": file("llama-b1234-bin-macos-arm64.tar.gz", 'a')}))]
    #[case::archive_under_a_model_name(json!({"type": "ovms", "flavor": "windows-x64", "model": file("ovms-2025.3-windows-x64.zip", 'a')}))]
    #[case::with_a_source(json!({"type": "ovms", "flavor": "windows-x64", "source": "vault_remote_archive", "archive": file("ovms-2025.3-windows-x64.zip", 'a')}))]
    fn a_runtime_upload_is_a_prebuilt_archive_with_its_flavor(#[case] raw: Value) {
        assert!(
            serde_json::from_value::<VaultRuntime<VaultUploadFile>>(raw.clone()).is_err(),
            "{raw}"
        );
    }

    #[rstest]
    #[case::merged_drafter(
        json!({"type": "gguf_vision_speculative", "model": file("LFM2.5-VL-3B-DSpark-merged-F16.gguf", 'a'), "mmproj": file("mmproj-LFM2.5-VL-3B-F16.gguf", 'b')}),
        &[(VaultFileRole::Model, VaultFileFormat::Gguf), (VaultFileRole::Mmproj, VaultFileFormat::Gguf)],
    )]
    #[case::separate_drafter(
        json!({"type": "gguf_vision_speculative", "model": file("LFM2.5-VL-3B-F16.gguf", 'a'), "mmproj": file("mmproj-LFM2.5-VL-3B-F16.gguf", 'b'), "draft": file("LFM2.5-VL-3B-DSpark-drafter-F16.gguf", 'c')}),
        &[(VaultFileRole::Model, VaultFileFormat::Gguf), (VaultFileRole::Mmproj, VaultFileFormat::Gguf), (VaultFileRole::Draft, VaultFileFormat::Gguf)],
    )]
    #[case::directory_pair(
        json!({"type": "torch_speculative", "target_dir_archive": file("LFM2.5-VL-3B.tar.gz", 'a'), "draft_dir_archive": file("LFM2.5-VL-3B-DSpark-4L.tar.gz", 'b')}),
        &[(VaultFileRole::TargetDirArchive, VaultFileFormat::Archive), (VaultFileRole::DraftDirArchive, VaultFileFormat::Archive)],
    )]
    #[case::runtime(
        json!({"type": "ovms", "flavor": "windows-x64", "archive": file("ovms-2025.3-windows-x64.zip", 'a')}),
        &[(VaultFileRole::Archive, VaultFileFormat::Archive)],
    )]
    fn each_file_carries_its_role_and_format(
        #[case] raw: Value,
        #[case] expected: &[(VaultFileRole, VaultFileFormat)],
    ) -> anyhow::Result<()> {
        let upload: VaultArtifact<VaultUploadFile> = parse(&raw)?;
        let files: Vec<_> = upload
            .files()
            .into_iter()
            .map(|(role, _)| (role, role.format()))
            .collect();
        assert_eq!(files, expected);
        Ok(())
    }
}
