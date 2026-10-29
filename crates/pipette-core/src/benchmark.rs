use std::collections::HashMap;
use std::path::Path;

use anyhow::Context;
use serde::de::{DeserializeOwned, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use toml::Table;

use pipette_plan_types::{
    BatchSize, BenchmarkDefinition, BenchmarkEntry, BenchmarkId, BenchmarkParams, BenchmarkType,
    Dataset, OrgId, VlmEvalRecipe, VlmPromptProfile,
};

/// Wire fields every submission may report, whatever it measured: the scorer
/// gates none of them on `benchmark_type`.
const UNIVERSAL_MEASUREMENT_FIELDS: &[&str] = &[
    "observation_max_swap_bytes",
    "observation_max_host_bytes",
    // The digest of the corpus the client verified: a mechanism, not a type, so
    // any benchmark that serves a corpus reports it under this name.
    "observation_dataset_digest",
    // The digest over a corpus traversal's replies, bare for the same reason.
    "observation_output_digest",
];

/// The result fields a submission of this type carries on the wire: what the
/// benchmark *measured*, not its configuration. An exhaustive `match`, so a new
/// type does not compile until its fields are declared here.
/// Includes the `_stddev` and legacy `max_*_bytes` spellings, and deliberately not
/// `model_params_*`, `benchmark_flags` or `runtime_cpu_variant`: configuration,
/// knowable whether or not the run produced a number.
pub fn measurement_fields(benchmark_type: BenchmarkType) -> &'static [&'static str] {
    match benchmark_type {
        BenchmarkType::PrefillThroughput => &["prefill_time_ms", "prefill_time_ms_stddev"],
        BenchmarkType::DecodeThroughput => &["decode_time_ms", "decode_time_ms_stddev"],
        BenchmarkType::EndToEndLatency => &["total_time_ms", "total_time_ms_stddev"],
        BenchmarkType::MaxMemoryUsage
        | BenchmarkType::VlMaxMemory
        | BenchmarkType::AsrMaxMemory => &[
            "max_host_bytes",
            "max_ram_bytes",
            "max_gpu_bytes",
            "max_vram_bytes",
            "max_npu_bytes",
        ],
        BenchmarkType::Eval | BenchmarkType::VlmEval | BenchmarkType::AsrEval => &["completions"],
        BenchmarkType::VlThroughput => &[
            "prompt_tokens",
            "image_tokens",
            "prompt_ms",
            "prompt_ms_stddev",
            "predicted_ms",
            "predicted_ms_stddev",
            "total_time_ms",
            "total_time_ms_stddev",
        ],
        BenchmarkType::AsrThroughput => &[
            "asr_prompt_tokens",
            "asr_audio_tokens",
            "asr_prompt_ms",
            "asr_prompt_ms_stddev",
            "asr_predicted_ms",
            "asr_predicted_ms_stddev",
            // The client's span, under VL's names: one quantity, timed alike.
            "total_time_ms",
            "total_time_ms_stddev",
        ],
        // Counters, not derived rates: the accept-length convention
        // differs between runtimes and papers, so the wire carries what
        // was counted and the scorer derives the columns from it. The
        // last three arrive only with speculation on.
        BenchmarkType::VlSpecDecode => &[
            "vl_spec_decode_turns",
            "vl_spec_decode_prompt_ms",
            "vl_spec_decode_predicted_ms",
            "vl_spec_decode_ttft_ms",
            "vl_spec_decode_decode_ms",
            "vl_spec_decode_e2e_ms",
            "vl_spec_decode_generated_tokens",
            "vl_spec_decode_drafted_tokens",
            "vl_spec_decode_accepted_tokens",
            "vl_spec_decode_turn_accept_length_sum",
        ],
    }
}

/// Every measurement field of every benchmark type, plus the universal ones,
/// derived from [`measurement_fields`].
pub fn all_measurement_fields() -> impl Iterator<Item = &'static str> {
    BenchmarkType::ALL
        .iter()
        .flat_map(|&benchmark_type| measurement_fields(benchmark_type))
        .chain(UNIVERSAL_MEASUREMENT_FIELDS)
        .copied()
}

/// A catalog entry: the definition a device parses off the catalog, and whose
/// entry it is.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Benchmark {
    #[serde(flatten)]
    pub def: BenchmarkDefinition,
    /// The organization this entry is private to; absent on a public entry. Not
    /// in the file: the loader records it from the `org-benchmarks/{org_id}/`
    /// path. Serialized when set, so a member can tell its own entries from the
    /// public ones.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub org_id: Option<OrgId>,
}

impl From<Benchmark> for pipette_plan_types::CatalogEntry {
    /// A catalog entry as the device reads it.
    fn from(benchmark: Benchmark) -> Self {
        Self {
            definition: benchmark.def,
            org_id: benchmark.org_id,
            samples: None,
        }
    }
}

impl Benchmark {
    pub fn benchmark_id(&self) -> &BenchmarkId {
        self.def.benchmark_id()
    }

    pub fn benchmark_type(&self) -> BenchmarkType {
        self.def.benchmark_type()
    }

    /// The corpus this benchmark pulls from its origin; `None` for every type
    /// that does not pull one.
    pub fn dataset(&self) -> Option<&Dataset> {
        self.def.dataset()
    }

    pub fn from_toml(benchmark_id: &str, content: &str) -> anyhow::Result<Self> {
        let parsed = toml::from_str(content)
            .map_err(|e| anyhow::anyhow!("{}", e.to_string().trim_end()))
            .and_then(|body| Self::from_body(BenchmarkId::try_new(benchmark_id)?, body));
        parsed.map_err(|e| anyhow::anyhow!("failed to parse benchmark {benchmark_id}: {e}"))
    }

    /// [`Self::from_toml`] for a JSON body, as an organization's catalog write sends it.
    pub fn from_json(benchmark_id: BenchmarkId, body: DefinitionObject) -> anyhow::Result<Self> {
        Self::from_body(benchmark_id.clone(), body.0)
            .map_err(|e| anyhow::anyhow!("failed to parse benchmark {benchmark_id}: {e}"))
    }

    /// The entry a body defines, parsed as the client's type for its
    /// `benchmark_type`: through the tagged enum, a key it does not name would go
    /// unseen.
    fn from_body(benchmark_id: BenchmarkId, mut body: Table) -> anyhow::Result<Self> {
        // The path names the entry, so a body naming one too is refused rather
        // than either winning.
        if body.contains_key("benchmark_id") {
            anyhow::bail!("unknown field `benchmark_id`");
        }
        let benchmark_type: BenchmarkType = body
            .remove("benchmark_type")
            .context("missing field `benchmark_type`")?
            .try_into()?;
        body.insert("benchmark_id".into(), benchmark_id.as_str().into());
        let body = toml::Value::Table(body);
        let def = match benchmark_type {
            BenchmarkType::PrefillThroughput => {
                BenchmarkDefinition::PrefillThroughput(strict(body)?)
            }
            BenchmarkType::DecodeThroughput => BenchmarkDefinition::DecodeThroughput(strict(body)?),
            BenchmarkType::EndToEndLatency => BenchmarkDefinition::EndToEndLatency(strict(body)?),
            BenchmarkType::MaxMemoryUsage => BenchmarkDefinition::MaxMemoryUsage(strict(body)?),
            BenchmarkType::Eval => BenchmarkDefinition::Eval(strict(body)?),
            BenchmarkType::VlmEval => BenchmarkDefinition::VlmEval(strict(body)?),
            BenchmarkType::AsrEval => BenchmarkDefinition::AsrEval(strict(body)?),
            BenchmarkType::VlThroughput => BenchmarkDefinition::VlThroughput(strict(body)?),
            BenchmarkType::VlMaxMemory => BenchmarkDefinition::VlMaxMemory(strict(body)?),
            BenchmarkType::VlSpecDecode => BenchmarkDefinition::VlSpecDecode(strict(body)?),
            BenchmarkType::AsrThroughput => BenchmarkDefinition::AsrThroughput(strict(body)?),
            BenchmarkType::AsrMaxMemory => BenchmarkDefinition::AsrMaxMemory(strict(body)?),
        };
        Self::from_def(def)
    }

    /// A public entry from a definition already parsed, held to the checks a
    /// file is, so a definition that could not be loaded cannot be written.
    ///
    /// A corpus `url` is dropped rather than refused: the server stamps its own
    /// on the catalog's entry, and a body copied from that entry carries it.
    /// The eval `samples` the entry is stamped with are refused: they are the scoring
    /// service's, not the entry's.
    pub fn from_def(mut def: BenchmarkDefinition) -> anyhow::Result<Self> {
        let stamped = match &def {
            BenchmarkDefinition::Eval(b) => b.samples.is_some(),
            BenchmarkDefinition::VlmEval(b) => b.samples.is_some(),
            // An asr_eval carries no samples field at all: they are its pinned
            // corpus, never stamped on the entry.
            BenchmarkDefinition::AsrEval(_)
            | BenchmarkDefinition::PrefillThroughput(_)
            | BenchmarkDefinition::AsrThroughput(_)
            | BenchmarkDefinition::AsrMaxMemory(_)
            | BenchmarkDefinition::DecodeThroughput(_)
            | BenchmarkDefinition::EndToEndLatency(_)
            | BenchmarkDefinition::MaxMemoryUsage(_)
            | BenchmarkDefinition::VlThroughput(_)
            | BenchmarkDefinition::VlMaxMemory(_)
            | BenchmarkDefinition::VlSpecDecode(_) => false,
        };
        if stamped {
            anyhow::bail!("unknown field `samples`");
        }
        // The rule derives each sample's seed from the entry's: with none pinned
        // there is nothing to derive from, and no client would act on it.
        let rule_without_seed = match &def {
            BenchmarkDefinition::VlmEval(b) => {
                b.parameter_seed_rule.is_some() && b.parameter_seed.is_none()
            }
            BenchmarkDefinition::AsrEval(b) => {
                b.parameter_seed_rule.is_some() && b.parameter_seed.is_none()
            }
            BenchmarkDefinition::Eval(_)
            | BenchmarkDefinition::PrefillThroughput(_)
            | BenchmarkDefinition::AsrThroughput(_)
            | BenchmarkDefinition::AsrMaxMemory(_)
            | BenchmarkDefinition::DecodeThroughput(_)
            | BenchmarkDefinition::EndToEndLatency(_)
            | BenchmarkDefinition::MaxMemoryUsage(_)
            | BenchmarkDefinition::VlThroughput(_)
            | BenchmarkDefinition::VlMaxMemory(_)
            | BenchmarkDefinition::VlSpecDecode(_) => false,
        };
        if rule_without_seed {
            anyhow::bail!(
                "benchmark {}: parameter_seed_rule needs a parameter_seed",
                def.benchmark_id()
            );
        }
        if let Ok(eval) = def.as_eval() {
            eval.check_max_context()
                .map_err(|e| anyhow::anyhow!("benchmark {}: {e}", def.benchmark_id()))?;
        }
        if let Ok(asr) = def.as_asr_eval() {
            asr.check_decoding()
                .map_err(|e| anyhow::anyhow!("benchmark {}: {e}", def.benchmark_id()))?;
        }
        if let Some(dataset) = def.dataset_mut() {
            dataset.url = None;
        }
        let checked = def
            .dataset()
            .map_or(Ok(()), crate::dataset::check)
            .map_err(anyhow::Error::from)
            .and_then(|()| match &def {
                BenchmarkDefinition::VlSpecDecode(b) => {
                    crate::dataset::check_vl_spec_decode(&b.dataset).map_err(anyhow::Error::from)
                }
                BenchmarkDefinition::VlmEval(b) => crate::dataset::check_vlm_eval(
                    b.dataset.as_deref(),
                    b.parameter_lmms_task.as_str(),
                    &b.parameter_dataset_version,
                    b.parameter_prompt_profile
                        .as_ref()
                        .map(VlmPromptProfile::as_str),
                )
                .map_err(anyhow::Error::from)
                .and_then(|()| check_vlm_recipe(b.recipe.as_ref()).map_err(anyhow::Error::from)),
                BenchmarkDefinition::AsrEval(b) => crate::dataset::check_asr_eval(
                    b.dataset.as_deref(),
                    b.parameter_asr_task.as_str(),
                    &b.parameter_dataset_version,
                    b.parameter_prompt_profile.as_str(),
                )
                .map_err(anyhow::Error::from)
                .and_then(|()| check_asr_recipe(b.recipe.as_ref()).map_err(anyhow::Error::from)),
                BenchmarkDefinition::PrefillThroughput(_)
                | BenchmarkDefinition::AsrThroughput(_)
                | BenchmarkDefinition::AsrMaxMemory(_)
                | BenchmarkDefinition::DecodeThroughput(_)
                | BenchmarkDefinition::EndToEndLatency(_)
                | BenchmarkDefinition::MaxMemoryUsage(_)
                | BenchmarkDefinition::Eval(_)
                | BenchmarkDefinition::VlThroughput(_)
                | BenchmarkDefinition::VlMaxMemory(_) => Ok(()),
            });
        checked.map_err(|e| anyhow::anyhow!("benchmark {}: {e}", def.benchmark_id()))?;
        Ok(Self { def, org_id: None })
    }

