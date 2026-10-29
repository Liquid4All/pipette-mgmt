//! The scheduler-mode [`SchedulerPlan`] and its static validation. It and the
//! local-dispatch [`Plan`](pipette_plan_types::Plan) each require top-level
//! fields the other lacks, so a mis-selected parser fails loudly.

use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use pipette_plan_types::{
    BenchmarkFlags, BenchmarkId, BenchmarkType, CapabilityFlag, ClientId, ClientRunSpec, Model,
    ModelFlags, NonEmptyVec, Runtime, RuntimeFlags, VariantError, resolve_benchmark_flags,
    resolve_benchmarks, resolve_model_flags, resolve_runtime_flags, runnable_pairs_of,
    runnable_type_pairs_of, validate_variant_flag_cells,
};

/// Who a cell may run on: a client that no exclusion names, and that is in
/// `clients` or carries every flag of `requires`. The server evaluates it.
///
/// Every list is sorted and deduplicated (`not_all_of` at both levels), so
/// identical cells collapse in [`SchedulerPlan::runnable_cells`] instead of
/// minting duplicate jobs. The fields are private to keep that normal form.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Eligibility {
    requires: Vec<CapabilityFlag>,
    clients: Vec<ClientId>,
    client_versions: Vec<CapabilityFlag>,
    exclude_clients: Vec<ClientId>,
    excludes: Vec<CapabilityFlag>,
    not_all_of: Vec<Vec<CapabilityFlag>>,
}

impl Eligibility {
    /// Takes the whole variant: two pairs of clauses share a type, and a
    /// positional swap would invert a denial into an allowance.
    fn new(variant: &SchedulerVariant) -> Self {
        // Groups normalize before the list does, so the list's order depends on
        // the groups as sets, as in the server's `BTreeSet` of `BTreeSet`s.
        let mut not_all_of: Vec<Vec<CapabilityFlag>> = variant
            .not_all_of
            .iter()
            .cloned()
            .map(normalized_flags)
            .collect();
        not_all_of.sort_unstable_by(|a, b| {
            a.iter()
                .map(CapabilityFlag::as_ref)
                .cmp(b.iter().map(CapabilityFlag::as_ref))
        });
        not_all_of.dedup();
        Self {
            requires: normalized_flags(variant.requires.clone()),
            clients: normalized_ids(variant.clients.clone()),
            client_versions: normalized_flags(
                variant
                    .client_versions
                    .iter()
                    .map(|version| version.flag().clone())
                    .collect(),
            ),
            exclude_clients: normalized_ids(variant.exclude_clients.clone()),
            excludes: normalized_flags(variant.excludes.clone()),
            not_all_of,
        }
    }

    /// Empty means eligibility is by the `clients` allowlist alone.
    pub fn requires(&self) -> &[CapabilityFlag] {
        &self.requires
    }

    /// Client ids eligible regardless of `requires`.
    pub fn clients(&self) -> &[ClientId] {
        &self.clients
    }

    /// The `client_version:<v>` flags of the builds the `requires` path admits.
    /// Empty admits any build.
    pub fn client_versions(&self) -> &[CapabilityFlag] {
        &self.client_versions
    }

    pub fn exclude_clients(&self) -> &[ClientId] {
        &self.exclude_clients
    }

    /// Flags that deny a client which carries any one of them.
    pub fn excludes(&self) -> &[CapabilityFlag] {
        &self.excludes
    }

    /// Groups of flags, each denying a client that carries the whole group.
    /// A parsed plan has no empty group, which would deny everybody.
    pub fn not_all_of(&self) -> &[Vec<CapabilityFlag>] {
        &self.not_all_of
    }
}

fn normalized_flags(mut flags: Vec<CapabilityFlag>) -> Vec<CapabilityFlag> {
    flags.sort_unstable_by(|a, b| a.as_ref().cmp(b.as_ref()));
    flags.dedup();
    flags
}

/// The server's `pipette_core::client::CLIENT_VERSION_NAMESPACE`.
const CLIENT_VERSION_NAMESPACE: &str = "client_version";

/// One `client_versions` value, held as the `client_version:<v>` flag it makes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct ClientVersion(CapabilityFlag);

/// The `<v>` alone, under the flag's own rule.
#[cfg(feature = "schemars")]
impl schemars::JsonSchema for ClientVersion {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ClientVersion".into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        generator.subschema_for::<CapabilityFlag>()
    }
}

/// A `client_versions` value that makes no canonical flag. An empty one would
/// make `client_version:`, which names no build.
#[derive(Debug, thiserror::Error)]
#[error(
    "`client_versions` value {0:?} does not make a canonical capability flag \
     `client_version:{0}`: a version is non-empty, lowercase and holds no whitespace"
)]
pub struct ClientVersionError(String);

impl ClientVersion {
    pub fn flag(&self) -> &CapabilityFlag {
        &self.0
    }

    /// The `<v>` of the flag.
    pub fn version(&self) -> &str {
        self.0
            .as_ref()
            .split_once(':')
            .map_or("", |(_, version)| version)
    }
}

impl TryFrom<String> for ClientVersion {
    type Error = ClientVersionError;

    fn try_from(version: String) -> Result<Self, Self::Error> {
        if version.is_empty() {
            return Err(ClientVersionError(version));
        }
        CapabilityFlag::try_new(format!("{CLIENT_VERSION_NAMESPACE}:{version}"))
            .map(Self)
            .map_err(|_| ClientVersionError(version))
    }
}

impl From<ClientVersion> for String {
    fn from(version: ClientVersion) -> Self {
        version.version().to_owned()
    }
}

fn normalized_ids(mut ids: Vec<ClientId>) -> Vec<ClientId> {
    ids.sort_unstable_by(|a, b| a.as_ref().cmp(b.as_ref()));
    ids.dedup();
    ids
}

/// A block of benchmarks × models × runtimes and the clients that may run it.
/// Expands to every compatible cell.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct SchedulerVariant {
    /// Crossed with `runtimes`; each model must pair with one of them.
    pub models: NonEmptyVec<Model>,
    /// Crossed with `models`; each runtime must load one of them.
    pub runtimes: NonEmptyVec<Runtime>,
    /// Canonical capability flags every eligible client must satisfy. Optional
    /// on its own, but a variant must set at least one of `requires`/`clients`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<CapabilityFlag>,
    /// Explicit management-server client allowlist (`ev1_…`). Optional on its
    /// own; see `requires`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clients: Vec<ClientId>,
    /// App builds a client must run one of to take the variant's jobs through
    /// `requires`: the `<v>` of its `client_version:<v>` flags. Defaults to
    /// empty, which admits any build. A client in `clients` runs at any build.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub client_versions: Vec<ClientVersion>,
    /// Client ids this variant never runs on, whatever flags they carry. A
    /// denial beats the `clients` roster. Defaults to empty, which denies
    /// nobody.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude_clients: Vec<ClientId>,
    /// Canonical capability flags that make a client ineligible. One carried
    /// flag is enough. Defaults to empty, which denies nobody.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excludes: Vec<CapabilityFlag>,
    /// Groups of canonical capability flags. A client that carries every flag
    /// of one group is ineligible. Defaults to empty, which denies nobody; an
    /// empty group denies everybody and validation rejects it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub not_all_of: Vec<Vec<CapabilityFlag>>,
    /// Per-variant benchmark override; falls back to the plan-level list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub benchmarks: Option<NonEmptyVec<BenchmarkId>>,
    /// Runtime flags, each for one cell this variant expands to, named by its
    /// `benchmark_type`, `runtime_type` and `model_type`. All three flag lists
    /// share the local-dispatch [`Variant`](pipette_plan_types::Variant)'s rules.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub runtime_flags: Vec<RuntimeFlags>,
    /// Generation flags, each for the eval cells its `benchmark_type` and
    /// `model_type` name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub model_flags: Vec<ModelFlags>,
    /// Run-driving flags, each for the cells its `benchmark_type`,
    /// `runtime_type` and `model_type` name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub benchmark_flags: Vec<BenchmarkFlags>,
}

