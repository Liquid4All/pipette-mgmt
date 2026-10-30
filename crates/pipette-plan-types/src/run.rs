//! One cell as a client runs it: what an engine is handed ([`RunRequest`](crate::run::RunRequest)) and
//! what it answers with ([`RunResponse`](crate::run::RunResponse)). Resolving a
//! [`ClientRunSpec`](crate::ClientRunSpec) into a request needs a workspace catalog and
//! artifact stores, so that step lives client-side in `pipette-cli`.
//!
//! A response is not sent: a client builds
//! [`BenchmarkSubmissionPayload`](crate::submission::BenchmarkSubmissionPayload) from it.

use serde::{Deserialize, Serialize};

use crate::benchmark::BenchmarkDefinition;
use crate::submission::{BenchmarkResultData, MemoryObservation, RuntimeOutput};
use crate::thermal::RunThermal;
use crate::{
    BenchmarkFlagRef, BenchmarkFlags, BenchmarkType, Model, ModelFlags, ModelType, Runtime,
    RuntimeFlagRef, RuntimeFlags, RuntimeType, Sha256,
};

/// Plan identity vs host-bound form of one artifact (runtime or model).
///
/// `Serialize` emits `declared` alone, so a digest of a whole [`RunRequest`] is
/// portable by construction. The halves are equal for an artifact the store
/// holds but cannot relocate (a docker image, Apple Foundation).
#[derive(Debug, Clone, Serialize)]
#[serde(transparent)]
pub struct DeclaredBound<T> {
    /// Plan / pull coordinate (storage key, descriptors, record).
    pub declared: T,
    /// Launch form on this host after ensure + bind_under; meaningless off it,
    /// hence not serialized.
    #[serde(skip)]
    pub bound: T,
}

impl<T: Clone> DeclaredBound<T> {
    /// Both halves the same value: nothing relocated it (yet).
    pub fn already_bound(value: T) -> Self {
        Self {
            declared: value.clone(),
            bound: value,
        }
    }
}

/// Everything a runtime seam needs for one cell.
///
/// The client logs this via `Debug` at dispatch, so a secret it gains must
/// redact itself the way [`AuthToken`](crate::AuthToken) does.
#[derive(Debug, Clone, Serialize)]
pub struct RunRequest {
    pub runtime: DeclaredBound<Runtime>,
    pub model: DeclaredBound<Model>,
    pub runtime_flags: Option<RuntimeFlags>,
    pub model_flags: Option<ModelFlags>,
    pub benchmark_flags: Option<BenchmarkFlags>,
    pub benchmark: BenchmarkDefinition,
}

impl RunRequest {
    /// The cell's flags in flat form, or an all-unset ref when the run carries
    /// none. Engines overlay their values and convert back for
    /// [`RunResponse::runtime_flags`], so reported flags are validated like an
    /// authored entry. The axis check backstops a caller that skipped `prepare`.
    pub fn runtime_flags_ref(&self) -> Result<RuntimeFlagRef, RunRequestError> {
        let axes = (
            self.benchmark.benchmark_type(),
            RuntimeType::of(&self.runtime.declared),
            ModelType::of(&self.model.declared),
        );
        let Some(flags) = self.runtime_flags.as_ref() else {
            return Ok(RuntimeFlagRef::new(axes.0, axes.1, axes.2));
        };
        if flags.axes() != axes {
            return Err(RunRequestError::RuntimeFlagsForAnotherCell {
                flags: Box::new(flags.clone()),
                cell: axes,
            });
        }
        Ok(RuntimeFlagRef::from(flags.clone()))
    }

    /// As [`Self::runtime_flags_ref`], for the caller's readiness block. A cell
    /// whose variant has no readiness field rejects one, correctly: it does not gate.
    pub fn benchmark_flags_ref(&self) -> Result<BenchmarkFlagRef, RunRequestError> {
        let axes = (
            self.benchmark.benchmark_type(),
            RuntimeType::of(&self.runtime.declared),
            ModelType::of(&self.model.declared),
        );
        let Some(flags) = self.benchmark_flags.as_ref() else {
            return Ok(BenchmarkFlagRef {
                benchmark_type: axes.0,
                runtime_type: axes.1,
                model_type: axes.2,
                http_timeout_seconds: None,
                bench_timeout_seconds: None,
                _doomloop_obsolete: None,
                readiness: None,
            });
        };
        if flags.axes() != axes {
            return Err(RunRequestError::BenchmarkFlagsForAnotherCell {
                flags: Box::new(flags.clone()),
                cell: axes,
            });
        }
        Ok(BenchmarkFlagRef::from(flags.clone()))
    }

