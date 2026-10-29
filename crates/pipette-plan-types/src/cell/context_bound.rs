//! Whether a cell can hold an eval sample to the context its entry bounds it
//! at ([`EvalBenchmark::check_cell_context`]).

use thiserror::Error;

use super::reserved_flags::deny_match;
use crate::{
    EvalBenchmark, EvalLlamacppCliStockToolsGgufText, MaxContextError, PositiveCount, RuntimeFlags,
    RuntimeType,
};

/// llama-server flags that resize a slot or let a sample run past it; the
/// `--no-*` spellings restate the defaults and pass.
const LLAMA_SERVER_SLOT_CONTEXT_FLAGS: &[&str] = &[
    "--kv-unified",
    "-kvu",
    "--kv-unified-per-slot",
    "--context-shift",
    "--parallel",
    "-np",
];

/// The same settings as llama-server's environment variables.
const LLAMA_SERVER_SLOT_CONTEXT_ENVS: &[&str] = &[
    "LLAMA_ARG_KV_UNIFIED",
    "LLAMA_ARG_KV_UNIFIED_PER_SLOT",
    "LLAMA_ARG_CONTEXT_SHIFT",
    "LLAMA_ARG_N_PARALLEL",
];

impl EvalBenchmark {
    /// Whether a cell on `runtime`, with `flags`, holds each sample to this
    /// entry's context bound. Here so clients share the rule, as with
    /// [`Self::sample_seed`]. A bounded cell must pin its context: clients size
    /// an unpinned one from `parameter_max_tokens` alone.
    pub fn check_cell_context(
        &self,
        runtime: RuntimeType,
        flags: Option<&RuntimeFlags>,
    ) -> Result<(), EvalContextError> {
        self.check_max_context()?;
        let Some(max_context) = self.parameter_max_context else {
            return self.check_room_for_a_prompt(flags);
        };
        match runtime {
            RuntimeType::DockerVllm | RuntimeType::UvVllm => match vllm_max_model_len(flags) {
                None => Err(EvalContextError::ContextUnpinned {
                    knob: "max_model_len",
                    value: u64::from(max_context.into_inner().unsigned_abs()),
                    max_context,
                }),
                Some(max_model_len) if max_model_len != max_context => {
                    Err(EvalContextError::MaxModelLenDisagrees {
                        max_model_len,
                        max_context,
                    })
                }
                Some(_) => Ok(()),
            },
            RuntimeType::LlamacppCliStockTools => {
                if let Some(RuntimeFlags::EvalLlamacppCliStockToolsGgufText(flags)) = flags {
                    return check_llama_server_slot(flags, max_context, self.parameter_max_tokens);
                }
                // The CLI's other eval cells carry no `ctx_size` to pin.
                Err(EvalContextError::ContextUnpinned {
                    knob: "ctx_size",
                    value: u64::from(max_context.into_inner().unsigned_abs()),
                    max_context,
                })
            }
            RuntimeType::LlamacppApkPipette
            | RuntimeType::LlamacppIosPipette
            | RuntimeType::MlxMacosPipette
            | RuntimeType::MlxIosPipette
            | RuntimeType::DockerSglang
            | RuntimeType::UvSglang
            | RuntimeType::UvOpenvino
            | RuntimeType::UvOnnxGenai
            | RuntimeType::Ovms
            | RuntimeType::OnnxGenaiIosPipette
            | RuntimeType::AppleFoundation
            | RuntimeType::TrtEdgellm => Err(EvalContextError::RuntimeCannotBound {
                runtime,
                max_context,
            }),
        }
    }

    /// A pin at or under the output cap bounds a context this entry leaves
    /// unbounded; MCQ requests ask for a few tokens and always fit.
    fn check_room_for_a_prompt(
        &self,
        flags: Option<&RuntimeFlags>,
    ) -> Result<(), EvalContextError> {
        match vllm_max_model_len(flags) {
            Some(max_model_len)
                if self.parameter_mcq_choices.is_none()
                    && max_model_len <= self.parameter_max_tokens =>
            {
                Err(EvalContextError::NoRoomForPrompt {
                    max_model_len,
                    max_tokens: self.parameter_max_tokens,
                })
            }
            _ => Ok(()),
        }
    }
}

