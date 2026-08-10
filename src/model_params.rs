//! `model_name → mill_params` lookup, loaded at startup from a single TOML
//! file (`model_params_mapping.toml`). The file lives in the storage backend root —
//! `{data_dir}/model_params_mapping.toml` for `local_fs`, `{prefix}/model_params_mapping.toml` in the
//! S3 bucket — so all instances of `pipette-mgmt` against the same backend
//! see the same catalog.
//!
//! Each entry has a *total* parameter count (drives RAM / disk footprint)
//! and an *active* parameter count (drives prefill/decode throughput).
//! For dense models the two are equal, so the TOML supports a shorthand
//! where a bare integer means `total = active = N`. MoE / selective-
//! activation models use the inline-table form to spell both values:
//!
//! ```toml
//! # model_params_mapping.toml
//! "LFM2-700M" = 742                                  # dense: total = active = 742
//! "LFM2-8B-A1B" = { total = 8340, active = 1500 }    # MoE: card states 8.3B total, 1.5B active
//! "gemma-4-E4B-it" = { total = 7996, active = 4500 } # MatFormer: 4.5B effective
//! ```
//!
//! Keys are normalized model names — lowercased, `org/` prefix stripped,
//! `:file.gguf` suffix stripped, trailing `.gguf` extension stripped, and
//! trailing distribution/quantization suffixes (`-GGUF`, `-MLX`, `-ONNX`,
//! `-compiled`, llama.cpp quant tags like `-Q4_K_M`/`-IQ3_M`/`-F16`/
//! `-BF16`) stripped — all case-insensitively, applied iteratively so
//! stacked suffixes collapse. The same normalization is applied to the
//! `model_name` on a submission before lookup, so all of these resolve to
//! the same entry as `"lfm2-700m"`:
//!
//! - `LFM2-700M`
//! - `LiquidAI/LFM2-700M`
//! - `LiquidAI/LFM2-700M-GGUF:Q4_0.gguf`
//! - `liquidai/lfm2-700m-gguf`
//! - `LiquidAI/LFM2-700M-MLX`
//! - `LiquidAI/LFM2-700M-MLX-bf16`
//! - `LiquidAI/LFM2-700M-ONNX`
//! - `LFM2-700M-Q4_K_M.gguf`
//! - `LFM2-700M-compiled`
//!
//! There is no fallback table baked into the binary: a fresh deployment
//! with no `model_params_mapping.toml` runs with an empty catalog, in which case the
//! scorer trusts whatever `model_params_total_millions` /
//! `model_params_active_millions` values the client supplied.
//!
//! When a submission has no `model_name`, the scorer matches these keys as
//! substrings of the opaque `model_descriptor` instead — see
//! [`ModelCatalog::resolve_from_descriptor`].

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use serde::Deserialize;

use crate::config::StorageConfig;

/// Resolved entry from the catalog. `active` is always populated — for
/// dense models it's set equal to `total` at load time so callers don't
/// have to branch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModelEntry {
    pub total: i32,
    pub active: i32,
}

/// Parsed TOML row before normalization. A bare integer means
/// `total = active = N`; the inline-table form spells both fields, with
/// `active` defaulting to `total` if omitted.
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

/// In-memory mapping from normalized `model_name` to its `ModelEntry`.
/// Constructed once at startup; cheap to `Arc::clone`.
#[derive(Clone, Debug, Default)]
pub struct ModelCatalog {
    map: Arc<HashMap<String, ModelEntry>>,
}

