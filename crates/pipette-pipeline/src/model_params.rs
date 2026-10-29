//! Parameter counts per model over the model-params rows mgmt keeps, keyed by
//! [`normalize`]d weight names so every spelling of a model hits one entry. The
//! scorer reads the names off a descriptor, never `model_name`; with no rows it
//! trusts the client's counts.

use std::collections::HashMap;
use std::sync::Arc;

use futures::{StreamExt, TryStreamExt};
use serde::Deserialize;

use pipette_core::model_params::ModelParams;
use pipette_plan_types::ModelKey;
use pipette_store::{ModelParamsStore, drain_model_params, warn_unreadable_model_params};

/// Resolved entry from the table. `active` is always set, equal to `total`
/// for a dense model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModelEntry {
    pub total: i32,
    pub active: i32,
}

/// A bare integer means `total = active = N`; `active` defaults to `total`.
#[derive(Deserialize)]
#[serde(untagged)]
enum RawEntry {
    Dense(i32),
    Sparse {
        total: i32,
        #[serde(default)]
        active: Option<i32>,
    },
}

#[derive(Clone, Debug, Default)]
pub struct ModelParamTable {
    map: Arc<HashMap<String, ModelEntry>>,
}

/// What [`ModelParamTable::resolve_typed_descriptor`] made of a descriptor. The
/// failures differ in fix: add a table entry, or teach the server the shape.
#[derive(Debug, PartialEq, Eq)]
pub enum TypedMatch {
    Resolved {
        key: String,
        entry: ModelEntry,
    },
    Uncatalogued,
    /// A partner format or a newer client; nothing is guessed from it.
    Unparsed,
}

impl ModelParamTable {
    /// Empty table: what a store with no rows loads as.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Parse a TOML body into a table, for `models import`. A key colliding with
    /// another after [`normalize`] is an error, so no entry silently shadows one.
    pub fn from_toml(content: &str) -> anyhow::Result<Self> {
        let raw: HashMap<String, RawEntry> = toml::from_str(content)
            .map_err(|e| anyhow::anyhow!("failed to parse the model params file: {e}"))?;
        let mut map: HashMap<String, ModelEntry> = HashMap::with_capacity(raw.len());
        raw.into_iter().try_for_each(|(k, v)| {
            let entry = match v {
                RawEntry::Dense(t) => {
                    if t <= 0 {
                        anyhow::bail!(
                            "model params file: entry {k:?} has non-positive value {t}; mill_params must be > 0"
                        );
                    }
                    ModelEntry {
                        total: t,
                        active: t,
                    }
                }
                RawEntry::Sparse { total, active } => {
                    if total <= 0 {
                        anyhow::bail!(
                            "model params file: entry {k:?} has non-positive total {total}; must be > 0"
                        );
                    }
                    let active = active.unwrap_or(total);
                    if active <= 0 {
                        anyhow::bail!(
                            "model params file: entry {k:?} has non-positive active {active}; must be > 0"
                        );
                    }
                    if active > total {
                        anyhow::bail!(
                            "model params file: entry {k:?} has active ({active}) > total ({total})"
                        );
                    }
                    ModelEntry { total, active }
                }
            };
            let normalized = normalize(&k);
            if map.insert(normalized.clone(), entry).is_some() {
                anyhow::bail!(
                    "model params file: duplicate entry for normalized key {normalized:?} (case-insensitive collision)"
                );
            }
            Ok(())
        })?;
        Ok(Self { map: Arc::new(map) })
    }

    /// A table of model-params rows. Their keys are already normalized, so
    /// they are used as they are.
    pub fn from_records(records: &[ModelParams]) -> Self {
        let map = records
            .iter()
            .map(|r| {
                let entry = ModelEntry {
                    total: r.total_millions.into_inner(),
                    active: r.active_millions.into_inner(),
                };
                (r.key.as_str().to_string(), entry)
            })
            .collect();
        Self { map: Arc::new(map) }
    }

    /// Read the table from the store's rows. An unreadable row is skipped with a
    /// warning: its model reads as uncatalogued.
    pub async fn load(store: &dyn ModelParamsStore) -> anyhow::Result<Self> {
        let listing = drain_model_params(store).await?;
        warn_unreadable_model_params(&listing.unreadable);
        let table = Self::from_records(&listing.items);
        tracing::debug!(entries = table.len(), "loaded model parameter table");
        Ok(table)
    }

    pub fn lookup(&self, model_name: &str) -> Option<ModelEntry> {
        self.map.get(&normalize(model_name)).copied()
    }