    /// The same entry, recorded as private to `org_id`.
    pub fn private_to(mut self, org_id: OrgId) -> Self {
        self.org_id = Some(org_id);
        self
    }

    /// The definition as the file a catalog directory holds, the inverse of
    /// [`Self::from_toml`]. The path carries the id and the organization.
    pub fn to_toml(&self) -> anyhow::Result<String> {
        let mut table = toml::Table::try_from(&self.def)?;
        table.remove("benchmark_id");
        Ok(toml::to_string(&table)?)
    }

    /// The parameters a client reads off this entry.
    pub fn params(&self) -> BenchmarkParams {
        BenchmarkParams::from(&self.def)
    }

    /// The reduced catalog entry: the id and [`Self::params`].
    pub fn entry(&self) -> BenchmarkEntry {
        BenchmarkEntry {
            benchmark_id: self.benchmark_id().clone(),
            benchmark_params: self.params(),
            org_id: self.org_id.clone(),
        }
    }

    /// Concurrent requests the run keeps in flight; `None` for every type
    /// that runs one at a time by definition.
    pub fn parameter_batch_size(&self) -> Option<BatchSize> {
        self.def.batch_size()
    }
}

/// `T` from `body`, refused for a top-level key `T` does not name, or one
/// within `recipe`: the client's types tolerate one, where a misspelled
/// parameter in a catalog entry would measure something it never asked for,
/// and a misspelled recipe key would let a cell off the recipe. Within
/// `dataset` the client's tolerance stands.
fn strict<T: DeserializeOwned>(body: toml::Value) -> anyhow::Result<T> {
    let mut unknown = None;
    let parsed: Result<T, _> = serde_ignored::deserialize(body, |path| {
        if let serde_ignored::Path::Map { parent, key } = path
            && (matches!(parent, serde_ignored::Path::Root) || in_recipe(parent))
        {
            unknown.get_or_insert(key);
        }
    });
    // A misspelled key is reported as itself, not as the key it left missing.
    if let Some(key) = unknown {
        anyhow::bail!("unknown field `{key}`");
    }
    parsed.map_err(|e| anyhow::anyhow!("{e}"))
}

/// Whether `path` lies within the top-level `recipe` table.
fn in_recipe(mut path: &serde_ignored::Path<'_>) -> bool {
    loop {
        path = match path {
            serde_ignored::Path::Root => return false,
            serde_ignored::Path::Map {
                parent: serde_ignored::Path::Root,
                key,
            } => return key == "recipe",
            serde_ignored::Path::Map { parent, .. }
            | serde_ignored::Path::Seq { parent, .. }
            | serde_ignored::Path::Some { parent }
            | serde_ignored::Path::NewtypeStruct { parent }
            | serde_ignored::Path::NewtypeVariant { parent } => parent,
        };
    }
}

/// A `PUT` body's JSON object as the table a catalog file holds. A repeated
/// key is refused at any depth, as TOML refuses one, where a plain map would
/// keep its last value; a `null` is no value, as an absent key is.
pub struct DefinitionObject(Table);

impl<'de> Deserialize<'de> for DefinitionObject {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match deserializer.deserialize_any(TomlValue)? {
            Some(toml::Value::Table(table)) => Ok(Self(table)),
            _ => Err(serde::de::Error::custom("a definition is a JSON object")),
        }
    }
}

/// Reads one JSON value as a TOML one; `None` for `null`.
struct TomlValue;

impl<'de> Visitor<'de> for TomlValue {
    type Value = Option<toml::Value>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_bool<E>(self, v: bool) -> Result<Self::Value, E> {
        Ok(Some(v.into()))
    }

    fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E> {
        Ok(Some(v.into()))
    }

    fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
        i64::try_from(v)
            .map(|v| Some(v.into()))
            .map_err(|_| E::custom(format!("integer {v} is out of range")))
    }

    fn visit_f64<E>(self, v: f64) -> Result<Self::Value, E> {
        Ok(Some(v.into()))
    }

    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E> {
        Ok(Some(v.into()))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut items = Vec::new();
        while let Some(item) = seq.next_element_seed(TomlValue)? {
            items.push(item.ok_or_else(|| serde::de::Error::custom("null in an array"))?);
        }
        Ok(Some(toml::Value::Array(items)))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut table = Table::new();
        let mut seen = std::collections::HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !seen.insert(key.clone()) {
                return Err(serde::de::Error::custom(format!("duplicate field `{key}`")));
            }
            if let Some(value) = map.next_value_seed(TomlValue)? {
                table.insert(key, value);
            }
        }
        Ok(Some(toml::Value::Table(table)))
    }
}

impl<'de> DeserializeSeed<'de> for TomlValue {
    type Value = Option<toml::Value>;

    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_any(self)
    }
}

/// A catalog read: the entries that parsed, and one message per entry that did
/// not. Serving prefers some entries to none; [`load_catalog`] is the strict
/// reading.
#[derive(Debug, Default)]
pub struct CatalogLoad {
    pub catalog: HashMap<BenchmarkId, Benchmark>,
    /// One message per unreadable entry, sorted so a report does not reorder
    /// between runs.
    pub errors: Vec<String>,
}

impl CatalogLoad {
    /// One `error!` per unreadable entry, and a summary. Called by the stores,
    /// which serve the entries that did parse.
    pub fn log_errors(&self) {
        if self.errors.is_empty() {
            return;
        }
        tracing::error!(
            failed = self.errors.len(),
            loaded = self.catalog.len(),
            "skipping unreadable benchmark catalog entries"
        );
        self.errors
            .iter()
            .for_each(|e| tracing::error!(detail = %e, "unreadable benchmark catalog entry"));
    }
}

/// Every entry under `benchmarks_dir`, capturing per-entry failures. A missing
/// or empty directory is still an error: a misconfigured deployment.
pub fn read_catalog(benchmarks_dir: &Path) -> anyhow::Result<CatalogLoad> {
    if !benchmarks_dir.exists() {
        anyhow::bail!(
            "benchmarks directory does not exist: {}",
            benchmarks_dir.display()
        );
    }
    let load = read_entries(benchmarks_dir)?;
    if load.catalog.is_empty() && load.errors.is_empty() {
        anyhow::bail!("no benchmark files found in {}", benchmarks_dir.display());
    }
    Ok(load)
}

/// The `.toml` files of one directory, parsed.
fn read_entries(dir: &Path) -> anyhow::Result<CatalogLoad> {
    let entries: Vec<_> = std::fs::read_dir(dir)?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "toml"))
        .collect();

    let (catalog, mut errors) = entries
        .into_iter()
        .map(|entry| load_entry(&entry.path()))
        .fold(
            (HashMap::new(), Vec::new()),
            |(mut catalog, mut errors), loaded| {
                match loaded {
                    Ok((id, benchmark)) => {
                        catalog.insert(id, benchmark);
                    }
                    Err(e) => errors.push(format!("{e:#}")),
                }
                (catalog, errors)
            },
        );
    errors.sort();

    Ok(CatalogLoad { catalog, errors })
}

/// One catalog entry, under the id its filename carries.
pub fn load_entry(path: &Path) -> anyhow::Result<(BenchmarkId, Benchmark)> {
    let benchmark_id = path
        .file_stem()
        .ok_or_else(|| anyhow::anyhow!("missing file stem for {}", path.display()))?
        .to_string_lossy()
        .to_string();
    let content =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let benchmark = Benchmark::from_toml(&benchmark_id, &content)?;
    Ok((BenchmarkId::try_new(benchmark_id)?, benchmark))
}

/// Every entry, or an error naming each one that failed: the strict reading the
/// shipped catalog's CI gate uses.
pub fn load_catalog(benchmarks_dir: &Path) -> anyhow::Result<HashMap<BenchmarkId, Benchmark>> {
    let load = read_catalog(benchmarks_dir)?;
    if !load.errors.is_empty() {
        anyhow::bail!(
            "{} of {} benchmark files failed to load:\n  {}",
            load.errors.len(),
            load.errors.len() + load.catalog.len(),
            load.errors.join("\n  ")
        );
    }
    Ok(load.catalog)
}

/// Why a vlm_eval entry's recipe cannot be published.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RecipeError {
    #[error(
        "a vlm_eval entry must say whose recipe it is: a `[recipe]` naming its model family, \
         the models it covers, their thinking mode and the public sources of its settings"
    )]
    Missing,
    #[error("recipe source {0} is not an https URL; a source is a public page a reader can open")]
    NotHttps(String),
    #[error("the recipe lists {0} twice")]
    RepeatedModel(String),
    #[error(
        "an asr_eval entry must say whose recipe it is: a `[recipe]` naming its model family, \
         the models it covers, `enable_thinking = false` and the public sources of its settings"
    )]
    AsrMissing,
    #[error(
        "an asr_eval recipe runs with enable_thinking = false, as every ASR run does; this one \
         says {0}"
    )]
    AsrThinking(pipette_plan_types::Thinking),
    #[error(
        "a vlm_eval recipe lists no languages: `languages` is the speech-recognition languages \
         an asr_eval recipe's card declares"
    )]
    VlmLanguages,
    #[error("the recipe lists language {0} twice")]
    RepeatedLanguage(String),
}

/// An asr_eval entry's recipe: a vlm_eval one's rules, and thinking always off
/// (stated, not left unset), which is what the client runs and the scorer
/// accepts.
fn check_asr_recipe(recipe: Option<&VlmEvalRecipe>) -> Result<(), RecipeError> {
    let recipe = recipe.ok_or(RecipeError::AsrMissing)?;
    check_recipe(Some(recipe))?;
    let languages = recipe.languages.as_deref().unwrap_or_default();
    for (i, language) in languages.iter().enumerate() {
        if languages.iter().skip(i + 1).any(|later| later == language) {
            return Err(RecipeError::RepeatedLanguage(language.to_string()));
        }
    }
    match recipe.enable_thinking {
        Some(false) => Ok(()),
        other => Err(RecipeError::AsrThinking(pipette_plan_types::Thinking(
            other,
        ))),
    }
}

/// A vlm_eval entry's recipe: the rules every recipe keeps, and no
/// `languages`, which only an asr_eval recipe's card declares.
fn check_vlm_recipe(recipe: Option<&VlmEvalRecipe>) -> Result<(), RecipeError> {
    check_recipe(recipe)?;
    match recipe.and_then(|recipe| recipe.languages.as_ref()) {
        Some(_) => Err(RecipeError::VlmLanguages),
        None => Ok(()),
    }
}