impl ModelCatalog {
    /// Empty catalog — every `lookup` returns `None`. Used when no
    /// `model_params_mapping.toml` is present and as the test default.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Parse a TOML body into a catalog. Validation:
    /// - `total > 0`
    /// - `active > 0` (defaults to `total` when omitted)
    /// - `active <= total` (active params are a subset of total params)
    /// - keys, after `normalize`, must be unique (case-insensitive
    ///   collision is an error so operators don't silently shadow an
    ///   entry with a differently-cased duplicate)
    pub fn from_toml(content: &str) -> anyhow::Result<Self> {
        let raw: HashMap<String, RawEntry> = toml::from_str(content)
            .map_err(|e| anyhow::anyhow!("failed to parse model_params_mapping.toml: {e}"))?;
        let mut map: HashMap<String, ModelEntry> = HashMap::with_capacity(raw.len());
        raw.into_iter().try_for_each(|(k, v)| {
            let entry = match v {
                RawEntry::Dense(t) => {
                    if t <= 0 {
                        anyhow::bail!(
                            "model_params_mapping.toml: entry {k:?} has non-positive value {t}; mill_params must be > 0"
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
                            "model_params_mapping.toml: entry {k:?} has non-positive total {total}; must be > 0"
                        );
                    }
                    let active = active.unwrap_or(total);
                    if active <= 0 {
                        anyhow::bail!(
                            "model_params_mapping.toml: entry {k:?} has non-positive active {active}; must be > 0"
                        );
                    }
                    if active > total {
                        anyhow::bail!(
                            "model_params_mapping.toml: entry {k:?} has active ({active}) > total ({total})"
                        );
                    }
                    ModelEntry { total, active }
                }
            };
            let normalized = normalize(&k);
            if map.insert(normalized.clone(), entry).is_some() {
                anyhow::bail!(
                    "model_params_mapping.toml: duplicate entry for normalized key {normalized:?} (case-insensitive collision)"
                );
            }
            Ok(())
        })?;
        Ok(Self { map: Arc::new(map) })
    }

    /// Load `model_params_mapping.toml` from the configured storage backend. Returns an
    /// empty catalog (with a `tracing::warn!`) if the file is absent —
    /// fresh setups don't have to ship a catalog before they can run.
    ///
    /// `path_override` lets operators point at a non-default location. When
    /// `None`, the path is `{data_dir}/model_params_mapping.toml` for
    /// `local_fs` and `{prefix}/model_params_mapping.toml` for `s3`. When
    /// `Some`, the value is used as-is: a filesystem path for `local_fs` and
    /// an object key for `s3` (the `prefix` is not prepended).
    pub async fn load(
        storage: &StorageConfig,
        path_override: Option<&Path>,
    ) -> anyhow::Result<Self> {
        match storage {
            StorageConfig::LocalFs { data_dir } => {
                let path = path_override
                    .map(std::path::Path::to_path_buf)
                    .unwrap_or_else(|| data_dir.join("model_params_mapping.toml"));
                match std::fs::read_to_string(&path) {
                    Ok(s) => {
                        let cat = Self::from_toml(&s)?;
                        tracing::info!(
                            path = %path.display(),
                            entries = cat.map.len(),
                            "loaded model catalog"
                        );
                        Ok(cat)
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        tracing::warn!(
                            path = %path.display(),
                            "model_params_mapping.toml not found; using empty catalog"
                        );
                        Ok(Self::empty())
                    }
                    Err(e) => Err(anyhow::anyhow!("failed to read {}: {e}", path.display())),
                }
            }
            StorageConfig::S3 { bucket, .. } => {
                use object_store::ObjectStoreExt;
                use object_store::path::Path as ObjPath;

                let (store, prefix) = crate::stores::build_s3_object_store(storage)?;
                let path = match path_override {
                    Some(p) => ObjPath::from(p.to_string_lossy().as_ref()),
                    None if prefix.is_empty() => ObjPath::from("model_params_mapping.toml"),
                    None => ObjPath::from(format!("{prefix}/model_params_mapping.toml")),
                };
                match store.get(&path).await {
                    Ok(r) => {
                        let bytes: bytes::Bytes = r.bytes().await?;
                        let s = std::str::from_utf8(&bytes)?;
                        let cat = Self::from_toml(s)?;
                        tracing::info!(
                            bucket = %bucket,
                            path = %path,
                            entries = cat.map.len(),
                            "loaded model catalog from S3"
                        );
                        Ok(cat)
                    }
                    Err(object_store::Error::NotFound { .. }) => {
                        tracing::warn!(
                            bucket = %bucket,
                            path = %path,
                            "model_params_mapping.toml not found in S3 bucket; using empty catalog"
                        );
                        Ok(Self::empty())
                    }
                    Err(e) => Err(e.into()),
                }
            }
        }
    }

    /// Resolve a `model_name` to its catalog entry, or `None` if the
    /// (normalized) name is not in the catalog. Lookup is
    /// case-insensitive — see [`normalize`].
    pub fn lookup(&self, model_name: &str) -> Option<ModelEntry> {
        self.map.get(&normalize(model_name)).copied()
    }