fn vllm_max_model_len(flags: Option<&RuntimeFlags>) -> Option<PositiveCount> {
    if let Some(
        RuntimeFlags::EvalDockerVllmTorch { max_model_len, .. }
        | RuntimeFlags::EvalUvVllmTorch { max_model_len, .. },
    ) = flags
    {
        *max_model_len
    } else {
        None
    }
}

/// llama-server splits `ctx_size` across `parallel` slots. Unset, its four
/// default slots share one pool, but requests are serial, so one slot has it all.
fn check_llama_server_slot(
    flags: &EvalLlamacppCliStockToolsGgufText,
    max_context: PositiveCount,
    max_tokens: PositiveCount,
) -> Result<(), EvalContextError> {
    if let Some(flag) = flags
        .raw
        .iter()
        .find(|token| deny_match(LLAMA_SERVER_SLOT_CONTEXT_FLAGS, token).is_some())
    {
        return Err(EvalContextError::RawChangesSlotContext {
            flag: flag.clone(),
            max_context,
        });
    }
    if let Some(env) = flags
        .envs
        .iter()
        .find(|env| LLAMA_SERVER_SLOT_CONTEXT_ENVS.contains(&env.name()))
    {
        return Err(EvalContextError::EnvChangesSlotContext {
            name: env.name().to_owned(),
            max_context,
        });
    }
    let slots = flags.parallel.map_or(1, |n| n.into_inner().unsigned_abs());
    // The llama.cpp client refuses a multi-slot run whose slot is under `max_tokens`.
    if slots > 1 && max_context < max_tokens {
        return Err(EvalContextError::SlotUnderMaxTokens {
            slots,
            max_context,
            max_tokens,
        });
    }
    let expected = u64::from(max_context.into_inner().unsigned_abs()) * u64::from(slots);
    match flags.ctx_size {
        None => Err(EvalContextError::ContextUnpinned {
            knob: "ctx_size",
            value: expected,
            max_context,
        }),
        Some(ctx_size) if u64::from(ctx_size) != expected => {
            Err(EvalContextError::CtxSizeDisagrees {
                ctx_size,
                max_context,
                slots,
                expected,
            })
        }
        Some(_) => Ok(()),
    }
}