/// The rules every catalog entry's recipe keeps: who it is for is part of
/// what the entry claims, and a reader must be able to check where each
/// setting came from.
fn check_recipe(recipe: Option<&VlmEvalRecipe>) -> Result<(), RecipeError> {
    let recipe = recipe.ok_or(RecipeError::Missing)?;
    if let Some(source) = recipe
        .sources
        .iter()
        .find(|source| !source.as_ref().starts_with("https://"))
    {
        return Err(RecipeError::NotHttps(source.to_string()));
    }
    for (i, line) in recipe.models.iter().enumerate() {
        if recipe
            .models
            .iter()
            .skip(i + 1)
            .any(|later| later.repo.same_as(&line.repo))
        {
            return Err(RecipeError::RepeatedModel(line.repo.to_string()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use anyhow::Context;
    use rstest::rstest;

    use pipette_plan_types::{
        EvalBenchmark, KnownAsrEvalTask, KnownAsrPromptProfile, KnownVlmPromptProfile, MinP,
        PositiveCount, PresencePenalty, RepeatPenalty, SamplingSeed, SamplingTemperature, SeedRule,
        TopK, TopP, VlMaxMemoryBenchmark, VlSpecDecodeBenchmark, VlThroughputBenchmark,
        VlmPromptProfile,
    };

    use crate::benchmark::*;
    use crate::types::Mint;

    /// Every shipped example parses through the loader a server runs at
    /// startup, so a malformed example is a server that will not boot.
    #[test]
    fn shipped_catalog_loads() -> anyhow::Result<()> {
        // `examples/` is workspace-level sample data, not this crate's.
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/benchmarks");
        let catalog = load_catalog(&dir)?;
        anyhow::ensure!(!catalog.is_empty(), "the shipped catalog is empty");
        // A submission declaring its params is compared against this projection.
        catalog.values().try_for_each(|b| {
            anyhow::ensure!(b.params().benchmark_type() == b.benchmark_type());
            Ok(())
        })
    }

    /// The public examples hold no vlm_eval entry: copied into a server's
    /// catalog by the quick start, each would be served to every client.
    #[test]
    fn the_public_examples_hold_no_org_private_entry() -> anyhow::Result<()> {
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/benchmarks");
        let catalog = load_catalog(&dir)?;
        let private: Vec<_> = catalog
            .iter()
            .filter(|(_, b)| b.def.as_vlm_eval().is_ok())
            .map(|(id, _)| id.to_string())
            .collect();
        anyhow::ensure!(
            private.is_empty(),
            "org-private entries in the public examples: {private:?}"
        );
        Ok(())
    }

    /// The public examples hold no asr_eval entry either: its corpus, every
    /// clip included, is served to every device that can read the entry.
    #[test]
    fn the_public_examples_hold_no_asr_eval_entry() -> anyhow::Result<()> {
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/benchmarks");
        let catalog = load_catalog(&dir)?;
        let mut private: Vec<_> = catalog
            .iter()
            .filter(|(_, b)| b.def.as_asr_eval().is_ok())
            .map(|(id, _)| id.to_string())
            .collect();
        private.sort();
        anyhow::ensure!(
            private.is_empty(),
            "asr_eval entries in the public examples: {private:?}"
        );
        Ok(())
    }

    /// A profile's recipe and pinned decoding as docs/benchmarks.md's asr_eval
    /// tables give them, for an organization's catalog: its id suffix, family,
    /// models (a repo, and the projector a line pins), the languages its card
    /// declares (`None`: it enumerates none), its public source and its knobs.
    struct AsrEvalRecipeRow {
        profile: KnownAsrPromptProfile,
        suffix: &'static str,
        family: &'static str,
        models: &'static [(&'static str, Option<&'static str>)],
        languages: Option<&'static [&'static str]>,
        source: &'static str,
        knobs: &'static str,
    }

    const ASR_EVAL_RECIPES: &[AsrEvalRecipeRow] = &[
        AsrEvalRecipeRow {
            profile: KnownAsrPromptProfile::Gemma4,
            suffix: "gemma4",
            family: "Gemma 4 E2B",
            models: &[
                ("ggml-org/gemma-4-E2B-it-GGUF", None),
                ("bartowski/google_gemma-4-E2B-it-GGUF", None),
            ],
            languages: None,
            source: "https://huggingface.co/google/gemma-4-E2B-it/blob/3e22461f65e89153144f8adb70e3b8c2cc9845a7/README.md",
            knobs: "parameter_max_tokens = 512\nparameter_temperature = 1.0\n\
                    parameter_top_p = 0.95\nparameter_top_k = 64\nparameter_seed = 0\n",
        },
        AsrEvalRecipeRow {
            profile: KnownAsrPromptProfile::Qwen25Omni,
            suffix: "qwen25_omni",
            family: "Qwen2.5-Omni 3B",
            models: &[
                ("ggml-org/Qwen2.5-Omni-3B-GGUF", None),
                ("Mungert/Qwen2.5-Omni-3B-GGUF", None),
            ],
            languages: Some(&["en", "zh", "yue", "fr"]),
            source: "https://huggingface.co/Qwen/Qwen2.5-Omni-3B/blob/f75b40e3da2003cdd6e1829b1f420ca70797c34e/README.md",
            knobs: "parameter_max_tokens = 256\nparameter_temperature = 0.0\n",
        },
        AsrEvalRecipeRow {
            profile: KnownAsrPromptProfile::Voxtral,
            suffix: "voxtral",
            family: "Voxtral Mini 3B",
            models: &[
                ("ggml-org/Voxtral-Mini-3B-2507-GGUF", None),
                ("bartowski/mistralai_Voxtral-Mini-3B-2507-GGUF", None),
            ],
            languages: Some(&["en", "es", "fr", "pt", "hi", "de", "nl", "it"]),
            source: "https://huggingface.co/mistralai/Voxtral-Mini-3B-2507/blob/3060fe34b35ba5d44202ce9ff3c097642914f8f3/README.md",
            knobs: "parameter_max_tokens = 500\nparameter_temperature = 0.0\n",
        },
        AsrEvalRecipeRow {
            profile: KnownAsrPromptProfile::MinicpmO,
            suffix: "minicpm_o_beam3",
            family: "MiniCPM-o 4.5",
            models: &[(
                "openbmb/MiniCPM-o-4_5-gguf",
                Some("audio/MiniCPM-o-4_5-audio-F16.gguf"),
            )],
            languages: Some(&["en", "zh"]),
            source: "https://huggingface.co/openbmb/MiniCPM-o-4_5/blob/d59117d19ddb8a28a62f1b5f4e3b34ac48d510a4/README.md",
            knobs: "parameter_max_tokens = 128\nparameter_num_beams = 3\n",
        },
        AsrEvalRecipeRow {
            profile: KnownAsrPromptProfile::Lfm25Audio,
            suffix: "lfm25_audio",
            family: "LFM2.5-Audio 1.5B",
            models: &[
                ("LiquidAI/LFM2.5-Audio-1.5B-GGUF", None),
                ("Mungert/LFM2.5-Audio-1.5B-GGUF", None),
            ],
            languages: Some(&["en"]),
            source: "https://huggingface.co/LiquidAI/LFM2.5-Audio-1.5B/blob/c362a0625dfe45aa588dce5f0ada28a7e5707628/README.md",
            knobs: "parameter_max_tokens = 256\nparameter_temperature = 0.0\n",
        },
        AsrEvalRecipeRow {
            profile: KnownAsrPromptProfile::Lfm3,
            suffix: "lfm3",
            family: "LFM3 (placeholder)",
            // A stand-in: LFM3 has no published repo.
            models: &[("org/lfm3-placeholder-gguf", None)],
            languages: None,
            source: "https://github.com/huggingface/open_asr_leaderboard",
            knobs: "parameter_max_tokens = 256\nparameter_temperature = 0.0\n",
        },
    ];

    /// An organization's whole asr_eval grid, built here as a deployment's
    /// catalog holds it: every task under every profile, each entry pinning
    /// its corpus (the testkit's tables) and carrying its profile's recipe.
    /// Ids are `asr_eval_<task without "asr_">_<suffix>`.
    fn write_asr_eval_grid(dir: &Path) -> anyhow::Result<()> {
        for row in ASR_EVAL_RECIPES {
            let profile = row.profile.as_ref();
            let mut recipe = format!("[recipe]\nfamily = \"{}\"\nmodels = [\n", row.family);
            for (repo, mmproj) in row.models {
                match mmproj {
                    Some(mmproj) => {
                        recipe += &format!("  {{ repo = \"{repo}\", mmproj = \"{mmproj}\" }},\n");
                    }
                    None => recipe += &format!("  {{ repo = \"{repo}\" }},\n"),
                }
            }
            recipe += &format!(
                "]\nenable_thinking = false\nsources = [\"{}\"]\n",
                row.source
            );
            if let Some(languages) = row.languages {
                let quoted: Vec<String> = languages.iter().map(|l| format!("\"{l}\"")).collect();
                recipe += &format!("languages = [{}]\n", quoted.join(", "));
            }
            for task in KnownAsrEvalTask::ALL {
                let task = task.as_ref();
                let tables = crate::dataset::asr_eval_catalog_tables(task, "2026.10.1", profile);
                let corpus = tables
                    .split("[recipe]")
                    .next()
                    .context("the testkit's tables open with the corpus pin")?;
                let id = format!(
                    "asr_eval_{}_{}",
                    task.trim_start_matches("asr_"),
                    row.suffix
                );
                std::fs::write(
                    dir.join(format!("{id}.toml")),
                    format!(
                        "benchmark_type = \"asr_eval\"\nparameter_asr_task = \"{task}\"\n\
                         parameter_dataset_version = \"2026.10.1\"\nparameter_sample_count = 2\n\
                         parameter_prompt_profile = \"{profile}\"\n{}{corpus}{recipe}",
                        row.knobs
                    ),
                )?;
            }
        }
        Ok(())
    }

    /// The grid loads through a server's loader: every profile runs all 16
    /// tasks under one recipe with thinking off, 96 entries, and no two
    /// entries for a task and dataset version cover the same repo, so
    /// ingestion never meets an ambiguous cell from it.
    #[test]
    fn every_asr_eval_profile_runs_every_task_under_one_recipe() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        write_asr_eval_grid(dir.path())?;
        let catalog = load_catalog(dir.path())?;
        let sorted: std::collections::BTreeMap<_, _> = catalog.iter().collect();
        let mut owner: std::collections::BTreeMap<(String, String, String), String> =
            std::collections::BTreeMap::new();
        let mut families: std::collections::BTreeMap<
            String,
            (
                std::collections::BTreeSet<String>,
                std::collections::BTreeSet<String>,
            ),
        > = std::collections::BTreeMap::new();
        for (id, benchmark) in &sorted {
            let asr = benchmark.def.as_asr_eval()?;
            let recipe = asr
                .recipe
                .as_ref()
                .context("an asr_eval entry carries a recipe")?;
            anyhow::ensure!(recipe.enable_thinking == Some(false), "{id} thinks");
            for line in recipe.models.iter() {
                let key = (
                    asr.parameter_asr_task.as_str().to_owned(),
                    asr.parameter_dataset_version.clone(),
                    line.repo.to_string().to_ascii_lowercase(),
                );
                if let Some(other) = owner.insert(key, id.to_string()) {
                    anyhow::bail!("{} is covered by {other} and {id}", line.repo);
                }
            }
            let (tasks, recipes) = families
                .entry(asr.parameter_prompt_profile.as_str().to_owned())
                .or_default();
            tasks.insert(asr.parameter_asr_task.as_str().to_owned());
            recipes.insert(serde_json::to_string(recipe)?);
        }
        anyhow::ensure!(sorted.len() == 96, "the grid has {} entries", sorted.len());
        anyhow::ensure!(
            families.len() == KnownAsrPromptProfile::ALL.len(),
            "{:?}",
            families.keys()
        );
        for (profile, (tasks, recipes)) in &families {
            anyhow::ensure!(
                tasks.len() == KnownAsrEvalTask::ALL.len(),
                "{profile} runs {} tasks: {tasks:?}",
                tasks.len()
            );
            anyhow::ensure!(
                recipes.len() == 1,
                "{profile} has {} recipes",
                recipes.len()
            );
        }
        Ok(())
    }

    /// The grid marks exactly the cells docs/benchmarks.md lists as outside
    /// their card's declared languages, and only Gemma 4's and LFM3's (whose
    /// recipes enumerate none) are unmarked either way.
    #[test]
    fn asr_eval_entries_mark_the_documented_cells() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        write_asr_eval_grid(dir.path())?;
        let catalog = load_catalog(dir.path())?;
        let mut outside = std::collections::BTreeSet::new();
        let mut unknown = std::collections::BTreeSet::new();
        let sorted: std::collections::BTreeMap<_, _> = catalog.iter().collect();
        for (id, benchmark) in sorted {
            let asr = benchmark.def.as_asr_eval()?;
            match asr.within_declared_languages() {
                Some(true) => {}
                Some(false) => {
                    outside.insert(id.as_str().to_owned());
                }
                None => {
                    unknown.insert(asr.parameter_prompt_profile.as_str().to_owned());
                }
            }
        }
        let fleurs = |locales: &[&str], suffix: &str| -> Vec<String> {
            locales
                .iter()
                .map(|l| format!("asr_eval_fleurs_{l}_{suffix}"))
                .collect()
        };
        let six = ["es_419", "pt_br", "de_de", "fr_fr", "it_it", "nl_nl"];
        let expected: std::collections::BTreeSet<String> = fleurs(&six, "lfm25_audio")
            .into_iter()
            .chain(fleurs(&six, "minicpm_o_beam3"))
            .chain(fleurs(
                &["es_419", "pt_br", "de_de", "it_it", "nl_nl"],
                "qwen25_omni",
            ))
            .collect();
        assert_eq!(outside, expected);
        assert_eq!(
            unknown,
            ["gemma4", "lfm3"].map(str::to_owned).into_iter().collect()
        );
        Ok(())
    }

    #[test]
    fn params_project_the_served_definition() -> anyhow::Result<()> {
        let b = Benchmark::from_toml(
            "decode_throughput_256_100_bs4",
            "benchmark_type = \"decode_throughput\"\n\
             parameter_prefill_tokens = 256\n\
             parameter_decode_tokens = 100\n\
             parameter_batch_size = 4",
        )?;
        assert_eq!(
            serde_json::to_value(b.params())?,
            serde_json::json!({
                "benchmark_type": "decode_throughput",
                "prefill_tokens": 256,
                "decode_tokens": 100,
                "batch_size": 4,
            })
        );
        Ok(())
    }

    const VL_CORPUS_DECODE_TOML: &str = r#"
benchmark_type = "vl_spec_decode"
parameter_draft_length = 8
parameter_max_tokens = 2048

[dataset]
format  = "vl-spec-decode-corpus/1"
adapter = "mmspec@1"
digest  = "1111111111111111111111111111111111111111111111111111111111111111"
bytes   = 1024
subset  = "GQA"

[dataset.upstream]
source   = "git"
url      = "github.com/acme/corpus"
revision = "0123456789abcdef0123456789abcdef01234567"
subpath  = "dataset/test"
identity = "2923603292c7dd463ad01a119f57965817778fc3"
digest   = "2222222222222222222222222222222222222222222222222222222222222222"
"#;

    /// A bogus key at the top level of an entry. `[dataset]` is the shared
    /// client type, which tolerates unknown keys by design.
    #[rstest]
    #[case::top_level("parameter_max_tokens = 2048")]
    fn test_unknown_field_is_rejected(#[case] anchor: &str) {
        let toml = VL_CORPUS_DECODE_TOML.replace(anchor, &format!("{anchor}\nnot_a_field = 1"));
        assert!(
            Benchmark::from_toml("vl_spec_decode_corpus_test_k8", &toml).is_err(),
            "an unknown key after {anchor:?} must fail to load"
        );
    }

    /// `Dataset::check` runs at load, so a broken entry is a server that will
    /// not boot rather than one that builds a corpus nobody asked for.
    #[test]
    fn test_vl_spec_decode_with_a_blank_subset_fails_to_load() {
        let toml = VL_CORPUS_DECODE_TOML.replace("subset  = \"GQA\"", "subset  = \"\"");
        assert!(Benchmark::from_toml("vl_spec_decode_corpus_test_k8", &toml).is_err());
    }

    #[test]
    fn test_parse_vl_spec_decode() -> anyhow::Result<()> {
        let benchmark =
            Benchmark::from_toml("vl_spec_decode_corpus_test_k8", VL_CORPUS_DECODE_TOML)?;
        assert_eq!(benchmark.benchmark_type(), BenchmarkType::VlSpecDecode);
        let BenchmarkDefinition::VlSpecDecode(VlSpecDecodeBenchmark {
            parameter_draft_length,
            ..
        }) = &benchmark.def
        else {
            anyhow::bail!("expected a vl_spec_decode definition");
        };
        assert_eq!(parameter_draft_length.into_inner(), 8);
        let dataset = benchmark
            .dataset()
            .ok_or_else(|| anyhow::anyhow!("a vl_spec_decode benchmark must carry a dataset"))?;
        assert_eq!(dataset.bytes, 1024);
        Ok(())
    }

    /// Absent reaches the wire as `0.0`, not as a gap a client fills.
    #[rstest]
    #[case::default("", Ok(0.0))]
    #[case::greedy("parameter_temperature = 0.0", Ok(0.0))]
    #[case::sampled("parameter_temperature = 0.8", Ok(0.8))]
    #[case::hottest("parameter_temperature = 2.0", Ok(2.0))]
    #[case::too_hot("parameter_temperature = 2.1", Err("Temperature is too big"))]
    #[case::negative("parameter_temperature = -0.1", Err("Temperature is too small"))]
    fn test_vl_spec_decode_temperature(
        #[case] line: &str,
        #[case] expected: Result<f64, &str>,
    ) -> anyhow::Result<()> {
        let toml = VL_CORPUS_DECODE_TOML.replace(
            "parameter_max_tokens = 2048",
            &format!("parameter_max_tokens = 2048\n{line}"),
        );
        let loaded = Benchmark::from_toml("vl_spec_decode_corpus_test_k8_t1", &toml);
        match expected {
            Ok(value) => {
                let benchmark = loaded?;
                let BenchmarkDefinition::VlSpecDecode(VlSpecDecodeBenchmark {
                    parameter_temperature,
                    ..
                }) = &benchmark.def
                else {
                    anyhow::bail!("expected a vl_spec_decode definition");
                };
                assert_eq!(parameter_temperature.as_f64(), value);
                assert_eq!(
                    serde_json::to_value(&benchmark)?["parameter_temperature"],
                    serde_json::json!(value),
                    "{line:?} on the wire"
                );
            }
            Err(names) => {
                let err = format!("{:#}", loaded.err().context("must be rejected")?);
                assert!(err.contains(names), "{line} rejected as {err:?}");
            }
        }
        Ok(())
    }

    /// Absent is the reference procedure, one request in flight; below one is
    /// a catalog error, not a run. The same rule on every type that takes it.
    #[rstest]
    #[case::default("", Some(1))]
    #[case::explicit("parameter_batch_size = 8", Some(8))]
    #[case::zero("parameter_batch_size = 0", None)]
    fn test_batch_size(
        #[case] line: &str,
        #[case] expected: Option<u32>,
        #[values(
            ("vl_spec_decode_corpus_test_k8", VL_CORPUS_DECODE_TOML),
            ("prefill_throughput_256", PREFILL_THROUGHPUT_TOML),
            ("decode_throughput_512_100", DECODE_THROUGHPUT_TOML),
            ("end_to_end_latency_256_256", E2E_LATENCY_TOML)
        )]
        fixture: (&str, &str),
    ) {
        let (id, base) = fixture;
        // The VL fixture ends inside `[dataset.upstream]`, so the line goes
        // after its last top-level key rather than at the end.
        let toml = if id.starts_with("vl_") {
            base.replace(
                "parameter_max_tokens = 2048",
                &format!("parameter_max_tokens = 2048\n{line}"),
            )
        } else {
            format!("{base}\n{line}")
        };
        let got = Benchmark::from_toml(id, &toml)
            .ok()
            .and_then(|b| b.parameter_batch_size())
            .map(BatchSize::as_u32);
        assert_eq!(got, expected);
    }

    const PREFILL_THROUGHPUT_TOML: &str = r#"