    /// The cell's runtime flags as resolved: the authored values, typed and
    /// checked for this cell.
    pub fn resolved_runtime_flags(&self) -> Result<RuntimeFlags, RunRequestError> {
        Ok(RuntimeFlags::try_from(self.runtime_flags_ref()?)?)
    }
}

/// A [`RunRequest`] carries flags resolved for a different cell than its own.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RunRequestError {
    #[error("runtime flags {flags:?} are not for this cell {cell:?}")]
    RuntimeFlagsForAnotherCell {
        flags: Box<RuntimeFlags>,
        cell: (BenchmarkType, RuntimeType, ModelType),
    },
    #[error("benchmark flags {flags:?} are not for this cell {cell:?}")]
    BenchmarkFlagsForAnotherCell {
        flags: Box<BenchmarkFlags>,
        cell: (BenchmarkType, RuntimeType, ModelType),
    },
    #[error(transparent)]
    RuntimeFlags(#[from] crate::cell::RuntimeFlagsError),
}

/// What one **run** answers with: metrics, streams, and the request's flags as
/// the run resolved them. The client pairs it with the [`RunRequest`] it was
/// handed, which is what the submitted descriptors come from.
pub struct RunResponse {
    pub result_data: BenchmarkResultData,
    /// What the runtime's processes printed, whole; `None` for an uncaptured
    /// stream. The submission sends the tail ([`RuntimeOutput::capped`]).
    pub runtime_output: RuntimeOutput,
    /// What the run reports that no other field names, submitted as
    /// `client_extras`: an object, at most
    /// [`MAX_CLIENT_EXTRAS_BYTES`](crate::submission::MAX_CLIENT_EXTRAS_BYTES)
    /// serialized. Empty sends none.
    pub client_extras: serde_json::Map<String, serde_json::Value>,
    /// Thermal snapshots bracketing each measured repetition, filled by the
    /// caller from the engine's gate marks: the probe is the caller's.
    pub thermal: RunThermal,
    /// The cell's benchmark flags with the readiness policy that applied. Filled
    /// by the caller: an engine gets an opaque gate and never learns the policy.
    pub benchmark_flags: Option<BenchmarkFlags>,
    /// Invocation preview recorded in the result extras. Empty for a runtime
    /// that doesn't shell out a named command (MLX runs in-process over HTTP).
    pub command: Vec<String>,
    /// The runtime executable when it's a distinct binary (llama.cpp); `None`
    /// for a runtime that launches a server (MLX/torch-oai).
    pub executable: Option<String>,
    /// [`RunRequest::runtime_flags`] with the values the engine derived filled
    /// in; the diff is what the run decided. `None` for a runtime that takes no
    /// flags. Flags a benchmark fixes can't appear (see
    /// [`RuntimeFlags::submission_value`](crate::RuntimeFlags::submission_value)).
    pub runtime_flags: Option<RuntimeFlags>,
    /// What memory the run held ([`MemoryObservation`]), on every benchmark
    /// kind. Filled by the **engine**, which alone has the child's pid; a
    /// platform without a sampler leaves the "observed nothing" default.
    pub memory: MemoryObservation,
    /// The sha256 of the corpus tar this run downloaded and verified, submitted
    /// as `observation_dataset_digest` whether or not it matched the catalog's
    /// pin. `None` for a run that pulled no corpus.
    pub dataset_digest: Option<Sha256>,
    /// The digest over a corpus traversal's replies, in order; `None` for
    /// every other kind. Submitted as `observation_output_digest`.
    pub output_digest: Option<Sha256>,
    /// One line per turn of a corpus traversal, in order; empty for every other
    /// kind. Never submitted: per-sample analysis reads it from local extras.
    pub turns: Vec<TurnObservation>,
}

/// What one turn of a corpus traversal produced, kept beside the run record
/// and never sent. The reply's sha256, not its text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnObservation {
    /// The corpus sample this turn belongs to, and its index within it.
    pub sample_id: String,
    pub turn: u32,
    pub text_sha256: Sha256,
    pub generated_tokens: u64,
    /// Zero on a row with no drafter.
    pub drafted_tokens: u64,
    pub accepted_tokens: u64,
    /// Decode-time target forwards the runtime counted (`verify_steps` in
    /// older records). Zero where it counted none, and then not written.
    #[serde(default, alias = "verify_steps", skip_serializing_if = "is_zero")]
    pub counted_forwards: u64,
    /// Whether the turn stopped on the model's own end-of-sequence rather than
    /// at `max_tokens`. Always written: `false` is a truncated reply.
    #[serde(default)]
    pub ended_on_eos: bool,
    /// The runtime's prefill and generation time for this turn.
    pub prompt_ms: f64,
    pub predicted_ms: f64,
    /// The client clock's three spans for this turn.
    pub ttft_ms: f64,
    pub decode_ms: f64,
    pub e2e_ms: f64,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