    /// Resolve a `model_descriptor` through the typed model it parses into. The
    /// only way a row gets table counts: `model_name` is client-set, and a text
    /// search would match revisions, paths and a VL model's projector.
    pub fn resolve_typed_descriptor(&self, descriptor: &str) -> TypedMatch {
        let Ok(model) = serde_json::from_str::<pipette_plan_types::Model>(descriptor) else {
            return TypedMatch::Unparsed;
        };
        lookup_names(&model)
            .into_iter()
            .find_map(|name| self.lookup(name).map(|entry| (normalize(name), entry)))
            .map_or(TypedMatch::Uncatalogued, |(key, entry)| {
                TypedMatch::Resolved { key, entry }
            })
    }

    /// Resolve a warehouse row's `model_descriptor`, then its
    /// `model_vault_descriptor`: an aliased vault run records both, and either
    /// model key applies.
    pub fn resolve_recorded(
        &self,
        model_descriptor: Option<&str>,
        model_vault_descriptor: Option<&str>,
    ) -> Option<(String, ModelEntry)> {
        [model_descriptor, model_vault_descriptor]
            .into_iter()
            .flatten()
            .find_map(
                |descriptor| match self.resolve_typed_descriptor(descriptor) {
                    TypedMatch::Resolved { key, entry } => Some((key, entry)),
                    TypedMatch::Uncatalogued | TypedMatch::Unparsed => None,
                },
            )
    }