benchmark_type = "prefill_throughput"
parameter_prefill_tokens = 256
"#;

    const ASR_THROUGHPUT_TOML: &str = r#"
benchmark_type = "asr_throughput"
parameter_audio_duration_ms = 29900
parameter_audio_sample_rate_hz = 16000
parameter_audio_channels = 1
parameter_text_tokens = 1024
parameter_decode_tokens = 256
"#;

    const ASR_MAX_MEMORY_TOML: &str = r#"
benchmark_type = "asr_max_memory"
parameter_audio_duration_ms = 2500
parameter_audio_sample_rate_hz = 16000
parameter_audio_channels = 1
parameter_text_tokens = 0
parameter_decode_tokens = 256
"#;

    const VL_MAX_MEMORY_TOML: &str = r#"
benchmark_type = "vl_max_memory"
parameter_image_width = 224
parameter_image_height = 224
parameter_text_tokens = 0
parameter_num_images = 1
"#;

    const VL_MAX_MEMORY_DECODE_TOML: &str = r#"
benchmark_type = "vl_max_memory"
parameter_image_width = 256
parameter_image_height = 256
parameter_text_tokens = 1024
parameter_decode_tokens = 256
parameter_num_images = 5
"#;

    /// The file a store writes is the file a loader reads: every fixture
    /// survives `to_toml` and comes back equal, defaults included, which is
    /// what lets a `PUT` body become a catalog entry without a second parser.
    #[rstest]
    #[case::vl_spec_decode(VL_CORPUS_DECODE_TOML)]
    #[case::prefill(PREFILL_THROUGHPUT_TOML)]
    #[case::decode(DECODE_THROUGHPUT_TOML)]
    #[case::e2e(E2E_LATENCY_TOML)]
    #[case::eval(EVAL_TOML)]
    #[case::vl_max_memory(VL_MAX_MEMORY_TOML)]
    #[case::asr_throughput(ASR_THROUGHPUT_TOML)]
    #[case::asr_max_memory(ASR_MAX_MEMORY_TOML)]
    #[case::vl_max_memory_decode(VL_MAX_MEMORY_DECODE_TOML)]
    #[case::vlm_eval(&vlm_eval("", None))]
    #[case::vlm_eval_penalties(&vlm_eval(
        "parameter_presence_penalty = -0.5\nparameter_repeat_penalty = 1.05",
        None
    ))]
    #[case::vlm_eval_prompt_profile(&vlm_eval(
        "parameter_prompt_profile = \"lfm2_5_vl_3b\"",
        Some("lfm2_5_vl_3b")
    ))]
    #[case::asr_eval(&asr_eval_minicpm())]
    #[case::asr_eval_sampled(&format!(
        "benchmark_type = \"asr_eval\"\nparameter_asr_task = \"asr_fleurs_de_de\"\n\
         parameter_dataset_version = \"2026.10.1\"\nparameter_sample_count = 862\n\
         parameter_prompt_profile = \"gemma4\"\nparameter_max_tokens = 512\n\
         parameter_temperature = 1.0\nparameter_top_p = 0.95\nparameter_top_k = 64\n\
         parameter_seed = 0\n{}",
        crate::dataset::asr_eval_catalog_tables("asr_fleurs_de_de", "2026.10.1", "gemma4")
    ))]
    fn test_toml_round_trips(#[case] toml: &str) -> anyhow::Result<()> {
        let parsed = Benchmark::from_toml("entry", toml)?;
        let again = Benchmark::from_toml("entry", &parsed.to_toml()?)?;
        assert_eq!(again, parsed);
        Ok(())
    }

    /// `from_def` is `from_toml` past the parse, so a definition that a file
    /// could not carry cannot be written as one either.
    #[test]
    fn test_from_def_runs_the_load_checks() -> anyhow::Result<()> {
        let mut def = Benchmark::from_toml("entry", VL_CORPUS_DECODE_TOML)?.def;
        if let BenchmarkDefinition::VlSpecDecode(b) = &mut def {
            b.dataset.subset = Some("  ".into());
        }
        let err = Benchmark::from_def(def)
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(err.starts_with("benchmark entry: "), "{err}");
        Ok(())
    }

    /// Every profile loads on every FLEURS locale, LFM2.5-Audio's included
    /// though its card lists English alone: a cell outside a card's declared
    /// languages is marked, never refused.
    #[rstest]
    #[case::fleurs_english("lfm25_audio", "asr_fleurs_en_us", 647)]
    #[case::fleurs_german("lfm25_audio", "asr_fleurs_de_de", 862)]
    #[case::english("lfm25_audio", "asr_esb_librispeech_test_clean", 2620)]
    #[case::another_profile("lfm3", "asr_fleurs_en_us", 647)]
    fn test_lfm25_audio_on_fleurs_loads(
        #[case] profile: &str,
        #[case] task: &str,
        #[case] samples: u32,
    ) -> anyhow::Result<()> {
        let toml = format!(
            "benchmark_type = \"asr_eval\"\nparameter_asr_task = \"{task}\"\n\
             parameter_dataset_version = \"2026.10.1\"\nparameter_sample_count = {samples}\n\
             parameter_prompt_profile = \"{profile}\"\nparameter_max_tokens = 256\n{}",
            crate::dataset::asr_eval_catalog_tables(task, "2026.10.1", profile)
        );
        Benchmark::from_toml("entry", &toml)?;
        Ok(())
    }

    /// An asr_eval recipe may list its card's languages, each once, and keeps
    /// them through a round trip; a vlm_eval recipe may not list any (the
    /// negative controls: the same lists on the other type, or repeated).
    #[rstest]
    #[case::asr_listed(true, "languages = [\"en\", \"zh\"]\n", None)]
    #[case::asr_repeated(
        true,
        "languages = [\"en\", \"en\"]\n",
        Some("lists language en twice")
    )]
    #[case::vlm_listed(
        false,
        "languages = [\"en\"]\n",
        Some("a vlm_eval recipe lists no languages")
    )]
    #[case::vlm_none(false, "", None)]
    fn test_recipe_languages(
        #[case] asr: bool,
        #[case] line: &str,
        #[case] refusal: Option<&str>,
    ) -> anyhow::Result<()> {
        let toml = if asr {
            asr_eval_minicpm()
        } else {
            vlm_eval("", None)
        };
        let toml = toml.replacen("[recipe]\n", &format!("[recipe]\n{line}"), 1);
        anyhow::ensure!(line.is_empty() || toml.contains(line), "{toml}");
        match (Benchmark::from_toml("entry", &toml), refusal) {
            (Ok(parsed), None) => {
                let again = Benchmark::from_toml("entry", &parsed.to_toml()?)?;
                assert_eq!(again, parsed);
            }
            (Err(e), Some(needle)) => assert!(format!("{e:#}").contains(needle), "{e:#}"),
            (got, want) => anyhow::bail!("{want:?}: got {got:?}"),
        }
        Ok(())
    }

    /// A count the client would refuse, or one the Int32 warehouse column cannot
    /// hold, is a load error, whichever type carries it.
    #[rstest]
    #[case::zero_max_tokens(EVAL_TOML, "parameter_max_tokens = 8192", "parameter_max_tokens = 0")]
    #[case::negative_max_tokens(
        EVAL_TOML,
        "parameter_max_tokens = 8192",
        "parameter_max_tokens = -1"
    )]
    #[case::huge_max_tokens(
        EVAL_TOML,
        "parameter_max_tokens = 8192",
        "parameter_max_tokens = 2147483648"
    )]
    #[case::negative_prefill(
        PREFILL_THROUGHPUT_TOML,
        "parameter_prefill_tokens = 256",
        "parameter_prefill_tokens = -1"
    )]
    #[case::huge_prefill(
        PREFILL_THROUGHPUT_TOML,
        "parameter_prefill_tokens = 256",
        "parameter_prefill_tokens = 2147483648"
    )]
    #[case::negative_text_tokens(
        VL_MAX_MEMORY_TOML,
        "parameter_text_tokens = 0",
        "parameter_text_tokens = -1"
    )]
    #[case::negative_asr_text_tokens(
        ASR_THROUGHPUT_TOML,
        "parameter_text_tokens = 1024",
        "parameter_text_tokens = -1"
    )]
    #[case::huge_asr_text_tokens(
        ASR_MAX_MEMORY_TOML,
        "parameter_text_tokens = 0",
        "parameter_text_tokens = 2147483648"
    )]
    #[case::negative_num_images(
        VL_MAX_MEMORY_TOML,
        "parameter_num_images = 1",
        "parameter_num_images = -1"
    )]
    #[case::huge_num_images(
        VL_MAX_MEMORY_TOML,
        "parameter_num_images = 1",
        "parameter_num_images = 4294967295"
    )]
    #[case::zero_draft_length(
        VL_CORPUS_DECODE_TOML,
        "parameter_draft_length = 8",
        "parameter_draft_length = 0"
    )]
    #[case::zero_batch(
        PREFILL_THROUGHPUT_TOML,
        "parameter_prefill_tokens = 256",
        "parameter_prefill_tokens = 256\nparameter_batch_size = 0"
    )]
    #[case::negative_batch(
        PREFILL_THROUGHPUT_TOML,
        "parameter_prefill_tokens = 256",
        "parameter_prefill_tokens = 256\nparameter_batch_size = -1"
    )]
    fn test_a_count_out_of_range_fails_to_load(
        #[case] base: &str,
        #[case] line: &str,
        #[case] replacement: &str,
        #[values("toml", "json")] form: &str,
    ) -> anyhow::Result<()> {
        let toml = base.replace(line, replacement);
        anyhow::ensure!(toml != base, "{line:?} is not in the fixture");
        let parsed = match form {
            "toml" => Benchmark::from_toml("entry", &toml),
            _ => from_json(&serde_json::to_string(&toml::from_str::<Table>(&toml)?)?),
        };
        assert!(parsed.is_err(), "{replacement} must be refused as {form}");
        Ok(())
    }

    /// A `PUT` body as the route reads it.
    fn from_json(body: &str) -> anyhow::Result<Benchmark> {
        Benchmark::from_json(BenchmarkId::try_new("entry")?, serde_json::from_str(body)?)
    }

    /// A body is refused naming the key at fault: one the client's type does not
    /// name, one the path or the server owns, or one given twice at any depth.
    #[rstest]
    #[case::unknown_key(
        r#"{"benchmark_type": "prefill_throughput", "parameter_prefill_tokens": 256, "nope": 1}"#,
        "unknown field `nope`"
    )]
    #[case::id_in_the_body(
        r#"{"benchmark_id": "x", "benchmark_type": "prefill_throughput", "parameter_prefill_tokens": 256}"#,
        "unknown field `benchmark_id`"
    )]
    #[case::samples(
        r#"{"benchmark_type": "eval", "parameter_eval_id": "e", "parameter_dataset_name": "d", "parameter_max_tokens": 16, "samples": []}"#,
        "unknown field `samples`"
    )]
    #[case::no_type(
        r#"{"parameter_prefill_tokens": 256}"#,
        "missing field `benchmark_type`"
    )]
    #[case::unknown_type(r#"{"benchmark_type": "nope"}"#, "unknown variant `nope`")]
    #[case::missing_parameter(
        r#"{"benchmark_type": "prefill_throughput"}"#,
        "missing field `parameter_prefill_tokens`"
    )]
    #[case::repeated_key(
        r#"{"benchmark_type": "prefill_throughput", "parameter_prefill_tokens": 1, "parameter_prefill_tokens": 2}"#,
        "duplicate field `parameter_prefill_tokens`"
    )]
    #[case::repeated_nested_key(
        r#"{"benchmark_type": "prefill_throughput", "parameter_prefill_tokens": 1, "x": {"a": 1, "a": 2}}"#,
        "duplicate field `a`"
    )]
    fn test_a_json_body_is_refused_naming_the_key(#[case] body: &str, #[case] refusal: &str) {
        let err = from_json(body)
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default();
        assert!(err.contains(refusal), "{body} refused as {err:?}");
    }

    /// `null` is no value, as in a definition the client serializes: an
    /// optional knob sent as one is unpinned.
    #[test]
    fn test_a_json_null_is_an_absent_key() -> anyhow::Result<()> {
        let entry = from_json(
            r#"{"benchmark_type": "eval", "parameter_eval_id": "e", "parameter_dataset_name": "d", "parameter_max_tokens": 16, "parameter_temperature": null}"#,
        )?;
        assert_eq!(entry.def.as_eval()?.parameter_temperature, None);
        Ok(())
    }

    /// The corpus url is the server's to stamp, so one a definition carries is
    /// neither kept nor written back: a file, or a body copied from the
    /// catalog's entry, cannot choose where a device downloads from.
    #[test]
    fn test_from_def_drops_a_corpus_url() -> anyhow::Result<()> {
        let toml = VL_CORPUS_DECODE_TOML.replace(
            "subset  = \"GQA\"",
            "subset  = \"GQA\"\nurl     = \"https://evil.example/x.tar\"",
        );
        let benchmark = Benchmark::from_toml("vl_spec_decode_corpus_test_k8", &toml)?;
        let dataset = benchmark
            .dataset()
            .ok_or_else(|| anyhow::anyhow!("a vl_spec_decode benchmark must carry a dataset"))?;
        assert_eq!(dataset.url, None);
        assert!(!benchmark.to_toml()?.contains("evil.example"));
        Ok(())
    }

    /// On the wire a private entry names its organization and a public one
    /// carries no key at all, so an older client's shape is unchanged. The
    /// file never carries it either way: the path does.
    #[test]
    fn test_org_id_is_on_the_wire_and_not_in_the_file() -> anyhow::Result<()> {
        let public = Benchmark::from_toml("prefill_throughput_256", PREFILL_THROUGHPUT_TOML)?;
        assert!(serde_json::to_value(&public)?.get("org_id").is_none());
        let acme = OrgId::mint();
        let private = public.clone().private_to(acme.clone());
        assert_eq!(serde_json::to_value(&private)?["org_id"], acme.as_str());
        assert!(!private.to_toml()?.contains("org_id"));
        assert!(
            Benchmark::from_toml(
                "prefill_throughput_256",
                &format!("org_id = \"{acme}\"\n{PREFILL_THROUGHPUT_TOML}")
            )
            .is_err(),
            "the key is not part of a definition"
        );
        Ok(())
    }

    #[test]
    fn test_parse_prefill_throughput() -> anyhow::Result<()> {
        let def = Benchmark::from_toml("entry", PREFILL_THROUGHPUT_TOML)?.def;
        assert!(matches!(def, BenchmarkDefinition::PrefillThroughput(_)));
        Ok(())
    }

    const DECODE_THROUGHPUT_TOML: &str = r#"