impl SchedulerVariant {
    pub fn eligibility(&self) -> Eligibility {
        Eligibility::new(self)
    }

    fn declares_eligibility(&self) -> bool {
        !self.requires.is_empty() || !self.clients.is_empty()
    }

    fn runnable_pairs(&self) -> Result<Vec<(&Model, &Runtime)>, VariantError> {
        runnable_pairs_of(&self.models, &self.runtimes)
    }

    fn runnable_type_pairs(&self) -> Result<Vec<(&Model, &Runtime, usize)>, VariantError> {
        runnable_type_pairs_of(&self.models, &self.runtimes)
    }
}

/// A validated scheduler-mode plan; see `validate_scheduler_plan` for what it
/// guarantees. `#[non_exhaustive]` blocks a struct literal that would skip it.
/// Not `Deserialize`: only [`crate::parse`], which takes the types, makes one.
#[derive(Debug, PartialEq, Eq, Serialize)]
#[cfg_attr(
    feature = "schemars",
    derive(schemars::JsonSchema),
    schemars(
        description = "A plan document: `[[variants]]` of benchmarks × models × runtimes, \
                       each with the clients that may run it."
    )
)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct SchedulerPlan {
    /// Optional plan-level expiry, stamped on every job at generation. It
    /// states either a definite instant (RFC 3339 / ISO 8601) or the literal
    /// `never`. When absent, each job states no expiry, and ingestion resolves
    /// one 30 days out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<PlanExpiresAt>,
    /// Whether the jobs this plan expands to may join a job that already runs
    /// the same cell. A plan that omits the field merges, which is what the
    /// server does by default. `false` gives the plan its own run of every
    /// cell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_duplicates: Option<bool>,
    /// Optional default benchmark list; a variant without its own `benchmarks`
    /// inherits this. When absent, every variant must set its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub benchmarks: Option<NonEmptyVec<BenchmarkId>>,
    /// Expanded in order; errors and edits number them from 0.
    pub variants: NonEmptyVec<SchedulerVariant>,
    /// Each benchmark's catalog type, given to [`crate::parse`]; not part of
    /// the document.
    #[serde(skip)]
    #[cfg_attr(feature = "schemars", schemars(skip))]
    benchmark_types: BTreeMap<BenchmarkId, BenchmarkType>,
}

/// Unvalidated serde intake; [`SchedulerPlan::from_intake`] validates it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SchedulerPlanIntake {
    #[serde(default)]
    expires_at: Option<PlanExpiresAt>,
    #[serde(default)]
    merge_duplicates: Option<bool>,
    #[serde(default)]
    benchmarks: Option<NonEmptyVec<BenchmarkId>>,
    variants: NonEmptyVec<SchedulerVariant>,
}

impl SchedulerPlanIntake {
    pub(crate) fn benchmark_ids(&self) -> impl Iterator<Item = &BenchmarkId> {
        (self.benchmarks.iter().flat_map(|b| b.iter())).chain(
            self.variants
                .iter()
                .flat_map(|v| v.benchmarks.iter().flat_map(|b| b.iter())),
        )
    }
}

/// A plan's `expires_at`: a definite instant, or `never` for jobs that the
/// server never expires on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanExpiresAt {
    At(OffsetDateTime),
    Never,
}

impl PlanExpiresAt {
    /// The one place that spells the wire form, so a job body and a
    /// reserialized plan agree.
    pub fn to_wire(self) -> Result<String, time::error::Format> {
        match self {
            Self::At(at) => at.format(&Rfc3339),
            Self::Never => Ok(NEVER.to_owned()),
        }
    }
}

/// The keyword that `pipette-mgmt` ingestion matches, spelled exactly.
const NEVER: &str = "never";

#[derive(Debug, thiserror::Error)]
#[error("expires_at must be an RFC 3339 timestamp or the literal \"never\", got \"{text}\"")]
struct PlanExpiresAtError {
    text: String,
    #[source]
    source: time::error::Parse,
}

/// TOML may also write the instant as a native datetime, which reads as this string.
#[cfg(feature = "schemars")]
impl schemars::JsonSchema for PlanExpiresAt {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> std::borrow::Cow<'static, str> {
        "PlanExpiresAt".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "anyOf": [{ "format": "date-time" }, { "const": NEVER }],
        })
    }
}

impl Serialize for PlanExpiresAt {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let wire = self.to_wire().map_err(serde::ser::Error::custom)?;
        serializer.serialize_str(&wire)
    }
}

impl<'de> Deserialize<'de> for PlanExpiresAt {
    /// Accepts a quoted string or a native TOML datetime. A naive datetime with
    /// no offset is refused, and `never` matches lowercase only, as ingestion does.
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Str(String),
            Native(toml::value::Datetime),
        }

        let text = match Raw::deserialize(deserializer)? {
            Raw::Str(s) => s,
            Raw::Native(dt) => dt.to_string(),
        };
        if text == NEVER {
            return Ok(Self::Never);
        }
        OffsetDateTime::parse(&text, &Rfc3339)
            .map(Self::At)
            .map_err(|source| serde::de::Error::custom(PlanExpiresAtError { text, source }))
    }
}

/// Why a plan is invalid or its cells do not resolve, at the variant
/// [`Self::index`].
#[derive(Debug, thiserror::Error)]
pub enum SchedulerPlanError {
    #[error(
        "variant {index}: must declare at least one of `requires` or `clients` \
         (a variant with neither matches no client)"
    )]
    NoEligibility { index: usize },
    #[error(
        "variant {index}: an empty `not_all_of` group excludes every client, \
         whatever its id or flags"
    )]
    EmptyNotAllOfGroup { index: usize },
    #[error("variant {index}: `client_versions` lists {version:?} twice")]
    DuplicateClientVersion { index: usize, version: String },
    // `error`, not `source`: the message already carries it, so a chain would repeat it.
    #[error("variant {index}: {error}")]
    Variant { index: usize, error: VariantError },
}

