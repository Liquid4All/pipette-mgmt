//! Expand a [`SchedulerPlan`] into the job bodies that plan ingestion stages.

use serde::Serialize;

use pipette_plan_types::{CapabilityFlag, ClientId, ClientRunSpec};

use crate::capability_rules::{
    CapabilityRuleError, VariantRuleError, resolve_effective_requirement, validate_capability_rules,
};
use crate::scheduler_plan::{PlanExpiresAt, SchedulerCell, SchedulerPlan, SchedulerPlanError};

/// The job bodies a plan expands to, and the warnings the capability rules
/// raised on the way.
#[derive(Debug, Clone, PartialEq)]
pub struct Expansion {
    /// One body per distinct rendering, in the order of `generate`'s
    /// `cell-NNN.json` files. A body keeps the model's HuggingFace token.
    pub bodies: Vec<serde_json::Value>,
    /// Faults that do not stop the plan, such as a capability path that no
    /// client can satisfy while the `clients` roster still can.
    pub warnings: Vec<PlanWarning>,
}

/// A fault that does not stop the plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanWarning {
    pub variant: usize,
    pub message: String,
}

/// A plan that parses but does not expand.
#[derive(Debug, thiserror::Error)]
pub enum ExpandError {
    #[error("the plan breaks a capability rule")]
    CapabilityRule(#[from] VariantRuleError),
    #[error("the plan does not resolve into cells")]
    Cells(#[source] SchedulerPlanError),
    #[error("resolving capability requirements for {runtime}")]
    Requirement {
        runtime: String,
        #[source]
        source: CapabilityRuleError,
    },
    #[error("rendering a job body as JSON")]
    Render(#[from] serde_json::Error),
}

impl ExpandError {
    /// The index of the variant at fault, when the fault belongs to one.
    pub fn variant(&self) -> Option<usize> {
        match self {
            Self::CapabilityRule(e) => Some(e.variant),
            Self::Cells(e) => Some(e.index()),
            Self::Requirement { .. } | Self::Render(_) => None,
        }
    }
}

/// One expanded cell in the form that ingestion stages and stores.
///
/// Field order is load-bearing: [`expand`] sorts on the rendered body. An empty
/// negative clause is omitted, never spelled `null`, which the server rejects.
#[derive(Debug, Clone, PartialEq, Serialize)]
struct JobBody {
    /// The variant's committed flags plus whatever the hardware rules injected.
    requires: Vec<CapabilityFlag>,
    /// The injected groups, then `client_versions` as one group. Always
    /// emitted, so the field's absence never has to mean `[]`.
    any_of: Vec<Vec<CapabilityFlag>>,
    clients: Vec<ClientId>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    exclude_clients: Vec<ClientId>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    excludes: Vec<CapabilityFlag>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    not_all_of: Vec<Vec<CapabilityFlag>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_at: Option<PlanExpiresAt>,
    #[serde(skip_serializing_if = "Option::is_none")]
    merge_duplicates: Option<bool>,
    /// Authoritative for the client. The server reads `benchmark`, and a
    /// failure record's descriptors from `model` and `runtime`.
    spec: ClientRunSpec,
}

impl JobBody {
    fn without_auth_token(&self) -> Self {
        Self {
            spec: ClientRunSpec {
                model: self.spec.model.without_auth_token(),
                ..self.spec.clone()
            },
            ..self.clone()
        }
    }

    fn from_cell(
        cell: &SchedulerCell,
        expires_at: Option<PlanExpiresAt>,
        merge_duplicates: Option<bool>,
    ) -> Result<Self, ExpandError> {
        let effective =
            resolve_effective_requirement(cell.eligibility.requires(), &cell.spec.runtime)
                .map_err(|source| ExpandError::Requirement {
                    runtime: cell.spec.runtime.to_string(),
                    source,
                })?;

        // The token stays: ingestion imports the model into the vault and drops
        // the token before it stores the body.
        let spec = cell.spec.clone();

        // A variant without versions adds no group, so its body does not change.
        let mut any_of = effective.any_of;
        let versions = cell.eligibility.client_versions();
        if !versions.is_empty() {
            any_of.push(versions.to_vec());
        }

        Ok(Self {
            requires: effective.requires,
            any_of,
            clients: cell.eligibility.clients().to_vec(),
            exclude_clients: cell.eligibility.exclude_clients().to_vec(),
            excludes: cell.eligibility.excludes().to_vec(),
            not_all_of: cell.eligibility.not_all_of().to_vec(),
            expires_at,
            merge_duplicates,
            spec,
        })
    }
}

/// An upper bound on the bodies [`expand`] returns, computed without building
/// one. Check a job limit against it before `expand`, which holds every body in
/// memory at once.
pub fn job_count(plan: &SchedulerPlan) -> Result<usize, ExpandError> {
    plan.cell_upper_bound().map_err(ExpandError::Cells)
}

/// Expand `plan` into its job bodies, validated and in a fixed order.
pub fn expand(plan: &SchedulerPlan) -> Result<Expansion, ExpandError> {
    let warnings = validate_capability_rules(plan)?;

    let mut rendered = plan
        .runnable_cells()
        .map_err(ExpandError::Cells)?
        .iter()
        .map(|cell| {
            let body = JobBody::from_cell(cell, plan.expires_at, plan.merge_duplicates)?;
            // Only a body with a token needs the second key, and a copy
            // without it for the first.
            let (key, gated) = match body.spec.model.auth_token() {
                Some(_) => (
                    serde_json::to_string_pretty(&body.without_auth_token())?,
                    Some(serde_json::to_string(&body.spec.model)?),
                ),
                None => (serde_json::to_string_pretty(&body)?, None),
            };
            Ok((key, gated, body))
        })
        .collect::<Result<Vec<_>, ExpandError>>()?;

    // Cells come out of a `HashSet`, so impose an order. The first key leaves
    // out the token, so a token does not move a body; the second puts a body
    // with a token before the same body without one.
    rendered.sort_by(|(a, a_gated, _), (b, b_gated, _)| {
        a.cmp(b)
            .then_with(|| (a_gated.is_none(), a_gated).cmp(&(b_gated.is_none(), b_gated)))
    });
    // Distinct cells can render one body: injected flags, or only the token
    // differs (one vault import serves every token). A body with a token never
    // merges with one without: it runs from the vault, the other from HuggingFace.
    rendered.dedup_by(|(a, a_gated, _), (b, b_gated, _)| {
        a == b && a_gated.is_some() == b_gated.is_some()
    });

    let bodies = rendered
        .iter()
        .map(|(_, _, body)| serde_json::to_value(body))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Expansion { bodies, warnings })
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use anyhow::Context;
    use rstest::rstest;
    use serde_json::Value;

    use super::*;

    /// An iOS Apple Foundation variant pinned to a client *and* requiring
    /// `os:ios`, and a `requires`-only macOS MLX variant.
    fn representative_plan() -> anyhow::Result<SchedulerPlan> {
        SchedulerPlan::parse(
            r#"
expires_at = "2026-08-01T00:00:00Z"
benchmarks = ["decode_throughput_512_100", "end_to_end_latency_512_256"]

[[variants]]
requires   = ["os:ios"]
clients    = ["ev1_9f2c"]
models     = [{ type = "apple_foundation_text" }]
runtimes   = [{ type = "apple_foundation", private_thermal = true }]
benchmarks = ["decode_throughput_512_100"]

[[variants]]
requires = ["os:macos"]
models   = [{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "LFM2.5-350M-MLX-4bit" }]
runtimes = [{ type = "mlx_macos_pipette", version = "0.20.0", flavor = "macos-arm64", source = { type = "pip_requirements_text", contents = "mlx-lm==0.20.0" } }]
"#,
        )
    }

    fn bodies(plan: &SchedulerPlan) -> anyhow::Result<Vec<Value>> {
        Ok(expand(plan)?.bodies)
    }

    fn only_body(plan: &SchedulerPlan) -> anyhow::Result<Value> {
        let [body] = bodies(plan)?
            .try_into()
            .map_err(|bodies: Vec<Value>| anyhow::anyhow!("one body, got {}", bodies.len()))?;
        Ok(body)
    }

    #[test]
    fn expands_the_matrix_into_one_body_per_cell() -> anyhow::Result<()> {
        let expansion = expand(&representative_plan()?)?;
        assert_eq!(expansion.bodies.len(), 3, "1 AFM cell + 2 MLX cells");
        assert!(expansion.warnings.is_empty(), "{:?}", expansion.warnings);
        Ok(())
    }

    fn counted_plan(name: &str) -> anyhow::Result<SchedulerPlan> {
        if name == "representative" {
            return representative_plan();
        }
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(format!("../../examples/plans/scheduler/{name}.toml"));
        let document = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        SchedulerPlan::parse(&document)
    }

    /// An equality, not `>=`: a bound that always answered `usize::MAX` would
    /// pass the weaker test and refuse every plan.
    #[rstest]
    #[case::representative("representative")]
    #[case::afm_mlx("afm-mlx")]
    #[case::llamacpp_vl_spec_decode("llamacpp-vl-spec-decode")]
    #[case::mlx_vl_spec_decode("mlx-vl-spec-decode")]
    fn job_count_is_exact_without_duplicate_cells(#[case] name: &str) -> anyhow::Result<()> {
        let plan = counted_plan(name)?;
        assert_eq!(job_count(&plan)?, bodies(&plan)?.len(), "{name}");
        Ok(())
    }

    #[test]
    fn job_count_counts_duplicate_cells_before_they_merge() -> anyhow::Result<()> {
        let variant = r#"
[[variants]]
requires = ["os:macos"]
models   = [{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "LFM2.5-350M-MLX-4bit" }]
runtimes = [{ type = "mlx_macos_pipette", version = "0.20.0", flavor = "macos-arm64", source = { type = "pip_requirements_text", contents = "mlx-lm==0.20.0" } }]
"#;
        let plan = SchedulerPlan::parse(&format!(
            "benchmarks = [\"decode_throughput_512_100\", \"end_to_end_latency_512_256\"]\n{variant}{variant}"
        ))?;
        assert_eq!(job_count(&plan)?, 4, "2 benchmarks x 1 pair x 2 variants");
        assert_eq!(bodies(&plan)?.len(), 2, "the second variant adds no cell");
        Ok(())
    }

    /// Repeated models and runtimes are counted by type: building this
    /// variant's product to validate or count it would take about 40 GB.
    #[test]
    fn job_count_does_not_build_the_model_runtime_product() -> anyhow::Result<()> {
        const N: usize = 50_000;
        let plan = SchedulerPlan::parse(&format!(
            "benchmarks = [\"decode_throughput_512_100\"]\n\
             [[variants]]\n\
             requires = [\"os:ios\"]\n\
             models = [{}]\n\
             runtimes = [{}]\n",
            vec![r#"{ type = "apple_foundation_text" }"#; N].join(", "),
            vec![r#"{ type = "apple_foundation" }"#; N].join(", "),
        ))?;
        assert_eq!(job_count(&plan)?, N * N);
        Ok(())
    }

    /// Two cells that render one body expand to one, also when
    /// `merge_duplicates` is `false`; of two tokens, the body keeps the first in
    /// sort order.
    #[rstest]
    #[case::auth_token(
        r#"requires = ["os:macos"]"#,
        r#"{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "r", auth_token = "hf_two" }"#,
        r#"requires = ["os:macos"]"#,
        r#"{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "r", auth_token = "hf_one" }"#,
        Some("hf_one")
    )]
    #[case::injected_flag(
        r#"requires = ["tag:lab-east"]"#,
        r#"{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "r" }"#,
        r#"requires = ["os:macos", "tag:lab-east"]"#,
        r#"{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "r" }"#,
        None
    )]
    fn cells_that_render_one_body_expand_to_one_body(
        #[case] first_requires: &str,
        #[case] first_model: &str,
        #[case] second_requires: &str,
        #[case] second_model: &str,
        #[case] token: Option<&str>,
    ) -> anyhow::Result<()> {
        let variant = |requires: &str, model: &str| {
            format!(
                r#"
[[variants]]
{requires}
models   = [{model}]
runtimes = [{{ type = "mlx_macos_pipette", version = "0.20.0", flavor = "macos-arm64", source = {{ type = "pip_requirements_text", contents = "mlx-lm==0.20.0" }} }}]
"#
            )
        };
        let plan = crate::parse_prefixed(&format!(
            "merge_duplicates = false\nbenchmarks = [\"decode_throughput_512_100\"]\n{}{}",
            variant(first_requires, first_model),
            variant(second_requires, second_model),
        ))?;
        assert_eq!(plan.runnable_cells()?.len(), 2, "two distinct cells");
        assert_eq!(job_count(&plan)?, 2, "the bound counts before the merge");
        let body = only_body(&plan)?;
        assert_eq!(
            body["spec"]["model"]["auth_token"].as_str(),
            token,
            "{body}"
        );
        Ok(())
    }