    /// Best-effort resolution from an opaque `model_descriptor` — the fallback
    /// used when a submission has no `model_name` (or an unrecognized one).
    /// Scans for catalog keys that occur as a substring of the (lowercased)
    /// descriptor; the descriptor's canonical form makes matching stable, and this
    /// never parses its schema or assumes any field (e.g. `repo_name`) is
    /// present, so it works for partner descriptors too. The longest matching
    /// key wins (most specific); an equal-length tie between distinct keys is
    /// ambiguous and yields `None`, as does no match — the caller then keeps
    /// the client-supplied params.
    pub fn resolve_from_descriptor(&self, descriptor: &str) -> Option<ModelEntry> {
        self.resolve_key_from_descriptor(descriptor).map(|(_, e)| e)
    }

    /// [`Self::resolve_from_descriptor`], but also returning the normalized
    /// catalog key that matched. `fix-model-param` needs the key to apply its
    /// `--model` filter to rows that carry no `model_name` — there is no other
    /// identity on such a row to compare the filter against.
    pub fn resolve_key_from_descriptor(&self, descriptor: &str) -> Option<(&str, ModelEntry)> {
        let hay = descriptor.to_lowercase();
        let (key, entry) = self
            .map
            .iter()
            .filter(|(k, _)| !k.is_empty() && hay.contains(k.as_str()))
            .max_by_key(|(k, _)| k.len())?;
        let ambiguous = self
            .map
            .keys()
            .filter(|k| k.len() == key.len() && hay.contains(k.as_str()))
            .count()
            > 1;
        (!ambiguous).then_some((key.as_str(), *entry))
    }