impl SchedulerPlanError {
    /// The index of the variant at fault.
    pub fn index(&self) -> usize {
        match self {
            Self::NoEligibility { index }
            | Self::EmptyNotAllOfGroup { index }
            | Self::DuplicateClientVersion { index, .. }
            | Self::Variant { index, .. } => *index,
        }
    }

    fn variant(index: usize) -> impl FnOnce(VariantError) -> Self {
        move |error| Self::Variant { index, error }
    }
}

impl SchedulerPlan {
    pub(crate) fn from_intake(
        intake: SchedulerPlanIntake,
        types: &BTreeMap<BenchmarkId, BenchmarkType>,
    ) -> Result<Self, SchedulerPlanError> {
        let plan = SchedulerPlan {
            expires_at: intake.expires_at,
            merge_duplicates: intake.merge_duplicates,
            benchmarks: intake.benchmarks,
            variants: intake.variants,
            benchmark_types: types.clone(),
        };
        validate_scheduler_plan(&plan)?;
        Ok(plan)
    }

    fn benchmark_type(&self, benchmark: &BenchmarkId) -> Option<BenchmarkType> {
        self.benchmark_types.get(benchmark).copied()
    }

    /// A variant naming an id the types lack may be any type, so its flags are
    /// not refused here: ingestion refuses that id by name, with the nearest.
    fn benchmark_types_of(&self, benchmarks: &[BenchmarkId]) -> Vec<BenchmarkType> {
        if !benchmarks
            .iter()
            .all(|b| self.benchmark_types.contains_key(b))
        {
            return BenchmarkType::ALL.to_vec();
        }
        benchmarks
            .iter()
            .filter_map(|b| self.benchmark_type(b))
            .collect()
    }

    /// The tests' parser, typing by prefix. Outside this crate, [`crate::parse`]
    /// is the entry point.
    #[cfg(test)]
    pub(crate) fn parse(toml_str: &str) -> anyhow::Result<Self> {
        Ok(crate::parse(toml_str, &crate::prefix_types(toml_str))?)
    }

    /// Every cell in document order: variant, then benchmark, then (model,
    /// runtime) pair. A cell two variants both produce appears twice.
    pub fn cells(&self) -> Result<Vec<SchedulerCell>, SchedulerPlanError> {
        self.variants
            .iter()
            .enumerate()
            .map(|(idx, variant)| {
                let pairs = variant
                    .runnable_pairs()
                    .map_err(SchedulerPlanError::variant(idx))?;
                let benchmarks = self.benchmarks_for_variant(idx, variant)?;
                let eligibility = variant.eligibility();
                Ok(benchmarks
                    .iter()
                    .flat_map(|benchmark| {
                        let eligibility = eligibility.clone();
                        let bt = self.benchmark_type(benchmark);
                        pairs.iter().map(move |(model, runtime)| SchedulerCell {
                            spec: ClientRunSpec {
                                benchmark: benchmark.clone(),
                                model: (*model).clone(),
                                runtime: (*runtime).clone(),
                                runtime_flags: resolve_runtime_flags(
                                    &variant.runtime_flags,
                                    bt,
                                    runtime,
                                    model,
                                ),
                                model_flags: resolve_model_flags(&variant.model_flags, bt, model),
                                benchmark_flags: resolve_benchmark_flags(
                                    &variant.benchmark_flags,
                                    bt,
                                    runtime,
                                    model,
                                ),
                            },
                            eligibility: eligibility.clone(),
                        })
                    })
                    .collect::<Vec<_>>())
            })
            .collect::<Result<Vec<_>, SchedulerPlanError>>()
            .map(|cells| cells.into_iter().flatten().collect())
    }

    /// The deduped set of [`Self::cells`].
    pub fn runnable_cells(&self) -> Result<HashSet<SchedulerCell>, SchedulerPlanError> {
        self.cells().map(|cells| cells.into_iter().collect())
    }

    /// An upper bound on [`Self::runnable_cells`], computed without building a
    /// cell. It saturates, so an overflow reads as too many cells, not too few.
    pub(crate) fn cell_upper_bound(&self) -> Result<usize, SchedulerPlanError> {
        self.variants
            .iter()
            .enumerate()
            .try_fold(0_usize, |total, (idx, variant)| {
                let pairs = variant
                    .runnable_type_pairs()
                    .map_err(SchedulerPlanError::variant(idx))?
                    .iter()
                    .fold(0_usize, |sum, &(_, _, count)| sum.saturating_add(count));
                let benchmarks = self.benchmarks_for_variant(idx, variant)?;
                Ok(total.saturating_add(benchmarks.len().saturating_mul(pairs)))
            })
    }

    fn benchmarks_for_variant<'a>(
        &'a self,
        variant_idx: usize,
        variant: &'a SchedulerVariant,
    ) -> Result<&'a [BenchmarkId], SchedulerPlanError> {
        resolve_benchmarks(variant.benchmarks.as_deref(), self.benchmarks.as_deref())
            .map_err(SchedulerPlanError::variant(variant_idx))
    }
}

fn validate_scheduler_plan(plan: &SchedulerPlan) -> Result<(), SchedulerPlanError> {
    plan.variants
        .iter()
        .enumerate()
        .try_for_each(|(idx, variant)| {
            if !variant.declares_eligibility() {
                return Err(SchedulerPlanError::NoEligibility { index: idx });
            }
            if variant.not_all_of.iter().any(Vec::is_empty) {
                return Err(SchedulerPlanError::EmptyNotAllOfGroup { index: idx });
            }
            let mut seen = HashSet::new();
            if let Some(twice) = variant.client_versions.iter().find(|v| !seen.insert(*v)) {
                return Err(SchedulerPlanError::DuplicateClientVersion {
                    index: idx,
                    version: twice.version().to_owned(),
                });
            }
            let pairs = variant
                .runnable_type_pairs()
                .map_err(SchedulerPlanError::variant(idx))?
                .into_iter()
                .map(|(model, runtime, _)| (model, runtime))
                .collect::<Vec<_>>();
            let benchmark_types =
                plan.benchmark_types_of(plan.benchmarks_for_variant(idx, variant)?);
            validate_variant_flag_cells(
                &pairs,
                &benchmark_types,
                &variant.runtime_flags,
                &variant.model_flags,
                &variant.benchmark_flags,
            )
            .map_err(SchedulerPlanError::variant(idx))
        })
}

/// One benchmark × model × runtime from a variant. Identity is spec and
/// eligibility together: a different eligibility is a different cell.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SchedulerCell {
    /// The model as authored, token included.
    pub spec: ClientRunSpec,
    pub eligibility: Eligibility,
}

#[cfg(test)]
mod tests {
    use anyhow::Context;

    use super::*;