    #[test]
    fn a_token_and_no_token_expand_to_two_bodies() -> anyhow::Result<()> {
        let runtime = r#"runtimes = [{ type = "mlx_macos_pipette", version = "0.20.0", flavor = "macos-arm64", source = { type = "pip_requirements_text", contents = "mlx-lm==0.20.0" } }]"#;
        let plan = crate::parse_prefixed(&format!(
            r#"benchmarks = ["decode_throughput_512_100"]

[[variants]]
requires = ["os:macos"]
models   = [{{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "r" }}]
{runtime}

[[variants]]
requires = ["os:macos"]
models   = [{{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "r", auth_token = "hf_one" }}]
{runtime}
"#
        ))?;
        let tokens: Vec<_> = bodies(&plan)?
            .iter()
            .map(|body| {
                body["spec"]["model"]["auth_token"]
                    .as_str()
                    .map(str::to_owned)
            })
            .collect();
        assert_eq!(tokens, [Some("hf_one".to_owned()), None]);
        Ok(())
    }

    /// An OVMS variant plans end to end: its flags route to the OVMS cells.
    #[test]
    fn an_ovms_variant_expands_with_its_flags() -> anyhow::Result<()> {
        let plan = SchedulerPlan::parse(
            r#"
benchmarks = ["decode_throughput_512_100", "end_to_end_latency_512_256"]

[[variants]]
requires = ["runtime:ovms"]
models   = [{ type = "openvino", source = "huggingface", org = "OpenVINO", repo_name = "LFM2.5-350M-int8-ov" }]
runtimes = [{ type = "ovms", source = "github_release", repository_url = "github.com/openvinotoolkit/model_server", repository_version = "v2026.4.0", flavor = "windows-x64" }]
runtime_flags = [
  { benchmark_type = "decode_throughput", runtime_type = "ovms", model_type = "openvino", device = "gpu", prefix_caching = false },
  { benchmark_type = "end_to_end_latency", runtime_type = "ovms", model_type = "openvino", device = "gpu" },
]
benchmark_flags = [{ benchmark_type = "decode_throughput", runtime_type = "ovms", model_type = "openvino", readiness = { max_wait_secs = 600 } }]
"#,
        )?;
        let bodies = bodies(&plan)?;
        assert_eq!(bodies.len(), 2);
        bodies.iter().try_for_each(|body| -> anyhow::Result<()> {
            assert_eq!(body["spec"]["runtime"]["type"], "ovms", "{body}");
            assert_eq!(body["spec"]["runtime_flags"]["device"], "gpu", "{body}");
            Ok(())
        })
    }