/// Why [`EvalBenchmark::check_cell_context`] refuses a cell.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum EvalContextError {
    #[error(transparent)]
    Entry(#[from] MaxContextError),
    #[error(
        "{runtime} cannot stop a sample at the benchmark's parameter_max_context = \
         {max_context}; only vLLM (docker_vllm, uv_vllm) and llama-server \
         (llamacpp_cli_stock_tools) can"
    )]
    RuntimeCannotBound {
        runtime: RuntimeType,
        max_context: PositiveCount,
    },
    #[error(
        "{knob} is unset, and a client sizes it from parameter_max_tokens alone, so it would \
         run the benchmark's parameter_max_context = {max_context} unbounded; pin {knob} = {value}"
    )]
    ContextUnpinned {
        knob: &'static str,
        value: u64,
        max_context: PositiveCount,
    },
    #[error(
        "max_model_len = {max_model_len} is not the benchmark's parameter_max_context = \
         {max_context}, where vLLM stops a sample; set max_model_len = {max_context}"
    )]
    MaxModelLenDisagrees {
        max_model_len: PositiveCount,
        max_context: PositiveCount,
    },
    #[error(
        "ctx_size = {ctx_size} does not give each of the {slots} llama-server slot(s) the \
         benchmark's parameter_max_context = {max_context}; set ctx_size = {expected} \
         (parameter_max_context \u{d7} parallel)"
    )]
    CtxSizeDisagrees {
        ctx_size: u32,
        max_context: PositiveCount,
        slots: u32,
        expected: u64,
    },
    #[error(
        "parallel = {slots} leaves each slot the benchmark's parameter_max_context = \
         {max_context}, under its parameter_max_tokens = {max_tokens}, and the llama.cpp client \
         refuses such a slot when it runs more than one; drop parallel, or run a benchmark \
         whose parameter_max_context equals its parameter_max_tokens"
    )]
    SlotUnderMaxTokens {
        slots: u32,
        max_context: PositiveCount,
        max_tokens: PositiveCount,
    },
    #[error(
        "raw flag {flag:?} changes the context a llama-server slot holds, which must stay the \
         benchmark's parameter_max_context = {max_context}; drop it, and set the typed \
         `parallel` for the slot count"
    )]
    RawChangesSlotContext {
        flag: String,
        max_context: PositiveCount,
    },
    #[error(
        "env {name} changes the context a llama-server slot holds, which must stay the \
         benchmark's parameter_max_context = {max_context}; drop it, and set the typed \
         `parallel` for the slot count"
    )]
    EnvChangesSlotContext {
        name: String,
        max_context: PositiveCount,
    },
    #[error(
        "max_model_len = {max_model_len} at or under the benchmark's parameter_max_tokens = \
         {max_tokens} bounds prompt plus output, which the benchmark does not: vLLM refuses \
         (HTTP 400) a prompt plus max_tokens past it, and a client that sends no max_tokens runs \
         a bounded sample under an unbounded id. Pin max_model_len above parameter_max_tokens, \
         leave it unset, or run a benchmark that sets parameter_max_context"
    )]
    NoRoomForPrompt {
        max_model_len: PositiveCount,
        max_tokens: PositiveCount,
    },
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use serde_json::{Value, json};
    use strum::IntoEnumIterator as _;

    use super::*;
    use crate::BenchmarkDefinition;

    /// A MATH-500 entry at liquid_evals' output cap, with `extra` fields.
    fn entry(extra: &Value) -> anyhow::Result<EvalBenchmark> {
        let mut wire = json!({
            "benchmark_id": "eval_math_500_2026.10.1",
            "benchmark_type": "eval",
            "parameter_eval_id": "math_500",
            "parameter_dataset_name": "2026.10.1",
            "parameter_max_tokens": 32768,
        });
        if let (Some(wire), Some(extra)) = (wire.as_object_mut(), extra.as_object()) {
            wire.extend(extra.clone());
        }
        let definition: BenchmarkDefinition = serde_json::from_value(wire)?;
        Ok(definition.as_eval()?.clone())
    }

    fn bounded() -> Value {
        json!({"parameter_max_context": 32768})
    }

    fn unbounded() -> Value {
        json!({})
    }

    fn mcq() -> Value {
        json!({"parameter_mcq_choices": ["A", "B"]})
    }

    /// The eval cell's flags on `runtime_type`, with `knobs` beside the axes.
    fn eval_flags(runtime_type: &str, knobs: &Value) -> anyhow::Result<RuntimeFlags> {
        let model_type = match runtime_type {
            "llamacpp_cli_stock_tools" | "llamacpp_ios_pipette" | "llamacpp_apk_pipette" => {
                "gguf_text"
            }
            "mlx_ios_pipette" => "mlx",
            _ => "torch",
        };
        let mut wire = json!({
            "benchmark_type": "eval",
            "runtime_type": runtime_type,
            "model_type": model_type,
        });
        if let (Some(wire), Some(knobs)) = (wire.as_object_mut(), knobs.as_object()) {
            wire.extend(knobs.clone());
        }
        Ok(serde_json::from_value(wire)?)
    }

    /// The check's verdict as `Ok(())` or the refusal's variant name.
    fn check(
        entry_extra: &Value,
        runtime_type: &str,
        knobs: &Value,
    ) -> anyhow::Result<Result<(), String>> {
        let runtime: RuntimeType = serde_json::from_value(json!(runtime_type))?;
        let flags = eval_flags(runtime_type, knobs)?;
        Ok(entry(entry_extra)?
            .check_cell_context(runtime, Some(&flags))
            .map_err(|e| format!("{e:?}")))
    }

    fn assert_verdict(checked: &Result<(), String>, expected: Result<(), &str>) {
        match expected {
            Ok(()) => assert_eq!(checked, &Ok(())),
            Err(variant) => assert!(
                checked.as_ref().is_err_and(|e| e.starts_with(variant)),
                "expected {variant}, got {checked:?}"
            ),
        }
    }

    const LLAMA: &str = "llamacpp_cli_stock_tools";

    #[rstest]
    // vLLM stops a sample at `max_model_len`, so a bounded cell pins it at the bound.
    #[case::vllm_at_the_bound(bounded(), "docker_vllm", json!({"max_model_len": 32768}), Ok(()))]
    #[case::uv_vllm_at_the_bound(bounded(), "uv_vllm", json!({"max_model_len": 32768}), Ok(()))]
    #[case::vllm_unpinned(bounded(), "docker_vllm", json!({}), Err("ContextUnpinned"))]
    #[case::uv_vllm_unpinned(bounded(), "uv_vllm", json!({}), Err("ContextUnpinned"))]
    #[case::vllm_above(bounded(), "docker_vllm", json!({"max_model_len": 40960}), Err("MaxModelLenDisagrees"))]
    #[case::uv_vllm_below(bounded(), "uv_vllm", json!({"max_model_len": 16384}), Err("MaxModelLenDisagrees"))]
    // llama-server gives each slot `ctx_size / parallel`; one slot when unset.
    #[case::llama_one_slot(bounded(), LLAMA, json!({"ctx_size": 32768}), Ok(()))]
    #[case::llama_one_slot_typed(bounded(), LLAMA, json!({"ctx_size": 32768, "parallel": 1}), Ok(()))]
    #[case::llama_four_slots(bounded(), LLAMA, json!({"ctx_size": 131072, "parallel": 4}), Ok(()))]
    #[case::llama_unpinned(bounded(), LLAMA, json!({}), Err("ContextUnpinned"))]
    #[case::llama_parallel_alone(bounded(), LLAMA, json!({"parallel": 64}), Err("ContextUnpinned"))]
    #[case::llama_too_big(bounded(), LLAMA, json!({"ctx_size": 40960}), Err("CtxSizeDisagrees"))]
    #[case::llama_four_sized_for_one(bounded(), LLAMA, json!({"ctx_size": 32768, "parallel": 4}), Err("CtxSizeDisagrees"))]
    #[case::llama_one_sized_for_four(bounded(), LLAMA, json!({"ctx_size": 131072}), Err("CtxSizeDisagrees"))]
    #[case::llama_slot_under_the_cap(json!({"parameter_max_context": 16384}), LLAMA, json!({"ctx_size": 65536, "parallel": 4}), Err("SlotUnderMaxTokens"))]
    #[case::llama_one_slot_under_the_cap(json!({"parameter_max_context": 16384}), LLAMA, json!({"ctx_size": 16384}), Ok(()))]
    // A runtime with eval flags of its own is refused on the runtime alone.
    #[case::docker_sglang(bounded(), "docker_sglang", json!({}), Err("RuntimeCannotBound"))]
    #[case::uv_sglang(bounded(), "uv_sglang", json!({}), Err("RuntimeCannotBound"))]
    #[case::ios(bounded(), "llamacpp_ios_pipette", json!({}), Err("RuntimeCannotBound"))]
    #[case::apk(bounded(), "llamacpp_apk_pipette", json!({}), Err("RuntimeCannotBound"))]
    #[case::mlx_ios(bounded(), "mlx_ios_pipette", json!({}), Err("RuntimeCannotBound"))]
    // A malformed entry is refused before any runtime rule.
    #[case::past_the_cap(json!({"parameter_max_context": 32769}), "docker_vllm", json!({"max_model_len": 32769}), Err("Entry(AboveMaxTokens"))]
    #[case::bounded_mcq(json!({"parameter_max_context": 16, "parameter_mcq_choices": ["A"]}), LLAMA, json!({"ctx_size": 16}), Err("Entry(WithMcqChoices"))]
    // An unbounded free-text vLLM cell needs room for a prompt beside the cap.
    #[case::unbounded_vllm_unset(unbounded(), "docker_vllm", json!({}), Ok(()))]
    #[case::unbounded_vllm_room(unbounded(), "uv_vllm", json!({"max_model_len": 40960}), Ok(()))]
    #[case::unbounded_vllm_one_token(unbounded(), "docker_vllm", json!({"max_model_len": 32769}), Ok(()))]
    #[case::unbounded_vllm_the_cap(unbounded(), "docker_vllm", json!({"max_model_len": 32768}), Err("NoRoomForPrompt"))]
    #[case::unbounded_uv_vllm_under(unbounded(), "uv_vllm", json!({"max_model_len": 4096}), Err("NoRoomForPrompt"))]
    #[case::unbounded_mcq_vllm(mcq(), "docker_vllm", json!({"max_model_len": 4096}), Ok(()))]
    // The slot rules are the bound's: an unbounded entry runs any of them.
    #[case::unbounded_llama(unbounded(), LLAMA, json!({"ctx_size": 4096, "parallel": 8, "raw": ["--kv-unified"]}), Ok(()))]
    fn a_cell_runs_its_entry_as_defined(
        #[case] entry_extra: Value,
        #[case] runtime_type: &str,
        #[case] knobs: Value,
        #[case] expected: Result<(), &str>,
    ) -> anyhow::Result<()> {
        assert_verdict(&check(&entry_extra, runtime_type, &knobs)?, expected);
        Ok(())
    }

    /// No runtime refuses an unbounded entry without flags; a bounded one runs
    /// only where a pin could hold it.
    #[test]
    fn every_runtime_has_a_verdict_on_a_bounded_entry() -> anyhow::Result<()> {
        let bounded = entry(&bounded())?;
        let unbounded = entry(&unbounded())?;
        RuntimeType::iter().try_for_each(|runtime| {
            let checked = bounded.check_cell_context(runtime, None);
            let pinnable = matches!(
                runtime,
                RuntimeType::DockerVllm | RuntimeType::UvVllm | RuntimeType::LlamacppCliStockTools
            );
            anyhow::ensure!(
                if pinnable {
                    matches!(checked, Err(EvalContextError::ContextUnpinned { .. }))
                } else {
                    matches!(checked, Err(EvalContextError::RuntimeCannotBound { .. }))
                },
                "{runtime}: {checked:?}"
            );
            anyhow::ensure!(
                unbounded.check_cell_context(runtime, None).is_ok(),
                "{runtime} refused an entry without the bound"
            );
            Ok(())
        })
    }

    /// The refusals name the value that would hold the bound.
    #[rstest]
    #[case::ctx_size(json!({"ctx_size": 32768, "parallel": 4}), "set ctx_size = 131072")]
    #[case::unpinned(json!({"parallel": 4}), "pin ctx_size = 131072")]
    fn a_refusal_names_the_size_that_fits(
        #[case] knobs: Value,
        #[case] names: &str,
    ) -> anyhow::Result<()> {
        let flags = eval_flags(LLAMA, &knobs)?;
        let refusal = entry(&bounded())?
            .check_cell_context(RuntimeType::LlamacppCliStockTools, Some(&flags))
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(refusal.contains(names), "{refusal}");
        Ok(())
    }

    /// Refused in every spelling `deny_match` reads; the `--no-*` forms are
    /// llama-server's defaults.
    #[rstest]
    #[case::kv_unified("--kv-unified", false)]
    #[case::kv_unified_short("-kvu", false)]
    #[case::kv_unified_valued("--kv-unified=true", false)]
    #[case::per_slot("--kv-unified-per-slot", false)]
    #[case::per_slot_valued("--kv-unified-per-slot=16384", false)]
    #[case::context_shift("--context-shift", false)]
    #[case::parallel("--parallel", false)]
    #[case::parallel_valued("--parallel=8", false)]
    #[case::parallel_glued("-np8", false)]
    #[case::no_kv_unified("--no-kv-unified", true)]
    #[case::no_kv_unified_short("-no-kvu", true)]
    #[case::no_context_shift("--no-context-shift", true)]
    #[case::unrelated("--jinja", true)]
    fn a_bounded_llama_server_raw_keeps_the_slot(
        #[case] token: &str,
        #[case] honours: bool,
    ) -> anyhow::Result<()> {
        let checked = check(
            &bounded(),
            LLAMA,
            &json!({"ctx_size": 32768, "raw": [token]}),
        )?;
        assert_verdict(
            &checked,
            if honours {
                Ok(())
            } else {
                Err("RawChangesSlotContext")
            },
        );
        assert!(
            honours
                || checked
                    .as_ref()
                    .is_err_and(|e| e.contains(&format!("{token:?}")))
        );
        Ok(())
    }

    #[rstest]
    #[case::kv_unified("LLAMA_ARG_KV_UNIFIED=1", false)]
    #[case::per_slot("LLAMA_ARG_KV_UNIFIED_PER_SLOT=16384", false)]
    #[case::context_shift("LLAMA_ARG_CONTEXT_SHIFT=1", false)]
    #[case::parallel_inherited("LLAMA_ARG_N_PARALLEL", false)]
    #[case::ggml("GGML_CUDA_GRAPH_OPT=1", true)]
    fn a_bounded_llama_server_env_keeps_the_slot(
        #[case] env: &str,
        #[case] honours: bool,
    ) -> anyhow::Result<()> {
        let checked = check(
            &bounded(),
            LLAMA,
            &json!({"ctx_size": 32768, "envs": [env]}),
        )?;
        assert_verdict(
            &checked,
            if honours {
                Ok(())
            } else {
                Err("EnvChangesSlotContext")
            },
        );
        Ok(())
    }
}