benchmark_type = "decode_throughput"
parameter_prefill_tokens = 512
parameter_decode_tokens = 100
"#;

    #[test]
    fn test_parse_decode_throughput() -> anyhow::Result<()> {
        let def = Benchmark::from_toml("entry", DECODE_THROUGHPUT_TOML)?.def;
        assert!(matches!(def, BenchmarkDefinition::DecodeThroughput(_)));
        Ok(())
    }

    const E2E_LATENCY_TOML: &str = r#"
benchmark_type = "end_to_end_latency"
parameter_prefill_tokens = 256
parameter_decode_tokens = 256
"#;

    #[test]
    fn test_parse_end_to_end_latency() -> anyhow::Result<()> {
        let def = Benchmark::from_toml("entry", E2E_LATENCY_TOML)?.def;
        assert!(matches!(def, BenchmarkDefinition::EndToEndLatency(_)));
        Ok(())
    }

    #[test]
    fn test_parse_max_memory_usage() -> anyhow::Result<()> {
        let toml = r#"
benchmark_type = "max_memory_usage"
parameter_prefill_tokens = 256
"#;
        let def = Benchmark::from_toml("entry", toml)?.def;
        assert!(matches!(def, BenchmarkDefinition::MaxMemoryUsage(_)));
        Ok(())
    }

    #[test]
    fn test_parse_eval() -> anyhow::Result<()> {
        let toml = r#"
benchmark_type = "eval"
parameter_eval_id = "mmlu_pro"
parameter_dataset_name = "edge_2026.03.2"
parameter_max_tokens = 1024
parameter_mcq_choices = ["A", "B", "C", "D"]
"#;
        let def = Benchmark::from_toml("entry", toml)?.def;
        let BenchmarkDefinition::Eval(EvalBenchmark {
            parameter_mcq_choices,
            ..
        }) = def
        else {
            anyhow::bail!("expected Eval");
        };
        let choices = parameter_mcq_choices.context("expected mcq_choices")?;
        assert_eq!(choices, vec!["A", "B", "C", "D"]);
        Ok(())
    }

    #[test]
    fn test_parse_eval_without_mcq() -> anyhow::Result<()> {
        let toml = r#"
benchmark_type = "eval"
parameter_eval_id = "ifeval"
parameter_dataset_name = "edge_2026.03.2"
parameter_max_tokens = 4096
"#;
        let def = Benchmark::from_toml("entry", toml)?.def;
        let BenchmarkDefinition::Eval(EvalBenchmark {
            parameter_mcq_choices,
            ..
        }) = def
        else {
            anyhow::bail!("expected Eval");
        };
        assert!(parameter_mcq_choices.is_none());
        Ok(())
    }

    const EVAL_TOML: &str = r#"