impl RunResponse {
    /// The metrics half, with provenance empty for the engine to fill;
    /// `thermal` and `benchmark_flags` are the caller's.
    pub fn new(result_data: BenchmarkResultData, runtime_output: RuntimeOutput) -> Self {
        Self {
            result_data,
            runtime_output,
            client_extras: serde_json::Map::new(),
            thermal: RunThermal::default(),
            benchmark_flags: None,
            command: Vec::new(),
            executable: None,
            runtime_flags: None,
            memory: MemoryObservation::default(),
            dataset_digest: None,
            output_digest: None,
            turns: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Context;
    use rstest::rstest;

    use super::*;
    use crate::benchmark::EvalBenchmark;
    use crate::runtime::cli_release;
    use crate::{
        AuthToken, GgufText, GgufTextSource, GgufVision, GgufVisionHfFiles, GgufVisionSource,
        HfRepo, LlamaCppFlavor, LlamacppCliStockTools, LlamacppCliStockToolsSource, Mlx,
        ModelSource, RepoSubpath, Torch,
    };

    /// The shared record/submit path relies on this for runtimes without an invocation.
    #[test]
    fn new_leaves_provenance_empty() {
        let result_data = BenchmarkResultData::DecodeThroughput {
            decode_time_ms: 1.0,
            decode_time_ms_stddev: None,
        };
        let runtime_output = RuntimeOutput {
            runtime_stdout: Some("out".into()),
            runtime_stderr: None,
        };
        let outcome = RunResponse::new(result_data.clone(), runtime_output.clone());
        assert_eq!(outcome.result_data, result_data);
        assert!(outcome.command.is_empty());
        assert_eq!(outcome.thermal, RunThermal::default());
        assert!(outcome.benchmark_flags.is_none());
        assert!(outcome.executable.is_none());
        assert!(outcome.runtime_flags.is_none());
        assert_eq!(outcome.memory, MemoryObservation::default());
        assert!(outcome.dataset_digest.is_none());
        assert!(outcome.output_digest.is_none());
        assert!(outcome.turns.is_empty());
        assert!(outcome.client_extras.is_empty());
        assert_eq!(outcome.runtime_output, runtime_output);
    }

    /// Serialized as `declared` alone; `already_bound` is the unrelocated case.
    #[test]
    fn declared_bound_serializes_the_declared_half() -> anyhow::Result<()> {
        let relocated = DeclaredBound {
            declared: "models/m.gguf",
            bound: "/store/models/m.gguf",
        };
        assert_eq!(
            serde_json::to_value(&relocated)?,
            serde_json::json!("models/m.gguf")
        );
        let held = DeclaredBound::already_bound("vllm/vllm-openai:v0.21.0");
        assert_eq!(held.declared, "vllm/vllm-openai:v0.21.0");
        assert_eq!(held.bound, "vllm/vllm-openai:v0.21.0");
        Ok(())
    }

    fn turn(counted_forwards: u64) -> anyhow::Result<TurnObservation> {
        Ok(TurnObservation {
            sample_id: "mmspec_001".to_owned(),
            turn: 0,
            text_sha256: crate::Sha256::try_new("0".repeat(64))?,
            generated_tokens: 128,
            drafted_tokens: 423,
            accepted_tokens: 84,
            counted_forwards,
            ended_on_eos: true,
            prompt_ms: 1.0,
            predicted_ms: 10.0,
            ttft_ms: 1.0,
            decode_ms: 10.0,
            e2e_ms: 12.0,
        })
    }

    /// A record written under the old `verify_steps` name still reads; a new
    /// one writes the current name, or nothing at zero.
    #[test]
    fn counted_forwards_reads_its_old_name_and_writes_its_new_one() -> anyhow::Result<()> {
        let old = serde_json::to_string(&turn(47)?)?.replace("counted_forwards", "verify_steps");
        assert!(old.contains("verify_steps"), "{old}");
        let read: TurnObservation = serde_json::from_str(&old)?;
        assert_eq!(read.counted_forwards, 47);

        let written = serde_json::to_string(&turn(2)?)?;
        assert!(written.contains("\"counted_forwards\":2"), "{written}");
        let uncounted = serde_json::to_string(&turn(0)?)?;
        assert!(!uncounted.contains("counted_forwards"), "{uncounted}");
        Ok(())
    }

    /// `false` is a truncated reply, so it is written; a record from before the
    /// field reads as `false`.
    #[test]
    fn ended_on_eos_is_always_written_and_defaults_to_false() -> anyhow::Result<()> {
        let mut truncated = turn(0)?;
        truncated.ended_on_eos = false;
        assert_eq!(serde_json::to_value(&truncated)?["ended_on_eos"], false);

        let mut old = serde_json::to_value(turn(0)?)?;
        old.as_object_mut()
            .context("a turn serializes to an object")?
            .remove("ended_on_eos");
        assert!(!serde_json::from_value::<TurnObservation>(old)?.ended_on_eos);
        Ok(())
    }

    const TOKEN: &str = "hf_tokenthatmustnotescape";

    fn gated_repo() -> anyhow::Result<HfRepo> {
        let mut repo = HfRepo::parse_org_repo("liquid-ai/gated")?;
        repo.auth_token = Some(AuthToken::try_new(TOKEN.to_owned())?);
        Ok(repo)
    }

    fn gated_gguf_text() -> anyhow::Result<Model> {
        Ok(Model::GgufText(GgufText {
            source: GgufTextSource::HuggingFace {
                repo: gated_repo()?,
                path: RepoSubpath::try_new("model.gguf".to_owned())?,
                sha256: None,
            },
        }))
    }

    /// One repo covering both files, so a single token reaches two paths.
    fn gated_gguf_vision() -> anyhow::Result<Model> {
        Ok(Model::GgufVision(GgufVision {
            source: GgufVisionSource::HuggingFace(GgufVisionHfFiles {
                repo: gated_repo()?,
                model: RepoSubpath::try_new("model.gguf".to_owned())?,
                model_sha256: None,
                mmproj: RepoSubpath::try_new("mmproj.gguf".to_owned())?,
                mmproj_sha256: None,
            }),
        }))
    }

    fn gated_mlx() -> anyhow::Result<Model> {
        Ok(Model::Mlx(Mlx {
            source: ModelSource::HuggingFace {
                repo: gated_repo()?,
                prefix: None,
            },
        }))
    }

    fn gated_torch() -> anyhow::Result<Model> {
        Ok(Model::Torch(Torch {
            source: ModelSource::HuggingFace {
                repo: gated_repo()?,
                prefix: None,
            },
        }))
    }

    /// Every optional group filled, so the dump walks the flag subtrees. The
    /// token is on both halves of `model`, so redacting only `declared` fails.
    fn gated_run_request(model: Model) -> anyhow::Result<RunRequest> {
        let runtime = Runtime::LlamacppCliStockTools(LlamacppCliStockTools {
            source: LlamacppCliStockToolsSource::GithubRelease(cli_release("b10142")?),
            flavor: LlamaCppFlavor::MacosArm64,
        });
        Ok(RunRequest {
            runtime: DeclaredBound::already_bound(runtime),
            model: DeclaredBound::already_bound(model),
            runtime_flags: Some(RuntimeFlags::EvalLlamacppCliStockToolsGgufText(
                crate::EvalLlamacppCliStockToolsGgufText {
                    threads: Some(4),
                    number_gpu_layers: None,
                    mmap: None,
                    load_mode: None,
                    flash_attention: None,
                    ctx_size: None,
                    no_cache: None,
                    parallel: None,
                    envs: Vec::new(),
                    raw: vec!["--verbose".to_owned()],
                },
            )),
            model_flags: Some(ModelFlags::EvalGgufText {
                enable_thinking: Some(true),
                reasoning_effort: None,
            }),
            benchmark_flags: Some(BenchmarkFlags::EvalLlamacppCliStockToolsGgufText {
                http_timeout_seconds: Some(crate::PositiveSeconds::try_new(15)?),
            }),
            benchmark: BenchmarkDefinition::Eval(EvalBenchmark {
                benchmark_id: crate::BenchmarkId::try_new("eval_gated".to_owned())?,
                parameter_eval_id: "gated".into(),
                parameter_dataset_name: "local".to_owned(),
                parameter_max_tokens: crate::PositiveCount::try_new(8)?,
                parameter_max_context: None,
                parameter_mcq_choices: None,
                parameter_temperature: None,
                parameter_top_p: None,
                parameter_top_k: None,
                parameter_min_p: None,
                parameter_seed: None,
                samples: None,
            }),
        })
    }

    /// `Debug` is logged at dispatch. One case per model shape: each has its
    /// own source enum and only the shared [`AuthToken`] redacts.
    #[rstest]
    #[case::gguf_text(gated_gguf_text)]
    #[case::gguf_vision(gated_gguf_vision)]
    #[case::mlx(gated_mlx)]
    #[case::torch(gated_torch)]
    fn debug_redacts_the_model_auth_token(
        #[case] gated_model: fn() -> anyhow::Result<Model>,
    ) -> anyhow::Result<()> {
        let dumped = format!("{:?}", gated_run_request(gated_model()?)?);
        assert!(
            !dumped.contains(TOKEN),
            "the dispatch log would publish the token: {dumped}"
        );
        assert!(
            dumped.contains("AuthToken(<redacted>)"),
            "no redaction marker, so the token never reached the dump: {dumped}"
        );
        Ok(())
    }

    /// Rebind to a runtime that differs from `declared` in every field, so an
    /// emitted `bound` cannot coincide with the declared one.
    fn rebind_runtime(req: &mut RunRequest) -> anyhow::Result<()> {
        req.runtime.bound = Runtime::LlamacppCliStockTools(LlamacppCliStockTools {
            source: LlamacppCliStockToolsSource::GithubRelease(cli_release("b1")?),
            flavor: LlamaCppFlavor::LinuxX64Cpu,
        });
        Ok(())
    }

    /// The host-bound halves must not reach the serialized projection.
    #[rstest]
    #[case::bound_runtime(rebind_runtime)]
    fn serialize_omits_host_state(
        #[case] mutate: fn(&mut RunRequest) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let baseline = serde_json::to_value(gated_run_request(gated_gguf_text()?)?)?;

        let mut moved = gated_run_request(gated_gguf_text()?)?;
        mutate(&mut moved)?;

        assert_eq!(
            serde_json::to_value(moved)?,
            baseline,
            "host state leaked into the serialized projection"
        );
        Ok(())
    }

    /// Engines overlay onto this ref and convert back, so it must round-trip.
    #[test]
    fn flag_refs_round_trip_the_cells_flags() -> anyhow::Result<()> {
        let req = gated_run_request(gated_gguf_text()?)?;
        assert_eq!(
            Some(RuntimeFlags::try_from(req.runtime_flags_ref()?)?),
            req.runtime_flags
        );
        assert_eq!(
            Some(BenchmarkFlags::try_from(req.benchmark_flags_ref()?)?),
            req.benchmark_flags
        );
        Ok(())
    }

    #[test]
    fn flag_refs_default_to_an_unset_ref_for_the_cell() -> anyhow::Result<()> {
        let mut req = gated_run_request(gated_gguf_text()?)?;
        req.runtime_flags = None;
        req.benchmark_flags = None;
        let axes = (
            crate::BenchmarkType::Eval,
            RuntimeType::LlamacppCliStockTools,
            ModelType::GgufText,
        );
        let runtime = RuntimeFlags::try_from(req.runtime_flags_ref()?)?;
        assert_eq!(runtime.axes(), axes);
        assert_eq!(runtime.knobs_json(), serde_json::json!({}));
        let benchmark = BenchmarkFlags::try_from(req.benchmark_flags_ref()?)?;
        assert_eq!(benchmark.axes(), axes);
        assert_eq!(benchmark.submission_value(), serde_json::json!({}));
        Ok(())
    }

    #[test]
    fn flag_refs_refuse_flags_for_another_cell() -> anyhow::Result<()> {
        let mut req = gated_run_request(gated_gguf_text()?)?;
        req.benchmark =
            BenchmarkDefinition::MaxMemoryUsage(crate::benchmark::MaxMemoryUsageBenchmark {
                benchmark_id: crate::BenchmarkId::try_new("max_memory_usage_512".to_owned())?,
                parameter_prefill_tokens: crate::NonNegativeCount::try_new(512)?,
            });
        for refusal in [
            req.runtime_flags_ref().map(drop),
            req.benchmark_flags_ref().map(drop),
        ] {
            let message = refusal.err().map(|e| e.to_string());
            assert!(
                message
                    .as_deref()
                    .is_some_and(|m| m.contains("are not for this cell")),
                "{message:?}"
            );
        }
        Ok(())
    }

    /// `DeclaredBound` is transparent, so `runtime` is a `Runtime`, not a
    /// wrapper object; callers digesting the request depend on this shape.
    #[test]
    fn serialize_keeps_declared_axes_unwrapped() -> anyhow::Result<()> {
        let req = gated_run_request(gated_gguf_text()?)?;
        let value = serde_json::to_value(&req)?;
        assert_eq!(
            value["runtime"],
            serde_json::to_value(&req.runtime.declared)?
        );
        assert_eq!(value["model"], serde_json::to_value(&req.model.declared)?);
        Ok(())
    }
}