    /// An iOS Apple Foundation variant pinned to a client *and* requiring
    /// `os:ios`, with its own benchmarks, and a `requires`-only macOS MLX one.
    fn representative_plan() -> String {
        r#"
expires_at = "2026-08-01T00:00:00Z"
benchmarks = ["decode_throughput_512_100", "end_to_end_latency_512_256"]

[[variants]]
requires   = ["os:ios"]
clients    = ["ev1_9f2c"]
models     = [{ type = "apple_foundation_text" }]
runtimes   = [{ type = "apple_foundation" }]
benchmarks = ["decode_throughput_512_100"]

[[variants]]
requires = ["os:macos"]
models   = [{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "LFM2.5-350M-MLX-4bit" }]
runtimes = [{ type = "mlx_macos_pipette", version = "0.20.0", flavor = "macos-arm64", source = { type = "pip_requirements_text", contents = "mlx-lm==0.20.0" } }]
"#
        .to_owned()
    }

    #[test]
    fn representative_plan_expands_to_expected_cells() -> anyhow::Result<()> {
        let plan = SchedulerPlan::parse(&representative_plan())?;

        let expected = OffsetDateTime::parse("2026-08-01T00:00:00Z", &Rfc3339)?;
        assert_eq!(plan.expires_at, Some(PlanExpiresAt::At(expected)));

        // variant 1: 1 benchmark × 1 model × 1 runtime = 1
        // variant 2: 2 benchmarks × 1 model × 1 runtime = 2
        let cells = plan.runnable_cells()?;
        assert_eq!(cells.len(), 3, "expected 3 cells, got {}", cells.len());

        let afm = cells
            .iter()
            .find(|c| matches!(c.spec.model, Model::AppleFoundationText))
            .context("AFM cell present")?;
        assert_eq!(afm.eligibility.requires().len(), 1);
        assert_eq!(afm.eligibility.requires()[0].as_ref(), "os:ios");
        assert_eq!(afm.eligibility.clients().len(), 1);
        assert_eq!(afm.eligibility.clients()[0].as_ref(), "ev1_9f2c");
        assert_eq!(afm.spec.benchmark.as_ref(), "decode_throughput_512_100");

        let mlx: Vec<_> = cells
            .iter()
            .filter(|c| matches!(c.spec.model, Model::Mlx(_)))
            .collect();
        assert_eq!(mlx.len(), 2);
        assert!(mlx.iter().all(|c| {
            c.eligibility.clients().is_empty()
                && c.eligibility
                    .requires()
                    .iter()
                    .any(|f| f.as_ref() == "os:macos")
        }));
        Ok(())
    }

    #[test]
    fn local_dispatch_parser_rejects_a_scheduler_plan() -> anyhow::Result<()> {
        // Assert a schema tell, so this can't pass for an incidental reason.
        let err = pipette_plan_types::Plan::parse(&representative_plan())
            .err()
            .context("expected the local-dispatch parser to reject a scheduler plan")?;
        let msg = err.to_string();
        assert!(
            msg.contains("expires_at") || msg.contains("plan_id"),
            "unexpected error: {err}"
        );
        Ok(())
    }

    #[test]
    fn set_equal_eligibility_dedups_to_one_cell() -> anyhow::Result<()> {
        let toml = r#"
benchmarks = ["decode_throughput_512_100"]

[[variants]]
requires = ["os:macos", "arch:arm64"]
models   = [{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "r" }]
runtimes = [{ type = "mlx_macos_pipette", version = "0.20.0", flavor = "macos-arm64", source = { type = "pip_requirements_text", contents = "mlx-lm==0.20.0" } }]

[[variants]]
requires = ["arch:arm64", "os:macos", "os:macos"]
models   = [{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "r" }]
runtimes = [{ type = "mlx_macos_pipette", version = "0.20.0", flavor = "macos-arm64", source = { type = "pip_requirements_text", contents = "mlx-lm==0.20.0" } }]
"#;
        let cells = SchedulerPlan::parse(toml)?.runnable_cells()?;
        assert_eq!(cells.len(), 1, "set-equal eligibility should dedup");
        Ok(())
    }