benchmark_type = "eval"
parameter_eval_id = "math_500"
parameter_dataset_name = "2026.06.1"
parameter_max_tokens = 8192
"#;

    #[test]
    fn eval_sampling_is_absent_until_pinned() -> anyhow::Result<()> {
        let bare = Benchmark::from_toml("eval_math_500_2026.06.1", EVAL_TOML)?;
        let BenchmarkDefinition::Eval(EvalBenchmark {
            parameter_temperature,
            parameter_top_p,
            parameter_top_k,
            parameter_min_p,
            parameter_seed,
            ..
        }) = &bare.def
        else {
            anyhow::bail!("expected an eval definition");
        };
        assert_eq!(*parameter_temperature, None);
        assert_eq!(*parameter_top_p, None);
        assert_eq!(*parameter_top_k, None);
        assert_eq!(*parameter_min_p, None);
        assert_eq!(*parameter_seed, None);

        let pinned = Benchmark::from_toml(
            "eval_math_500_2026.06.1_t1",
            &format!(
                "{EVAL_TOML}\nparameter_temperature = 1.0\nparameter_top_p = 0.95\n\
                 parameter_top_k = 64\nparameter_min_p = 0.0\nparameter_seed = 7\n"
            ),
        )?;
        let BenchmarkDefinition::Eval(EvalBenchmark {
            parameter_temperature,
            parameter_top_p,
            parameter_top_k,
            parameter_min_p,
            parameter_seed,
            ..
        }) = &pinned.def
        else {
            anyhow::bail!("expected an eval definition");
        };
        assert_eq!(
            *parameter_temperature,
            Some(SamplingTemperature::try_new(1.0)?)
        );
        assert_eq!(*parameter_top_p, Some(TopP::try_new(0.95)?));
        assert_eq!(*parameter_top_k, Some(TopK::try_new(64)?));
        assert_eq!(*parameter_min_p, Some(MinP::try_new(0.0)?));
        assert_eq!(*parameter_seed, Some(SamplingSeed::try_new(7)?));
        Ok(())
    }

    #[rstest]
    #[case::temperature_high("parameter_temperature = 2.1", "Temperature is too big")]
    #[case::temperature_negative("parameter_temperature = -0.1", "Temperature is too small")]
    #[case::temperature_nan("parameter_temperature = nan", "Temperature is not finite")]
    #[case::top_p_high("parameter_top_p = 1.5", "TopP is too big")]
    #[case::min_p_high("parameter_min_p = 1.5", "MinP is too big")]
    #[case::top_k_negative("parameter_top_k = -1", "TopK is too small")]
    #[case::seed_negative("parameter_seed = -1", "Seed is too small")]
    // A text eval keeps the repeat offset: it takes no rule to change it.
    #[case::seed_rule(
        "parameter_seed = 0\nparameter_seed_rule = \"per_sample_sha256\"",
        "unknown field `parameter_seed_rule`"
    )]
    fn eval_sampling_out_of_range_is_rejected(
        #[case] line: &str,
        #[case] names: &str,
    ) -> anyhow::Result<()> {
        let toml = format!("{EVAL_TOML}\n{line}\n");
        let err = Benchmark::from_toml("eval_math_500_2026.06.1_bad", &toml)
            .err()
            .with_context(|| format!("{line} must be rejected"))?;
        // The entry id plus this message is the whole diagnostic an operator
        // editing the file gets.
        let err = format!("{err:#}");
        assert!(err.contains(names), "{line} rejected as {err:?}");
        Ok(())
    }

    /// An edge is a pin like any other: greedy and a disabled cut must land as
    /// values, or no plan can ask for them.
    #[rstest]
    #[case::greedy("parameter_temperature = 0.0", |d: &BenchmarkDefinition| matches!(d, BenchmarkDefinition::Eval(EvalBenchmark { parameter_temperature, .. }) if *parameter_temperature == SamplingTemperature::try_new(0.0).ok()))]
    #[case::hottest("parameter_temperature = 2.0", |d: &BenchmarkDefinition| matches!(d, BenchmarkDefinition::Eval(EvalBenchmark { parameter_temperature, .. }) if *parameter_temperature == SamplingTemperature::try_new(2.0).ok()))]
    #[case::top_p_open("parameter_top_p = 1.0", |d: &BenchmarkDefinition| matches!(d, BenchmarkDefinition::Eval(EvalBenchmark { parameter_top_p, .. }) if *parameter_top_p == TopP::try_new(1.0).ok()))]
    #[case::min_p_off("parameter_min_p = 0.0", |d: &BenchmarkDefinition| matches!(d, BenchmarkDefinition::Eval(EvalBenchmark { parameter_min_p, .. }) if *parameter_min_p == MinP::try_new(0.0).ok()))]
    #[case::top_k_off("parameter_top_k = 0", |d: &BenchmarkDefinition| matches!(d, BenchmarkDefinition::Eval(EvalBenchmark { parameter_top_k, .. }) if *parameter_top_k == TopK::try_new(0).ok()))]
    #[case::seed_zero("parameter_seed = 0", |d: &BenchmarkDefinition| matches!(d, BenchmarkDefinition::Eval(EvalBenchmark { parameter_seed, .. }) if *parameter_seed == SamplingSeed::try_new(0).ok()))]
    fn eval_sampling_edges_land_as_values(
        #[case] line: &str,
        #[case] pinned: fn(&BenchmarkDefinition) -> bool,
    ) -> anyhow::Result<()> {
        let entry = Benchmark::from_toml(
            "eval_math_500_2026.06.1_edge",
            &format!("{EVAL_TOML}\n{line}\n"),
        )?;
        assert!(pinned(&entry.def), "{line} must land as a value");
        Ok(())
    }

    /// The API serializes the entry itself, so an unpinned eval must not grow
    /// null keys in its response.
    #[test]
    fn eval_sampling_is_elided_from_the_wire_when_unpinned() -> anyhow::Result<()> {
        let bare = Benchmark::from_toml("eval_math_500_2026.06.1", EVAL_TOML)?;
        let value = serde_json::to_value(&bare)?;
        [
            "parameter_max_context",
            "parameter_mcq_choices",
            "parameter_temperature",
            "parameter_top_p",
            "parameter_top_k",
            "parameter_min_p",
            "parameter_seed",
        ]
        .iter()
        .for_each(|knob| {
            assert!(value.get(knob).is_none(), "{knob} must not reach the wire");
        });

        let pinned = Benchmark::from_toml(
            "eval_math_500_2026.06.1_t0",
            &format!("{EVAL_TOML}\nparameter_temperature = 0.0\n"),
        )?;
        assert_eq!(
            serde_json::to_value(&pinned)?["parameter_temperature"],
            serde_json::json!(0.0)
        );
        Ok(())
    }

    const ASR_EVAL_MINICPM_TOML: &str = r#"
benchmark_type = "asr_eval"
parameter_asr_task = "asr_esb_librispeech_test_clean"
parameter_dataset_version = "2026.10.1"
parameter_sample_count = 2620
parameter_prompt_profile = "minicpm_o"
parameter_max_tokens = 128
parameter_num_beams = 3
parameter_beam_repetition_penalty = 1.2
parameter_beam_length_penalty = 1.0
"#;

    /// [`ASR_EVAL_MINICPM_TOML`] with `extra` top-level lines, and the tables a
    /// catalog entry carries: its pin and its recipe.
    fn asr_eval_minicpm_with(extra: &str) -> String {
        format!(
            "{ASR_EVAL_MINICPM_TOML}{extra}{}",
            crate::dataset::asr_eval_catalog_tables(
                "asr_esb_librispeech_test_clean",
                "2026.10.1",
                "minicpm_o"
            )
        )
    }

    fn asr_eval_minicpm() -> String {
        asr_eval_minicpm_with("")
    }

    /// An asr_eval entry loads strictly: it holds no samples (they are its
    /// corpus), and its decoding must be coherent.
    #[rstest]
    #[case::samples("samples = []\n", "unknown field `samples`")]
    #[case::sampling_beside_beams("parameter_temperature = 0.7\n", "samples nothing")]
    #[case::reasoning_is_no_parameter("parameter_enable_thinking = false\n", "unknown field")]
    fn a_bad_asr_eval_entry_is_refused(#[case] line: &str, #[case] expected: &str) {
        let err = Benchmark::from_toml("asr_eval_bad", &asr_eval_minicpm_with(line))
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default();
        assert!(err.contains(expected), "{err}");
    }

    #[test]
    fn an_asr_eval_entry_loads_typed() -> anyhow::Result<()> {
        let parsed = Benchmark::from_toml(
            "asr_eval_esb_librispeech_test_clean_2026.10.1_minicpm_o_beam3",
            &asr_eval_minicpm(),
        )?;
        let asr = parsed.def.as_asr_eval()?;
        assert!(asr.beam_search());
        assert_eq!(parsed.benchmark_type(), BenchmarkType::AsrEval);
        let dataset = parsed.dataset().context("an asr_eval serves its corpus")?;
        assert_eq!(
            dataset.format,
            pipette_plan_types::CorpusFormat::AsrEvalCorpusV1
        );
        assert_eq!(
            asr.recipe.as_ref().and_then(|r| r.enable_thinking),
            Some(false)
        );
        Ok(())
    }

    /// An asr_eval pins the samples of its own task, version and profile (the
    /// profile required), and carries a recipe whose thinking is off.
    #[rstest]
    #[case::unpinned(ASR_EVAL_MINICPM_TOML.to_owned(), "must pin the samples it runs")]
    #[case::no_profile(
        asr_eval_minicpm().replace("prompt_profile  = \"minicpm_o\"\n", ""),
        "but the entry runs asr_esb_librispeech_test_clean/2026.10.1?prompt_profile=minicpm_o"
    )]
    #[case::other_profile(
        asr_eval_minicpm().replace("prompt_profile  = \"minicpm_o\"", "prompt_profile  = \"voxtral\""),
        "would not hold the samples"
    )]
    #[case::other_task(
        asr_eval_minicpm().replace("eval_id         = \"asr_esb_librispeech_test_clean\"", "eval_id = \"asr_esb_ami_cleaned\""),
        "pins evals-server:asr_esb_ami_cleaned/2026.10.1"
    )]
    #[case::vlm_corpus(
        asr_eval_minicpm().replace("asr-eval-corpus/1", "vlm-eval-corpus/1"),
        "needs a asr-eval-corpus/1 dataset, not vlm-eval-corpus/1"
    )]
    #[case::git_adapter(
        asr_eval_minicpm().replace("evals-server@1", "mmspec@1"),
        "do not go together"
    )]
    #[case::no_recipe(
        asr_eval_minicpm().split("[recipe]").next().unwrap_or_default().to_owned(),
        "an asr_eval entry must say whose recipe it is"
    )]
    #[case::thinking_on(
        asr_eval_minicpm().replace("enable_thinking = false", "enable_thinking = true"),
        "runs with enable_thinking = false, as every ASR run does; this one says true"
    )]
    #[case::thinking_unset(
        asr_eval_minicpm().replace("enable_thinking = false\n", ""),
        "this one says unset"
    )]
    #[case::seed_rule_without_seed(
        asr_eval_minicpm_with("parameter_seed_rule = \"per_sample_sha256\"\n"),
        "parameter_seed_rule needs a parameter_seed"
    )]
    #[case::not_https(
        asr_eval_minicpm().replace("https://huggingface.co", "http://huggingface.co"),
        "is not an https URL"
    )]
    fn an_asr_eval_pins_its_samples_and_its_recipe(
        #[case] toml: String,
        #[case] names: &str,
    ) -> anyhow::Result<()> {
        let err =
            Benchmark::from_toml("asr_eval_esb_librispeech_test_clean_minicpm_o_beam3", &toml)
                .err()
                .context("the entry must be refused")?;
        let err = format!("{err:#}");
        assert!(err.contains(names), "{err}");
        Ok(())
    }

    /// A copied asr_eval entry's `url` is the server's to stamp, as for a
    /// vlm_eval.
    #[test]
    fn an_asr_eval_corpus_url_is_dropped_on_load() -> anyhow::Result<()> {
        let toml = asr_eval_minicpm().replace(
            "bytes   = 1024",
            "bytes   = 1024\nurl     = \"/datasets/elsewhere\"",
        );
        let benchmark = Benchmark::from_toml("asr_eval_x", &toml)?;
        assert_eq!(benchmark.dataset().and_then(|d| d.url.as_ref()), None);
        Ok(())
    }

    const VLM_EVAL_TOML: &str = r#"
benchmark_type = "vlm_eval"
parameter_lmms_task = "mmstar"
parameter_dataset_version = "2026.10.1"
parameter_sample_count = 1500
parameter_max_tokens = 8192
"#;

    /// The evals server's MMStar samples for `profile`, as `dataset-cache pins`
    /// prints them.
    fn vlm_eval_dataset(profile: Option<&str>) -> String {
        let profile = profile.map_or(String::new(), |p| format!("prompt_profile = \"{p}\"\n"));
        format!(
            r#"
[dataset]
format  = "vlm-eval-corpus/1"
adapter = "evals-server@1"
digest  = "1111111111111111111111111111111111111111111111111111111111111111"
bytes   = 58777600

[dataset.upstream]
source          = "evals_server"
eval_id         = "mmstar"
dataset_version = "2026.10.1"
{profile}identity        = "9274637bfb21faac6739590ccf9f459267e9e619f8c58f7dcffacf4c01c654d1"
"#
        )
    }

    /// Whose settings the MMStar entry holds.
    const VLM_EVAL_RECIPE: &str = r#"