    /// A TensorRT Edge-LLM variant plans end to end: its two VL cells take
    /// their flags, and the rules add the Jetson GPU the archive needs.
    #[test]
    fn a_trt_edgellm_variant_expands_with_its_flags() -> anyhow::Result<()> {
        let plan = SchedulerPlan::parse(
            r#"
benchmarks = ["vl_throughput_512x512_text0_decode64_img1", "vl_max_memory_512x512_text0_img1"]

[[variants]]
requires = ["tag:lab-orin"]
models   = [{ type = "gguf_vision", source = "vault_url", model = "/vault/resources/vault_00000000-0000-0000-0000-000000000001/content", mmproj = "/vault/resources/vault_00000000-0000-0000-0000-000000000002/content", principal_id = "org_01936d", custom_label = "LFM3-VL-1.9B BF16" }]
runtimes = [{ type = "trt_edgellm", source = "relative_dir", dir = "trt-edgellm", flavor = "jetson-orin-cuda-13" }]
runtime_flags = [
  { benchmark_type = "vl_throughput", runtime_type = "trt_edgellm", model_type = "gguf_vision", precision = "w8a16", raw = ["--robo-embodiment", "0"] },
  { benchmark_type = "vl_max_memory", runtime_type = "trt_edgellm", model_type = "gguf_vision", precision = "w8a16" },
]
benchmark_flags = [{ benchmark_type = "vl_throughput", runtime_type = "trt_edgellm", model_type = "gguf_vision", http_timeout_seconds = 300 }]
"#,
        )?;
        let bodies = bodies(&plan)?;
        assert_eq!(bodies.len(), 2);
        bodies.iter().try_for_each(|body| -> anyhow::Result<()> {
            assert_eq!(body["spec"]["runtime"]["type"], "trt_edgellm", "{body}");
            assert_eq!(
                body["spec"]["runtime_flags"]["precision"], "w8a16",
                "{body}"
            );
            assert_eq!(
                body["requires"],
                serde_json::json!(["accel:gpu", "cpu_vendor:nvidia", "tag:lab-orin"]),
                "{body}"
            );
            Ok(())
        })
    }

