//! The local-dispatch [`Plan`] and its supporting types, re-exported flat
//! from the crate root.

mod runnable_cell;
mod transport;

pub use runnable_cell::{RunnableCell, TypedCell};
pub use transport::{ShellType, TransportConfig};

use std::{
    collections::{BTreeSet, HashSet},
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{
    AuthToken, BenchmarkFlags, BenchmarkId, BenchmarkType, ClientId, Model, ModelFlags,
    NonEmptyVec, Runtime, RuntimeFlags, VariantError, benchmark_type_of, resolve_benchmark_flags,
    resolve_benchmarks, resolve_model_flags, resolve_runtime_flags, runnable_pairs_of,
    validate_variant_flag_cells,
};

/// One block of models, runtimes, clients and an optional benchmark
/// override, expanded to `models × runtimes × clients × benchmarks`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub struct Variant {
    pub models: NonEmptyVec<Model>,
    pub runtimes: NonEmptyVec<Runtime>,
    pub clients: NonEmptyVec<ClientId>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_benchmark_ids"
    )]
    pub benchmarks: Option<NonEmptyVec<BenchmarkId>>,
    /// Per-cell runtime flags, each naming its `(benchmark, runtime, model)`
    /// cell; every entry must match a real cell in this variant. Shipped to
    /// the client as `--runtime-flags`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub runtime_flags: Vec<RuntimeFlags>,
    /// Per-cell model-generation flags keyed on `(benchmark, model)`, eval
    /// cells only; forwarded on the eval invocation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub model_flags: Vec<ModelFlags>,
    /// Per-cell run flags keyed on `(benchmark, runtime, model)`, such as
    /// pipette's HTTP-client timeout.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub benchmark_flags: Vec<BenchmarkFlags>,
}

impl Variant {
    /// The (model, runtime) pairs filtered by [`is_compatible`](crate::is_compatible).
    /// Errors on an orphan model or runtime (no compatible counterpart), almost
    /// certainly an authoring mistake; other incompatible pairs drop silently.
    pub fn runnable_pairs(&self) -> Result<Vec<(&Model, &Runtime)>, VariantError> {
        runnable_pairs_of(&self.models, &self.runtimes)
    }

    fn resolved_runtime_flags(
        &self,
        benchmark: Option<BenchmarkType>,
        runtime: &Runtime,
        model: &Model,
    ) -> Option<RuntimeFlags> {
        resolve_runtime_flags(&self.runtime_flags, benchmark, runtime, model)
    }

    fn resolved_model_flags(
        &self,
        benchmark: Option<BenchmarkType>,
        model: &Model,
    ) -> Option<ModelFlags> {
        resolve_model_flags(&self.model_flags, benchmark, model)
    }

    fn resolved_benchmark_flags(
        &self,
        benchmark: Option<BenchmarkType>,
        runtime: &Runtime,
        model: &Model,
    ) -> Option<BenchmarkFlags> {
        resolve_benchmark_flags(&self.benchmark_flags, benchmark, runtime, model)
    }
}

/// The matrix portion of a plan: a default benchmark list and the variants
/// whose cartesian product expands to cells. Flattened into [`Plan`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub struct Matrix {
    /// Default benchmark list, overridden by a variant's own `benchmarks`;
    /// when absent, every variant must provide one.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_benchmark_ids"
    )]
    pub benchmarks: Option<NonEmptyVec<BenchmarkId>>,
    pub variants: NonEmptyVec<Variant>,
}

/// Why a document is not a valid local-dispatch [`Plan`]. Validation failures
/// reach `parse`/`load` inside `Parse`/`ParseFile`: serde's `try_from` stringifies them.
#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    #[error("reading {}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parsing {}", path.display())]
    ParseFile {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error(transparent)]
    Parse(#[from] toml::de::Error),
    #[error("duplicate transport client_id {client_id:?}")]
    DuplicateTransport { client_id: String },
    #[error(
        "variant {variant}: clients refers to unknown transport {client:?}; \
         declared transports: {declared:?}"
    )]
    UnknownClient {
        variant: usize,
        client: String,
        declared: BTreeSet<String>,
    },
    #[error(
        "transport {client_id:?}: adb_over_ssh requires shell = \"posix\" \
         (it describes the Android device shell)"
    )]
    NonPosixAdbHop { client_id: String },
    // `error`, not `source`: the message already carries it, so a chain would repeat it.
    #[error("variant {index}: {error}")]
    Variant { index: usize, error: VariantError },
}

/// A validated plan: transport `client_id`s are unique and every variant's
/// `clients` entry names a declared transport. `#[non_exhaustive]` blocks
/// struct literals, so every construction path runs `validate_plan`.
#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, try_from = "PlanIntake")]
#[non_exhaustive]
pub struct Plan {
    pub plan_id: String,
    #[serde(default, skip_serializing_if = "RetryConfig::is_default")]
    pub retry: RetryConfig,
    pub transports: NonEmptyVec<TransportConfig>,
    /// Flattened: `benchmarks` and `variants` sit at the TOML root.
    #[serde(flatten)]
    pub matrix: Matrix,
}

/// Unvalidated serde intake: `try_from` forces every `Plan` deserialization
/// through `validate_plan`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanIntake {
    plan_id: String,
    #[serde(default)]
    retry: RetryConfig,
    transports: NonEmptyVec<TransportConfig>,
    #[serde(flatten)]
    matrix: Matrix,
}

impl TryFrom<PlanIntake> for Plan {
    type Error = PlanError;

    fn try_from(intake: PlanIntake) -> Result<Self, Self::Error> {
        let plan = Plan {
            plan_id: intake.plan_id,
            retry: intake.retry,
            transports: intake.transports,
            matrix: intake.matrix,
        };
        validate_plan(&plan)?;
        Ok(plan)
    }
}

#[derive(Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RetryConfig {
    #[serde(default)]
    pub max_consecutive_failures: usize,
}

impl RetryConfig {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

impl Plan {
    pub fn load(path: &Path) -> Result<Self, PlanError> {
        let raw = fs::read_to_string(path).map_err(|source| PlanError::Read {
            path: path.to_owned(),
            source,
        })?;
        toml::from_str(&raw).map_err(|source| PlanError::ParseFile {
            path: path.to_owned(),
            source,
        })
    }

    pub fn parse(toml_str: &str) -> Result<Self, PlanError> {
        Ok(toml::from_str(toml_str)?)
    }

    /// Every executable cell (variant × benchmark × model × runtime), filtered
    /// by compatibility. Errors on an orphan model or runtime rather than
    /// silently dropping its cells.
    pub fn cells(&self) -> Result<Vec<TypedCell<'_>>, PlanError> {
        self.matrix
            .variants
            .iter()
            .enumerate()
            .map(|(idx, variant)| {
                let pairs = variant.runnable_pairs().map_err(in_variant(idx))?;
                let benchmarks = self.benchmarks_for_variant(idx, variant)?;
                Ok(benchmarks
                    .iter()
                    .flat_map(|benchmark| {
                        pairs.iter().map(move |(model, runtime)| TypedCell {
                            variant_idx: idx,
                            benchmark,
                            model,
                            runtime,
                            allowed_clients: variant.clients.as_ref(),
                        })
                    })
                    .collect::<Vec<_>>())
            })
            .collect::<Result<Vec<_>, PlanError>>()
            .map(|chunks| chunks.into_iter().flatten().collect())
    }