    #[test]
    fn a_vlm_eval_variant_takes_runtime_and_benchmark_flags() -> anyhow::Result<()> {
        // The fleet runs a `vlm_eval` cell with per-device flags: the thinking
        // mode, the slot count, and a longer HTTP timeout for a slow phone. A
        // plan that carried any of them was refused at ingestion ("no benchmark
        // flags defined") before the cell existed in this crate's port of the
        // plan vocabulary, so the only `vlm_eval` cell a plan could author was a
        // bare one.
        let toml = r#"
benchmarks = ["vlm_eval_mmstar"]

[[variants]]
requires = ["os:android"]
models   = [{ type = "gguf_vision", source = "huggingface", org = "LiquidAI", repo_name = "r", model = "m.gguf", mmproj = "p.gguf" }]
runtimes = [{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", repository_version = "b10216", flavor = "android-arm64-v8a" }]
runtime_flags = [{ benchmark_type = "vlm_eval", runtime_type = "llamacpp_cli_stock_tools", model_type = "gguf_vision", threads = 8, number_gpu_layers = 0, flash_attention = "off" }]
model_flags = [{ benchmark_type = "vlm_eval", model_type = "gguf_vision", enable_thinking = true }]
benchmark_flags = [{ benchmark_type = "vlm_eval", runtime_type = "llamacpp_cli_stock_tools", model_type = "gguf_vision", http_timeout_seconds = 1800 }]
"#;
        let cells = SchedulerPlan::parse(toml)?.runnable_cells()?;
        let [cell] = cells.iter().collect::<Vec<_>>()[..] else {
            anyhow::bail!("expected one cell, got {}", cells.len());
        };
        assert!(matches!(
            cell.spec.runtime_flags,
            Some(pipette_plan_types::RuntimeFlags::VlmEvalLlamacppCliStockToolsGgufVision(_))
        ));
        assert!(cell.spec.benchmark_flags.is_some());
        Ok(())
    }

    const UNPREFIXED_VL_PLAN: &str = r#"
benchmarks = ["robo_orch_vl_throughput_512x512_text3396_decode47_img3"]

[[variants]]
requires = ["os:linux"]
models   = [{ type = "gguf_vision", source = "huggingface", org = "LiquidAI", repo_name = "r", model = "m.gguf", mmproj = "p.gguf" }]
runtimes = [{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", repository_version = "b10216", flavor = "linux-arm64" }]
runtime_flags = [{ benchmark_type = "vl_throughput", runtime_type = "llamacpp_cli_stock_tools", model_type = "gguf_vision", threads = 6 }]
"#;

    fn unprefixed_vl_types(
        benchmark_type: BenchmarkType,
    ) -> anyhow::Result<BTreeMap<BenchmarkId, BenchmarkType>> {
        Ok(BTreeMap::from([(
            BenchmarkId::try_new(
                "robo_orch_vl_throughput_512x512_text3396_decode47_img3".to_owned(),
            )?,
            benchmark_type,
        )]))
    }

    #[test]
    fn an_unprefixed_id_takes_the_flags_of_its_catalog_type() -> anyhow::Result<()> {
        let cells = crate::parse(
            UNPREFIXED_VL_PLAN,
            &unprefixed_vl_types(BenchmarkType::VlThroughput)?,
        )?
        .runnable_cells()?;
        let [cell] = cells.iter().collect::<Vec<_>>()[..] else {
            anyhow::bail!("expected one cell, got {}", cells.len());
        };
        assert!(matches!(
            cell.spec.runtime_flags,
            Some(pipette_plan_types::RuntimeFlags::VlThroughputLlamacppCliStockToolsGgufVision(_))
        ));
        Ok(())
    }

    /// A private id may start with another type's prefix; only the given type
    /// counts.
    #[test]
    fn the_given_type_beats_a_misleading_prefix() -> anyhow::Result<()> {
        let id = "decode_throughput_acme_vl";
        let text = UNPREFIXED_VL_PLAN
            .replace("robo_orch_vl_throughput_512x512_text3396_decode47_img3", id);
        let types = BTreeMap::from([(
            BenchmarkId::try_new(id.to_owned())?,
            BenchmarkType::VlThroughput,
        )]);
        let cells = crate::parse(&text, &types)?.runnable_cells()?;
        let [cell] = cells.iter().collect::<Vec<_>>()[..] else {
            anyhow::bail!("expected one cell, got {}", cells.len());
        };
        assert!(matches!(
            cell.spec.runtime_flags,
            Some(pipette_plan_types::RuntimeFlags::VlThroughputLlamacppCliStockToolsGgufVision(_))
        ));
        let Err(crate::PlanParseError::Structure(SchedulerPlanError::Variant {
            index: 0,
            error: VariantError::RuntimeFlagsMatchNoCell,
        })) = crate::parse(&text, &crate::prefix_types(&text))
        else {
            anyhow::bail!("expected the prefix type to refuse the vl_throughput entry");
        };
        Ok(())
    }

    #[test]
    fn an_unprefixed_id_is_checked_against_its_catalog_type() -> anyhow::Result<()> {
        let untyped = crate::parse(UNPREFIXED_VL_PLAN, &BTreeMap::new())?;
        assert!(
            untyped
                .runnable_cells()?
                .iter()
                .all(|cell| cell.spec.runtime_flags.is_none()),
            "an id outside the types is untyped, refused by ingestion, and takes no flags"
        );
        let Err(crate::PlanParseError::Structure(SchedulerPlanError::Variant {
            index: 0,
            error: VariantError::RuntimeFlagsMatchNoCell,
        })) = crate::parse(
            UNPREFIXED_VL_PLAN,
            &unprefixed_vl_types(BenchmarkType::VlMaxMemory)?,
        )
        else {
            anyhow::bail!("expected the vl_throughput entry to match no cell");
        };
        Ok(())
    }

    #[test]
    fn a_vllm_vlm_eval_variant_takes_its_flags() -> anyhow::Result<()> {
        // The same cell on vLLM: a repo's own weights, the client-side fan-out,
        // the thinking mode and the HTTP timeout.
        let toml = r#"
benchmarks = ["vlm_eval_mmstar"]

[[variants]]
requires = ["tag:cluster-amd"]
models   = [{ type = "torch", source = "huggingface", org = "Qwen", repo_name = "Qwen3.5-2B" }]
runtimes = [{ type = "uv_vllm", server_version = "0.31.0", build = "rocm723", python_version = "3.12", source = { type = "pip_requirements_text", contents = "vllm==0.31.0" } }]
runtime_flags = [{ benchmark_type = "vlm_eval", runtime_type = "uv_vllm", model_type = "torch", parallel = 64, max_model_len = 49152 }]
model_flags = [{ benchmark_type = "vlm_eval", model_type = "torch", enable_thinking = true }]
benchmark_flags = [{ benchmark_type = "vlm_eval", runtime_type = "uv_vllm", model_type = "torch", http_timeout_seconds = 1800 }]
"#;
        let cells = SchedulerPlan::parse(toml)?.runnable_cells()?;
        let [cell] = cells.iter().collect::<Vec<_>>()[..] else {
            anyhow::bail!("expected one cell, got {}", cells.len());
        };
        assert!(matches!(
            cell.spec.runtime_flags,
            Some(pipette_plan_types::RuntimeFlags::VlmEvalUvVllmTorch { .. })
        ));
        assert!(matches!(
            cell.spec.model_flags,
            Some(pipette_plan_types::ModelFlags::VlmEvalTorch {
                enable_thinking: Some(true)
            })
        ));
        assert!(cell.spec.benchmark_flags.is_some());
        Ok(())
    }

    #[test]
    fn set_equal_exclusions_dedup_to_one_cell() -> anyhow::Result<()> {
        // `not_all_of` is a set of sets: group order, member order and a repeat
        // all normalize away.
        let toml = r#"
benchmarks = ["decode_throughput_512_100"]

[[variants]]
requires = ["os:macos"]
exclude_clients = ["ev1_b", "ev1_a"]
excludes = ["gpu:none", "job_retry"]
not_all_of = [["runtime:llama_cpp", "job_retry"], ["gpu:none"]]
models   = [{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "r" }]
runtimes = [{ type = "mlx_macos_pipette", version = "0.20.0", flavor = "macos-arm64", source = { type = "pip_requirements_text", contents = "mlx-lm==0.20.0" } }]

[[variants]]
requires = ["os:macos"]
exclude_clients = ["ev1_a", "ev1_b", "ev1_a"]
excludes = ["job_retry", "gpu:none"]
not_all_of = [["gpu:none"], ["job_retry", "runtime:llama_cpp"]]
models   = [{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "r" }]
runtimes = [{ type = "mlx_macos_pipette", version = "0.20.0", flavor = "macos-arm64", source = { type = "pip_requirements_text", contents = "mlx-lm==0.20.0" } }]
"#;
        let cells = SchedulerPlan::parse(toml)?.runnable_cells()?;
        assert_eq!(cells.len(), 1, "set-equal exclusions should dedup");
        Ok(())
    }

    #[test]
    fn an_exclusion_keeps_two_variants_apart() -> anyhow::Result<()> {
        // Two cells, as the server's content key keeps the same two bodies apart.
        let toml = r#"
benchmarks = ["decode_throughput_512_100"]

[[variants]]
requires = ["os:macos"]
models   = [{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "r" }]
runtimes = [{ type = "mlx_macos_pipette", version = "0.20.0", flavor = "macos-arm64", source = { type = "pip_requirements_text", contents = "mlx-lm==0.20.0" } }]

[[variants]]
requires = ["os:macos"]
exclude_clients = ["ev1_dead"]
models   = [{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "r" }]
runtimes = [{ type = "mlx_macos_pipette", version = "0.20.0", flavor = "macos-arm64", source = { type = "pip_requirements_text", contents = "mlx-lm==0.20.0" } }]
"#;
        let cells = SchedulerPlan::parse(toml)?.runnable_cells()?;
        assert_eq!(cells.len(), 2, "a denial is part of cell identity");
        Ok(())
    }

    #[test]
    fn cells_keep_the_document_order_and_its_repeats() -> anyhow::Result<()> {
        let toml = r#"
benchmarks = ["end_to_end_latency_512_256", "decode_throughput_512_100"]

[[variants]]
requires = ["os:macos"]
models   = [
  { type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "b" },
  { type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "a" },
]
runtimes = [{ type = "mlx_macos_pipette", version = "0.20.0", flavor = "macos-arm64", source = { type = "pip_requirements_text", contents = "mlx-lm==0.20.0" } }]

[[variants]]
requires   = ["os:macos"]
models     = [{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "b" }]
runtimes   = [{ type = "mlx_macos_pipette", version = "0.20.0", flavor = "macos-arm64", source = { type = "pip_requirements_text", contents = "mlx-lm==0.20.0" } }]
benchmarks = ["decode_throughput_512_100"]
"#;
        let plan = SchedulerPlan::parse(toml)?;
        let order: Vec<(String, String)> = plan
            .cells()?
            .iter()
            .map(|cell| (cell.spec.benchmark.to_string(), cell.spec.model.to_string()))
            .collect();
        let expected = [
            ("end_to_end_latency_512_256", "LiquidAI/b"),
            ("end_to_end_latency_512_256", "LiquidAI/a"),
            ("decode_throughput_512_100", "LiquidAI/b"),
            ("decode_throughput_512_100", "LiquidAI/a"),
            ("decode_throughput_512_100", "LiquidAI/b"),
        ]
        .map(|(benchmark, model)| (benchmark.to_owned(), model.to_owned()));
        assert_eq!(order, expected);
        assert_eq!(plan.runnable_cells()?.len(), 4, "the repeat merges");
        Ok(())
    }

    fn plan_with(line: &str) -> String {
        format!(
            r#"{line}
benchmarks = ["decode_throughput_512_100"]

[[variants]]
requires = ["os:macos"]
models   = [{{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "r" }}]
runtimes = [{{ type = "mlx_macos_pipette", version = "0.20.0", flavor = "macos-arm64", source = {{ type = "pip_requirements_text", contents = "mlx-lm==0.20.0" }} }}]
"#
        )
    }

    #[rstest::rstest]
    #[case::quoted_rfc3339(r#"expires_at = "2026-08-01T00:00:00Z""#, Some("2026-08-01T00:00:00Z"))]
    #[case::native_toml_datetime("expires_at = 2026-08-01T00:00:00Z", Some("2026-08-01T00:00:00Z"))]
    #[case::never(r#"expires_at = "never""#, Some("never"))]
    #[case::omitted("", None)]
    fn expires_at_accepts_every_spelling(
        #[case] line: &str,
        #[case] expected: Option<&str>,
    ) -> anyhow::Result<()> {
        let parsed = SchedulerPlan::parse(&plan_with(line))?.expires_at;
        let wire = parsed.map(PlanExpiresAt::to_wire).transpose()?;
        assert_eq!(wire.as_deref(), expected);
        Ok(())
    }

    /// Both `expires_at` spellings survive a serialize/parse round trip, which
    /// covers the hand-written `Serialize` in each variant.
    #[rstest::rstest]
    #[case::instant(representative_plan())]
    #[case::never(plan_with(r#"expires_at = "never""#))]
    #[case::client_versions(plan_with_client_versions(r#"["2.0", "1.0"]"#))]
    fn round_trips_through_toml(#[case] toml: String) -> anyhow::Result<()> {
        let plan = SchedulerPlan::parse(&toml)?;
        let reserialized = toml::to_string(&plan)?;
        let reparsed = SchedulerPlan::parse(&reserialized)?;
        assert_eq!(plan, reparsed);
        Ok(())
    }

    #[rstest::rstest]
    #[case::requires_only(r#"requires = ["os:macos"]"#)]
    #[case::clients_only(r#"clients  = ["ev1_abc"]"#)]
    fn a_variant_with_one_eligibility_clause_is_accepted(
        #[case] eligibility: &str,
    ) -> anyhow::Result<()> {
        SchedulerPlan::parse(&format!(
            r#"
benchmarks = ["decode_throughput_512_100"]

[[variants]]
{eligibility}
models   = [{{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "r" }}]
runtimes = [{{ type = "mlx_macos_pipette", version = "0.20.0", flavor = "macos-arm64", source = {{ type = "pip_requirements_text", contents = "mlx-lm==0.20.0" }} }}]
"#
        ))?;
        Ok(())
    }

    /// Each case asserts a distinguishing substring, so it can't pass because
    /// the TOML failed for an unrelated reason.
    #[rstest::rstest]
    #[case::no_eligibility(
        r#"benchmarks = ["decode_throughput_512_100"]
[[variants]]
models   = [{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "r" }]
runtimes = [{ type = "mlx_macos_pipette", version = "0.20.0", flavor = "macos-arm64", source = { type = "pip_requirements_text", contents = "mlx-lm==0.20.0" } }]"#,
        &["requires", "clients"]
    )]
    #[case::non_canonical_flag(
        r#"benchmarks = ["decode_throughput_512_100"]
[[variants]]
requires = ["os:iOS"]
models   = [{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "r" }]
runtimes = [{ type = "mlx_macos_pipette", version = "0.20.0", flavor = "macos-arm64", source = { type = "pip_requirements_text", contents = "mlx-lm==0.20.0" } }]"#,
        &["CapabilityFlag"]
    )]
    #[case::incompatible_model_runtime(
        r#"benchmarks = ["decode_throughput_512_100"]
[[variants]]
requires = ["os:macos"]
models   = [{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "r" }]
runtimes = [{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b5000", flavor = "macos-arm64" }]"#,
        &["no compatible runtime"]
    )]
    #[case::orphan_runtime(
        r#"benchmarks = ["decode_throughput_512_100"]
[[variants]]
requires = ["os:macos"]
models   = [{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "r" }]
runtimes = [
  { type = "mlx_macos_pipette", version = "0.20.0", flavor = "macos-arm64", source = { type = "pip_requirements_text", contents = "mlx-lm==0.20.0" } },
  { type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b5000", flavor = "macos-arm64" },
]"#,
        &["no compatible model"]
    )]
    // A local-dispatch-only key: the distinct formats reject each other's docs.
    #[case::transports_key(
        r#"benchmarks = ["decode_throughput_512_100"]