    /// Number of entries — exposed for log lines and tests.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

/// Strip the parts of `model_name` that don't identify the underlying
/// architecture and lowercase the result.
///
/// Steps:
/// 1. Strip any `org/` prefix (everything up to and including the last `/`).
/// 2. Strip any `:file.gguf` suffix (everything from the first `:`).
/// 3. Lowercase.
/// 4. Strip a trailing `.gguf` filename extension.
/// 5. Iteratively strip trailing distribution tags (`-gguf`, `-mlx`,
///    `-onnx`, `-compiled`) and llama.cpp / HuggingFace quantization
///    tags (`-q4_0`, `-q4_k_m`, `-iq3_m`, `-f16`, `-bf16`, ...), until
///    none apply. This collapses stacked suffixes like `-MLX-bf16` or
///    `-Q4_K_M.gguf`.
///
/// All catalog keys go through this on load, and every lookup applies it
/// to the input — so `LiquidAI/LFM2-700M-GGUF:Q4_0.gguf`,
/// `LiquidAI/LFM2-700M-MLX`, `LFM2-700M-Q4_K_M.gguf`, and `LFM2-700M`
/// all hit the same entry.
///
/// `pub(crate)` so row filters (e.g. `fix-model-param --model`) collapse
/// variants exactly as catalog lookup does.
pub(crate) fn normalize(model_name: &str) -> String {
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

/// If `s` ends in a strippable distribution / quantization tag, return
/// the length of `s` with that tag removed. Returns `None` when no tag
/// applies — that's the loop termination signal in `normalize`.
fn trailing_tag_prefix_len(s: &str) -> Option<usize> {
    const DISTRIBUTION_TAGS: &[&str] = &["-gguf", "-mlx", "-onnx", "-compiled"];

    for tag in DISTRIBUTION_TAGS {
        if let Some(rest) = s.strip_suffix(tag) {
            return Some(rest.len());
        }
    }
    let (prefix, last) = s.rsplit_once('-')?;
    is_quant_tag(last).then_some(prefix.len())
}

/// Recognize the lowercase quantization tag forms that show up after the
/// final `-` in distributed model names: `q4_0`, `q4_k_m`, `q5_k_s`,
/// `q6_k`, `q8_0`, `iq2_xs`, `iq3_m`, `iq4_xs`, `f16`, `f32`, `fp16`,
/// `bf16`, `int4`, `int8`, and the mlx-community `Nbit` form (`2bit`,
/// `3bit`, `4bit`, `6bit`, `8bit`).
///
/// The `q*` / `iq*` rule requires a digit immediately after the prefix
/// so it doesn't accidentally consume meaningful tokens like `quant` or
/// `iqr`. Likewise the `Nbit` rule requires the prefix to be all digits
/// so it doesn't strip tokens like `habit` or `64bit`-style platform tags
/// that aren't quant indicators we'd want to collapse.
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
    let Some(rest) = tag.strip_prefix("iq").or_else(|| tag.strip_prefix('q')) else {
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
    use crate::model_params::*;
    use rstest::rstest;

    const CATALOG: &str = r#"
"LFM2-700M" = 742
"LFM2.5-1.2B-Instruct" = 1170
"LFM2-8B-A1B" = { total = 8340, active = 1500 }
"LFM2.5-8B-A1B" = { total = 8340, active = 1500 }
"gemma-4-E4B-it" = { total = 7996, active = 4000 }
"#;

    fn make_catalog() -> ModelCatalog {
        ModelCatalog::from_toml(CATALOG).expect("test catalog must parse")
    }

    #[test]
    fn dense_shorthand_sets_total_equal_to_active() {
        let cat = make_catalog();
        let e = cat.lookup("LFM2-700M").unwrap();
        assert_eq!(e.total, 742);
        assert_eq!(e.active, 742);
    }

    /// Descriptor resolution is a substring match against the opaque string:
    /// field-agnostic (`no_repo_name_field` needs no `repo_name`), position-
    /// agnostic (`nested_identity` finds a name nested in a sub-object, as in
    /// the multi-artifact vision shape), longest key wins (`longest_match_wins`),
    /// and an equal-length tie or no match yields `None`. `742` / `500` / `3000`
    /// are the resolved totals.
    #[rstest]
    #[case::repo_name_substring(
        CATALOG,
        r#"{"filename":"LFM2-700M-Q4_0.gguf","org":"LiquidAI","repo_name":"LFM2-700M-GGUF","type":"hf_gguf_text"}"#,
        Some(742)
    )]
    #[case::no_repo_name_field(
        CATALOG,
        r#"{"artifact_path":"models/lfm2-700m/weights.bin","vendor":"acme"}"#,
        Some(742)
    )]
    #[case::nested_identity(
        "\"LFM2.5-Vision-3B\" = 3000",
        r#"{"model":{"org":"LiquidAI","repo_name":"LFM2.5-Vision-3B","path":"q4_K_M.gguf"},"mmproj":{"org":"LiquidAI","repo_name":"LFM2.5-Vision-3B","path":"mmproj-f16.gguf"},"type":"gguf_vision"}"#,
        Some(3000)
    )]
    #[case::longest_match_wins(
        "\"LFM2-700M\" = 742\n\"LFM2-700M-VL\" = 500",
        r#"{"repo_name":"LFM2-700M-VL-GGUF"}"#,
        Some(500)
    )]
    #[case::ambiguous_tie(
        "\"model-aaa\" = 100\n\"model-bbb\" = 200",
        r#"{"a":"model-aaa","b":"model-bbb"}"#,
        None
    )]
    #[case::no_match(
        CATALOG,
        r#"{"repo_name":"some-unlisted-model","type":"hf_gguf_text"}"#,
        None
    )]
    fn resolve_from_descriptor_cases(
        #[case] toml: &str,
        #[case] descriptor: &str,
        #[case] expected_total: Option<i32>,
    ) -> anyhow::Result<()> {
        let cat = ModelCatalog::from_toml(toml)?;
        assert_eq!(
            cat.resolve_from_descriptor(descriptor).map(|e| e.total),
            expected_total
        );
        Ok(())
    }

    #[test]
    fn sparse_form_keeps_distinct_values() {
        let cat = make_catalog();
        let e = cat.lookup("LFM2-8B-A1B").unwrap();
        assert_eq!(e.total, 8340);
        assert_eq!(e.active, 1500);
    }

    #[test]
    fn sparse_active_defaults_to_total_when_omitted() {
        let cat = ModelCatalog::from_toml(r#""x" = { total = 500 }"#).unwrap();
        let e = cat.lookup("x").unwrap();
        assert_eq!(e.total, 500);
        assert_eq!(e.active, 500);
    }

    #[test]
    fn lookup_strips_org_prefix() {
        let cat = make_catalog();
        assert_eq!(cat.lookup("LiquidAI/LFM2-700M").unwrap().total, 742);
    }

    #[test]
    fn lookup_strips_gguf_and_colon() {
        let cat = make_catalog();
        let e = cat.lookup("LiquidAI/LFM2-8B-A1B-GGUF:Q4_0.gguf").unwrap();
        assert_eq!(e.total, 8340);
        assert_eq!(e.active, 1500);
    }

    #[test]
    fn lookup_strips_lowercase_gguf() {
        let cat = make_catalog();
        // Some repos use lowercase `gguf` in the suffix
        let e = cat.lookup("liquidai/lfm2-8b-a1b-gguf").unwrap();
        assert_eq!(e.total, 8340);
    }

    #[test]
    fn lookup_resolves_lfm25_8b_a1b_gated_gguf_name() {
        let cat = make_catalog();
        let e = cat.lookup("LiquidAI/LFM2.5-8B-A1B-GGUF").unwrap();
        assert_eq!(e.total, 8340);
        assert_eq!(e.active, 1500);
    }

    #[test]
    fn lookup_strips_mlx_suffix() {
        let cat = make_catalog();
        for input in [
            "LiquidAI/LFM2.5-1.2B-Instruct-MLX",
            "LiquidAI/LFM2.5-1.2B-Instruct-mlx",
            "LFM2.5-1.2B-Instruct-MLX",
            // Stacked: `-MLX-bf16` collapses via quant-then-distribution
            // strip in the loop.
            "LiquidAI/LFM2.5-1.2B-Instruct-MLX-bf16",
        ] {
            assert_eq!(
                cat.lookup(input).map(|e| e.total),
                Some(1170),
                "lookup({input:?}) should resolve to LFM2.5-1.2B-Instruct",
            );
        }
    }

    #[test]
    fn lookup_strips_onnx_suffix() {
        let cat = make_catalog();
        for input in [
            "LiquidAI/LFM2-700M-ONNX",
            "LiquidAI/LFM2-700M-onnx",
            "LFM2-700M-ONNX",
        ] {
            assert_eq!(
                cat.lookup(input).map(|e| e.total),
                Some(742),
                "lookup({input:?}) should resolve to LFM2-700M",
            );
        }
    }

    #[test]
    fn lookup_strips_compiled_suffix() {
        let cat = make_catalog();
        let e = cat.lookup("LFM2.5-1.2B-Instruct-compiled").unwrap();
        assert_eq!(e.total, 1170);
    }

    #[test]
    fn lookup_strips_gguf_filename_extension() {
        let cat = make_catalog();
        // Bare filename — no `org/` prefix and no `:` delimiter, so the
        // existing colon-strip didn't apply. The `.gguf` extension and
        // the `-Q4_K_M` quant tag both have to come off.
        for input in [
            "LFM2-700M-Q4_K_M.gguf",
            "LFM2-700M-Q4_0.gguf",
            "LFM2-700M-IQ3_M.gguf",
            "LFM2-700M-F16.gguf",
            "LFM2-700M-BF16.gguf",
        ] {
            assert_eq!(
                cat.lookup(input).map(|e| e.total),
                Some(742),
                "lookup({input:?}) should resolve to LFM2-700M",
            );
        }
    }

    #[test]
    fn lookup_strips_mlx_community_nbit_suffix() {
        let cat = make_catalog();
        // mlx-community publishes quants under names like
        // `mlx-community/<model>-4bit` / `-6bit` / `-8bit`. These need to
        // resolve to the same catalog entry as the unquantized model.
        for input in [
            "mlx-community/LFM2-700M-4bit",
            "mlx-community/LFM2-700M-8bit",
            "mlx-community/LFM2-700M-2bit",
            "LiquidAI/LFM2-700M-MLX-4bit",
            "LiquidAI/LFM2-700M-MLX-8bit",
        ] {
            assert_eq!(
                cat.lookup(input).map(|e| e.total),
                Some(742),
                "lookup({input:?}) should resolve to LFM2-700M",
            );
        }
        // Sparse / MoE entries still carry their distinct active value
        // through the strip.
        let e = cat.lookup("mlx-community/gemma-4-e4b-it-4bit").unwrap();
        assert_eq!(e.total, 7996);
        assert_eq!(e.active, 4000);
    }

    #[test]
    fn lookup_strips_stacked_suffixes() {
        let cat = make_catalog();
        // Operator-built artifact names sometimes stack distribution
        // and quant tags. The iterative strip should peel them off
        // regardless of order.
        for input in [
            "LFM2-700M-MLX-Q4_0.gguf",
            "LFM2-700M-Q4_0-MLX",
            "LiquidAI/LFM2-700M-GGUF-Q4_K_M",
            "LFM2-700M-compiled-Q4_0",
        ] {
            assert_eq!(
                cat.lookup(input).map(|e| e.total),
                Some(742),
                "lookup({input:?}) should resolve to LFM2-700M",
            );
        }
    }

    #[test]
    fn normalize_does_not_strip_non_quant_tokens() {
        // Catalog token endings that look superficially like quant tags
        // but aren't — these must be preserved so distinct entries don't
        // collide.
        let cat = ModelCatalog::from_toml(
            r#"
"LFM2-2.6B" = 2569
"granite-4.0-h-1b" = 1462
"gemma-3-270m-it" = 270
"#,
        )
        .unwrap();
        assert_eq!(cat.lookup("LFM2-2.6B").unwrap().total, 2569);
        assert_eq!(cat.lookup("granite-4.0-h-1b").unwrap().total, 1462);
        assert_eq!(cat.lookup("gemma-3-270m-it").unwrap().total, 270);
    }

    #[test]
    fn lookup_is_case_insensitive() {
        let cat = make_catalog();
        // Catalog key is "LFM2-700M"; lookup with different casings
        // should all hit the same entry.
        for input in [
            "LFM2-700M",
            "lfm2-700m",
            "LiquidAI/LFM2-700M",
            "liquidai/lfm2-700m",
            "LIQUIDAI/LFM2-700M-GGUF",
            "liquidai/lfm2-700m-gguf:q4_0.gguf",
        ] {
            assert_eq!(
                cat.lookup(input).map(|e| e.total),
                Some(742),
                "lookup({input:?}) should resolve to LFM2-700M",
            );
        }
    }

    #[test]
    fn rejects_case_insensitive_duplicate_keys() {
        let err = ModelCatalog::from_toml(
            r#"
"LFM2-700M" = 700
"lfm2-700m" = 800
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("duplicate"), "got: {err}");
    }

    #[test]
    fn lookup_unknown_returns_none() {
        let cat = make_catalog();
        assert!(cat.lookup("not-a-real-model").is_none());
    }

    #[test]
    fn empty_catalog_returns_none_for_everything() {
        let cat = ModelCatalog::empty();
        assert!(cat.lookup("LFM2-700M").is_none());
    }

    #[test]
    fn rejects_non_positive_dense_value() {
        let err = ModelCatalog::from_toml(r#""bad" = 0"#).unwrap_err();
        assert!(err.to_string().contains("non-positive"));
    }

    #[test]
    fn rejects_non_positive_total() {
        let err = ModelCatalog::from_toml(r#""bad" = { total = 0 }"#).unwrap_err();
        assert!(err.to_string().contains("non-positive total"));
    }

    #[test]
    fn rejects_active_greater_than_total() {
        let err = ModelCatalog::from_toml(r#""bad" = { total = 100, active = 200 }"#).unwrap_err();
        assert!(err.to_string().contains("active (200) > total (100)"));
    }

    #[test]
    fn rejects_non_positive_active() {
        let err = ModelCatalog::from_toml(r#""bad" = { total = 100, active = 0 }"#).unwrap_err();
        assert!(err.to_string().contains("non-positive active"));
    }

    #[test]
    fn parse_error_is_clear() {
        let err = ModelCatalog::from_toml("not = valid = toml").unwrap_err();
        assert!(
            err.to_string()
                .contains("failed to parse model_params_mapping.toml")
        );
    }

    #[tokio::test]
    async fn load_local_fs_uses_default_path_when_override_none() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("model_params_mapping.toml"),
            r#""LFM2-700M" = 700"#,
        )
        .unwrap();
        let storage = StorageConfig::local_fs(dir.path().to_path_buf());
        let cat = ModelCatalog::load(&storage, None).await.unwrap();
        assert_eq!(cat.lookup("LFM2-700M").unwrap().total, 700);
    }

    #[tokio::test]
    async fn load_local_fs_honors_path_override() {
        let dir = tempfile::tempdir().unwrap();
        // Default location is empty; override points elsewhere.
        let custom = dir.path().join("custom-models.toml");
        std::fs::write(&custom, r#""LFM2-700M" = 999"#).unwrap();
        let storage = StorageConfig::local_fs(dir.path().to_path_buf());
        let cat = ModelCatalog::load(&storage, Some(&custom)).await.unwrap();
        assert_eq!(cat.lookup("LFM2-700M").unwrap().total, 999);
    }

    #[tokio::test]
    async fn load_local_fs_override_missing_returns_empty_catalog() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist.toml");
        let storage = StorageConfig::local_fs(dir.path().to_path_buf());
        let cat = ModelCatalog::load(&storage, Some(&missing)).await.unwrap();
        assert!(cat.is_empty());
    }
}