    /// The deduped cells the runner processes; errors as [`Plan::cells`].
    pub fn runnable_cells(&self) -> Result<HashSet<RunnableCell>, PlanError> {
        self.matrix
            .variants
            .iter()
            .enumerate()
            .map(|(idx, variant)| {
                let pairs = variant.runnable_pairs().map_err(in_variant(idx))?;
                let benchmarks = self.benchmarks_for_variant(idx, variant)?;
                let allowed_clients: Vec<ClientId> = variant.clients.iter().cloned().collect();
                Ok(benchmarks
                    .iter()
                    .flat_map(|benchmark| {
                        let allowed_clients = allowed_clients.clone();
                        let bt = benchmark_type_of(benchmark);
                        pairs.iter().map(move |(model, runtime)| RunnableCell {
                            benchmark: benchmark.clone(),
                            model: (*model).clone(),
                            runtime: (*runtime).clone(),
                            allowed_clients: allowed_clients.clone(),
                            runtime_flags: variant.resolved_runtime_flags(bt, runtime, model),
                            model_flags: variant.resolved_model_flags(bt, model),
                            benchmark_flags: variant.resolved_benchmark_flags(bt, runtime, model),
                        })
                    })
                    .collect::<HashSet<_>>())
            })
            .collect::<Result<Vec<_>, PlanError>>()
            .map(|sets| sets.into_iter().flatten().collect())
    }

    /// The first model access token, forwarded as [`HF_TOKEN_ENV`](crate::HF_TOKEN_ENV);
    /// a plan is expected to use a single HF account.
    pub fn auth_token(&self) -> Option<&AuthToken> {
        self.matrix
            .variants
            .iter()
            .flat_map(|v| v.models.iter())
            .find_map(|m| m.auth_token())
    }

    fn benchmarks_for_variant<'a>(
        &'a self,
        variant_idx: usize,
        variant: &'a Variant,
    ) -> Result<&'a [BenchmarkId], PlanError> {
        resolve_benchmarks(
            variant.benchmarks.as_deref(),
            self.matrix.benchmarks.as_deref(),
        )
        .map_err(in_variant(variant_idx))
    }
}

fn in_variant(index: usize) -> impl FnOnce(VariantError) -> PlanError {
    move |error| PlanError::Variant { index, error }
}

fn validate_plan(plan: &Plan) -> Result<(), PlanError> {
    // try_fold doubles as dup-detection and as the membership index
    // for the variant→client pass below.
    let names: HashSet<&ClientId> =
        plan.transports
            .iter()
            .try_fold(HashSet::new(), |mut acc, t| {
                if acc.insert(t.client_id()) {
                    Ok(acc)
                } else {
                    Err(PlanError::DuplicateTransport {
                        client_id: t.client_id().as_str().to_owned(),
                    })
                }
            })?;

    let unknown = plan
        .matrix
        .variants
        .iter()
        .enumerate()
        .flat_map(|(idx, v)| v.clients.iter().map(move |c| (idx, c)))
        .find(|(_, c)| !names.contains(c));
    if let Some((idx, client)) = unknown {
        return Err(PlanError::UnknownClient {
            variant: idx,
            client: client.as_str().to_owned(),
            declared: names.iter().map(|c| c.as_str().to_owned()).collect(),
        });
    }

    // Surface orphans at parse time rather than at first expansion.
    plan.cells().map(|_| ())?;
    reject_non_posix_adb_hop(plan)?;
    validate_variant_flags(plan)
}

/// `adb_over_ssh` renders the device command for posix on both hops, so a
/// `powershell` device would mangle it rather than reject it.
fn reject_non_posix_adb_hop(plan: &Plan) -> Result<(), PlanError> {
    match plan.transports.iter().find(|t| {
        matches!(
            t,
            TransportConfig::AdbOverSsh {
                shell: ShellType::PowerShell,
                ..
            }
        )
    }) {
        Some(t) => Err(PlanError::NonPosixAdbHop {
            client_id: t.client_id().as_str().to_owned(),
        }),
        None => Ok(()),
    }
}

/// Each variant's authored flags: no duplicate cells, and every entry matches
/// a real cell. Which knobs a cell accepts is checked by the flag types' `TryFrom`.
fn validate_variant_flags(plan: &Plan) -> Result<(), PlanError> {
    plan.matrix
        .variants
        .iter()
        .enumerate()
        .try_for_each(|(idx, variant)| {
            let pairs = variant.runnable_pairs().map_err(in_variant(idx))?;
            let benchmark_types: Vec<BenchmarkType> = plan
                .benchmarks_for_variant(idx, variant)?
                .iter()
                .filter_map(benchmark_type_of)
                .collect();
            validate_variant_flag_cells(
                &pairs,
                &benchmark_types,
                &variant.runtime_flags,
                &variant.model_flags,
                &variant.benchmark_flags,
            )
            .map_err(in_variant(idx))
        })
}

/// Deserialize `benchmarks`, saying how to fix a refused id: left to the
/// derive, an old-form `"remote/eval_smoke"` reads as a refused `/` on line 1.
fn deserialize_benchmark_ids<'de, D>(
    deserializer: D,
) -> Result<Option<NonEmptyVec<BenchmarkId>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let Some(raw) = Option::<Vec<String>>::deserialize(deserializer)? else {
        return Ok(None);
    };
    let ids = raw
        .into_iter()
        .map(|entry| {
            BenchmarkId::try_new(entry.clone()).map_err(|error| {
                serde::de::Error::custom(
                    if matches!(entry.split_once('/'), Some(("local" | "remote", _))) {
                        format!(
                            "benchmark \"{}\" is not a bare id; a plan distributes ids and \
                             the client resolves them, so drop any `local/` or `remote/` prefix",
                            entry.escape_default()
                        )
                    } else {
                        format!("benchmark \"{}\": {error}", entry.escape_default())
                    },
                )
            })
        })
        .collect::<Result<Vec<_>, D::Error>>()?;
    NonEmptyVec::try_new(ids)
        .map(Some)
        .map_err(serde::de::Error::custom)
}

/// A baseline valid plan TOML around one variant block; shared with
/// `model`'s parse tests.
#[cfg(test)]
pub(crate) fn plan_toml(variant_block: &str) -> String {
    format!(
        r#"benchmarks = ["prefill_throughput_256"]

[[variants]]
{variant_block}
"#
    )
}

#[cfg(test)]
fn single(cells: &HashSet<RunnableCell>) -> anyhow::Result<&RunnableCell> {
    assert_eq!(cells.len(), 1, "expected exactly one cell");
    cells
        .iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("non-empty after len-check"))
}

#[cfg(test)]
fn minimal_outer_plan() -> &'static str {
    r#"
plan_id          = "test"
benchmarks       = ["prefill_throughput_512"]

[[transports]]
client_id   = "mac-1"
type        = "ssh"
host        = "h"
user        = "u"
binary_path = "/bin/pipette"
work_dir    = "/tmp"
shell       = "posix"