[[transports]]
client_id   = "mac-1"
type        = "local"
binary_path = "/bin/pipette"
work_dir    = "/tmp"
[[variants]]
requires = ["os:macos"]
models   = [{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "r" }]
runtimes = [{ type = "mlx_macos_pipette", version = "0.20.0", flavor = "macos-arm64", source = { type = "pip_requirements_text", contents = "mlx-lm==0.20.0" } }]"#,
        &["unknown field `transports`"]
    )]
    #[case::malformed_expires_at(
        r#"expires_at = "soon"
benchmarks = ["decode_throughput_512_100"]
[[variants]]
requires = ["os:macos"]
models   = [{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "r" }]
runtimes = [{ type = "mlx_macos_pipette", version = "0.20.0", flavor = "macos-arm64", source = { type = "pip_requirements_text", contents = "mlx-lm==0.20.0" } }]"#,
        &["never", "soon"]
    )]
    // The keyword matches lowercase exactly, as `pipette-mgmt` ingestion does.
    #[case::miscased_never(
        r#"expires_at = "Never"
benchmarks = ["decode_throughput_512_100"]
[[variants]]
requires = ["os:macos"]
models   = [{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "r" }]
runtimes = [{ type = "mlx_macos_pipette", version = "0.20.0", flavor = "macos-arm64", source = { type = "pip_requirements_text", contents = "mlx-lm==0.20.0" } }]"#,
        &["never", "Never"]
    )]
    // A local datetime with no offset fails the RFC 3339 parse.
    #[case::naive_datetime(
        r#"expires_at = 2026-08-01T00:00:00