    const OPENVINO_CELL: &str = r#"models   = [{ type = "openvino", source = "huggingface", org = "OpenVINO", repo_name = "LFM2.5-350M-int8-ov" }]
runtimes = [{ type = "uv_openvino", server_version = "2026.2.1", python_version = "3.11", source = { type = "pip_requirements_text", contents = "openvino-genai==2026.2.1.0" } }]
runtime_flags = [{ benchmark_type = "decode_throughput", runtime_type = "uv_openvino", model_type = "openvino", device = "cpu" }]"#;
    const ONNX_CUDA_CELL: &str = r#"models   = [{ type = "onnx_genai", source = "huggingface", org = "microsoft", repo_name = "Phi-3-mini-4k-instruct-onnx", prefix = "cuda/cuda-int4-rtn-block-32" }]
runtimes = [{ type = "uv_onnx_genai", server_version = "0.16.0", execution_provider = "cuda", python_version = "3.11", source = { type = "pip_requirements_text", contents = "onnxruntime-genai-cuda==0.16.0" } }]"#;
    const OVMS_WINDOWS_CELL: &str = r#"models   = [{ type = "openvino", source = "huggingface", org = "OpenVINO", repo_name = "LFM2.5-350M-int8-ov" }]
runtimes = [{ type = "ovms", source = "github_release", repository_url = "github.com/openvinotoolkit/model_server", repository_version = "v2026.4.0", flavor = "windows-x64" }]
runtime_flags = [{ benchmark_type = "decode_throughput", runtime_type = "ovms", model_type = "openvino", device = "gpu" }]"#;