[[variants]]
clients  = ["mac-1"]
models   = [{ type = "gguf_text", source = "huggingface", org = "org", repo_name = "repo", path = "Q4_K_M.gguf" }]
runtimes = [{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b9050", flavor = "macos-arm64" }]
"#
}

#[cfg(test)]
mod tests {
    use anyhow::Context;
    use rstest::rstest;

    use super::*;
    use crate::runtime::cli_release;
    use crate::*;

    fn cli_source(version: &str) -> anyhow::Result<LlamacppCliStockToolsSource> {
        Ok(LlamacppCliStockToolsSource::GithubRelease(cli_release(
            version,
        )?))
    }

    const MINIMAL_VARIANT: &str = r#"models = [{ type = "torch", source = "huggingface", org = "x", repo_name = "y" }]
runtimes = [{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b5000", flavor = "windows-x64-vulkan" }]
clients = ["ev1_c"]"#;

    fn minimal_plan_toml() -> String {
        plan_toml(MINIMAL_VARIANT)
    }

    fn gguf_text(repo: &str) -> anyhow::Result<Model> {
        Ok(Model::GgufText(GgufText {
            source: GgufTextSource::HuggingFace {
                repo: HfRepo::parse_org_repo(repo)?,
                path: RepoSubpath::try_new("a.gguf".to_owned())?,
                sha256: None,
            },
        }))
    }
    fn gguf_vision(repo: &str) -> anyhow::Result<Model> {
        Ok(Model::GgufVision(GgufVision {
            source: GgufVisionSource::HuggingFace(GgufVisionHfFiles {
                repo: HfRepo::parse_org_repo(repo)?,
                model: RepoSubpath::try_new("a.gguf".to_owned())?,
                model_sha256: None,
                mmproj: RepoSubpath::try_new("mm.gguf".to_owned())?,
                mmproj_sha256: None,
            }),
        }))
    }
    fn mlx_model(repo: &str) -> anyhow::Result<Model> {
        Ok(Model::Mlx(Mlx {
            source: ModelSource::HuggingFace {
                repo: HfRepo::parse_org_repo(repo)?,
                prefix: None,
            },
        }))
    }
    fn torch_model(repo: &str) -> anyhow::Result<Model> {
        Ok(Model::Torch(Torch {
            source: ModelSource::HuggingFace {
                repo: HfRepo::parse_org_repo(repo)?,
                prefix: None,
            },
        }))
    }
    fn llamacpp_rt() -> anyhow::Result<Runtime> {
        Ok(Runtime::LlamacppCliStockTools(LlamacppCliStockTools {
            source: cli_source("b9050")?,
            flavor: LlamaCppFlavor::MacosArm64,
        }))
    }
    fn mlx_rt() -> anyhow::Result<Runtime> {
        Ok(Runtime::MlxMacosPipette(MlxMacosPipette {
            version: NonEmptyString::try_new("0.31".to_owned())?,
            flavor: MlxMacosPipetteFlavor::MacosArm64,
            source: UvRuntimeSource::PipRequirementsText {
                contents: NonEmptyString::try_new("mlx-lm==0.31\n".to_owned())?,
                install_flags: None,
            },
        }))
    }
    fn uv_vllm_rt() -> anyhow::Result<Runtime> {
        Ok(Runtime::UvVllm(UvVllm {
            server_version: UvServerVersion::try_new("0.10.0".to_owned())?,
            build: UvBuild::try_new("cu121".to_owned())?,
            python_version: UvPythonVersion::try_new("3.12".to_owned())?,
            source: UvRuntimeSource::PipRequirementsText {
                contents: NonEmptyString::try_new("vllm==0.10.0\n".to_owned())?,
                install_flags: None,
            },
        }))
    }

    fn make_variant(models: Vec<Model>, runtimes: Vec<Runtime>) -> anyhow::Result<Variant> {
        Ok(Variant {
            models: NonEmptyVec::try_new(models)?,
            runtimes: NonEmptyVec::try_new(runtimes)?,
            clients: NonEmptyVec::try_new(vec![ClientId::try_new("ev1".to_owned())?])?,
            benchmarks: None,
            runtime_flags: vec![],
            model_flags: vec![],
            benchmark_flags: vec![],
        })
    }

    fn client_refs(cs: &[ClientId]) -> Vec<&str> {
        cs.iter().map(|c| c.as_ref()).collect()
    }

    #[test]
    fn non_empty_vec_fields_rejected_when_blank() {
        let cases: &[(&str, &str)] = &[
            (
                "empty models",
                "models = []\n\
                     runtimes = [{ type = \"llamacpp_cli_stock_tools\", source = \"github_release\", repository_url = \"github.com/ggml-org/llama.cpp\", version = \"b5000\", flavor = \"windows-x64-vulkan\" }]\n\
                     clients = [\"ev1_c\"]",
            ),
            (
                "empty runtimes",
                "models = [{ type = \"torch\", source = \"huggingface\", org = \"x\", repo_name = \"y\" }]\n\
                     runtimes = []\n\
                     clients = [\"ev1_c\"]",
            ),
            (
                "empty clients",
                "models = [{ type = \"torch\", source = \"huggingface\", org = \"x\", repo_name = \"y\" }]\n\
                     runtimes = [{ type = \"llamacpp_cli_stock_tools\", source = \"github_release\", repository_url = \"github.com/ggml-org/llama.cpp\", version = \"b5000\", flavor = \"windows-x64-vulkan\" }]\n\
                     clients = []",
            ),
            (
                "empty per-variant benchmarks",
                "models = [{ type = \"torch\", source = \"huggingface\", org = \"x\", repo_name = \"y\" }]\n\
                     runtimes = [{ type = \"llamacpp_cli_stock_tools\", source = \"github_release\", repository_url = \"github.com/ggml-org/llama.cpp\", version = \"b5000\", flavor = \"windows-x64-vulkan\" }]\n\
                     clients = [\"ev1_c\"]\n\
                     benchmarks = []",
            ),
        ];
        for (label, variant_block) in cases {
            let toml_str = plan_toml(variant_block);
            assert!(
                toml::from_str::<Matrix>(&toml_str).is_err(),
                "{label} must reject"
            );
        }
    }

    #[test]
    fn plan_rejects_empty_top_level_fields() {
        let toml_str = format!("benchmarks = []\n\n[[variants]]\n{MINIMAL_VARIANT}\n");
        assert!(toml::from_str::<Matrix>(&toml_str).is_err());

        let toml_str = r#"benchmarks = ["prefill_throughput_256"]
variants = []
"#;
        assert!(toml::from_str::<Matrix>(toml_str).is_err());
    }

    #[test]
    fn variant_benchmarks_override_when_set() -> anyhow::Result<()> {
        let toml_str = plan_toml(&format!(
            "{MINIMAL_VARIANT}\nbenchmarks = [\"decode_throughput_512_100\"]"
        ));
        let plan: Matrix = toml::from_str(&toml_str)?;
        let overrides = plan.variants[0]
            .benchmarks
            .as_ref()
            .context("override present")?;
        assert_eq!(overrides.len(), 1);
        assert_eq!(overrides[0].as_ref(), "decode_throughput_512_100");
        Ok(())
    }

    #[test]
    fn minimal_plan_is_valid() -> anyhow::Result<()> {
        let toml_str = minimal_plan_toml();
        toml::from_str::<Matrix>(&toml_str)?;
        Ok(())
    }

    #[derive(Debug, Clone, Copy)]
    enum Expected {
        Accept { pair_count: usize },
        Reject,
    }

    #[rstest::rstest]
    // Cases pass fallible builders: `?` cannot appear inside `#[case(...)]`.
    #[case::gguf_text_with_llamacpp(
            vec![gguf_text("org/repo")],
            vec![llamacpp_rt()],
            Expected::Accept { pair_count: 1 },
        )]
    #[case::gguf_vision_with_llamacpp(
            vec![gguf_vision("org/repo")],
            vec![llamacpp_rt()],
            Expected::Accept { pair_count: 1 },
        )]
    #[case::mlx_with_mlx(
            vec![mlx_model("org/repo")],
            vec![mlx_rt()],
            Expected::Accept { pair_count: 1 },
        )]
    #[case::torch_with_uv_vllm(
            vec![torch_model("org/repo")],
            vec![uv_vllm_rt()],
            Expected::Accept { pair_count: 1 },
        )]
    // Mixed kinds, fully matched: `pair_count` is the matched pairs, not the
    // full product (4).
    #[case::mixed_kinds_all_matched(
            vec![gguf_text("org/repo"), mlx_model("org/repo")],
            vec![llamacpp_rt(), mlx_rt()],
            Expected::Accept { pair_count: 2 },
        )]
    // Reject: a model has no compatible runtime in the variant.
    #[case::gguf_with_only_mlx_runtime(
            vec![gguf_text("org/repo")],
            vec![mlx_rt()],
            Expected::Reject,
        )]
    #[case::mlx_with_only_llamacpp(
            vec![mlx_model("org/repo")],
            vec![llamacpp_rt()],
            Expected::Reject,
        )]
    #[case::torch_with_only_llamacpp(
            vec![torch_model("org/repo")],
            vec![llamacpp_rt()],
            Expected::Reject,
        )]
    // Reject: a model dangles even though some other model finds its match.
    #[case::orphan_mlx_model_among_llamacpp(
            vec![gguf_text("org/repo"), mlx_model("org/repo")],
            vec![llamacpp_rt()],
            Expected::Reject,
        )]
    // Reject: a runtime dangles even though some other runtime finds its match.
    #[case::orphan_mlx_runtime_among_gguf(
            vec![gguf_text("org/repo")],
            vec![llamacpp_rt(), mlx_rt()],
            Expected::Reject,
        )]
    fn variant_runnable_pairs(
        #[case] models: Vec<anyhow::Result<Model>>,
        #[case] runtimes: Vec<anyhow::Result<Runtime>>,
        #[case] expected: Expected,
    ) -> anyhow::Result<()> {
        let models = models.into_iter().collect::<anyhow::Result<Vec<_>>>()?;
        let runtimes = runtimes.into_iter().collect::<anyhow::Result<Vec<_>>>()?;
        let variant = make_variant(models, runtimes)?;
        match (variant.runnable_pairs(), expected) {
            (Ok(pairs), Expected::Accept { pair_count }) => {
                if pairs.len() != pair_count {
                    return Err(anyhow::anyhow!(
                        "expected {pair_count} runnable pairs, got {}",
                        pairs.len()
                    ));
                }
            }
            (Err(violation), Expected::Reject) => {
                let msg = format!("{violation}");
                assert!(
                    msg.contains("no compatible"),
                    "msg missing 'no compatible': {msg}"
                );
            }
            (Ok(pairs), Expected::Reject) => {
                return Err(anyhow::anyhow!("expected rejection, got Ok({pairs:?})"));
            }
            (Err(e), Expected::Accept { .. }) => {
                return Err(anyhow::anyhow!("expected acceptance, got: {e}"));
            }
        }
        Ok(())
    }

    #[test]
    fn minimal_outer_plan_parses() -> anyhow::Result<()> {
        let plan = Plan::parse(minimal_outer_plan())?;
        assert_eq!(plan.plan_id, "test");
        assert_eq!(plan.transports.len(), 1);
        assert_eq!(plan.transports[0].client_id().as_str(), "mac-1");
        Ok(())
    }

    #[test]
    fn cells_expand_to_cartesian_product_per_variant() -> anyhow::Result<()> {
        let toml_str = r#"
plan_id          = "x"
benchmarks       = ["b1", "b2"]

[[transports]]
client_id = "t1"
type = "local"
binary_path = "/bin"
work_dir = "/tmp"
shell = "posix"

[[variants]]
clients  = ["t1"]
models   = [
  { type = "gguf_text", source = "huggingface", org = "o", repo_name = "r", path = "a.gguf" },
  { type = "gguf_text", source = "huggingface", org = "o", repo_name = "r", path = "b.gguf" },
]
runtimes = [{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b1", flavor = "macos-arm64" }]
"#;
        let plan = Plan::parse(toml_str)?;
        let cells = plan.cells()?;
        // 2 benchmarks × 2 models × 1 runtime = 4 cells
        assert_eq!(cells.len(), 4);
        assert!(cells.iter().all(|c| c.variant_idx == 0));
        assert!(
            cells
                .iter()
                .all(|c| client_refs(c.allowed_clients) == vec!["t1"])
        );
        Ok(())
    }

    /// A plan template for the vision cells: one gguf_vision model on the
    /// llama.cpp CLI, the given benchmark ids, and `extra` variant lines.
    fn vision_plan(benchmarks: &str, extra: &str) -> String {
        format!(
            r#"
plan_id    = "vision"
benchmarks = {benchmarks}

[[transports]]
client_id = "t1"
type = "local"
binary_path = "/bin"
work_dir = "/tmp"
shell = "posix"

[[variants]]
clients  = ["t1"]
models   = [{{ type = "gguf_vision", source = "huggingface", org = "o", repo_name = "r", model = "m.gguf", mmproj = "p.gguf" }}]
runtimes = [{{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", repository_version = "b1", flavor = "macos-arm64" }}]
{extra}
"#
        )
    }

    /// An `asr_eval` cell states its thinking mode (off, as every asr_eval
    /// recipe runs) in its model flags, as a `vlm_eval` does.
    #[test]
    fn an_asr_eval_cell_takes_its_model_flags() -> anyhow::Result<()> {
        let plan = Plan::parse(&vision_plan(
            r#"["asr_eval_fleurs_nl_nl_voxtral"]"#,
            r#"model_flags = [
  { benchmark_type = "asr_eval", model_type = "gguf_vision", enable_thinking = false },
]"#,
        ))?;
        let cells = plan.runnable_cells()?;
        let cell = single(&cells)?;
        assert_eq!(
            cell.model_flags,
            Some(crate::ModelFlags::AsrEvalGgufVision {
                enable_thinking: Some(false)
            })
        );
        Ok(())
    }

    /// A `vlm_eval` cell takes runtime, benchmark and model flags like the
    /// other llama.cpp cells: the slot count reaches the cell the client runs
    /// and travels as the `--runtime-flags` JSON, and whether the model thinks is
    /// its model flags' `enable_thinking`, as on a text eval. Before the cell
    /// existed here, the whole plan was refused with "no benchmark flags
    /// defined", so a plan could only carry a `vlm_eval` cell bare.
    #[test]
    fn a_vlm_eval_cell_takes_its_flags() -> anyhow::Result<()> {
        let plan = Plan::parse(&vision_plan(
            r#"["vlm_eval_mmstar"]"#,
            r#"runtime_flags = [{ benchmark_type = "vlm_eval", runtime_type = "llamacpp_cli_stock_tools", model_type = "gguf_vision", threads = 4, parallel = 4 }]
benchmark_flags = [{ benchmark_type = "vlm_eval", runtime_type = "llamacpp_cli_stock_tools", model_type = "gguf_vision", http_timeout_seconds = 1200 }]
model_flags = [{ benchmark_type = "vlm_eval", model_type = "gguf_vision", enable_thinking = true }]"#,
        ))?;
        let cells = plan.runnable_cells()?;
        let cell = single(&cells)?;
        let Some(crate::RuntimeFlags::VlmEvalLlamacppCliStockToolsGgufVision(flags)) =
            &cell.runtime_flags
        else {
            anyhow::bail!(
                "expected vlm_eval runtime flags, got {:?}",
                cell.runtime_flags
            );
        };
        assert_eq!(flags.threads, Some(4));
        assert_eq!(
            flags.parallel.map(crate::PositiveCount::into_inner),
            Some(4)
        );
        assert!(matches!(
            cell.benchmark_flags,
            Some(crate::BenchmarkFlags::VlmEvalLlamacppCliStockToolsGgufVision { .. })
        ));
        assert_eq!(
            cell.model_flags,
            Some(crate::ModelFlags::VlmEvalGgufVision {
                enable_thinking: Some(true)
            })
        );
        Ok(())
    }

    /// Whether a model thinks is a model flag on every eval, so `reasoning` is
    /// refused as a runtime flag on the `vlm_eval` cell as on the others,
    /// rather than accepted and dropped.
    #[rstest::rstest]
    #[case::vlm_eval(r#"["vlm_eval_mmstar"]"#, "vlm_eval")]
    #[case::vl_throughput(r#"["vl_throughput_224x224_t0_d8_f1"]"#, "vl_throughput")]
    fn reasoning_is_not_a_runtime_flag(#[case] benchmarks: &str, #[case] benchmark_type: &str) {
        let err = Plan::parse(&vision_plan(
            benchmarks,
            &format!(
                r#"runtime_flags = [{{ benchmark_type = "{benchmark_type}", runtime_type = "llamacpp_cli_stock_tools", model_type = "gguf_vision", reasoning = "on" }}]"#
            ),
        ))
        .err()
        .map(|e| e.to_string());
        assert!(
            err.as_deref().is_some_and(|e| e.contains("reasoning")),
            "{err:?}"
        );
    }

    #[test]
    fn variant_benchmark_override_replaces_plan_benchmarks() -> anyhow::Result<()> {
        let toml_str = r#"
plan_id          = "x"
benchmarks       = ["plan-bench"]

[[transports]]
client_id = "t1"
type = "local"
binary_path = "/bin"
work_dir = "/tmp"
shell = "posix"

[[variants]]
clients  = ["t1"]
models   = [{ type = "gguf_text", source = "huggingface", org = "o", repo_name = "r", path = "a.gguf" }]
runtimes = [{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b1", flavor = "macos-arm64" }]
benchmarks = ["variant-bench-a", "variant-bench-b"]
"#;
        let plan = Plan::parse(toml_str)?;
        let cells = plan.cells()?;
        assert_eq!(cells.len(), 2);
        let names: Vec<_> = cells.iter().map(|c| c.benchmark.as_ref()).collect();
        assert!(names.contains(&"variant-bench-a"));
        assert!(names.contains(&"variant-bench-b"));
        assert!(!names.contains(&"plan-bench"));
        Ok(())
    }

    #[test]
    fn variant_benchmarks_allow_missing_plan_benchmarks() -> anyhow::Result<()> {
        let toml_str = r#"
plan_id          = "x"

[[transports]]
client_id = "t1"
type = "local"
binary_path = "/bin"
work_dir = "/tmp"
shell = "posix"

[[variants]]
clients  = ["t1"]
models   = [{ type = "gguf_text", source = "huggingface", org = "o", repo_name = "r", path = "a.gguf" }]
runtimes = [{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b1", flavor = "macos-arm64" }]
benchmarks = ["variant-bench"]
"#;
        let plan = Plan::parse(toml_str)?;
        assert!(plan.matrix.benchmarks.is_none());
        let cells = plan.cells()?;
        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0].benchmark.as_ref(), "variant-bench");
        let runnable_cells = plan.runnable_cells()?;
        assert_eq!(single(&runnable_cells)?.benchmark.as_ref(), "variant-bench");
        Ok(())
    }

    #[test]
    fn variant_without_benchmarks_rejected_when_plan_benchmarks_missing() -> anyhow::Result<()> {
        let toml_str = r#"
plan_id          = "x"

[[transports]]
client_id = "t1"
type = "local"
binary_path = "/bin"
work_dir = "/tmp"
shell = "posix"

[[variants]]
clients  = ["t1"]
models   = [{ type = "gguf_text", source = "huggingface", org = "o", repo_name = "r", path = "a.gguf" }]
runtimes = [{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b1", flavor = "macos-arm64" }]
"#;
        let err = Plan::parse(toml_str)
            .err()
            .ok_or_else(|| anyhow::anyhow!("expected missing benchmarks to reject"))?;
        let msg = format!("{err:#}");
        assert!(msg.contains("variant 0"), "got: {msg}");
        assert!(msg.contains("benchmarks"), "got: {msg}");
        Ok(())
    }

    #[test]
    fn unknown_client_ref_rejected() -> anyhow::Result<()> {
        let toml_str = r#"
plan_id          = "x"
benchmarks       = ["b1"]

[[transports]]
client_id = "t1"
type = "local"
binary_path = "/bin"
work_dir = "/tmp"
shell = "posix"

[[variants]]
clients  = ["does-not-exist"]
models   = [{ type = "gguf_text", source = "huggingface", org = "o", repo_name = "r", path = "a.gguf" }]
runtimes = [{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b1", flavor = "macos-arm64" }]
"#;
        let err = Plan::parse(toml_str)
            .err()
            .ok_or_else(|| anyhow::anyhow!("should reject unknown client"))?;
        let msg = err.to_string();
        assert!(msg.contains("unknown transport"), "got: {msg}");
        assert!(msg.contains("does-not-exist"), "got: {msg}");
        Ok(())
    }

    #[test]
    fn duplicate_transport_names_rejected() -> anyhow::Result<()> {
        let toml_str = r#"
plan_id          = "x"
benchmarks       = ["b1"]

[[transports]]
client_id = "dup"
type = "local"
binary_path = "/bin"
work_dir = "/tmp"
shell = "posix"

[[transports]]
client_id = "dup"
type = "local"
binary_path = "/bin2"
work_dir = "/tmp"
shell = "posix"

[[variants]]
clients  = ["dup"]
models   = [{ type = "gguf_text", source = "huggingface", org = "o", repo_name = "r", path = "a.gguf" }]
runtimes = [{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b1", flavor = "macos-arm64" }]
"#;
        let err = Plan::parse(toml_str)
            .err()
            .ok_or_else(|| anyhow::anyhow!("should reject duplicate client_id"))?;
        let msg = format!("{err:#}");
        assert!(
            msg.contains("duplicate transport client_id \"dup\""),
            "got: {msg}"
        );
        Ok(())
    }

    #[test]
    fn load_reads_what_parse_reads() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("plan.toml");
        let toml_str = format!(
            "plan_id = \"x\"\n{}\n[[transports]]\nclient_id = \"mac-1\"\ntype = \"local\"\nbinary_path = \"/bin/pipette\"\nwork_dir = \"/tmp\"\n",
            plan_toml(
                r#"clients  = ["mac-1"]
models   = [{ type = "gguf_text", source = "huggingface", org = "o", repo_name = "r", path = "a.gguf" }]
runtimes = [{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b1", flavor = "macos-arm64" }]"#
            )
        );
        fs::write(&path, &toml_str)?;
        assert_eq!(Plan::load(&path)?, Plan::parse(&toml_str)?);
        let missing = dir.path().join("missing.toml");
        let err = Plan::load(&missing).err().map(|e| format!("{e:#}"));
        assert!(
            err.as_deref().is_some_and(|e| e.contains("missing.toml")),
            "{err:?}"
        );
        Ok(())
    }

    /// An id outside `ClientId`'s grammar could never be referenced from `clients`.
    #[test]
    fn transport_client_id_outside_the_id_grammar_is_rejected() -> anyhow::Result<()> {
        let toml_str = r#"
plan_id          = "x"
benchmarks       = ["b1"]

[[transports]]
client_id = "mac.1"
type = "local"
binary_path = "/bin"
work_dir = "/tmp"
shell = "posix"

[[variants]]
clients  = ["mac-1"]
models   = [{ type = "gguf_text", source = "huggingface", org = "o", repo_name = "r", path = "a.gguf" }]
runtimes = [{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b1", flavor = "macos-arm64" }]
"#;
        let err = Plan::parse(toml_str)
            .err()
            .ok_or_else(|| anyhow::anyhow!("should reject client_id \"mac.1\""))?;
        let msg = format!("{err:#}");
        assert!(msg.contains("client id contains '.'"), "got: {msg}");
        Ok(())
    }

    #[test]
    fn adb_over_ssh_rejects_a_powershell_device_shell() -> anyhow::Result<()> {
        let toml_str = r#"
plan_id    = "x"
benchmarks = ["prefill_throughput_512"]

[[transports]]
client_id   = "phone-1"
type        = "adb_over_ssh"
host        = "controller"
serial      = "S1"
shell       = "powershell"
binary_path = "/data/local/tmp/pipette-evals/pipette"
work_dir    = "/data/local/tmp/pipette-evals"

[[variants]]
clients  = ["phone-1"]
models   = [{ type = "gguf_text", source = "huggingface", org = "o", repo_name = "r", path = "a.gguf" }]
runtimes = [{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b1", flavor = "android-arm64-v8a" }]
"#;
        let err = Plan::parse(toml_str)
            .err()
            .ok_or_else(|| anyhow::anyhow!("should reject a powershell adb_over_ssh transport"))?;
        assert!(
            err.to_string().contains(r#"requires shell = "posix""#),
            "unexpected error: {err:#}"
        );
        Ok(())
    }

    #[test]
    fn cells_cartesian_includes_runtime_axis() -> anyhow::Result<()> {
        let toml_str = r#"
plan_id          = "x"
benchmarks       = ["b1"]

[[transports]]
client_id = "t1"
type = "local"
binary_path = "/bin"
work_dir = "/tmp"
shell = "posix"

[[variants]]
clients  = ["t1"]
models   = [{ type = "gguf_text", source = "huggingface", org = "o", repo_name = "r", path = "a.gguf" }]
runtimes = [
  { type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b1", flavor = "macos-arm64" },
  { type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b2", flavor = "macos-arm64" },
  { type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b3", flavor = "macos-arm64" },
]
"#;
        let plan = Plan::parse(toml_str)?;
        // 1 benchmark × 1 model × 3 runtimes = 3 cells
        let cells = plan.cells()?;
        assert_eq!(cells.len(), 3);
        let versions: Vec<&str> = cells
            .iter()
            .map(|c| {
                if let Runtime::LlamacppCliStockTools(rt) = c.runtime {
                    rt.source.reference()
                } else {
                    ""
                }
            })
            .collect();
        assert!(versions.contains(&"b1"));
        assert!(versions.contains(&"b2"));
        assert!(versions.contains(&"b3"));
        Ok(())
    }

    #[test]
    fn empty_transports_array_rejected() -> anyhow::Result<()> {
        let toml_str = r#"
plan_id          = "x"
benchmarks       = ["b1"]
transports       = []

[[variants]]
clients  = ["t1"]
models   = [{ type = "gguf_text", source = "huggingface", org = "o", repo_name = "r", path = "a.gguf" }]
runtimes = [{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b1", flavor = "macos-arm64" }]
"#;
        // NonEmptyVec deserialization rejects `[]` before our
        // cross-field validation runs.
        let err = Plan::parse(toml_str)
            .err()
            .ok_or_else(|| anyhow::anyhow!("should reject empty transports"))?;
        assert!(
            err.to_string().contains("empty") || err.to_string().contains("not be empty"),
            "got: {}",
            err
        );
        Ok(())
    }

    #[test]
    fn missing_transports_rejected() -> anyhow::Result<()> {
        let toml_str = r#"
plan_id          = "x"
benchmarks       = ["b1"]

[[variants]]
clients  = ["t1"]
models   = [{ type = "gguf_text", source = "huggingface", org = "o", repo_name = "r", path = "a.gguf" }]
runtimes = [{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b1", flavor = "macos-arm64" }]
"#;
        let err = Plan::parse(toml_str)
            .err()
            .ok_or_else(|| anyhow::anyhow!("should reject missing transports"))?;
        assert!(err.to_string().contains("transports") || err.to_string().contains("missing"));
        Ok(())
    }

    #[test]
    fn outer_plan_round_trips_through_toml() -> anyhow::Result<()> {
        let plan = Plan::parse(minimal_outer_plan())?;
        let emitted = toml::to_string(&plan)?;
        let reparsed = Plan::parse(&emitted)?;
        assert_eq!(plan, reparsed);
        // And byte-equal wire form.
        let reemitted = toml::to_string(&reparsed)?;
        assert_eq!(emitted, reemitted);
        Ok(())
    }

    /// A set retry budget survives serialization; only the default is omitted.
    #[test]
    fn a_set_retry_budget_is_serialized() -> anyhow::Result<()> {
        let toml_str = r#"
plan_id          = "x"
benchmarks       = ["b1"]

[retry]
max_consecutive_failures = 3

[[transports]]
client_id = "t1"
type = "local"
binary_path = "/bin"
work_dir = "/tmp"

[[variants]]
clients  = ["t1"]
models   = [{ type = "gguf_text", source = "huggingface", org = "o", repo_name = "r", path = "a.gguf" }]
runtimes = [{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b1", flavor = "macos-arm64" }]
"#;
        let plan = Plan::parse(toml_str)?;
        assert_eq!(plan.retry.max_consecutive_failures, 3);
        assert_eq!(Plan::parse(&toml::to_string(&plan)?)?, plan);
        Ok(())
    }

    #[test]
    fn serialized_omits_absent_optional_fields() -> anyhow::Result<()> {
        // No `user`, `port` or `parallelism`: none may be emitted.
        let toml_str = r#"
plan_id          = "x"
benchmarks       = ["b1"]

[[transports]]
client_id = "t1"
type = "ssh"
host = "h"
binary_path = "/bin"
work_dir = "/tmp"
shell = "posix"

[[variants]]
clients  = ["t1"]
models   = [{ type = "gguf_text", source = "huggingface", org = "o", repo_name = "r", path = "a.gguf" }]
runtimes = [{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b1", flavor = "macos-arm64" }]
"#;
        let plan = Plan::parse(toml_str)?;
        let emitted = toml::to_string(&plan)?;
        assert!(!emitted.contains("user ="), "got: {emitted}");
        assert!(!emitted.contains("port ="), "got: {emitted}");
        assert!(!emitted.contains("[retry]"), "got: {emitted}");
        assert!(
            !emitted.contains("max_consecutive_failures"),
            "got: {emitted}"
        );
        assert!(
            !emitted.contains("cooldown_seconds"),
            "cooldown_seconds is no longer a plan-types field; got: {emitted}"
        );
        Ok(())
    }

    #[test]
    fn cells_isolate_clients_per_variant() -> anyhow::Result<()> {
        let toml_str = r#"
plan_id          = "x"
benchmarks       = ["b1"]

[[transports]]
client_id = "t1"
type = "local"
binary_path = "/bin"
work_dir = "/tmp"
shell = "posix"

[[transports]]
client_id = "t2"
type = "local"
binary_path = "/bin"
work_dir = "/tmp"
shell = "posix"

[[variants]]
clients  = ["t1"]
models   = [{ type = "gguf_text", source = "huggingface", org = "o", repo_name = "r", path = "a.gguf" }]
runtimes = [{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b1", flavor = "macos-arm64" }]

[[variants]]
clients  = ["t2", "t1"]
models   = [{ type = "gguf_text", source = "huggingface", org = "o", repo_name = "r", path = "b.gguf" }]
runtimes = [{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b1", flavor = "macos-arm64" }]
"#;
        let plan = Plan::parse(toml_str)?;
        let cells = plan.cells()?;
        assert_eq!(cells.len(), 2);
        let v0 = cells
            .iter()
            .find(|c| c.variant_idx == 0)
            .ok_or_else(|| anyhow::anyhow!("missing variant 0"))?;
        let v1 = cells
            .iter()
            .find(|c| c.variant_idx == 1)
            .ok_or_else(|| anyhow::anyhow!("missing variant 1"))?;
        assert_eq!(v0.model.to_string(), "o/r:a.gguf");
        assert_eq!(v1.model.to_string(), "o/r:b.gguf");
        assert_eq!(client_refs(v0.allowed_clients), vec!["t1"]);
        // As authored, not sorted: it is a view into the variant's `clients`.
        assert_eq!(client_refs(v1.allowed_clients), vec!["t2", "t1"]);
        Ok(())
    }

    #[rstest]
    // The in-process apk cells type `load_mode`, not the `mmap` boolean upstream
    // deprecated, so an entry spelling the boolean is refused by knob name.
    #[case::in_process_apk_mmap_boolean(
        r#"
plan_id    = "x"
benchmarks = ["eval_smoke"]

[[transports]]
client_id = "t1"
type = "local"
binary_path = "/bin"
work_dir = "/tmp"
shell = "posix"

[[variants]]
clients  = ["t1"]
models   = [{ type = "gguf_text", source = "huggingface", org = "o", repo_name = "r", path = "a.gguf" }]
runtimes = [{ type = "llamacpp_apk_pipette", repository_url = "github.com/ggml-org/llama.cpp", version = "b9050", flavor = "android-arm64-v8" }]
runtime_flags = [{ benchmark_type = "eval", runtime_type = "llamacpp_apk_pipette", model_type = "gguf_text", mmap = false }]
"#,
        "mmap"
    )]
    // A flag for a benchmark the variant never runs matches no cell.
    #[case::matches_no_cell(
        r#"
plan_id    = "x"
benchmarks = ["prefill_throughput_512"]

[[transports]]
client_id = "t1"
type = "local"
binary_path = "/bin"
work_dir = "/tmp"
shell = "posix"

[[variants]]
clients  = ["t1"]
models   = [{ type = "gguf_text", source = "huggingface", org = "o", repo_name = "r", path = "a.gguf" }]
runtimes = [{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b9050", flavor = "macos-arm64" }]
runtime_flags = [{ benchmark_type = "eval", runtime_type = "llamacpp_cli_stock_tools", model_type = "gguf_text", ctx_size = 8192 }]
"#,
        "matches no cell"
    )]
    fn rejects_invalid_runtime_flags(#[case] toml_str: &str, #[case] expected: &str) {
        let result = Plan::parse(toml_str);
        assert!(result.is_err(), "expected rejection");
        if let Err(err) = result {
            assert!(format!("{err:#}").contains(expected), "got: {err:#}");
        }
    }

    #[test]
    fn model_flags_resolve_only_to_the_matching_type_and_benchmark() -> anyhow::Result<()> {
        // Flags for (eval, gguf_text) only: not the eval gguf_vision cell, nor
        // either prefill cell.
        let toml_str = r#"
plan_id    = "x"
benchmarks = ["eval_smoke", "prefill_throughput_512"]

[[transports]]
client_id = "t1"
type = "local"
binary_path = "/bin/pipette"
work_dir = "/tmp/wd"
shell = "posix"

[[variants]]
clients     = ["t1"]
models      = [
  { type = "gguf_text",   source = "huggingface", org = "o", repo_name = "r", path = "a.gguf" },
  { type = "gguf_vision", source = "huggingface", org = "o", repo_name = "r", model = "a.gguf", mmproj = "m.gguf" },
]
runtimes    = [{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b9050", flavor = "macos-arm64" }]
model_flags = [{ benchmark_type = "eval", model_type = "gguf_text", enable_thinking = true }]
"#;
        let plan = Plan::parse(toml_str)?;
        let cells = plan.runnable_cells()?;
        assert_eq!(cells.len(), 4);
        let flagged: Vec<_> = cells.iter().filter(|c| c.model_flags.is_some()).collect();
        assert_eq!(flagged.len(), 1);
        let cell = flagged[0];
        assert!(cell.benchmark.as_ref().contains("eval"));
        assert!(matches!(cell.model, Model::GgufText(_)));
        assert_eq!(
            cell.model_flags,
            Some(ModelFlags::EvalGgufText {
                enable_thinking: Some(true),
                reasoning_effort: None,
            })
        );
        Ok(())
    }

    /// A one-variant plan whose benchmark and flags block vary per case.
    fn dup_flag_plan(benchmark: &str, flags_block: &str) -> String {
        format!(
            r#"plan_id    = "x"
benchmarks = ["{benchmark}"]

[[transports]]
client_id = "t1"
type = "local"
binary_path = "/bin/pipette"
work_dir = "/tmp/wd"
shell = "posix"

[[variants]]
clients  = ["t1"]
models   = [{{ type = "gguf_text", source = "huggingface", org = "o", repo_name = "r", path = "a.gguf" }}]
runtimes = [{{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b9050", flavor = "macos-arm64" }}]
{flags_block}
"#
        )
    }

    /// The message carries the fix: it is the migration path for plan files
    /// written when `benchmarks = ["remote/<id>"]` was the form.
    #[rstest]
    #[case::remote("remote/eval_smoke")]
    #[case::local("local/eval_smoke")]
    fn addressed_benchmark_id_rejected_with_guidance(
        #[case] addressed: &str,
    ) -> anyhow::Result<()> {
        let err = Plan::parse(&dup_flag_plan(addressed, ""))
            .err()
            .context("an addressed benchmark id should be rejected")?;
        let message = format!("{err:#}");
        assert!(message.contains(addressed), "got: {message}");
        assert!(
            message.contains("is not a bare id") && message.contains("drop any"),
            "the message must say what to do; got: {message}"
        );
        Ok(())
    }

    /// Refused for anything but a `local/` or `remote/` prefix, the message names
    /// the character rather than suggesting a prefix to drop.
    #[rstest]
    #[case::leading_dot(".hidden", '.')]
    #[case::colon("a:b", ':')]
    #[case::other_side("elsewhere/x", '/')]
    #[case::slash_after_colon("a:b/c", ':')]
    fn a_non_catalog_refusal_names_the_char(
        #[case] id: &str,
        #[case] ch: char,
    ) -> anyhow::Result<()> {
        let err = Plan::parse(&dup_flag_plan(id, ""))
            .err()
            .context("an invalid benchmark id should be rejected")?;
        let message = format!("{err:#}");
        assert!(
            message.contains(&format!("benchmark \"{}\"", id.escape_default())),
            "got: {message}"
        );
        assert!(
            message.contains(&format!("'{}'", ch.escape_default())),
            "got: {message}"
        );
        assert!(!message.contains("drop any"), "got: {message}");
        Ok(())
    }

    /// The refused entry is quoted with every control and non-ASCII char
    /// escaped, so a plan cannot forge a line or pass a look-alike off as ASCII.
    #[rstest]
    #[case::addressed(
        r"remote/a\nb\u200B\u0430",
        r#"benchmark "remote/a\nb\u{200b}\u{430}" is not a bare id"#
    )]
    #[case::charset(
        r"a\nb\u200B",
        r#"benchmark "a\nb\u{200b}": benchmark id contains '\n'"#
    )]
    #[case::look_alike(r"b\u0430", r#"benchmark "b\u{430}": benchmark id contains '\u{430}'"#)]
    fn a_refused_benchmark_entry_is_escaped(
        #[case] toml_id: &str,
        #[case] rendered: &str,
    ) -> anyhow::Result<()> {
        let err = Plan::parse(&dup_flag_plan(toml_id, ""))
            .err()
            .context("the entry should be rejected")?;
        let message = format!("{err:#}");
        assert!(message.contains(rendered), "got: {message}");
        Ok(())
    }

    /// Two `runtime_flags` entries for one `(benchmark, runtime, model)` key:
    /// the resolver would silently pick one.
    #[test]
    fn duplicate_runtime_flags_key_rejected() -> anyhow::Result<()> {
        let toml = dup_flag_plan(
            "prefill_throughput_512",
            r#"runtime_flags = [
  { benchmark_type = "prefill_throughput", runtime_type = "llamacpp_cli_stock_tools", model_type = "gguf_text", threads = 4 },
  { benchmark_type = "prefill_throughput", runtime_type = "llamacpp_cli_stock_tools", model_type = "gguf_text", threads = 8 },
]"#,
        );
        let err = Plan::parse(&toml)
            .err()
            .context("duplicate runtime_flags key should be rejected")?;
        assert!(
            format!("{err:#}").contains("variant 0: duplicate runtime_flags"),
            "got: {err:#}"
        );
        Ok(())
    }

    /// Same for `model_flags`, keyed on `(benchmark, model)`.
    #[test]
    fn duplicate_model_flags_key_rejected() -> anyhow::Result<()> {
        let toml = dup_flag_plan(
            "eval_smoke",
            r#"model_flags = [
  { benchmark_type = "eval", model_type = "gguf_text", enable_thinking = true },
  { benchmark_type = "eval", model_type = "gguf_text", enable_thinking = false },
]"#,
        );
        let err = Plan::parse(&toml)
            .err()
            .context("duplicate model_flags key should be rejected")?;
        assert!(
            format!("{err:#}").contains("variant 0: duplicate model_flags"),
            "got: {err:#}"
        );
        Ok(())
    }

    /// Same for `benchmark_flags`.
    #[test]
    fn duplicate_benchmark_flags_key_rejected() -> anyhow::Result<()> {
        let toml = dup_flag_plan(
            "eval_smoke",
            r#"benchmark_flags = [
  { benchmark_type = "eval", runtime_type = "llamacpp_cli_stock_tools", model_type = "gguf_text", http_timeout_seconds = 600 },
  { benchmark_type = "eval", runtime_type = "llamacpp_cli_stock_tools", model_type = "gguf_text", http_timeout_seconds = 900 },
]"#,
        );
        let err = Plan::parse(&toml)
            .err()
            .context("duplicate benchmark_flags key should be rejected")?;
        assert!(
            format!("{err:#}").contains("variant 0: duplicate benchmark_flags"),
            "got: {err:#}"
        );
        Ok(())
    }

    #[test]
    fn runnable_cells_deduplicate_across_variants() -> anyhow::Result<()> {
        let toml_str = r#"
plan_id          = "x"
benchmarks       = ["b1"]

[[transports]]
client_id = "t1"
type = "local"
binary_path = "/bin"
work_dir = "/tmp"
shell = "posix"

[[variants]]
clients  = ["t1"]
models   = [{ type = "gguf_text", source = "huggingface", org = "o", repo_name = "r", path = "a.gguf" }]
runtimes = [{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b9050", flavor = "macos-arm64" }]

[[variants]]
clients  = ["t1"]
models   = [{ type = "gguf_text", source = "huggingface", org = "o", repo_name = "r", path = "a.gguf" }]
runtimes = [{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b9050", flavor = "macos-arm64" }]
"#;
        let plan = Plan::parse(toml_str)?;
        let cells = plan.runnable_cells()?;
        assert_eq!(cells.len(), 1);
        Ok(())
    }

    #[test]
    fn runnable_cell_cartesian_product() -> anyhow::Result<()> {
        // 1 benchmark × 2 models × 2 runtimes × 1 variant = 4 cells.
        let toml_str = r#"
plan_id          = "x"
benchmarks       = ["b1"]

[[transports]]
client_id = "t1"
type = "local"
binary_path = "/bin"
work_dir = "/tmp"
shell = "posix"

[[variants]]
clients  = ["t1"]
models   = [
  { type = "gguf_text", source = "huggingface", org = "o", repo_name = "r", path = "a.gguf" },
  { type = "gguf_text", source = "huggingface", org = "o", repo_name = "r", path = "b.gguf" },
]
runtimes = [
  { type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b9050", flavor = "macos-arm64" },
  { type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b9051", flavor = "macos-arm64" },
]
"#;
        let plan = Plan::parse(toml_str)?;
        let cells = plan.runnable_cells()?;
        assert_eq!(cells.len(), 4);
        let refs: Vec<(String, String)> = cells
            .iter()
            .map(|c| (c.model.to_string(), c.runtime.to_string()))
            .collect();
        let rt = |v: &str| format!("github.com/ggml-org/llama.cpp@{v}:macos-arm64");
        assert!(refs.contains(&("o/r:a.gguf".into(), rt("b9050"))));
        assert!(refs.contains(&("o/r:a.gguf".into(), rt("b9051"))));
        assert!(refs.contains(&("o/r:b.gguf".into(), rt("b9050"))));
        assert!(refs.contains(&("o/r:b.gguf".into(), rt("b9051"))));
        Ok(())
    }

    #[rstest::rstest]
    #[case::no_model_declares_auth(
        r#"{ type = "gguf_text", source = "huggingface", org = "o", repo_name = "r", path = "a.gguf" }"#,
        false
    )]
    #[case::second_model_declares_auth(
        r#"
  { type = "gguf_text", source = "huggingface", org = "o", repo_name = "r1", path = "a.gguf" },
  { type = "gguf_text", source = "huggingface", org = "o", repo_name = "r2", path = "b.gguf", auth_token = "hf_test_xxx" }
"#,
        true
    )]
    fn plan_auth_token_reflects_any_model(
        #[case] models: &str,
        #[case] expect: bool,
    ) -> anyhow::Result<()> {
        let toml_str = format!(
            r#"
plan_id          = "x"
benchmarks       = ["b"]

[[transports]]
client_id = "t1"
type = "local"
binary_path = "/bin"
work_dir = "/tmp"
shell = "posix"

[[variants]]
clients  = ["t1"]
models   = [{models}]
runtimes = [{{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b1", flavor = "macos-arm64" }}]
"#
        );
        let plan = Plan::parse(&toml_str)?;
        assert_eq!(plan.auth_token().is_some(), expect);
        Ok(())
    }

    /// A GGUF model with only an MLX runtime; see [`crate::is_compatible`].
    #[test]
    fn parse_rejects_orphan_model() -> anyhow::Result<()> {
        let toml_str = r#"
plan_id          = "x"
benchmarks       = ["b"]

[[transports]]
client_id = "t1"
type = "local"
binary_path = "/bin"
work_dir = "/tmp"
shell = "posix"

[[variants]]
clients  = ["t1"]
models   = [{ type = "gguf_text", source = "huggingface", org = "o", repo_name = "r", path = "a.gguf" }]
runtimes = [{ type = "mlx_macos_pipette", version = "0.31", flavor = "macos-arm64", source = { type = "pip_requirements_text", contents = "mlx-lm==0.31" } }]
"#;
        let err = Plan::parse(toml_str)
            .err()
            .ok_or_else(|| anyhow::anyhow!("expected Plan::parse to reject orphan model"))?;
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no compatible"),
            "expected error to name 'no compatible': {msg}"
        );
        assert!(
            msg.contains("variant 0"),
            "expected error to name the offending variant: {msg}"
        );
        Ok(())
    }

    /// One variant can host several backends on the same transports; the cells
    /// are the matched subset of the product.
    #[test]
    fn parse_accepts_mixed_kinds_when_fully_matched() -> anyhow::Result<()> {
        let toml_str = r#"
plan_id          = "x"
benchmarks       = ["b"]

[[transports]]
client_id = "t1"
type = "local"
binary_path = "/bin"
work_dir = "/tmp"
shell = "posix"

[[variants]]
clients  = ["t1"]
models   = [
  { type = "gguf_text", source = "huggingface", org = "o", repo_name = "r1", path = "a.gguf" },
  { type = "mlx", source = "huggingface",       org = "o", repo_name = "r2" },
]
runtimes = [
  { type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b1", flavor = "macos-arm64" },
  { type = "mlx_macos_pipette", version = "0.31", flavor = "macos-arm64", source = { type = "pip_requirements_text", contents = "mlx-lm==0.31" } },
]
"#;
        let plan = Plan::parse(toml_str)?;
        let cells = plan.runnable_cells()?;
        // The full product is 4; gguf×mlx and mlx×llamacpp drop.
        assert_eq!(cells.len(), 2);
        Ok(())
    }

    #[rstest::rstest]
    #[case::defaults_to_one("", 1)]
    #[case::explicit_value("parallelism = 3", 3)]
    fn parallelism_parses(
        #[case] transport_extra: &str,
        #[case] expected: usize,
    ) -> anyhow::Result<()> {
        let toml_str = format!(
            r#"
plan_id    = "x"
benchmarks = ["b1"]

[[transports]]
client_id   = "t1"
type        = "local"
binary_path = "/bin"
work_dir    = "/tmp"
shell       = "posix"
{transport_extra}

[[variants]]
clients  = ["t1"]
models   = [{{ type = "gguf_text", source = "huggingface", org = "o", repo_name = "r", path = "a.gguf" }}]
runtimes = [{{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b1", flavor = "macos-arm64" }}]
"#
        );
        let plan = Plan::parse(&toml_str)?;
        let t = plan
            .transports
            .iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("expected one transport"))?;
        assert_eq!(t.parallelism().get(), expected);
        Ok(())
    }

    #[test]
    fn parallelism_omitted_from_emitted_toml_when_default() -> anyhow::Result<()> {
        let toml_str = r#"
plan_id    = "x"
benchmarks = ["b1"]

[[transports]]
client_id   = "t1"
type        = "local"
binary_path = "/bin"
work_dir    = "/tmp"
shell       = "posix"

[[variants]]
clients  = ["t1"]
models   = [{ type = "gguf_text", source = "huggingface", org = "o", repo_name = "r", path = "a.gguf" }]
runtimes = [{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b1", flavor = "macos-arm64" }]
"#;
        let plan = Plan::parse(toml_str)?;
        let emitted = toml::to_string(&plan)?;
        assert!(!emitted.contains("parallelism"), "got: {emitted}");
        Ok(())
    }
}