[recipe]
family          = "Qwen3.5"
models          = [{ repo = "unsloth/Qwen3.5-2B-GGUF" }, { repo = "unsloth/Qwen3.5-4B-GGUF" }]
enable_thinking = true
sources         = ["https://huggingface.co/Qwen/Qwen3.5-2B"]
"#;

    /// An MMStar entry with `extra` top-level lines, pinning the samples of
    /// `profile`: the tables go last, so the lines stay top-level.
    fn vlm_eval(extra: &str, profile: Option<&str>) -> String {
        format!(
            "{VLM_EVAL_TOML}{extra}\n{}{VLM_EVAL_RECIPE}",
            vlm_eval_dataset(profile)
        )
    }

    /// A vlm_eval entry says whose recipe it is, with sources a reader can
    /// open, each model once.
    #[rstest]
    #[case::no_recipe(
        format!("{VLM_EVAL_TOML}\n{}", vlm_eval_dataset(None)),
        "must say whose recipe it is"
    )]
    #[case::not_https(
        vlm_eval("", None).replace("https://huggingface.co", "http://huggingface.co"),
        "is not an https URL"
    )]
    #[case::a_file(
        vlm_eval("", None).replace("https://huggingface.co/Qwen/Qwen3.5-2B", "file:///card.md"),
        "is not an https URL"
    )]
    #[case::twice(
        vlm_eval("", None).replace("unsloth/Qwen3.5-4B-GGUF", "Unsloth/qwen3.5-2b-gguf"),
        "lists unsloth/Qwen3.5-2B-GGUF twice"
    )]
    #[case::a_sampling_knob_in_the_recipe(
        vlm_eval("", None).replace("enable_thinking = true", "temperature = 0.6"),
        "unknown field `temperature`"
    )]
    #[case::a_misspelled_model_key(
        vlm_eval("", None).replace(r#"{ repo = "unsloth/Qwen3.5-2B-GGUF" }"#, r#"{ repo = "unsloth/Qwen3.5-2B-GGUF", mmprj = "mmproj-F16.gguf" }"#),
        "unknown field `mmprj`"
    )]
    fn a_vlm_eval_entry_says_whose_recipe_it_is(
        #[case] toml: String,
        #[case] names: &str,
    ) -> anyhow::Result<()> {
        let err = Benchmark::from_toml("vlm_eval_mmstar", &toml)
            .err()
            .context("the entry must be refused")?;
        let err = format!("{err:#}");
        assert!(err.contains(names), "{err}");
        Ok(())
    }

    /// The recipe decides who may run an entry, not what a run measures, so
    /// it is not a parameter: two entries differing only there share their
    /// parameters.
    #[test]
    fn the_recipe_is_not_a_parameter() -> anyhow::Result<()> {
        let one = Benchmark::from_toml("vlm_eval_mmstar", &vlm_eval("", None))?;
        let other = Benchmark::from_toml(
            "vlm_eval_mmstar",
            &vlm_eval("", None).replace("enable_thinking = true", "enable_thinking = false"),
        )?;
        assert_ne!(one.def, other.def);
        assert_eq!(
            serde_json::to_value(one.params())?,
            serde_json::to_value(other.params())?
        );
        Ok(())
    }

    /// A vlm_eval pins its samples, and only the ones the scorer grades it on.
    #[rstest]
    #[case::unpinned(VLM_EVAL_TOML.to_owned(), "must pin the samples it runs")]
    #[case::other_profile(vlm_eval("", Some("internvl")), "would not hold the samples")]
    #[case::other_task(
        vlm_eval("", None).replace("eval_id         = \"mmstar\"", "eval_id = \"pope\""),
        "pins evals-server:pope/2026.10.1, but the entry runs mmstar/2026.10.1"
    )]
    #[case::other_version(
        vlm_eval("", None).replace(
            "eval_id         = \"mmstar\"\ndataset_version = \"2026.10.1\"",
            "eval_id         = \"mmstar\"\ndataset_version = \"2026.07.1\""
        ),
        "pins evals-server:mmstar/2026.07.1"
    )]
    #[case::git_corpus(
        vlm_eval("", None)
            .replace("vlm-eval-corpus/1", "vl-spec-decode-corpus/1")
            .replace("evals-server@1", "mmspec@1"),
        "do not go together"
    )]
    fn a_vlm_eval_pins_the_samples_it_is_graded_on(
        #[case] toml: String,
        #[case] names: &str,
    ) -> anyhow::Result<()> {
        let err = Benchmark::from_toml("vlm_eval_mmstar", &toml)
            .err()
            .context("the entry must be refused")?;
        let err = format!("{err:#}");
        assert!(err.contains(names), "{err}");
        Ok(())
    }

    /// A copied entry's `url` is the server's to stamp, as on vl_spec_decode.
    #[test]
    fn a_vlm_eval_corpus_url_is_dropped_on_load() -> anyhow::Result<()> {
        let toml = vlm_eval("", None).replace(
            "bytes   = 58777600",
            "bytes   = 58777600\nurl     = \"/datasets/elsewhere\"",
        );
        let benchmark = Benchmark::from_toml("vlm_eval_mmstar", &toml)?;
        let dataset = benchmark
            .dataset()
            .context("a vlm_eval entry carries its dataset")?;
        assert_eq!(dataset.url, None);
        assert_eq!(dataset.bytes, 58_777_600);
        Ok(())
    }

    /// The strict loader takes every sampling knob a vlm_eval may pin, and an
    /// entry that pins none is served without them.
    #[test]
    fn vlm_eval_sampling_is_absent_until_pinned() -> anyhow::Result<()> {
        let bare = Benchmark::from_toml("vlm_eval_mmstar", &vlm_eval("", None))?;
        let wire = serde_json::to_value(&bare)?;
        let knobs = [
            "parameter_temperature",
            "parameter_top_p",
            "parameter_top_k",
            "parameter_min_p",
            "parameter_presence_penalty",
            "parameter_repeat_penalty",
            "parameter_seed",
            "parameter_seed_rule",
        ];
        knobs.iter().for_each(|knob| {
            assert!(wire.get(knob).is_none(), "{knob} must not reach the wire");
        });

        let pinned = Benchmark::from_toml(
            "vlm_eval_mmstar_temp0.7_topp0.8_topk20",
            &vlm_eval(
                "parameter_temperature = 0.7\nparameter_top_p = 0.8\n\
                 parameter_top_k = 20\nparameter_min_p = 0.0\nparameter_presence_penalty = 1.5\n\
                 parameter_repeat_penalty = 1.0\nparameter_seed = 7\n\
                 parameter_seed_rule = \"per_sample_sha256\"",
                None,
            ),
        )?;
        let vlm = pinned.def.as_vlm_eval()?;
        assert_eq!(
            vlm.sampling_temperature(),
            SamplingTemperature::try_new(0.7)?
        );
        assert_eq!(vlm.parameter_top_p, Some(TopP::try_new(0.8)?));
        assert_eq!(vlm.parameter_top_k, Some(TopK::try_new(20)?));
        assert_eq!(vlm.parameter_min_p, Some(MinP::try_new(0.0)?));
        assert_eq!(
            vlm.parameter_presence_penalty,
            Some(PresencePenalty::try_new(1.5)?)
        );
        assert_eq!(
            vlm.parameter_repeat_penalty,
            Some(RepeatPenalty::try_new(1.0)?)
        );
        assert_eq!(vlm.parameter_seed, Some(SamplingSeed::try_new(7)?));
        assert_eq!(vlm.parameter_seed_rule, Some(SeedRule::PerSampleSha256));
        let wire = serde_json::to_value(&pinned)?;
        knobs.iter().for_each(|knob| {
            assert!(wire.get(knob).is_some(), "{knob} must reach the wire");
        });
        Ok(())
    }

    #[rstest]
    #[case::temperature_high("parameter_temperature = 2.1", "Temperature is too big")]
    #[case::top_p_high("parameter_top_p = 1.5", "TopP is too big")]
    #[case::presence_low("parameter_presence_penalty = -2.5", "PresencePenalty is too small")]
    #[case::presence_high("parameter_presence_penalty = 2.5", "PresencePenalty is too big")]
    #[case::repeat_zero("parameter_repeat_penalty = 0.0", "RepeatPenalty is too small")]
    #[case::repeat_high("parameter_repeat_penalty = 2.5", "RepeatPenalty is too big")]
    #[case::misspelled(
        "parameter_repetition_penalty = 1.05",
        "unknown field `parameter_repetition_penalty`"
    )]
    #[case::profile_not_a_name("parameter_prompt_profile = 3", "invalid type: integer `3`")]
    #[case::seed_rule_without_seed(
        "parameter_seed_rule = \"per_sample_sha256\"",
        "parameter_seed_rule needs a parameter_seed"
    )]
    #[case::seed_rule_unknown(
        "parameter_seed = 0\nparameter_seed_rule = \"per_row\"",
        "unknown variant `per_row`"
    )]
    fn a_bad_vlm_eval_knob_is_rejected(
        #[case] line: &str,
        #[case] names: &str,
    ) -> anyhow::Result<()> {
        let toml = vlm_eval(line, None);
        let err = Benchmark::from_toml("vlm_eval_mmstar_bad", &toml)
            .err()
            .with_context(|| format!("{line} must be rejected"))?;
        let err = format!("{err:#}");
        assert!(err.contains(names), "{line} rejected as {err:?}");
        Ok(())
    }

    /// The strict loader takes a vlm_eval's prompt profile, keeps a name this
    /// server doesn't know as written for the evals server to judge, and serves
    /// an entry that names none without it.
    #[rstest]
    #[case::dataset_prompt("", None)]
    #[case::known(
        "parameter_prompt_profile = \"internvl\"",
        Some(VlmPromptProfile::Known(KnownVlmPromptProfile::Internvl))
    )]
    #[case::unknown(
        "parameter_prompt_profile = \"qwen2_vl\"",
        Some(VlmPromptProfile::Unknown("qwen2_vl".to_string()))
    )]
    fn a_vlm_eval_names_its_prompt_profile(
        #[case] line: &str,
        #[case] expected: Option<VlmPromptProfile>,
    ) -> anyhow::Result<()> {
        let profile = expected.as_ref().map(VlmPromptProfile::as_str);
        let parsed = Benchmark::from_toml("vlm_eval_mmstar", &vlm_eval(line, profile))?;
        let vlm = parsed.def.as_vlm_eval()?;
        assert_eq!(vlm.parameter_prompt_profile, expected);
        let served = expected.as_ref().map(VlmPromptProfile::as_str);
        let wire = serde_json::to_value(&parsed)?;
        assert_eq!(
            wire.get("parameter_prompt_profile")
                .and_then(serde_json::Value::as_str),
            served
        );
        let params = serde_json::to_value(parsed.params())?;
        assert_eq!(
            params
                .get("prompt_profile")
                .and_then(serde_json::Value::as_str),
            served
        );
        Ok(())
    }

    /// liquid_evals' MATH-500 bound: prompt plus output stop at the output cap.
    const EVAL_CTX_TOML: &str = r#"