benchmarks = ["decode_throughput_512_100"]
[[variants]]
requires = ["os:macos"]
models   = [{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "r" }]
runtimes = [{ type = "mlx_macos_pipette", version = "0.20.0", flavor = "macos-arm64", source = { type = "pip_requirements_text", contents = "mlx-lm==0.20.0" } }]"#,
        &["never", "2026-08-01T00:00:00"]
    )]
    // Serde's own type error carries the field, so the opt-out needs no rule.
    #[case::non_boolean_merge_duplicates(
        r#"merge_duplicates = "yes"
benchmarks = ["decode_throughput_512_100"]
[[variants]]
requires = ["os:macos"]
models   = [{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "r" }]
runtimes = [{ type = "mlx_macos_pipette", version = "0.20.0", flavor = "macos-arm64", source = { type = "pip_requirements_text", contents = "mlx-lm==0.20.0" } }]"#,
        &["merge_duplicates", "expected a boolean"]
    )]
    #[case::excludes_not_an_array(
        r#"benchmarks = ["decode_throughput_512_100"]
[[variants]]
requires = ["os:macos"]
excludes = "os:ios"
models   = [{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "r" }]
runtimes = [{ type = "mlx_macos_pipette", version = "0.20.0", flavor = "macos-arm64", source = { type = "pip_requirements_text", contents = "mlx-lm==0.20.0" } }]"#,
        &["excludes", "sequence"]
    )]
    #[case::exclude_clients_not_an_array(
        r#"benchmarks = ["decode_throughput_512_100"]
[[variants]]
requires = ["os:macos"]
exclude_clients = "ev1_abc"
models   = [{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "r" }]
runtimes = [{ type = "mlx_macos_pipette", version = "0.20.0", flavor = "macos-arm64", source = { type = "pip_requirements_text", contents = "mlx-lm==0.20.0" } }]"#,
        &["exclude_clients", "sequence"]
    )]
    // An empty id is the right shape, so the `ClientId` newtype rejects it.
    #[case::exclude_clients_empty_id(
        r#"benchmarks = ["decode_throughput_512_100"]
[[variants]]
requires = ["os:macos"]
exclude_clients = [""]
models   = [{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "r" }]
runtimes = [{ type = "mlx_macos_pipette", version = "0.20.0", flavor = "macos-arm64", source = { type = "pip_requirements_text", contents = "mlx-lm==0.20.0" } }]"#,
        &["exclude_clients", "client id must not be empty"]
    )]
    #[case::not_all_of_not_grouped(
        r#"benchmarks = ["decode_throughput_512_100"]
[[variants]]
requires = ["os:macos"]
not_all_of = ["os:ios"]
models   = [{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "r" }]
runtimes = [{ type = "mlx_macos_pipette", version = "0.20.0", flavor = "macos-arm64", source = { type = "pip_requirements_text", contents = "mlx-lm==0.20.0" } }]"#,
        &["not_all_of", "sequence"]
    )]
    // The empty *list* of groups excludes nobody, so the rule reads the groups.
    #[case::empty_not_all_of_group(
        r#"benchmarks = ["decode_throughput_512_100"]
[[variants]]
requires = ["os:macos"]
not_all_of = [[]]
models   = [{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "r" }]
runtimes = [{ type = "mlx_macos_pipette", version = "0.20.0", flavor = "macos-arm64", source = { type = "pip_requirements_text", contents = "mlx-lm==0.20.0" } }]"#,
        &["variant 0", "empty `not_all_of` group excludes every client"]
    )]
    // A variant that only excludes matches nobody, as one with neither does.
    #[case::exclusion_only_variant(
        r#"benchmarks = ["decode_throughput_512_100"]
[[variants]]
excludes = ["os:ios"]
exclude_clients = ["ev1_abc"]
models   = [{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "r" }]
runtimes = [{ type = "mlx_macos_pipette", version = "0.20.0", flavor = "macos-arm64", source = { type = "pip_requirements_text", contents = "mlx-lm==0.20.0" } }]"#,
        &["requires", "clients"]
    )]
    #[case::missing_benchmarks(
        r#"[[variants]]
requires = ["os:macos"]
models   = [{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "r" }]
runtimes = [{ type = "mlx_macos_pipette", version = "0.20.0", flavor = "macos-arm64", source = { type = "pip_requirements_text", contents = "mlx-lm==0.20.0" } }]"#,
        &["variant 0: benchmarks must be set"]
    )]
    // Refused as it is read, so the document's own line names it.
    #[case::client_version_uppercase(
        &plan_with_client_versions(r#"["2026.10.02-ABC"]"#),
        &["client_versions", "\"2026.10.02-ABC\"", "canonical"]
    )]
    #[case::client_version_whitespace(
        &plan_with_client_versions(r#"["1.0 beta"]"#),
        &["client_versions", "\"1.0 beta\"", "canonical"]
    )]
    // `client_version:` is canonical, but it names no build.
    #[case::client_version_empty(
        &plan_with_client_versions(r#"[""]"#),
        &["client_versions", "value \"\"", "non-empty"]
    )]
    #[case::client_version_repeated(
        &plan_with_client_versions(r#"["1.0", "2.0", "1.0"]"#),
        &["variant 0", "lists \"1.0\" twice"]
    )]
    #[case::client_versions_not_an_array(
        &plan_with_client_versions(r#""1.0""#),
        &["client_versions", "sequence"]
    )]
    #[case::duplicate_runtime_flags(
        r#"benchmarks = ["prefill_throughput_512"]