    /// No client reports `os:linux` or `os:windows`, so the rules inject
    /// neither: the body keeps the author's flags, plus CUDA's `accel:gpu`.
    #[rstest]
    #[case::openvino_by_tag(OPENVINO_CELL, "tag:lab-east", &["tag:lab-east"])]
    #[case::onnx_cuda_by_tag(
        ONNX_CUDA_CELL,
        "tag:lab-h100-eval",
        &["accel:gpu", "tag:lab-h100-eval"]
    )]
    #[case::ovms_by_tag(OVMS_WINDOWS_CELL, "tag:lab-east", &["tag:lab-east"])]
    fn a_linux_or_windows_variant_keeps_its_authors_flags(
        #[case] cell: &str,
        #[case] requires: &str,
        #[case] expected: &[&str],
    ) -> anyhow::Result<()> {
        let plan = SchedulerPlan::parse(&format!(
            "benchmarks = [\"decode_throughput_512_100\"]\n\n[[variants]]\nrequires = [\"{requires}\"]\n{cell}\n"
        ))?;
        let expansion = expand(&plan)?;
        assert!(expansion.warnings.is_empty(), "{:?}", expansion.warnings);
        let [body] = expansion.bodies.as_slice() else {
            anyhow::bail!("expected one cell, got {:?}", expansion.bodies);
        };
        assert_eq!(body["requires"], serde_json::json!(expected), "{body}");
        assert_eq!(body["any_of"], serde_json::json!([]), "{body}");
        Ok(())
    }

    #[test]
    fn expansion_is_reproducible() -> anyhow::Result<()> {
        assert_eq!(
            bodies(&representative_plan()?)?,
            bodies(&representative_plan()?)?
        );
        Ok(())
    }

    #[rstest]
    #[case("job_id", "is minted at ingestion")]
    #[case("plan_id", "lives only in the manifest")]
    #[case("benchmark_id", "is derived from `spec`")]
    #[case("model_descriptor", "is derived from `spec`")]
    #[case("runtime_descriptor", "is derived from `spec`")]
    fn no_body_carries_a_server_owned_field(
        #[case] field: &str,
        #[case] reason: &str,
    ) -> anyhow::Result<()> {
        bodies(&representative_plan()?)?
            .iter()
            .try_for_each(|body| -> anyhow::Result<()> {
                let object = body.as_object().context("a job body is a JSON object")?;
                anyhow::ensure!(!object.contains_key(field), "{field} {reason}: {body}");
                Ok(())
            })
    }

    /// Validation guarantees `requires` or `clients`; this holds it after the
    /// rules run, since a job with neither is claimable by nobody.
    #[test]
    fn every_body_declares_a_way_to_be_eligible() -> anyhow::Result<()> {
        bodies(&representative_plan()?)?
            .iter()
            .try_for_each(|body| -> anyhow::Result<()> {
                let requires = body["requires"]
                    .as_array()
                    .context("requires is an array")?;
                let clients = body["clients"].as_array().context("clients is an array")?;
                assert!(
                    !requires.is_empty() || !clients.is_empty(),
                    "neither requires nor clients: {body}"
                );
                assert!(body["any_of"].is_array(), "any_of is always emitted");
                Ok(())
            })
    }

    /// A plan that states no value omits the key, never `null`, so the server
    /// applies its own default.
    #[rstest]
    #[case::expiry_instant(
        "expires_at",
        r#"expires_at = "2026-08-01T00:00:00Z""#,
        Some(serde_json::json!("2026-08-01T00:00:00Z"))
    )]
    #[case::expiry_never(
        "expires_at",
        r#"expires_at = "never""#,
        Some(serde_json::json!("never"))
    )]
    #[case::expiry_omitted("expires_at", "", None)]
    #[case::merge_opted_out(
        "merge_duplicates",
        "merge_duplicates = false",
        Some(serde_json::json!(false))
    )]
    #[case::merge_opted_in(
        "merge_duplicates",
        "merge_duplicates = true",
        Some(serde_json::json!(true))
    )]
    #[case::merge_omitted("merge_duplicates", "", None)]
    fn the_plan_envelope_field_is_stamped_on_every_job(
        #[case] field: &str,
        #[case] line: &str,
        #[case] expected: Option<Value>,
    ) -> anyhow::Result<()> {
        let plan = SchedulerPlan::parse(&format!(
            r#"{line}
benchmarks = ["decode_throughput_512_100"]

[[variants]]
requires = ["os:macos"]
models   = [{{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "LFM2.5-350M-MLX-4bit" }}]
runtimes = [{{ type = "mlx_macos_pipette", version = "0.20.0", flavor = "macos-arm64", source = {{ type = "pip_requirements_text", contents = "mlx-lm==0.20.0" }} }}]
"#
        ))?;
        bodies(&plan)?
            .iter()
            .try_for_each(|body| -> anyhow::Result<()> {
                let object = body.as_object().context("a job body is a JSON object")?;
                assert_eq!(object.get(field), expected.as_ref(), "{field} in {body}");
                Ok(())
            })
    }

    /// In the normal form [`Eligibility`](crate::Eligibility) holds. A clause
    /// the variant does not state omits the key.
    #[rstest]
    #[case::exclude_clients(
        "exclude_clients",
        r#"exclude_clients = ["ev1_b", "ev1_a", "ev1_b"]"#,
        Some(serde_json::json!(["ev1_a", "ev1_b"]))
    )]
    #[case::exclude_clients_omitted("exclude_clients", "", None)]
    #[case::excludes(
        "excludes",
        r#"excludes = ["os_version:15.0", "gpu:none"]"#,
        Some(serde_json::json!(["gpu:none", "os_version:15.0"]))
    )]
    #[case::excludes_omitted("excludes", "", None)]
    #[case::not_all_of(
        "not_all_of",
        r#"not_all_of = [["runtime:mlx", "job_retry"]]"#,
        Some(serde_json::json!([["job_retry", "runtime:mlx"]]))
    )]
    #[case::not_all_of_omitted("not_all_of", "", None)]
    fn each_exclusion_clause_reaches_every_job_body(
        #[case] field: &str,
        #[case] line: &str,
        #[case] expected: Option<Value>,
    ) -> anyhow::Result<()> {
        let plan = SchedulerPlan::parse(&format!(
            r#"benchmarks = ["decode_throughput_512_100", "end_to_end_latency_512_256"]

[[variants]]
requires = ["os:macos"]
{line}
models   = [{{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "LFM2.5-350M-MLX-4bit" }}]
runtimes = [{{ type = "mlx_macos_pipette", version = "0.20.0", flavor = "macos-arm64", source = {{ type = "pip_requirements_text", contents = "mlx-lm==0.20.0" }} }}]
"#
        ))?;
        let bodies = bodies(&plan)?;
        assert_eq!(bodies.len(), 2, "one body per benchmark");
        bodies.iter().try_for_each(|body| -> anyhow::Result<()> {
            let object = body.as_object().context("a job body is a JSON object")?;
            assert_eq!(object.get(field), expected.as_ref(), "{field} in {body}");
            Ok(())
        })
    }

    /// An iOS Apple Foundation plan, so the rules inject a device family beside
    /// whatever `line` adds.
    fn afm_plan(line: &str) -> anyhow::Result<SchedulerPlan> {
        SchedulerPlan::parse(&format!(
            r#"benchmarks = ["decode_throughput_512_100"]

[[variants]]
requires = ["os:ios", "tag:lab-phone"]
{line}
models   = [{{ type = "apple_foundation_text" }}]
runtimes = [{{ type = "apple_foundation" }}]
"#
        ))
    }

    #[test]
    fn client_versions_become_the_last_any_of_group() -> anyhow::Result<()> {
        let body = bodies(&afm_plan(
            r#"client_versions = ["2026.10.02-430a0bf-internal", "2026.09.30-8aed2cc-internal"]"#,
        )?)?
        .pop()
        .context("one cell")?;
        let groups = body["any_of"].as_array().context("any_of is an array")?;
        assert_eq!(
            groups.len(),
            2,
            "the device family and the versions: {body}"
        );
        assert!(
            groups[0].to_string().contains("device:iphone"),
            "the injected family comes first: {body}"
        );
        assert_eq!(
            groups[1],
            serde_json::json!([
                "client_version:2026.09.30-8aed2cc-internal",
                "client_version:2026.10.02-430a0bf-internal"
            ])
        );
        Ok(())
    }

    /// A roster-only variant takes no injected group, but it keeps its
    /// versions: the body records what the author wrote.
    #[test]
    fn a_roster_only_variant_keeps_its_versions() -> anyhow::Result<()> {
        let plan = SchedulerPlan::parse(
            r#"
benchmarks = ["decode_throughput_512_100"]

[[variants]]
clients  = ["ev1_9f2c"]
client_versions = ["1.0"]
models   = [{ type = "apple_foundation_text" }]
runtimes = [{ type = "apple_foundation" }]
"#,
        )?;
        let body = bodies(&plan)?.pop().context("one cell")?;
        assert_eq!(
            body["any_of"],
            serde_json::json!([["client_version:1.0"]]),
            "{body}"
        );
        Ok(())
    }

    /// No versions leaves the body as it was, so its content key does not move.
    #[rstest]
    #[case::empty_list("client_versions = []")]
    #[case::omitted("")]
    fn no_client_versions_leave_the_body_unchanged(#[case] line: &str) -> anyhow::Result<()> {
        let without = bodies(&afm_plan("")?)?;
        let with = bodies(&afm_plan(line)?)?;
        assert_eq!(
            serde_json::to_string(&with)?,
            serde_json::to_string(&without)?
        );
        let groups = with
            .first()
            .and_then(|body| body["any_of"].as_array())
            .context("any_of is an array")?;
        assert!(
            !groups
                .iter()
                .any(|g| g.to_string().contains("client_version:")),
            "{groups:?}"
        );
        Ok(())
    }

    #[test]
    fn a_clause_is_never_spelled_null() -> anyhow::Result<()> {
        bodies(&representative_plan()?)?
            .iter()
            .try_for_each(|body| -> anyhow::Result<()> {
                let object = body.as_object().context("a job body is a JSON object")?;
                ["exclude_clients", "excludes", "not_all_of"]
                    .iter()
                    .for_each(|field| {
                        assert_eq!(object.get(*field), None, "{field} in {body}");
                    });
                Ok(())
            })
    }

    /// A claiming client re-parses `spec` as a `ClientRunSpec`, so it must
    /// decode, validate and encode back to the same value.
    #[test]
    fn the_spec_parses_back_as_a_client_run_spec() -> anyhow::Result<()> {
        bodies(&representative_plan()?)?
            .iter()
            .try_for_each(|body| -> anyhow::Result<()> {
                let spec: ClientRunSpec = serde_json::from_value(body["spec"].clone())?;
                spec.validate()?;
                assert_eq!(serde_json::to_value(&spec)?, body["spec"], "{body}");
                Ok(())
            })
    }

    /// The rules inject the supported-iPhone family into the iOS AFM cell.
    #[test]
    fn hardware_rules_reach_the_expanded_body() -> anyhow::Result<()> {
        let afm = bodies(&representative_plan()?)?
            .into_iter()
            .find(|body| body["requires"].to_string().contains("os:ios"))
            .context("the plan has an iOS variant")?;

        let groups = afm["any_of"].as_array().context("any_of is an array")?;
        let devices = groups
            .iter()
            .find(|group| group.to_string().contains("device:iphone"))
            .context("the iOS AFM rules inject a supported-device family")?;
        assert!(
            devices.as_array().is_some_and(|g| g.len() > 1),
            "a device family is a disjunction, not a pin: {devices}"
        );
        assert_eq!(
            afm["clients"],
            serde_json::json!(["ev1_9f2c"]),
            "the variant's own allowlist survives rule resolution"
        );
        Ok(())
    }

    /// A roster-only MLX body must carry no `os:macos` and no chip family:
    /// either would let every Mac on the server claim it.
    #[test]
    fn a_roster_only_body_carries_no_injected_requirement() -> anyhow::Result<()> {
        let plan = SchedulerPlan::parse(
            r#"
benchmarks = ["decode_throughput_512_100"]

[[variants]]
clients  = ["ev1_9f2c"]
models   = [{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "LFM2.5-350M-MLX-4bit" }]
runtimes = [{ type = "mlx_macos_pipette", version = "0.20.0", flavor = "macos-arm64", source = { type = "pip_requirements_text", contents = "mlx-lm==0.20.0" } }]
"#,
        )?;
        let body = bodies(&plan)?.pop().context("one cell")?;
        assert_eq!(body["requires"], serde_json::json!([]), "{body}");
        assert_eq!(body["any_of"], serde_json::json!([]), "{body}");
        assert_eq!(body["clients"], serde_json::json!(["ev1_9f2c"]), "{body}");
        Ok(())
    }

    /// The token sits on the model alone, so ingestion can import the model.
    #[test]
    fn a_plan_authored_hf_token_reaches_the_job_body_model() -> anyhow::Result<()> {
        let plan = crate::parse_prefixed(
            r#"
benchmarks = ["decode_throughput_512_100"]

[[variants]]
requires = ["os:macos"]
models   = [{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "gated-repo", auth_token = "hf_supersecret" }]
runtimes = [{ type = "mlx_macos_pipette", version = "0.20.0", flavor = "macos-arm64", source = { type = "pip_requirements_text", contents = "mlx-lm==0.20.0" } }]
"#,
        )?;
        let body = only_body(&plan)?;
        assert_eq!(body["spec"]["model"]["auth_token"], "hf_supersecret");
        let mut rest = body.clone();
        if let Some(model) = rest["spec"]["model"].as_object_mut() {
            model.remove("auth_token");
        }
        assert!(!rest.to_string().contains("hf_supersecret"), "{rest}");
        Ok(())
    }

    /// A reader that derives the runs from `cells` names what the jobs run,
    /// token and environment values included.
    #[test]
    fn every_body_spec_is_a_cell_spec() -> anyhow::Result<()> {
        let plan = SchedulerPlan::parse(
            r#"
benchmarks = ["prefill_throughput_512", "decode_throughput_512_100"]

[[variants]]
clients  = ["ev1_9f2c"]
models   = [{ type = "gguf_text", source = "huggingface", org = "o", repo_name = "gated", path = "a.gguf", auth_token = "hf_supersecret" }]
runtimes = [{ type = "llamacpp_cli_stock_tools", source = "remote_archive", url = "host.example/llamacpp/bin.tar.gz", flavor = "macos-arm64" }]
runtime_flags = [{ benchmark_type = "prefill_throughput", runtime_type = "llamacpp_cli_stock_tools", model_type = "gguf_text", envs = ["GGML_METAL=1", "LLAMA_API_KEY=sk-device"] }]
"#,
        )?;
        let cells = plan.cells()?;
        anyhow::ensure!(
            cells
                .iter()
                .all(|cell| cell.spec.model.auth_token().is_some()),
            "a cell keeps the authored token"
        );
        let from_bodies: HashSet<String> = bodies(&plan)?
            .iter()
            .map(|body| body["spec"].to_string())
            .collect();
        let from_cells = cells
            .iter()
            .map(|cell| serde_json::to_value(&cell.spec).map(|spec| spec.to_string()))
            .collect::<Result<HashSet<_>, _>>()?;
        assert_eq!(from_bodies, from_cells);
        assert_eq!(from_cells.len(), 2);
        let rendered = format!("{from_bodies:?}");
        assert!(rendered.contains("hf_supersecret"), "{rendered}");
        assert!(rendered.contains("LLAMA_API_KEY=sk-device"), "{rendered}");
        assert!(rendered.contains("https://host.example/"), "{rendered}");
        Ok(())
    }

    /// The error names the variant at fault, as a field and in its message.
    #[test]
    fn a_rule_violating_plan_expands_to_nothing() -> anyhow::Result<()> {
        let plan = SchedulerPlan::parse(
            r#"
benchmarks = ["decode_throughput_512_100"]

[[variants]]
requires = ["os:ios"]
models   = [{ type = "apple_foundation_text" }]
runtimes = [{ type = "apple_foundation" }]

[[variants]]
requires = ["os:android"]
models   = [{ type = "apple_foundation_text" }]
runtimes = [{ type = "apple_foundation" }]
"#,
        )?;
        let Err(error @ ExpandError::CapabilityRule(_)) = expand(&plan) else {
            anyhow::bail!("Apple Foundation on os:android is a contradiction");
        };
        assert_eq!(error.variant(), Some(1), "{error:?}");
        let message = format!("{:#}", anyhow::Error::new(error));
        anyhow::ensure!(message.contains("variant 1"), "{message}");
        Ok(())
    }

    /// Each warning's variant index is the one its message names.
    #[test]
    fn a_warning_names_its_variant_as_a_field() -> anyhow::Result<()> {
        let plan = SchedulerPlan::parse(
            r#"
benchmarks = ["decode_throughput_512_100"]

[[variants]]
requires = ["os:macos"]
models   = [{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "LFM2.5-350M-MLX-4bit" }]
runtimes = [{ type = "mlx_macos_pipette", version = "0.20.0", flavor = "macos-arm64", source = { type = "pip_requirements_text", contents = "mlx-lm==0.20.0" } }]

[[variants]]
requires        = ["os:macos"]
clients         = ["ev1_a"]
exclude_clients = ["ev1_a"]
client_versions = ["1.0"]
models          = [{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "LFM2-700M-MLX-4bit" }]
runtimes        = [{ type = "mlx_macos_pipette", version = "0.20.0", flavor = "macos-arm64", source = { type = "pip_requirements_text", contents = "mlx-lm==0.20.0" } }]

[[variants]]
requires        = ["os:macos"]
clients         = ["ev1_b"]
client_versions = ["1.0"]
models          = [{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "LFM2-1.2B-MLX-4bit" }]
runtimes        = [{ type = "mlx_macos_pipette", version = "0.20.0", flavor = "macos-arm64", source = { type = "pip_requirements_text", contents = "mlx-lm==0.20.0" } }]
"#,
        )?;
        let warnings = expand(&plan)?.warnings;
        let variants: Vec<usize> = warnings.iter().map(|warning| warning.variant).collect();
        assert_eq!(variants, [1, 2], "{warnings:?}");
        warnings.iter().for_each(|warning| {
            assert!(
                warning
                    .message
                    .starts_with(&format!("variant {}", warning.variant)),
                "{}",
                warning.message
            );
        });
        Ok(())
    }

    #[test]
    fn a_rule_warning_is_returned_with_the_bodies() -> anyhow::Result<()> {
        let plan = SchedulerPlan::parse(
            r#"
benchmarks = ["decode_throughput_512_100"]

[[variants]]
requires        = ["os:macos"]
clients         = ["ev1_a"]
exclude_clients = ["ev1_a"]
models          = [{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "LFM2.5-350M-MLX-4bit" }]
runtimes        = [{ type = "mlx_macos_pipette", version = "0.20.0", flavor = "macos-arm64", source = { type = "pip_requirements_text", contents = "mlx-lm==0.20.0" } }]
"#,
        )?;
        let expansion = expand(&plan)?;
        assert_eq!(expansion.bodies.len(), 1);
        anyhow::ensure!(
            expansion
                .warnings
                .iter()
                .any(|warning| warning.message.contains("the roster adds nobody")),
            "{:?}",
            expansion.warnings
        );
        Ok(())
    }

    fn eval_plan(model: &str, runtime: &str, model_type: &str, knobs: &str) -> String {
        format!(
            r#"benchmarks = ["eval_smoke"]

[[variants]]
requires    = ["tag:lab-h100-eval"]
models      = [{model}]
runtimes    = [{runtime}]
model_flags = [{{ benchmark_type = "eval", model_type = "{model_type}", {knobs} }}]
"#
        )
    }

    const TORCH_MODEL: &str =
        r#"{ type = "torch", source = "huggingface", org = "openai", repo_name = "gpt-oss-20b" }"#;
    const VLLM_RUNTIME: &str = r#"{ type = "docker_vllm", image_name = "vllm/vllm-openai", image_tag = "v0.21.0", flavor = "nvidia_gpu" }"#;

    #[test]
    fn a_reasoning_effort_reaches_the_jobs_spec() -> anyhow::Result<()> {
        let plan = SchedulerPlan::parse(&eval_plan(
            TORCH_MODEL,
            VLLM_RUNTIME,
            "torch",
            r#"reasoning_effort = "low""#,
        ))?;
        let body = only_body(&plan)?;
        assert_eq!(
            body["spec"]["model_flags"],
            serde_json::json!({
                "model_type": "torch",
                "benchmark_type": "eval",
                "reasoning_effort": "low",
            }),
            "{body}"
        );
        Ok(())
    }

    #[test]
    fn a_reasoning_effort_on_an_mlx_eval_is_refused() -> anyhow::Result<()> {
        let document = eval_plan(
            r#"{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "LFM2.5-350M-MLX-4bit" }"#,
            r#"{ type = "mlx_macos_pipette", version = "0.20.0", flavor = "macos-arm64", source = { type = "pip_requirements_text", contents = "mlx-lm==0.20.0" } }"#,
            "mlx",
            r#"reasoning_effort = "low""#,
        );
        let Err(error) = SchedulerPlan::parse(&document) else {
            anyhow::bail!("expected the plan to be refused");
        };
        let message = format!("{error:#}");
        anyhow::ensure!(
            message.contains("reasoning_effort is not a valid model flag"),
            "{message}"
        );
        Ok(())
    }
}