    /// Every entry under its normalized key, in key order.
    pub fn entries(&self) -> Vec<(&str, ModelEntry)> {
        let mut entries: Vec<_> = self.map.iter().map(|(k, e)| (k.as_str(), *e)).collect();
        entries.sort_unstable_by_key(|(key, _)| *key);
        entries
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

/// What [`import`] made of a file's entries, each list in key order.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ImportReport {
    /// Keys that had no row and were given one.
    pub added: Vec<ModelKey>,
    /// Keys whose row holds other counts. The row is kept.
    pub differing: Vec<ModelKey>,
    /// Entries whose row already holds the file's counts.
    pub unchanged: usize,
}

/// Add a row for each of `file`'s entries the store lacks. An existing row is
/// kept, so an import never undoes an edit; every entry is checked before the
/// first write.
pub async fn import(
    store: &dyn ModelParamsStore,
    file: &ModelParamTable,
    dry_run: bool,
) -> anyhow::Result<ImportReport> {
    let rows = ModelParamTable::load(store).await?;
    let now = chrono::Utc::now();
    let keyed = file
        .entries()
        .into_iter()
        .map(|(name, entry)| {
            let key = ModelKey::try_new(name).map_err(|e| {
                anyhow::Error::new(e).context(format!("entry {name:?} cannot be a row key"))
            })?;
            Ok((key, entry))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let (held, missing): (Vec<_>, Vec<_>) = keyed
        .into_iter()
        .partition(|(key, _)| rows.map.contains_key(key.as_str()));
    let (unchanged, differing): (Vec<_>, Vec<_>) = held
        .into_iter()
        .partition(|(key, entry)| rows.map.get(key.as_str()) == Some(entry));
    let added = missing
        .into_iter()
        .map(|(key, entry)| ModelParams::imported(key, entry.total, entry.active, now))
        .collect::<Result<Vec<_>, _>>()?;
    if !dry_run {
        futures::stream::iter(&added)
            .map(anyhow::Ok)
            .try_for_each(|row| store.put_model_params(row))
            .await?;
    }
    Ok(ImportReport {
        added: added.into_iter().map(|row| row.key).collect(),
        differing: differing.into_iter().map(|(key, _)| key).collect(),
        unchanged: unchanged.len(),
    })
}

/// The key a descriptor would be catalogued under, known or not: what a
/// coverage report keys a miss on, so an operator reads the string to add.
pub fn descriptor_key(descriptor: &str) -> Option<String> {
    let model: pipette_plan_types::Model = serde_json::from_str(descriptor).ok()?;
    model_key(&model)
}

/// [`descriptor_key`] of a model already typed.
pub fn model_key(model: &pipette_plan_types::Model) -> Option<String> {
    lookup_names(model).first().map(|name| normalize(name))
}

/// The names a typed model could be catalogued under, most specific first. A
/// VL model yields only its main file: the projector must never resolve.
/// Not on [`pipette_plan_types::Model`]: that vocabulary is shared with clients.
fn lookup_names(model: &pipette_plan_types::Model) -> Vec<&str> {
    use pipette_plan_types::{
        GgufTextSource, GgufVisionSource, GgufVisionSpeculativeSource, Mlx, MlxSpeculative, Model,
        ModelSource, OnnxGenai, Openvino, Torch, TorchSpeculative,
    };

    match model {
        Model::GgufText(m) => match &m.source {
            GgufTextSource::HuggingFace { repo, path, .. } => {
                vec![repo.repo_name.as_ref(), file_name(path.as_ref())]
            }
            GgufTextSource::RelativeFile { path } => vec![file_name(path.as_ref())],
            GgufTextSource::AbsoluteFile { path } => vec![file_name(path.as_ref())],
            GgufTextSource::Url { url, .. } => vec![file_name(url.as_ref())],
            // A vault path always ends in `/content`, so its file name names
            // nothing; the resource id is the artifact's one stable name.
            GgufTextSource::VaultUrl { url, .. } => vec![url.resource_id().as_ref()],
        },
        Model::GgufVision(m) => match &m.source {
            GgufVisionSource::HuggingFace(f) => {
                vec![f.repo.repo_name.as_ref(), file_name(f.model.as_ref())]
            }
            GgufVisionSource::RelativeFiles(f) => vec![file_name(f.model.as_ref())],
            GgufVisionSource::AbsoluteFiles(f) => vec![file_name(f.model.as_ref())],
            GgufVisionSource::Url(f) => vec![file_name(f.model.as_ref())],
            GgufVisionSource::VaultUrl(f) => vec![f.model.resource_id().as_ref()],
        },
        // The drafter is not a model key, like the projector: the target's count
        // is what matters.
        Model::GgufVisionSpeculative(m) => match &m.source {
            GgufVisionSpeculativeSource::HuggingFace { files: f, .. } => {
                vec![f.repo.repo_name.as_ref(), file_name(f.model.as_ref())]
            }
            GgufVisionSpeculativeSource::RelativeFiles { files: f, .. } => {
                vec![file_name(f.model.as_ref())]
            }
            GgufVisionSpeculativeSource::AbsoluteFiles { files: f, .. } => {
                vec![file_name(f.model.as_ref())]
            }
            GgufVisionSpeculativeSource::Url { files: f, .. } => vec![file_name(f.model.as_ref())],
            GgufVisionSpeculativeSource::VaultUrl { files: f, .. } => {
                vec![f.model.resource_id().as_ref()]
            }
        },
        // A `prefix` names a variant within a repo, not a model, and an MLX or torch
        // drafter is not a model key either.
        Model::Mlx(Mlx { source })
        | Model::Torch(Torch { source })
        | Model::Openvino(Openvino { source })
        | Model::OnnxGenai(OnnxGenai { source })
        | Model::MlxSpeculative(MlxSpeculative { target: source, .. })
        | Model::TorchSpeculative(TorchSpeculative { target: source, .. }) => match source {
            ModelSource::HuggingFace { repo, .. } => vec![repo.repo_name.as_ref()],
            ModelSource::VaultArchive { url, .. } => vec![url.resource_id().as_ref()],
            ModelSource::RelativeDir { dir } => vec![file_name(dir.as_ref())],
            ModelSource::AbsoluteDir { dir } => vec![file_name(dir.as_ref())],
        },
        // Ships with the OS, so no repo or file; the wire tag is its model key.
        Model::AppleFoundationText => vec!["apple_foundation_text"],
    }
}

/// The final segment of a `/`-separated path: the artifact's own name.
fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Strip the parts of `model_name` that don't identify the architecture and
/// lowercase it: `org/`, `:file`, `.gguf`, then distribution, quantizer and
/// quant tags until none apply.
///
/// Stored keys are this function's output, so folding a name differently
/// needs a migration of the `model_params` keys.
pub fn normalize(model_name: &str) -> String {
    let after_slash = model_name.rsplit_once('/').map_or(model_name, |(_, n)| n);
    let before_colon = after_slash.split_once(':').map_or(after_slash, |(n, _)| n);
    let mut s = before_colon.to_lowercase();

    if let Some(rest) = s.strip_suffix(".gguf") {
        s.truncate(rest.len());
    }
    while let Some(stripped_len) = trailing_tag_prefix_len(&s) {
        s.truncate(stripped_len);
    }
    s
}

/// The length of `s` without its trailing distribution or quant tag, or
/// `None` when none applies.
fn trailing_tag_prefix_len(s: &str) -> Option<usize> {
    const DISTRIBUTION_TAGS: &[&str] = &["-gguf", "-mlx", "-onnx", "-compiled"];
    /// Who quantized it: unsloth's `-UD-` and mradermacher's `i1` sit mid-name,
    /// where the quant-tag rule never reaches them.
    const QUANTIZER_TAGS: &[&str] = &["-ud", "-i1", ".i1"];

    for tag in DISTRIBUTION_TAGS.iter().chain(QUANTIZER_TAGS) {
        if let Some(rest) = s.strip_suffix(tag) {
            return Some(rest.len());
        }
    }
    let (prefix, last) = s.rsplit_once('-')?;
    is_quant_tag(last).then_some(prefix.len())
}

/// The lowercase quant tags after the final `-`: `q4_k_m`, `iq3_m`, `bf16`,
/// `mxfp4`, `4bit` and kin. The prefixed forms need a digit after the prefix,
/// and `Nbit` all digits, so `quant`, `iqr` or `habit` survive.
fn is_quant_tag(tag: &str) -> bool {
    if matches!(tag, "f16" | "f32" | "fp16" | "bf16" | "int4" | "int8") {
        return true;
    }
    if let Some(prefix) = tag.strip_suffix("bit")
        && !prefix.is_empty()
        && prefix.bytes().all(|b| b.is_ascii_digit())
    {
        return true;
    }
    let Some(rest) = tag
        .strip_prefix("iq")
        .or_else(|| tag.strip_prefix("mxfp"))
        .or_else(|| tag.strip_prefix("nvfp"))
        .or_else(|| tag.strip_prefix('q'))
    else {
        return false;
    };
    let mut bytes = rest.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    first.is_ascii_digit()
        && bytes.all(|b| b.is_ascii_digit() || b == b'_' || b.is_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use anyhow::Context;
    use rstest::rstest;

    use crate::model_params::*;

    const TABLE: &str = r#"
"LFM2-700M" = 742
"LFM2.5-1.2B-Instruct" = 1170
"LFM2-8B-A1B" = { total = 8340, active = 1500 }
"LFM2.5-8B-A1B" = { total = 8340, active = 1500 }
"gemma-4-E4B-it" = { total = 7996, active = 4000 }
"#;

    fn test_config(dir: &std::path::Path) -> pipette_core::config::Config {
        pipette_core::config::Config {
            storage: pipette_core::config::StorageConfig::local_fs(dir.to_path_buf()),
            auth_storage: pipette_core::config::StorageConfig::local_fs(dir.to_path_buf()),
            ..pipette_core::config::Config::default()
        }
    }

    fn make_table() -> anyhow::Result<ModelParamTable> {
        ModelParamTable::from_toml(TABLE).context("test table must parse")
    }

    #[test]
    fn dense_shorthand_sets_total_equal_to_active() -> anyhow::Result<()> {
        let table = make_table()?;
        let e = table
            .lookup("LFM2-700M")
            .context("the table must know this model")?;
        assert_eq!(e.total, 742);
        assert_eq!(e.active, 742);
        Ok(())
    }
    /// A typed descriptor resolves off the fields that name the weights, not
    /// whatever else the blob holds.
    #[rstest]
    #[case::gguf_text_repo(
        r#"{"type":"gguf_text","source":"huggingface","org":"LiquidAI","repo_name":"LFM2-700M-GGUF","path":"LFM2-700M-Q4_K_M.gguf"}"#,
        Some(742)
    )]
    #[case::gguf_text_local_file(
        r#"{"type":"gguf_text","source":"relative_file","path":"models/LFM2-700M-Q4_K_M.gguf"}"#,
        Some(742)
    )]
    #[case::mlx_directory(
        r#"{"type":"mlx","source":"huggingface","org":"LiquidAI","repo_name":"LFM2-700M-MLX-4bit"}"#,
        Some(742)
    )]
    #[case::unlisted(
        r#"{"type":"mlx","source":"huggingface","org":"LiquidAI","repo_name":"nobody-catalogued-this"}"#,
        None
    )]
    /// The token must not cost a gated repo's row its counts by failing the parse.
    #[case::gated_repo_carries_a_token(
        r#"{"type":"gguf_text","source":"huggingface","org":"LiquidAI","repo_name":"LFM2-700M-GGUF","path":"LFM2-700M-Q4_K_M.gguf","requires_auth":true,"auth_token":"hf_secret"}"#,
        Some(742)
    )]
    fn typed_descriptor_resolves_by_field(
        #[case] descriptor: &str,
        #[case] expected_total: Option<i32>,
    ) -> anyhow::Result<()> {
        let table = make_table()?;
        let total = match table.resolve_typed_descriptor(descriptor) {
            TypedMatch::Resolved { entry, .. } => Some(entry.total),
            TypedMatch::Uncatalogued | TypedMatch::Unparsed => None,
        };
        assert_eq!(total, expected_total);
        Ok(())
    }

    /// A VL model names only its main file, never the projector.
    #[rstest]
    #[case::gguf_text_hf(
        r#"{"type":"gguf_text","source":"huggingface","org":"LiquidAI","repo_name":"LFM2-700M-GGUF","path":"LFM2-700M-Q4_K_M.gguf"}"#,
        vec!["LFM2-700M-GGUF", "LFM2-700M-Q4_K_M.gguf"]
    )]
    #[case::gguf_text_relative(
        r#"{"type":"gguf_text","source":"relative_file","path":"models/LFM2-700M-Q4_K_M.gguf"}"#,
        vec!["LFM2-700M-Q4_K_M.gguf"]
    )]
    #[case::gguf_vision_skips_mmproj(
        r#"{"type":"gguf_vision","source":"huggingface","org":"LiquidAI","repo_name":"LFM2.5-Vision-3B","model":"LFM2.5-Vision-3B-Q4_K_M.gguf","mmproj":"mmproj-f16.gguf"}"#,
        vec!["LFM2.5-Vision-3B", "LFM2.5-Vision-3B-Q4_K_M.gguf"]
    )]
    #[case::mlx(
        r#"{"type":"mlx","source":"huggingface","org":"LiquidAI","repo_name":"LFM2.5-350M-MLX-4bit"}"#,
        vec!["LFM2.5-350M-MLX-4bit"]
    )]
    #[case::openvino_ignores_prefix(
        r#"{"type":"openvino","source":"huggingface","org":"LiquidAI","repo_name":"LFM2.5-350M-ov","prefix":"int4-sym-cw"}"#,
        vec!["LFM2.5-350M-ov"]
    )]
    #[case::torch_speculative_names_the_target(
        r#"{"type":"torch_speculative","target":{"source":"huggingface","org":"LiquidAI","repo_name":"LFM2.5-VL-3B"},"draft":{"source":"huggingface","org":"LiquidAI","repo_name":"LFM2.5-VL-3B-DSpark-4L"}}"#,
        vec!["LFM2.5-VL-3B"]
    )]
    #[case::apple_foundation_uses_its_wire_tag(
        r#"{"type":"apple_foundation_text"}"#,
        vec!["apple_foundation_text"]
    )]
    fn lookup_names_name_the_weights(
        #[case] json: &str,
        #[case] expected: Vec<&str>,
    ) -> anyhow::Result<()> {
        let model: pipette_plan_types::Model = serde_json::from_str(json)?;
        assert_eq!(lookup_names(&model), expected);
        Ok(())
    }

    /// The typed path reads the `model` field and never sees `mmproj`.
    #[test]
    fn a_vl_projector_is_not_the_model() -> anyhow::Result<()> {
        let table = ModelParamTable::from_toml(
            "\"LFM2.5-Vision-3B\" = 3000\n\"mmproj-LFM2.5-Vision-3B-f16\" = 400",
        )?;
        let descriptor = r#"{"type":"gguf_vision","source":"huggingface","org":"LiquidAI","repo_name":"LFM2.5-Vision-3B","model":"LFM2.5-Vision-3B-Q4_K_M.gguf","mmproj":"mmproj-LFM2.5-Vision-3B-f16.gguf"}"#;

        let TypedMatch::Resolved { key, entry } = table.resolve_typed_descriptor(descriptor) else {
            anyhow::bail!("the typed path must resolve a catalogued VL model");
        };
        assert_eq!(key, "lfm2.5-vision-3b");
        assert_eq!(entry.total, 3000);

        Ok(())
    }

    /// The projector here is catalogued, and its count would be a wrong answer.
    #[test]
    fn a_parsed_but_uncatalogued_model_resolves_to_nothing() -> anyhow::Result<()> {
        let table = ModelParamTable::from_toml("\"mmproj-LFM2.5-Vision-3B-f16\" = 400")?;
        let descriptor = r#"{"type":"gguf_vision","source":"huggingface","org":"LiquidAI","repo_name":"LFM2.5-Vision-3B","model":"LFM2.5-Vision-3B-Q4_K_M.gguf","mmproj":"mmproj-LFM2.5-Vision-3B-f16.gguf"}"#;

        assert_eq!(
            table.resolve_typed_descriptor(descriptor),
            TypedMatch::Uncatalogued
        );
        Ok(())
    }

    /// The catalogued name sits in plain sight inside it; nothing is guessed.
    #[test]
    fn an_unparseable_descriptor_resolves_to_nothing() -> anyhow::Result<()> {
        let table = make_table()?;
        let descriptor = r#"{"partner_format":true,"weights":"LFM2-700M-GGUF"}"#;

        assert_eq!(
            table.resolve_typed_descriptor(descriptor),
            TypedMatch::Unparsed
        );
        assert_eq!(descriptor_key(descriptor), None);
        Ok(())
    }

    #[test]
    fn the_reporting_key_does_not_need_a_table_entry() {
        let descriptor = r#"{"type":"gguf_text","source":"huggingface","org":"IFM","repo_name":"K2-Horizon-3.7B-GGUF","path":"K2-Horizon-3.7B-Q4_K_M.gguf"}"#;
        assert_eq!(
            descriptor_key(descriptor).as_deref(),
            Some("k2-horizon-3.7b")
        );
    }

    #[test]
    fn sparse_form_keeps_distinct_values() -> anyhow::Result<()> {
        let table = make_table()?;
        let e = table
            .lookup("LFM2-8B-A1B")
            .context("the table must know this model")?;
        assert_eq!(e.total, 8340);
        assert_eq!(e.active, 1500);
        Ok(())
    }

    #[test]
    fn sparse_active_defaults_to_total_when_omitted() -> anyhow::Result<()> {
        let table = ModelParamTable::from_toml(r#""x" = { total = 500 }"#)?;
        let e = table
            .lookup("x")
            .context("the table must know this model")?;
        assert_eq!(e.total, 500);
        assert_eq!(e.active, 500);
        Ok(())
    }

    #[test]
    fn lookup_strips_org_prefix() -> anyhow::Result<()> {
        let table = make_table()?;
        assert_eq!(
            table
                .lookup("LiquidAI/LFM2-700M")
                .context("the table must know this model")?
                .total,
            742
        );
        Ok(())
    }

    #[test]
    fn lookup_strips_gguf_and_colon() -> anyhow::Result<()> {
        let table = make_table()?;
        let e = table
            .lookup("LiquidAI/LFM2-8B-A1B-GGUF:Q4_0.gguf")
            .context("the table must know this model")?;
        assert_eq!(e.total, 8340);
        assert_eq!(e.active, 1500);
        Ok(())
    }

    #[test]
    fn lookup_strips_lowercase_gguf() -> anyhow::Result<()> {
        let table = make_table()?;
        // Some repos use lowercase `gguf` in the suffix
        let e = table
            .lookup("liquidai/lfm2-8b-a1b-gguf")
            .context("the table must know this model")?;
        assert_eq!(e.total, 8340);
        Ok(())
    }

    #[test]
    fn lookup_resolves_lfm25_8b_a1b_gated_gguf_name() -> anyhow::Result<()> {
        let table = make_table()?;
        let e = table
            .lookup("LiquidAI/LFM2.5-8B-A1B-GGUF")
            .context("the table must know this model")?;
        assert_eq!(e.total, 8340);
        assert_eq!(e.active, 1500);
        Ok(())
    }

    #[test]
    fn lookup_strips_mlx_suffix() -> anyhow::Result<()> {
        let table = make_table()?;
        for input in [
            "LiquidAI/LFM2.5-1.2B-Instruct-MLX",
            "LiquidAI/LFM2.5-1.2B-Instruct-mlx",
            "LFM2.5-1.2B-Instruct-MLX",
            // Stacked: `-MLX-bf16` collapses via quant-then-distribution
            // strip in the loop.
            "LiquidAI/LFM2.5-1.2B-Instruct-MLX-bf16",
        ] {
            assert_eq!(
                table.lookup(input).map(|e| e.total),
                Some(1170),
                "lookup({input:?}) should resolve to LFM2.5-1.2B-Instruct",
            );
        }
        Ok(())
    }

    #[test]
    fn lookup_strips_onnx_suffix() -> anyhow::Result<()> {
        let table = make_table()?;
        for input in [
            "LiquidAI/LFM2-700M-ONNX",
            "LiquidAI/LFM2-700M-onnx",
            "LFM2-700M-ONNX",
        ] {
            assert_eq!(
                table.lookup(input).map(|e| e.total),
                Some(742),
                "lookup({input:?}) should resolve to LFM2-700M",
            );
        }
        Ok(())
    }

    #[test]
    fn lookup_strips_compiled_suffix() -> anyhow::Result<()> {
        let table = make_table()?;
        let e = table
            .lookup("LFM2.5-1.2B-Instruct-compiled")
            .context("the table must know this model")?;
        assert_eq!(e.total, 1170);
        Ok(())
    }

    #[test]
    fn lookup_strips_gguf_filename_extension() -> anyhow::Result<()> {
        let table = make_table()?;
        // The `.gguf` extension and the `-Q4_K_M` tag both have to come off.
        for input in [
            "LFM2-700M-Q4_K_M.gguf",
            "LFM2-700M-Q4_0.gguf",
            "LFM2-700M-IQ3_M.gguf",
            "LFM2-700M-F16.gguf",
            "LFM2-700M-BF16.gguf",
        ] {
            assert_eq!(
                table.lookup(input).map(|e| e.total),
                Some(742),
                "lookup({input:?}) should resolve to LFM2-700M",
            );
        }
        Ok(())
    }

    #[test]
    fn lookup_strips_mlx_community_nbit_suffix() -> anyhow::Result<()> {
        let table = make_table()?;
        // mlx-community publishes quants as `<model>-4bit` / `-6bit` / `-8bit`.
        for input in [
            "mlx-community/LFM2-700M-4bit",
            "mlx-community/LFM2-700M-8bit",
            "mlx-community/LFM2-700M-2bit",
            "LiquidAI/LFM2-700M-MLX-4bit",
            "LiquidAI/LFM2-700M-MLX-8bit",
        ] {
            assert_eq!(
                table.lookup(input).map(|e| e.total),
                Some(742),
                "lookup({input:?}) should resolve to LFM2-700M",
            );
        }
        // Sparse / MoE entries still carry their distinct active value
        // through the strip.
        let e = table
            .lookup("mlx-community/gemma-4-e4b-it-4bit")
            .context("the table must know this model")?;
        assert_eq!(e.total, 7996);
        assert_eq!(e.active, 4000);
        Ok(())
    }

    #[test]
    fn lookup_strips_block_scaled_float_suffix() -> anyhow::Result<()> {
        let table = make_table()?;
        // MLX conversions are also published under the OCP microscaling
        // (`mxfp*`) and NVIDIA (`nvfp*`) block-scaled float formats.
        for input in [
            "LiquidAI/LFM2-700M-MLX-mxfp4",
            "LiquidAI/LFM2-700M-MLX-mxfp8",
            "LiquidAI/LFM2-700M-MLX-nvfp4",
            "LFM2-700M-MXFP4",
            "LFM2-700M-nvfp4.gguf",
        ] {
            assert_eq!(
                table.lookup(input).map(|e| e.total),
                Some(742),
                "lookup({input:?}) should resolve to LFM2-700M",
            );
        }
        Ok(())
    }

    #[test]
    fn lookup_strips_stacked_suffixes() -> anyhow::Result<()> {
        let table = make_table()?;
        // Operator-built names stack distribution and quant tags in any order.
        for input in [
            "LFM2-700M-MLX-Q4_0.gguf",
            "LFM2-700M-Q4_0-MLX",
            "LiquidAI/LFM2-700M-GGUF-Q4_K_M",
            "LFM2-700M-compiled-Q4_0",
        ] {
            assert_eq!(
                table.lookup(input).map(|e| e.total),
                Some(742),
                "lookup({input:?}) should resolve to LFM2-700M",
            );
        }
        Ok(())
    }

    #[test]
    fn lookup_strips_quantizer_markers() -> anyhow::Result<()> {
        let table = make_table()?;
        // Both mirror most popular GGUFs under a mid-name marker, so without
        // stripping it their spellings miss an entry the plain repo hits.
        for input in [
            "unsloth/LFM2-700M-GGUF:LFM2-700M-UD-Q4_K_XL.gguf",
            "LFM2-700M-UD-Q4_K_XL.gguf",
            "LFM2-700M-UD-IQ2_M",
            "mradermacher/LFM2-700M-i1-GGUF",
            "mradermacher/LFM2-700M-i1-GGUF:LFM2-700M.i1-Q4_K_M.gguf",
            "LFM2-700M.i1-Q4_K_M.gguf",
        ] {
            assert_eq!(
                table.lookup(input).map(|e| e.total),
                Some(742),
                "lookup({input:?}) should resolve to LFM2-700M",
            );
        }
        Ok(())
    }

    #[test]
    fn normalize_does_not_strip_non_quant_tokens() -> anyhow::Result<()> {
        // Endings that look like quant tags but aren't must survive, or distinct
        // entries collide.
        let table = ModelParamTable::from_toml(
            r#"
"LFM2-2.6B" = 2569
"granite-4.0-h-1b" = 1462
"gemma-3-270m-it" = 270
"model-mxfpx" = 100
"#,
        )?;
        assert_eq!(
            table
                .lookup("LFM2-2.6B")
                .context("the table must know this model")?
                .total,
            2569
        );
        assert_eq!(
            table
                .lookup("granite-4.0-h-1b")
                .context("the table must know this model")?
                .total,
            1462
        );
        assert_eq!(
            table
                .lookup("gemma-3-270m-it")
                .context("the table must know this model")?
                .total,
            270
        );
        // `mxfp` / `nvfp` need a digit after the prefix to count as a
        // quant tag, so a token that merely starts with one survives.
        assert_eq!(
            table
                .lookup("model-mxfpx")
                .context("the table must know this model")?
                .total,
            100
        );
        Ok(())
    }

    #[test]
    fn lookup_is_case_insensitive() -> anyhow::Result<()> {
        let table = make_table()?;
        for input in [
            "LFM2-700M",
            "lfm2-700m",
            "LiquidAI/LFM2-700M",
            "liquidai/lfm2-700m",
            "LIQUIDAI/LFM2-700M-GGUF",
            "liquidai/lfm2-700m-gguf:q4_0.gguf",
        ] {
            assert_eq!(
                table.lookup(input).map(|e| e.total),
                Some(742),
                "lookup({input:?}) should resolve to LFM2-700M",
            );
        }
        Ok(())
    }

    #[test]
    fn rejects_case_insensitive_duplicate_keys() -> anyhow::Result<()> {
        let err = ModelParamTable::from_toml(
            r#"
"LFM2-700M" = 700
"lfm2-700m" = 800
"#,
        )
        .err()
        .context("this table must be rejected")?;
        assert!(err.to_string().contains("duplicate"), "got: {err}");
        Ok(())
    }

    #[test]
    fn lookup_unknown_returns_none() -> anyhow::Result<()> {
        let table = make_table()?;
        assert!(table.lookup("not-a-real-model").is_none());
        Ok(())
    }

    #[test]
    fn empty_table_returns_none_for_everything() {
        let table = ModelParamTable::empty();
        assert!(table.lookup("LFM2-700M").is_none());
    }

    #[test]
    fn rejects_non_positive_dense_value() -> anyhow::Result<()> {
        let err = ModelParamTable::from_toml(r#""bad" = 0"#)
            .err()
            .context("this table must be rejected")?;
        assert!(err.to_string().contains("non-positive"));
        Ok(())
    }

    #[test]
    fn rejects_non_positive_total() -> anyhow::Result<()> {
        let err = ModelParamTable::from_toml(r#""bad" = { total = 0 }"#)
            .err()
            .context("this table must be rejected")?;
        assert!(err.to_string().contains("non-positive total"));
        Ok(())
    }

    #[test]
    fn rejects_active_greater_than_total() -> anyhow::Result<()> {
        let err = ModelParamTable::from_toml(r#""bad" = { total = 100, active = 200 }"#)
            .err()
            .context("this table must be rejected")?;
        assert!(err.to_string().contains("active (200) > total (100)"));
        Ok(())
    }

    #[test]
    fn rejects_non_positive_active() -> anyhow::Result<()> {
        let err = ModelParamTable::from_toml(r#""bad" = { total = 100, active = 0 }"#)
            .err()
            .context("this table must be rejected")?;
        assert!(err.to_string().contains("non-positive active"));
        Ok(())
    }

    #[test]
    fn parse_error_is_clear() -> anyhow::Result<()> {
        let err = ModelParamTable::from_toml("not = valid = toml")
            .err()
            .context("this table must be rejected")?;
        assert!(
            err.to_string()
                .contains("failed to parse the model params file")
        );
        Ok(())
    }

    #[tokio::test]
    async fn load_reads_the_stores_rows_and_none_is_an_empty_table() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = pipette_store_fs::build_local_fs_stores(&test_config(dir.path()))?;
        let store = stores.model_params.as_ref();
        assert!(ModelParamTable::load(store).await?.is_empty());

        store
            .put_model_params(&pipette_store_testkit::a_model_params("lfm2-700m", 700)?)
            .await?;
        assert_eq!(
            ModelParamTable::load(store)
                .await?
                .lookup("LiquidAI/LFM2-700M-GGUF"),
            Some(ModelEntry {
                total: 700,
                active: 700
            })
        );
        Ok(())
    }

    #[tokio::test]
    async fn import_adds_missing_rows_and_keeps_an_edited_one() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = pipette_store_fs::build_local_fs_stores(&test_config(dir.path()))?;
        let store = stores.model_params.as_ref();
        let edited = pipette_store_testkit::a_model_params("lfm2-700m", 742)?;
        store.put_model_params(&edited).await?;
        let file = ModelParamTable::from_toml(
            r#"
"LFM2-700M" = 700
"LFM2-8B-A1B" = { total = 8340, active = 1500 }
"#,
        )?;
        let moe = ModelKey::try_new("lfm2-8b-a1b")?;

        let dry = import(store, &file, true).await?;
        assert_eq!(dry.added, vec![moe.clone()]);
        assert!(store.get_model_params(&moe).await?.is_none());

        let report = import(store, &file, false).await?;
        assert_eq!(
            report,
            ImportReport {
                added: vec![moe.clone()],
                differing: vec![edited.key.clone()],
                unchanged: 0,
            }
        );
        let row = store.get_model_params(&moe).await?.context("the row")?;
        assert_eq!(
            (
                row.total_millions.into_inner(),
                row.active_millions.into_inner()
            ),
            (8340, 1500)
        );
        assert_eq!(row.updated_by, None);
        assert_eq!(store.get_model_params(&edited.key).await?, Some(edited));

        let again = import(store, &file, false).await?;
        assert!(again.added.is_empty());
        assert_eq!(again.unchanged, 1);
        Ok(())
    }

    /// The bad key sorts last, so only a check ahead of the writes keeps the
    /// good entry out.
    #[tokio::test]
    async fn import_adds_nothing_from_a_file_with_a_bad_key() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = pipette_store_fs::build_local_fs_stores(&test_config(dir.path()))?;
        let store = stores.model_params.as_ref();
        let file = ModelParamTable::from_toml("\"LFM2-700M\" = 700\n\"ZZ Bad\" = 100\n")?;

        let err = import(store, &file, false)
            .await
            .err()
            .context("a key with a space is refused")?;
        assert!(format!("{err:#}").contains(r#"entry "zz bad" cannot be a row key"#));
        assert!(drain_model_params(store).await?.items.is_empty());
        Ok(())
    }
}