benchmark_type = "eval"
parameter_eval_id = "math_500"
parameter_dataset_name = "2026.10.1"
parameter_max_tokens = 32768
parameter_max_context = 32768
"#;

    #[rstest]
    #[case::bounded(EVAL_CTX_TOML, Some(32768))]
    #[case::unbounded(EVAL_TOML, None)]
    fn test_parse_eval_max_context(
        #[case] toml: &str,
        #[case] expected: Option<i32>,
    ) -> anyhow::Result<()> {
        let entry = Benchmark::from_toml("entry", toml)?;
        assert_eq!(
            entry
                .def
                .as_eval()?
                .parameter_max_context
                .map(PositiveCount::into_inner),
            expected
        );
        assert_eq!(
            serde_json::to_value(entry.params())?.get("max_context"),
            expected.map(serde_json::Value::from).as_ref()
        );
        assert_eq!(
            Benchmark::from_toml("entry", &entry.to_toml()?)?.def,
            entry.def
        );
        Ok(())
    }

    /// The rules are tested beside `check_max_context`; here, that both loaders run it.
    #[rstest]
    fn test_a_malformed_context_bound_fails_to_load(
        #[values("toml", "json")] form: &str,
    ) -> anyhow::Result<()> {
        let toml = EVAL_CTX_TOML.replace(
            "parameter_max_context = 32768",
            "parameter_max_context = 32769",
        );
        let parsed = match form {
            "toml" => Benchmark::from_toml("entry", &toml),
            _ => from_json(&serde_json::to_string(&toml::from_str::<Table>(&toml)?)?),
        };
        let err = format!("{:#}", parsed.err().context("must be refused")?);
        assert!(
            err.contains("parameter_max_context (32769) exceeds parameter_max_tokens (32768)"),
            "{form} refused as {err:?}"
        );
        Ok(())
    }

    #[test]
    fn test_from_def_refuses_a_context_bound_past_the_cap() -> anyhow::Result<()> {
        let mut def = Benchmark::from_toml("entry", EVAL_CTX_TOML)?.def;
        if let BenchmarkDefinition::Eval(b) = &mut def {
            b.parameter_max_context = Some(PositiveCount::try_new(65536)?);
        }
        let err = Benchmark::from_def(def)
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(
            err.starts_with("benchmark entry: parameter_max_context (65536)"),
            "{err}"
        );
        Ok(())
    }

    #[test]
    fn test_load_catalog() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::write(
            dir.path().join("prefill_throughput_256.toml"),
            r#"benchmark_type = "prefill_throughput"
parameter_prefill_tokens = 256"#,
        )?;
        std::fs::write(
            dir.path().join("decode_throughput_512_100.toml"),
            r#"benchmark_type = "decode_throughput"
parameter_prefill_tokens = 512
parameter_decode_tokens = 100"#,
        )?;

        let catalog = load_catalog(dir.path())?;
        assert_eq!(catalog.len(), 2);
        assert!(catalog.contains_key(&BenchmarkId::try_new("prefill_throughput_256")?));
        assert!(catalog.contains_key(&BenchmarkId::try_new("decode_throughput_512_100")?));
        Ok(())
    }

    /// Serving takes the entries it has: a broken file is captured, not fatal.
    #[test]
    fn test_read_catalog_captures_errors_and_keeps_the_rest() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::write(
            dir.path().join("prefill_throughput_256.toml"),
            "benchmark_type = \"prefill_throughput\"\nparameter_prefill_tokens = 256",
        )?;
        std::fs::write(
            dir.path().join("bad_unknown_key.toml"),
            "benchmark_type = \"prefill_throughput\"\nparameter_prefill_tokens = 1\nnope = 1",
        )?;

        let load = read_catalog(dir.path())?;
        assert_eq!(load.catalog.len(), 1);
        assert!(
            load.catalog
                .contains_key(&BenchmarkId::try_new("prefill_throughput_256")?)
        );
        assert_eq!(load.errors.len(), 1);
        assert!(
            load.errors[0].contains("bad_unknown_key"),
            "{:?}",
            load.errors
        );
        Ok(())
    }

    /// Counts an older server stored before the client's bounds were enforced
    /// are skipped like any unreadable entry, not fatal to the catalog.
    #[test]
    fn test_read_catalog_skips_counts_stored_before_the_bounds() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::write(
            dir.path().join("prefill_throughput_256.toml"),
            PREFILL_THROUGHPUT_TOML,
        )?;
        [
            ("eval_zero", EVAL_TOML.replace("8192", "0")),
            ("eval_negative", EVAL_TOML.replace("8192", "-5")),
            ("eval_huge", EVAL_TOML.replace("8192", "2147483648")),
        ]
        .iter()
        .try_for_each(|(id, toml)| std::fs::write(dir.path().join(format!("{id}.toml")), toml))?;

        let load = read_catalog(dir.path())?;
        assert_eq!(
            load.catalog
                .keys()
                .map(BenchmarkId::as_str)
                .collect::<Vec<_>>(),
            ["prefill_throughput_256"]
        );
        assert_eq!(load.errors.len(), 3, "{:?}", load.errors);
        Ok(())
    }

    /// One report lists every broken file, so an operator fixes them in a
    /// single pass instead of meeting the next one on each restart.
    #[test]
    fn test_load_catalog_reports_every_bad_file() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::write(
            dir.path().join("prefill_throughput_256.toml"),
            "benchmark_type = \"prefill_throughput\"\nparameter_prefill_tokens = 256",
        )?;
        std::fs::write(
            dir.path().join("bad_unknown_key.toml"),
            "benchmark_type = \"prefill_throughput\"\nparameter_prefill_tokens = 1\nnope = 1",
        )?;
        std::fs::write(
            dir.path().join("bad_missing_key.toml"),
            "benchmark_type = \"prefill_throughput\"",
        )?;

        let Err(e) = load_catalog(dir.path()) else {
            anyhow::bail!("a catalog holding two broken files must not load");
        };
        let msg = format!("{e:#}");
        assert!(msg.contains("2 of 3"), "{msg}");
        assert!(msg.contains("bad_unknown_key"), "{msg}");
        assert!(msg.contains("bad_missing_key"), "{msg}");
        Ok(())
    }

    /// `read_to_string` alone reports only "Is a directory", which does not say
    /// which entry to go and look at.
    #[test]
    fn test_load_catalog_read_error_names_the_file() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::create_dir(dir.path().join("looks_like_a_benchmark.toml"))?;

        let Err(e) = load_catalog(dir.path()) else {
            anyhow::bail!("a directory named *.toml must not load");
        };
        let msg = format!("{e:#}");
        assert!(msg.contains("looks_like_a_benchmark.toml"), "{msg}");
        Ok(())
    }

    #[test]
    fn test_load_catalog_missing_dir() {
        let result = load_catalog(Path::new("/nonexistent"));
        assert!(result.is_err());
    }

    /// Either ASR type parses its clip, text and decode count, and a missing
    /// text line means none.
    #[rstest]
    #[case::throughput(BenchmarkType::AsrThroughput, 16000, 1, Some(1024), 128)]
    #[case::memory_no_text(BenchmarkType::AsrMaxMemory, 48000, 2, None, 256)]
    fn test_parse_asr(
        #[case] kind: BenchmarkType,
        #[case] rate: i32,
        #[case] channels: i32,
        #[case] text: Option<i32>,
        #[case] decode: i32,
    ) -> anyhow::Result<()> {
        let text_line = text
            .map(|n| format!("parameter_text_tokens = {n}\n"))
            .unwrap_or_default();
        let b = Benchmark::from_toml(
            &format!("{kind}_9400ms_text{}_decode{decode}", text.unwrap_or(0)),
            &format!(
                "benchmark_type = \"{kind}\"\n\
                 parameter_audio_duration_ms = 9400\n\
                 parameter_audio_sample_rate_hz = {rate}\n\
                 parameter_audio_channels = {channels}\n{text_line}\
                 parameter_decode_tokens = {decode}"
            ),
        )?;
        assert_eq!(b.benchmark_type(), kind);
        assert_eq!(
            serde_json::to_value(b.params())?,
            serde_json::json!({
                "benchmark_type": kind.to_string(),
                "audio_duration_ms": 9400,
                "audio_sample_rate_hz": rate,
                "audio_channels": channels,
                "text_tokens": text.unwrap_or(0),
                "decode_tokens": decode,
            })
        );
        Ok(())
    }

    /// The ladder benchmarks.md documents (every clip length with and without
    /// text, 16 kHz mono, 256 decoded tokens, both types) loads through the
    /// server's startup loader under its documented ids. The entries live in the
    /// deployment's catalog, not `examples/`, so they are spelled as it spells them.
    #[test]
    fn the_documented_asr_ladder_loads() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let expected = [2500, 5000, 15000, 29900, 119900]
            .into_iter()
            .flat_map(|ms| [0, 1024].map(|text| (ms, text)))
            .flat_map(|(ms, text)| {
                ["asr_throughput", "asr_max_memory"].map(|kind| (ms, text, kind))
            })
            .map(|(ms, text, kind)| {
                let id = format!("{kind}_{ms}ms_text{text}_decode256");
                std::fs::write(
                    dir.path().join(format!("{id}.toml")),
                    format!(
                        "benchmark_type = \"{kind}\"\n\
                         parameter_audio_duration_ms = {ms}\n\
                         parameter_audio_sample_rate_hz = 16000\n\
                         parameter_audio_channels = 1\n\
                         parameter_text_tokens = {text}\n\
                         parameter_decode_tokens = 256\n"
                    ),
                )?;
                Ok((
                    id,
                    serde_json::json!({
                        "benchmark_type": kind,
                        "audio_duration_ms": ms,
                        "audio_sample_rate_hz": 16000,
                        "audio_channels": 1,
                        "text_tokens": text,
                        "decode_tokens": 256,
                    }),
                ))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let catalog = load_catalog(dir.path())?;
        assert_eq!(catalog.len(), 20);
        expected.into_iter().try_for_each(|(id, params)| {
            let b = catalog
                .get(&BenchmarkId::try_new(&id)?)
                .with_context(|| format!("{id} not loaded"))?;
            // Readers holding only the id (pipette-ctl, result records) agree.
            assert_eq!(BenchmarkType::from_id(&id), Some(b.benchmark_type()));
            assert_eq!(serde_json::to_value(b.params())?, params, "{id}");
            Ok(())
        })
    }

    /// An entry every client would refuse does not parse: one decoded token
    /// times nothing, and a clip past the client's cap is never rendered. The
    /// bounds are the plan types', so client and server hold the same ones.
    /// Either type without a decode count is refused: neither has a default.
    #[rstest]
    #[case::two_tokens_at_cap(600_000, Some(2), None)]
    #[case::one_token(9_400, Some(1), Some("AsrDecodeTokens is too small"))]
    #[case::too_long(600_001, Some(128), Some("AudioDurationMillis is too big"))]
    #[case::no_decode(9_400, None, Some("missing field `parameter_decode_tokens`"))]
    fn an_asr_entry_is_bounded_at_load(
        #[case] duration_ms: i32,
        #[case] decode_tokens: Option<i32>,
        #[case] refusal: Option<&str>,
        #[values("asr_throughput", "asr_max_memory")] benchmark_type: &str,
    ) -> anyhow::Result<()> {
        let decode = decode_tokens
            .map(|n| format!("parameter_decode_tokens = {n}\n"))
            .unwrap_or_default();
        let toml = format!(
            "benchmark_type = \"{benchmark_type}\"\n\
             parameter_audio_duration_ms = {duration_ms}\n\
             parameter_audio_sample_rate_hz = 16000\n\
             parameter_audio_channels = 1\n{decode}"
        );
        match (Benchmark::from_toml("asr_entry", &toml), refusal) {
            (Ok(_), None) => {}
            (Err(e), Some(expected)) => assert!(e.to_string().contains(expected), "{e:#}"),
            (got, refusal) => anyhow::bail!("expected {refusal:?}, got {got:?}"),
        }
        Ok(())
    }

    /// An ASR submission measures its own six `asr_` fields and the client's
    /// span, which it shares with VL throughput by name: the client's result
    /// enum is untagged, so the six must not collide with a VL field, and the
    /// span is the one quantity both time alike.
    #[test]
    fn asr_measurement_fields_are_their_own() {
        let asr = measurement_fields(BenchmarkType::AsrThroughput);
        assert_eq!(
            asr,
            [
                "asr_prompt_tokens",
                "asr_audio_tokens",
                "asr_prompt_ms",
                "asr_prompt_ms_stddev",
                "asr_predicted_ms",
                "asr_predicted_ms_stddev",
                "total_time_ms",
                "total_time_ms_stddev",
            ]
        );
        let vl = measurement_fields(BenchmarkType::VlThroughput);
        let shared: Vec<&str> = asr.iter().copied().filter(|f| vl.contains(f)).collect();
        assert_eq!(shared, ["total_time_ms", "total_time_ms_stddev"]);
        // The same fields as the other peak-memory benchmarks, so the wire
        // carries nothing of its own.
        assert_eq!(
            measurement_fields(BenchmarkType::AsrMaxMemory),
            measurement_fields(BenchmarkType::VlMaxMemory)
        );
    }

    #[test]
    fn test_parse_vl_throughput() -> anyhow::Result<()> {
        let toml = r#"
benchmark_type = "vl_throughput"
parameter_image_width = 384
parameter_image_height = 512
parameter_text_tokens = 32
parameter_decode_tokens = 128
"#;
        let def = Benchmark::from_toml("entry", toml)?.def;
        let BenchmarkDefinition::VlThroughput(VlThroughputBenchmark {
            parameter_image_width,
            parameter_image_height,
            parameter_text_tokens,
            parameter_decode_tokens,
            parameter_num_images,
            ..
        }) = def
        else {
            anyhow::bail!("expected VlThroughput");
        };
        assert_eq!(parameter_image_width.into_inner(), 384);
        assert_eq!(parameter_image_height.into_inner(), 512);
        assert_eq!(parameter_text_tokens.into_inner(), 32);
        assert_eq!(parameter_decode_tokens.into_inner(), 128);
        assert_eq!(parameter_num_images.into_inner(), 1);
        Ok(())
    }

    // `explicit` sets every field but decode; `defaults` omits text/num_images/decode
    // to exercise the serde defaults (0 text tokens, 1 image, no decode count).
    #[rstest]
    #[case::explicit(
        r#"
benchmark_type = "vl_max_memory"
parameter_image_width = 512
parameter_image_height = 512
parameter_text_tokens = 1024
parameter_num_images = 5
"#,
        512,
        512,
        1024,
        None,
        5
    )]
    #[case::defaults(
        r#"
benchmark_type = "vl_max_memory"
parameter_image_width = 256
parameter_image_height = 256
"#,
        256,
        256,
        0,
        None,
        1
    )]
    #[case::decode(VL_MAX_MEMORY_DECODE_TOML, 256, 256, 1024, Some(256), 5)]
    fn test_parse_vl_max_memory(
        #[case] toml: &str,
        #[case] width: i32,
        #[case] height: i32,
        #[case] text_tokens: i32,
        #[case] decode_tokens: Option<i32>,
        #[case] num_images: i32,
    ) -> anyhow::Result<()> {
        let def = Benchmark::from_toml("entry", toml)?.def;
        let BenchmarkDefinition::VlMaxMemory(VlMaxMemoryBenchmark {
            parameter_image_width,
            parameter_image_height,
            parameter_text_tokens,
            parameter_decode_tokens,
            parameter_num_images,
            ..
        }) = def
        else {
            anyhow::bail!("expected VlMaxMemory");
        };
        assert_eq!(parameter_image_width.into_inner(), width);
        assert_eq!(parameter_image_height.into_inner(), height);
        assert_eq!(parameter_text_tokens.into_inner(), text_tokens);
        assert_eq!(
            parameter_decode_tokens.map(PositiveCount::into_inner),
            decode_tokens
        );
        assert_eq!(parameter_num_images.into_inner(), num_images);
        Ok(())
    }

    /// An entry without a decode count serves the body and params it had
    /// before the field, so its ETag and warehouse params id do not move.
    #[test]
    fn a_vl_max_memory_entry_without_a_decode_count_is_unchanged() -> anyhow::Result<()> {
        let benchmark = Benchmark::from_toml(
            "vl_max_memory_256x256_text1024_img5",
            "benchmark_type = \"vl_max_memory\"\n\
             parameter_image_width = 256\n\
             parameter_image_height = 256\n\
             parameter_text_tokens = 1024\n\
             parameter_num_images = 5",
        )?;
        let params = benchmark.params();
        assert_eq!(
            serde_json::to_string(&pipette_plan_types::CatalogEntry::from(benchmark))?,
            r#"{"benchmark_type":"vl_max_memory","benchmark_id":"vl_max_memory_256x256_text1024_img5","parameter_image_width":256,"parameter_image_height":256,"parameter_text_tokens":1024,"parameter_num_images":5}"#
        );
        assert_eq!(
            pipette_plan_types::canonical_json(&serde_json::to_value(&params)?),
            r#"{"benchmark_type":"vl_max_memory","image_height":256,"image_width":256,"num_images":5,"text_tokens":1024}"#
        );
        assert_eq!(
            pipette_plan_types::descriptor_digest(&params)?,
            "a16344f65b5d6c4378e810df61356319722b9fec0d05bafb5b52976f3b22e814"
        );
        Ok(())
    }
}