[[variants]]
requires = ["os:macos"]
models   = [{ type = "gguf_text", source = "huggingface", org = "o", repo_name = "r", path = "a.gguf" }]
runtimes = [{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b9050", flavor = "macos-arm64" }]
runtime_flags = [
  { benchmark_type = "prefill_throughput", runtime_type = "llamacpp_cli_stock_tools", model_type = "gguf_text", threads = 4 },
  { benchmark_type = "prefill_throughput", runtime_type = "llamacpp_cli_stock_tools", model_type = "gguf_text", threads = 8 },
]"#,
        &["variant 0: duplicate runtime_flags entry for the same cell triple"]
    )]
    fn rejects_invalid_scheduler_plan(
        #[case] toml: &str,
        #[case] expect_substrings: &[&str],
    ) -> anyhow::Result<()> {
        let err = SchedulerPlan::parse(toml)
            .err()
            .context("expected the plan to be rejected")?;
        let msg = format!("{err:#}");
        expect_substrings.iter().for_each(|needle| {
            assert!(msg.contains(needle), "error missing {needle:?}: {msg}");
        });
        Ok(())
    }

    /// The intake skips serde's `try_from`, which would make the error a string.
    /// No top-level `benchmarks`, so only `no_benchmarks` reaches that check.
    #[rstest::rstest]
    #[case::no_eligibility(
        "",
        |e: &SchedulerPlanError| matches!(e, SchedulerPlanError::NoEligibility { index: 1 })
    )]
    #[case::empty_not_all_of_group(
        "requires = [\"os:macos\"]\nnot_all_of = [[]]",
        |e: &SchedulerPlanError| matches!(e, SchedulerPlanError::EmptyNotAllOfGroup { index: 1 })
    )]
    #[case::duplicate_client_version(
        "requires = [\"os:macos\"]\nclient_versions = [\"1.0\", \"1.0\"]",
        |e: &SchedulerPlanError| matches!(
            e,
            SchedulerPlanError::DuplicateClientVersion { index: 1, version } if version == "1.0"
        )
    )]
    #[case::no_benchmarks(
        "requires = [\"os:macos\"]",
        |e: &SchedulerPlanError| matches!(
            e,
            SchedulerPlanError::Variant { index: 1, error: VariantError::NoBenchmarks }
        )
    )]
    fn validation_reports_a_typed_fault(
        #[case] lines: &str,
        #[case] expected: fn(&SchedulerPlanError) -> bool,
    ) -> anyhow::Result<()> {
        let mlx = r#"models   = [{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "r" }]
runtimes = [{ type = "mlx_macos_pipette", version = "0.20.0", flavor = "macos-arm64", source = { type = "pip_requirements_text", contents = "mlx-lm==0.20.0" } }]"#;
        let document = format!(
            "[[variants]]\nrequires = [\"os:macos\"]\nbenchmarks = [\"decode_throughput_512_100\"]\n{mlx}\n\n[[variants]]\n{lines}\n{mlx}\n"
        );
        let intake: SchedulerPlanIntake = toml::from_str(&document)?;
        let error = SchedulerPlan::from_intake(intake, &crate::prefix_types(&document))
            .err()
            .context("expected the plan to be rejected")?;
        anyhow::ensure!(expected(&error), "unexpected rejection {error:?}");
        Ok(())
    }

    fn plan_with_client_versions(versions: &str) -> String {
        format!(
            r#"benchmarks = ["decode_throughput_512_100"]
[[variants]]
requires = ["os:macos"]
client_versions = {versions}
models   = [{{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "r" }}]
runtimes = [{{ type = "mlx_macos_pipette", version = "0.20.0", flavor = "macos-arm64", source = {{ type = "pip_requirements_text", contents = "mlx-lm==0.20.0" }} }}]
"#
        )
    }

    /// The planner names no server crate, so it keeps a copy of the namespace.
    #[test]
    fn the_client_version_namespace_is_the_servers() {
        assert_eq!(
            CLIENT_VERSION_NAMESPACE,
            pipette_core::client::CLIENT_VERSION_NAMESPACE
        );
    }

    /// The listed versions are a set: order does not split a cell, and a
    /// different set does.
    #[rstest::rstest]
    #[case::reordered(r#"["2.0", "1.0"]"#, 1)]
    #[case::another_set(r#"["1.0", "3.0"]"#, 2)]
    #[case::none_listed("[]", 2)]
    fn client_versions_are_part_of_cell_identity(
        #[case] second: &str,
        #[case] cells: usize,
    ) -> anyhow::Result<()> {
        let variant = |versions: &str| {
            format!(
                r#"
[[variants]]
requires = ["os:macos"]
client_versions = {versions}
models   = [{{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "r" }}]
runtimes = [{{ type = "mlx_macos_pipette", version = "0.20.0", flavor = "macos-arm64", source = {{ type = "pip_requirements_text", contents = "mlx-lm==0.20.0" }} }}]
"#
            )
        };
        let toml = format!(
            "benchmarks = [\"decode_throughput_512_100\"]\n{}{}",
            variant(r#"["1.0", "2.0"]"#),
            variant(second)
        );
        assert_eq!(SchedulerPlan::parse(&toml)?.runnable_cells()?.len(), cells);
        Ok(())
    }

    /// The cell carries the versions as `client_version:` flags, sorted.
    #[test]
    fn a_cell_carries_its_client_version_flags() -> anyhow::Result<()> {
        let plan = SchedulerPlan::parse(&plan_with_client_versions(r#"["2.0", "1.0"]"#))?;
        let cell = plan
            .runnable_cells()?
            .into_iter()
            .next()
            .context("one cell")?;
        let flags: Vec<&str> = cell
            .eligibility
            .client_versions()
            .iter()
            .map(CapabilityFlag::as_ref)
            .collect();
        assert_eq!(flags, ["client_version:1.0", "client_version:2.0"]);
        Ok(())
    }

    fn scheduler_example_paths() -> anyhow::Result<Vec<std::path::PathBuf>> {
        let dir = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../examples/plans/scheduler"
        );
        let paths = std::fs::read_dir(dir)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::io::Result<Vec<_>>>()?
            .into_iter()
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("toml"))
            .collect::<Vec<_>>();
        assert!(!paths.is_empty(), "no scheduler example plans in {dir}");
        Ok(paths)
    }

    /// Expanding, not just parsing, catches a plan that expands to nothing.
    #[test]
    fn shipped_scheduler_examples_expand() -> anyhow::Result<()> {
        scheduler_example_paths()?.iter().try_for_each(|path| {
            let toml = std::fs::read_to_string(path)?;
            let plan = SchedulerPlan::parse(&toml)
                .map_err(|e| anyhow::anyhow!("{}: {e:#}", path.display()))?;
            let cells = plan
                .runnable_cells()
                .map_err(|e| anyhow::anyhow!("{}: {e:#}", path.display()))?;
            anyhow::ensure!(
                !cells.is_empty(),
                "{}: produced no runnable cells",
                path.display()
            );
            anyhow::Ok(())
        })
    }

    /// Keeps the format split true of real documents, not just fragments.
    #[test]
    fn a_scheduler_example_does_not_parse_as_a_local_dispatch_plan() -> anyhow::Result<()> {
        scheduler_example_paths()?.iter().try_for_each(|path| {
            let toml = std::fs::read_to_string(path)?;
            anyhow::ensure!(
                pipette_plan_types::Plan::parse(&toml).is_err(),
                "{} parsed as a local-dispatch plan",
                path.display()
            );
            anyhow::Ok(())
        })
    }
}
