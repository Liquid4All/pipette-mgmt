//! A plan's cells, and the command the local runner dispatches for one.

use crate::{
    BenchmarkFlags, BenchmarkId, ClientId, ClientRunSpec, Model, ModelFlags, PositiveSeconds,
    Runtime, RuntimeFlags, TransportConfig,
};

/// One benchmark × model × runtime cell borrowed from one variant, runnable
/// on any of `allowed_clients`.
#[derive(Debug)]
pub struct TypedCell<'a> {
    pub variant_idx: usize,
    pub benchmark: &'a BenchmarkId,
    pub model: &'a Model,
    pub runtime: &'a Runtime,
    /// Transports this cell may dispatch to, as the variant's `clients` lists them.
    pub allowed_clients: &'a [ClientId],
}

/// Plan-internal executable unit: a [`ClientRunSpec`] plus which clients may run it.
///
/// Routing identity is `(benchmark, model, runtime, allowed_clients)`, not the
/// variant index, so equal tuples deduplicate. Cell keys hash the work axes alone
/// (the [`ClientRunSpec`]), so reordering `[[variants]]` keeps the state file valid.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RunnableCell {
    pub benchmark: BenchmarkId,
    pub model: Model,
    pub runtime: Runtime,
    pub allowed_clients: Vec<ClientId>,
    /// Resolved from the variant's `runtime_flags` (at most one entry matches);
    /// shipped as `--runtime-flags`, see [`Self::runtime_flags_json`].
    pub runtime_flags: Option<RuntimeFlags>,
    /// Resolved from the variant's `model_flags`; `None` off eval or when the
    /// variant authored none.
    pub model_flags: Option<ModelFlags>,
    /// Resolved from the variant's `benchmark_flags`; `None` when the variant
    /// authored none.
    pub benchmark_flags: Option<BenchmarkFlags>,
}

impl From<&RunnableCell> for ClientRunSpec {
    /// Work payload served to a client (drops plan-only `allowed_clients`).
    fn from(cell: &RunnableCell) -> Self {
        Self {
            benchmark: cell.benchmark.clone(),
            model: cell.model.clone(),
            runtime: cell.runtime.clone(),
            runtime_flags: cell.runtime_flags.clone(),
            model_flags: cell.model_flags.clone(),
            benchmark_flags: cell.benchmark_flags.clone(),
        }
    }
}

impl From<RunnableCell> for ClientRunSpec {
    fn from(cell: RunnableCell) -> Self {
        ClientRunSpec::from(&cell)
    }
}

impl RunnableCell {
    /// Identity, not wire: `pipette-ctl` hashes this into the cell key that resume and
    /// rerun match on, so it keeps the axes [`Self::runtime_flags_json`] drops. Mirrors
    /// [`ModelFlags::canonical_string`]. A change re-keys flagged cells: see the pinned
    /// digest in `pipette-ctl`.
    pub fn runtime_flags_canonical_string(&self) -> Option<String> {
        let flags = self.runtime_flags.as_ref()?;
        // Serializes through `RuntimeFlagRef`: the flat entry with its axes, as authored.
        Some(serde_json::to_string(flags).unwrap_or_default())
    }

    pub fn runtime_flags_json(&self) -> Option<String> {
        let flags = self.runtime_flags.as_ref()?;
        // The knobs alone: the client resolves its cell from `--benchmark`, `--runtime`
        // and `--model`, so axes on the wire could only agree or contradict.
        // Fall closed: an empty payload makes the client reject `--runtime-flags`
        // loudly, where `.ok()` would drop the flags silently.
        Some(serde_json::to_string(&flags.knobs_json()).unwrap_or_default())
    }

    /// `--sync` is added by the runner — not included here.
    pub fn build_argv(
        &self,
        transport: &TransportConfig,
    ) -> Result<Vec<String>, serde_json::Error> {
        // iOS takes the app's `headlessrun` args and uploads in-process
        // (`submit=1`), so the runner must not append `--sync`.
        if matches!(
            transport,
            TransportConfig::Ios { .. } | TransportConfig::IosOverSsh { .. }
        ) {
            return self.ios_headless_args();
        }
        // A serialization failure is surfaced, not defaulted to an empty arg
        // that would produce a silently broken command line.
        let model = serde_json::to_string(&self.model)?;
        let runtime = serde_json::to_string(&self.runtime)?;
        let mut argv = vec![
            transport.binary_path().into(),
            "--work-dir".into(),
            transport.work_dir().into(),
            "benchmarks".into(),
            "run".into(),
            "--benchmark".into(),
            self.benchmark.as_ref().into(),
            "--model".into(),
            model,
            "--runtime".into(),
            runtime,
        ];
        // The entry itself, not a one-element array: every client accepts the bare object.
        if let Some(flags) = &self.model_flags {
            let knobs = serde_json::to_string(flags).unwrap_or_default();
            argv.push("--model-flags".into());
            argv.push(knobs);
        }
        // Carried by any server-driven cell (eval and vl-throughput), not only eval.
        if let Some(bf) = &self.benchmark_flags {
            extend_with_eval_knobs(&mut argv, bf.http_timeout());
            // The per-cell readiness deadline, distinct from
            // PIPETTE_READINESS_MAX_WAIT_SECS, the plan driver's fleet-wide waiver.
            if let Some(secs) = bf.readiness().and_then(|r| r.max_wait_secs) {
                argv.push("--readiness-max-wait-secs".into());
                argv.push(secs.to_string());
            }
            // Only when true, so an unset knob leaves the runner's argv unchanged.
            if bf.readiness().and_then(|r| r.skip_thermal) == Some(true) {
                argv.push("--readiness-skip-thermal".into());
            }
            if let Some(celsius) = bf.readiness().and_then(|r| r.skin_below_c) {
                argv.push("--readiness-skin-below-c".into());
                argv.push(celsius.to_string());
            }
            // Forward the whole-workload deadline; unforwarded, the peak-memory
            // cells ran on the client default and `vl_spec_decode` ran unbounded.
            if let Some(secs) = bf.bench_timeout() {
                argv.push("--bench-timeout-seconds".into());
                argv.push(secs.to_string());
            }
        }
        if let Some(knobs) = self.runtime_flags_json() {
            argv.push("--runtime-flags".into());
            argv.push(knobs);
        }
        Ok(argv)
    }

    /// The tokens after the app bundle in `xcrun devicectl … <bundle> <args>`.
    /// `submit=1` is the on-device `--sync`; Apple Foundation needs no `model=`.
    fn ios_headless_args(&self) -> Result<Vec<String>, serde_json::Error> {
        let mut args = vec![
            "headlessrun".to_string(),
            // Canonical JSON, not a type tag: the device checks its own build against it.
            format!("runtime={}", serde_json::to_string(&self.runtime)?),
        ];
        if !matches!(self.runtime, Runtime::AppleFoundation(_)) {
            // Canonical JSON, as `--model`: not `Display`, the log/warehouse identifier
            // no client parses.
            args.push(format!("model={}", serde_json::to_string(&self.model)?));
        }
        args.push(format!("benchmarks={}", self.benchmark.as_ref()));
        if let Some(knobs) = self.runtime_flags_json() {
            args.push(format!("runtime-flags={knobs}"));
        }
        args.push("submit=1".to_string());
        Ok(args)
    }
}

fn extend_with_eval_knobs(argv: &mut Vec<String>, http_timeout_seconds: Option<PositiveSeconds>) {
    if let Some(t) = http_timeout_seconds {
        argv.push("--http-timeout-seconds".into());
        argv.push(t.to_string());
    }
}

#[cfg(test)]
mod ios_transport_tests {
    use super::*;
    use crate::runtime::cli_release;
    use crate::{
        HfRepo, LlamaCppFlavor, LlamacppCliStockTools, LlamacppCliStockToolsSource, ModelSource,
        Torch,
    };

    fn ios_cfg() -> anyhow::Result<TransportConfig> {
        Ok(toml::from_str(
            "type = \"ios\"\nclient_id = \"iphone15\"\ndevice_udid = \"UDID-123\"\n",
        )?)
    }

    fn cell(benchmark: &str, model: Model, runtime: Runtime) -> anyhow::Result<RunnableCell> {
        Ok(RunnableCell {
            benchmark: BenchmarkId::try_new(benchmark.to_string())?,
            model,
            runtime,
            allowed_clients: Vec::new(),
            runtime_flags: None,
            model_flags: None,
            benchmark_flags: None,
        })
    }

    #[test]
    fn ios_build_argv_afm_omits_model() -> anyhow::Result<()> {
        let cell = cell(
            "decode_throughput_256",
            Model::AppleFoundationText,
            Runtime::AppleFoundation(Default::default()),
        )?;
        // AFM: no `model=` (the OS supplies it), `submit=1`, no `--sync`.
        assert_eq!(
            cell.build_argv(&ios_cfg()?)?,
            vec![
                "headlessrun",
                r#"runtime={"type":"apple_foundation"}"#,
                "benchmarks=decode_throughput_256",
                "submit=1",
            ]
        );
        Ok(())
    }

    /// The only test of an on-device runtime's spelling: the device compares it
    /// byte for byte against its own `Runtime.thisBuild`.
    #[test]
    fn ios_build_argv_carries_the_on_device_runtime_verbatim() -> anyhow::Result<()> {
        let runtime: Runtime = serde_json::from_str(
            r#"{"type":"llamacpp_ios_pipette","repository_url":"github.com/ggml-org/llama.cpp","repository_version":"b10216","flavor":"ios-arm64"}"#,
        )?;
        // Parsed rather than constructed, so the fixture is the plan spelling itself.
        let model: Model = serde_json::from_str(
            r#"{"type":"gguf_text","source":"huggingface","org":"LiquidAI","repo_name":"LFM2.5-350M-GGUF","path":"LFM2.5-350M-Q4_0.gguf"}"#,
        )?;
        let argv = cell("prefill_throughput_256", model, runtime)?.build_argv(&ios_cfg()?)?;

        assert_eq!(
            argv[1],
            r#"runtime={"type":"llamacpp_ios_pipette","repository_url":"github.com/ggml-org/llama.cpp","repository_version":"b10216","flavor":"ios-arm64"}"#
        );
        Ok(())
    }

    #[test]
    fn ios_build_argv_non_afm_includes_model() -> anyhow::Result<()> {
        let runtime = Runtime::LlamacppCliStockTools(LlamacppCliStockTools {
            source: LlamacppCliStockToolsSource::GithubRelease(cli_release("b5000")?),
            flavor: LlamaCppFlavor::MacosArm64,
        });
        let model = Model::Torch(Torch {
            source: ModelSource::HuggingFace {
                repo: HfRepo::parse_org_repo("org/repo")?,
                prefix: None,
            },
        });
        assert_eq!(
            cell("eval_gpqa", model, runtime)?.build_argv(&ios_cfg()?)?,
            vec![
                "headlessrun",
                r#"runtime={"type":"llamacpp_cli_stock_tools","source":"github_release","repository_url":"github.com/ggml-org/llama.cpp","repository_version":"b5000","flavor":"macos-arm64"}"#,
                // Canonical JSON, not `Display`: the client parses this spelling, and
                // `Display` (`{repo}[:{path}]`) is the log/warehouse identifier.
                r#"model={"type":"torch","source":"huggingface","org":"org","repo_name":"repo"}"#,
                "benchmarks=eval_gpqa",
                "submit=1",
            ]
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use rstest::rstest;

    use crate::plan::{minimal_outer_plan, single};
    use crate::*;

    fn local_transport() -> anyhow::Result<TransportConfig> {
        Ok(TransportConfig::Local {
            client_id: ClientId::try_new("t1")?,
            binary_path: "/bin/pipette".into(),
            work_dir: "/tmp".into(),
            shell: ShellType::Posix,
            parallelism: NonZeroUsize::new(1).ok_or_else(|| anyhow::anyhow!("1 is non-zero"))?,
        })
    }

    #[test]
    fn runnable_cell_gguf_text_model_ref() -> anyhow::Result<()> {
        let plan = Plan::parse(minimal_outer_plan())?;
        let cells = plan.runnable_cells()?;
        let cell = single(&cells)?;
        assert_eq!(cell.model.to_string(), "org/repo:Q4_K_M.gguf");
        // Display carries the full source coordinate, not just the ref.
        assert_eq!(
            cell.runtime.to_string(),
            "github.com/ggml-org/llama.cpp@b9050:macos-arm64"
        );
        Ok(())
    }

    #[test]
    fn runnable_cell_vision_model_identity_includes_mmproj() -> anyhow::Result<()> {
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
models   = [{ type = "gguf_vision", source = "huggingface", org = "lab", repo_name = "VL-3B", model = "q4.gguf", mmproj = "mm-f16.gguf" }]
runtimes = [{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b9050", flavor = "android-arm64-v8a" }]
"#;
        let plan = Plan::parse(toml_str)?;
        let cells = plan.runnable_cells()?;
        let cell = single(&cells)?;
        assert_eq!(
            cell.model.to_string(),
            "lab/VL-3B:q4.gguf+lab/VL-3B:mm-f16.gguf"
        );
        assert!(matches!(cell.model, Model::GgufVision(_)));
        // The renamed flavor wire form (not the kebab-cased variant name),
        // prefixed by the full source coordinate.
        assert_eq!(
            cell.runtime.to_string(),
            "github.com/ggml-org/llama.cpp@b9050:android-arm64-v8a"
        );
        Ok(())
    }

    #[test]
    fn runnable_cell_mlx_model_ref_is_bare_repo() -> anyhow::Result<()> {
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
models   = [{ type = "mlx", source = "huggingface", org = "lab", repo_name = "LFM2.5-350M-MLX-4bit" }]
runtimes = [{ type = "mlx_macos_pipette", version = "0.31.2",
              source = { type = "pip_requirements_text", contents = "mlx-lm==0.31.2" },
              flavor = "macos-arm64" }]
"#;
        let plan = Plan::parse(toml_str)?;
        let cells = plan.runnable_cells()?;
        let cell = single(&cells)?;
        assert_eq!(cell.model.to_string(), "lab/LFM2.5-350M-MLX-4bit");
        // Bare version — no flavor suffix.
        assert_eq!(cell.runtime.to_string(), "0.31.2");
        assert!(matches!(cell.runtime, Runtime::MlxMacosPipette(_)));
        Ok(())
    }

    /// The iOS transport carries the plan's knobs, in the app's `key=value` grammar.
    #[test]
    fn the_ios_transport_ships_the_resolved_knobs() -> anyhow::Result<()> {
        let toml_str = r#"
plan_id          = "x"
benchmarks       = ["prefill_throughput_256"]

[[transports]]
client_id = "phone"
type = "ios"
device_udid = "00000000-000000000000000A"

[[variants]]
clients  = ["phone"]
models   = [{ type = "gguf_text", source = "huggingface", org = "o", repo_name = "r", path = "a.gguf" }]
runtimes = [{ type = "llamacpp_ios_pipette", repository_url = "github.com/ggml-org/llama.cpp", version = "b9050", flavor = "ios-arm64" }]
runtime_flags = [{ benchmark_type = "prefill_throughput", runtime_type = "llamacpp_ios_pipette", model_type = "gguf_text", threads = 4 }]
"#;
        let plan = Plan::parse(toml_str)?;
        let cells = plan.runnable_cells()?;
        let cell = single(&cells)?;
        let argv = cell.build_argv(&plan.transports[0])?;

        let knobs = argv
            .iter()
            .find_map(|a| a.strip_prefix("runtime-flags="))
            .ok_or_else(|| anyhow::anyhow!("expected runtime-flags in {argv:?}"))?;
        assert!(knobs.contains(r#""threads":4"#), "got: {knobs}");
        // The app derives the cell from `runtime=`/`model=`/`benchmarks=`, all of which
        // this argv already carries, so the axes stay off the wire here too.
        assert!(!knobs.contains("benchmark_type"), "got: {knobs}");
        Ok(())
    }

    /// The cell key hashes the canonical string, so it keeps the axes the wire
    /// form drops; the client payload carries the flags either way.
    #[test]
    fn runtime_flags_key_keeps_the_axes_the_wire_drops() -> anyhow::Result<()> {
        let toml_str = r#"
plan_id          = "x"
benchmarks       = ["prefill_throughput_256"]

[[transports]]
client_id = "phone"
type = "ios"
device_udid = "00000000-000000000000000A"

[[variants]]
clients  = ["phone"]
models   = [{ type = "gguf_text", source = "huggingface", org = "o", repo_name = "r", path = "a.gguf" }]
runtimes = [{ type = "llamacpp_ios_pipette", repository_url = "github.com/ggml-org/llama.cpp", version = "b9050", flavor = "ios-arm64" }]
runtime_flags = [{ benchmark_type = "prefill_throughput", runtime_type = "llamacpp_ios_pipette", model_type = "gguf_text", threads = 4 }]
"#;
        let plan = Plan::parse(toml_str)?;
        let cells = plan.runnable_cells()?;
        let cell = single(&cells)?;
        assert_eq!(
            cell.runtime_flags_canonical_string().as_deref(),
            Some(
                r#"{"runtime_type":"llamacpp_ios_pipette","model_type":"gguf_text","benchmark_type":"prefill_throughput","threads":4}"#
            )
        );
        assert_eq!(
            cell.runtime_flags_json().as_deref(),
            Some(r#"{"threads":4}"#)
        );

        let spec = ClientRunSpec::from(cell);
        assert_eq!(spec.benchmark.as_str(), "prefill_throughput_256");
        assert_eq!(spec.runtime_flags, cell.runtime_flags);
        assert_eq!(spec.model, cell.model);
        assert_eq!(spec.runtime, cell.runtime);
        Ok(())
    }

    #[test]
    fn runnable_cell_ships_typed_flags_as_json() -> anyhow::Result<()> {
        let toml_str = r#"
plan_id          = "x"
benchmarks       = ["prefill_throughput_256"]

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
runtime_flags = [{ benchmark_type = "prefill_throughput", runtime_type = "llamacpp_cli_stock_tools", model_type = "gguf_text", threads = 4, number_gpu_layers = 99 }]
"#;
        let plan = Plan::parse(toml_str)?;
        let cells = plan.runnable_cells()?;
        let cell = single(&cells)?;
        // Rendering to `-t 4 -ngl 99` is the client's job.
        let json = cell
            .runtime_flags_json()
            .ok_or_else(|| anyhow::anyhow!("expected runtime flags json"))?;
        assert!(json.contains("\"threads\":4"), "got: {json}");
        assert!(json.contains("\"number_gpu_layers\":99"), "got: {json}");
        // The axes the entry was selected by are not on the wire: the client resolves the
        // cell from `--benchmark`/`--runtime`/`--model` before it reads any flags.
        ["benchmark_type", "runtime_type", "model_type"]
            .iter()
            .for_each(|axis| {
                assert!(
                    !json.contains(axis),
                    "{axis} should be stripped, got: {json}"
                )
            });
        // It round-trips against the cell the client resolved, which is what supplies them.
        let back = RuntimeFlags::from_cell_json(
            &json,
            RuntimeType::LlamacppCliStockTools,
            ModelType::GgufText,
            BenchmarkType::PrefillThroughput,
        )?;
        assert_eq!(back, cell.runtime_flags);
        Ok(())
    }

    #[test]
    fn benchmark_flags_resolve_and_emit_eval_run_knobs() -> anyhow::Result<()> {
        let toml_str = r#"
plan_id    = "x"
benchmarks = ["eval_smoke"]

[[transports]]
client_id = "t1"
type = "local"
binary_path = "/bin/pipette"
work_dir = "/tmp/wd"
shell = "posix"

[[variants]]
clients         = ["t1"]
models          = [{ type = "gguf_text", source = "huggingface", org = "o", repo_name = "r", path = "a.gguf" }]
runtimes        = [{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b9050", flavor = "macos-arm64" }]
benchmark_flags = [{ benchmark_type = "eval", runtime_type = "llamacpp_cli_stock_tools", model_type = "gguf_text", http_timeout_seconds = 600, doomloop = { exact_repeat = { required = 5, window = 8192 } } }]
"#;
        let plan = Plan::parse(toml_str)?;
        let cells = plan.runnable_cells()?;
        let cell = single(&cells)?;
        let bf = cell
            .benchmark_flags
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("benchmark_flags resolved onto the eval cell"))?;
        assert_eq!(bf.http_timeout().map(u64::from), Some(600));

        let argv = cell.build_argv(&local_transport()?)?;
        assert!(
            argv.windows(2)
                .any(|w| w == ["--http-timeout-seconds", "600"])
        );
        // `doomloop` is still authored above; it parses and no flag reaches the runner.
        assert!(!argv.iter().any(|a| a.starts_with("--doomloop-")));
        Ok(())
    }

    #[test]
    fn runnable_cell_build_argv_full() -> anyhow::Result<()> {
        let toml_str = r#"
plan_id          = "x"
benchmarks       = ["prefill_throughput_512"]

[[transports]]
client_id = "t1"
type = "local"
binary_path = "/bin/pipette"
work_dir = "/tmp/wd"
shell = "posix"

[[variants]]
clients  = ["t1"]
models   = [{ type = "gguf_text", source = "huggingface", org = "o", repo_name = "r", path = "a.gguf" }]
runtimes = [{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b9050", flavor = "macos-arm64" }]
runtime_flags = [{ benchmark_type = "prefill_throughput", runtime_type = "llamacpp_cli_stock_tools", model_type = "gguf_text", threads = 4, number_gpu_layers = 99 }]
"#;
        let plan = Plan::parse(toml_str)?;
        let cells = plan.runnable_cells()?;
        let argv = single(&cells)?.build_argv(&local_transport()?)?;

        assert_eq!(argv[0], "/bin/pipette");
        assert!(argv.windows(2).any(|w| w == ["--work-dir", "/tmp"]));
        assert!(argv.iter().any(|a| a == "benchmarks"));
        assert!(argv.iter().any(|a| a == "run"));

        assert!(
            argv.windows(2)
                .any(|w| w == ["--benchmark", "prefill_throughput_512"])
        );
        // `--model` is canonical JSON, round-tripping to the authored model.
        let model_idx = argv
            .iter()
            .position(|a| a == "--model")
            .ok_or_else(|| anyhow::anyhow!("--model present"))?;
        let model: Model = serde_json::from_str(&argv[model_idx + 1])?;
        assert_eq!(model.to_string(), "o/r:a.gguf");
        let runtime_idx = argv
            .iter()
            .position(|a| a == "--runtime")
            .ok_or_else(|| anyhow::anyhow!("--runtime present"))?;
        let runtime: Runtime = serde_json::from_str(&argv[runtime_idx + 1])?;
        assert_eq!(runtime.cli_ref(), "b9050:macos-arm64");
        let rf_idx = argv
            .iter()
            .position(|a| a == "--runtime-flags")
            .ok_or_else(|| anyhow::anyhow!("--runtime-flags present"))?;
        let payload = &argv[rf_idx + 1];
        assert!(payload.contains("\"threads\":4"), "got: {payload}");
        assert!(
            payload.contains("\"number_gpu_layers\":99"),
            "got: {payload}"
        );

        assert!(!argv.iter().any(|a| a == "--mmproj"));
        Ok(())
    }

    /// A `gguf_vision` cell inlines its projector in the `--model` JSON and emits
    /// no separate `--mmproj`; the projector round-trips through `parse_model_arg`.
    #[test]
    fn build_argv_gguf_vision_inlines_projector_without_mmproj() -> anyhow::Result<()> {
        let toml_str = r#"
plan_id    = "x"
benchmarks = ["eval_smoke"]

[[transports]]
client_id = "t1"
type = "local"
binary_path = "/bin/pipette"
work_dir = "/tmp/wd"
shell = "posix"

[[variants]]
clients  = ["t1"]
models   = [{ type = "gguf_vision", source = "huggingface", org = "o", repo_name = "r", model = "a.gguf", mmproj = "m.gguf" }]
runtimes = [{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b9050", flavor = "macos-arm64" }]
"#;
        let plan = Plan::parse(toml_str)?;
        let cells = plan.runnable_cells()?;
        let argv = single(&cells)?.build_argv(&local_transport()?)?;

        assert!(!argv.iter().any(|a| a == "--mmproj"));
        let model_idx = argv
            .iter()
            .position(|a| a == "--model")
            .ok_or_else(|| anyhow::anyhow!("--model present"))?;
        let model: Model = serde_json::from_str(&argv[model_idx + 1])?;
        assert!(matches!(model, Model::GgufVision(_)));
        // Projector is part of the typed model / Display — no separate flag.
        assert!(model.to_string().contains('+'));
        Ok(())
    }

    #[test]
    fn runnable_cell_build_argv_omits_optional_flags_when_unset() -> anyhow::Result<()> {
        let plan = Plan::parse(minimal_outer_plan())?;
        let cells = plan.runnable_cells()?;
        let argv = single(&cells)?.build_argv(&local_transport()?)?;
        assert!(!argv.iter().any(|a| a == "--model-flags"));
        assert!(!argv.iter().any(|a| a == "--http-timeout-seconds"));
        assert!(!argv.iter().any(|a| a == "--runtime-flags"));
        assert!(!argv.iter().any(|a| a == "--mmproj"));
        Ok(())
    }

    #[test]
    fn build_argv_emits_model_flags_for_eval_cell() -> anyhow::Result<()> {
        let toml_str = r#"
plan_id    = "x"
benchmarks = ["eval_smoke"]

[[transports]]
client_id = "t1"
type = "local"
binary_path = "/bin/pipette"
work_dir = "/tmp/wd"
shell = "posix"

[[variants]]
clients     = ["t1"]
models      = [{ type = "gguf_text", source = "huggingface", org = "o", repo_name = "r", path = "a.gguf" }]
runtimes    = [{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b9050", flavor = "macos-arm64" }]
model_flags = [{ benchmark_type = "eval", model_type = "gguf_text", enable_thinking = true }]
"#;
        let plan = Plan::parse(toml_str)?;
        let cells = plan.runnable_cells()?;
        let argv = single(&cells)?.build_argv(&local_transport()?)?;
        let flags_json = argv
            .windows(2)
            .find(|w| w[0] == "--model-flags")
            .map(|w| w[1].as_str())
            .ok_or_else(|| anyhow::anyhow!("expected --model-flags"))?;
        // The entry itself, not wrapped in an array.
        let parsed: ModelFlags = serde_json::from_str(flags_json)?;
        assert_eq!(parsed.enable_thinking(), Some(true));
        Ok(())
    }

    #[test]
    fn build_argv_emits_readiness_max_wait_for_timing_cell() -> anyhow::Result<()> {
        let toml_str = r#"
plan_id    = "x"
benchmarks = ["prefill_throughput_512"]

[[transports]]
client_id = "t1"
type = "local"
binary_path = "/bin/pipette"
work_dir = "/tmp/wd"
shell = "posix"

[[variants]]
clients         = ["t1"]
models          = [{ type = "gguf_text", source = "huggingface", org = "o", repo_name = "r", path = "a.gguf" }]
runtimes        = [{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b9050", flavor = "macos-arm64" }]
benchmark_flags = [{ benchmark_type = "prefill_throughput", runtime_type = "llamacpp_cli_stock_tools", model_type = "gguf_text", readiness = { max_wait_secs = 1800 } }]
"#;
        let plan = Plan::parse(toml_str)?;
        let cells = plan.runnable_cells()?;
        let argv = single(&cells)?.build_argv(&local_transport()?)?;
        assert!(
            argv.windows(2)
                .any(|w| w == ["--readiness-max-wait-secs", "1800"])
        );
        Ok(())
    }

    /// Unforwarded, a peak-memory cell silently ran on the client's default.
    #[test]
    fn build_argv_forwards_an_authored_bench_deadline() -> anyhow::Result<()> {
        let toml_str = r#"
plan_id    = "x"
benchmarks = ["max_memory_usage_512"]

[[transports]]
client_id = "t1"
type = "local"
binary_path = "/bin/pipette"
work_dir = "/tmp/wd"
shell = "posix"

[[variants]]
clients         = ["t1"]
models          = [{ type = "gguf_text", source = "huggingface", org = "o", repo_name = "r", path = "a.gguf" }]
runtimes        = [{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b9050", flavor = "macos-arm64" }]
benchmark_flags = [{ benchmark_type = "max_memory_usage", runtime_type = "llamacpp_cli_stock_tools", model_type = "gguf_text", bench_timeout_seconds = 1200 }]
"#;
        let plan = Plan::parse(toml_str)?;
        let cells = plan.runnable_cells()?;
        let argv = single(&cells)?.build_argv(&local_transport()?)?;
        assert!(
            argv.windows(2)
                .any(|w| w == ["--bench-timeout-seconds", "1200"]),
            "{argv:?}"
        );
        Ok(())
    }

    /// A prefill plan with a `readiness` placeholder, not `format!`, so the
    /// inline-table braces stay readable.
    const PREFILL_PLAN_WITH_READINESS: &str = r#"
plan_id    = "x"
benchmarks = ["prefill_throughput_512"]

[[transports]]
client_id = "t1"
type = "local"
binary_path = "/bin/pipette"
work_dir = "/tmp/wd"
shell = "posix"

[[variants]]
clients         = ["t1"]
models          = [{ type = "gguf_text", source = "huggingface", org = "o", repo_name = "r", path = "a.gguf" }]
runtimes        = [{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b9050", flavor = "macos-arm64" }]
benchmark_flags = [{ benchmark_type = "prefill_throughput", runtime_type = "llamacpp_cli_stock_tools", model_type = "gguf_text", readiness = { __READINESS__ } }]
"#;

    fn argv_for_prefill_with_readiness(readiness: &str) -> anyhow::Result<Vec<String>> {
        let toml_str = PREFILL_PLAN_WITH_READINESS.replace("__READINESS__", readiness);
        let plan = Plan::parse(&toml_str)?;
        let cells = plan.runnable_cells()?;
        Ok(single(&cells)?.build_argv(&local_transport()?)?)
    }

    /// Emitting the flag on an unset or `false` knob would waive the thermal
    /// criterion on every cell in the plan.
    #[rstest]
    #[case("skip_thermal = true", true)]
    #[case("skip_thermal = false", false)]
    #[case("max_wait_secs = 900", false)]
    fn build_argv_emits_readiness_skip_thermal_only_when_true(
        #[case] readiness: &str,
        #[case] want_flag: bool,
    ) -> anyhow::Result<()> {
        let argv = argv_for_prefill_with_readiness(readiness)?;
        assert_eq!(
            argv.iter().any(|a| a == "--readiness-skip-thermal"),
            want_flag,
            "readiness = {{ {readiness} }} produced argv {argv:?}",
        );
        Ok(())
    }

    /// The runner declares it `ArgAction::SetTrue`: a value after it would bind
    /// as the next argument.
    #[test]
    fn readiness_skip_thermal_is_emitted_as_a_bare_flag() -> anyhow::Result<()> {
        let argv = argv_for_prefill_with_readiness("skip_thermal = true")?;
        let at = argv
            .iter()
            .position(|a| a == "--readiness-skip-thermal")
            .ok_or_else(|| anyhow::anyhow!("expected --readiness-skip-thermal in {argv:?}"))?;
        if let Some(next) = argv.get(at + 1) {
            anyhow::ensure!(
                next.starts_with("--"),
                "expected no value after --readiness-skip-thermal, found {next:?}",
            );
        }
        Ok(())
    }

    #[test]
    fn build_argv_emits_both_readiness_knobs_together() -> anyhow::Result<()> {
        let argv = argv_for_prefill_with_readiness("max_wait_secs = 1800, skip_thermal = true")?;
        assert!(
            argv.windows(2)
                .any(|w| w == ["--readiness-max-wait-secs", "1800"])
        );
        assert!(argv.iter().any(|a| a == "--readiness-skip-thermal"));
        Ok(())
    }

    #[rstest]
    #[case("skin_below_c = 34", Some("34"))]
    #[case("max_wait_secs = 900", None)]
    fn build_argv_forwards_skin_threshold(
        #[case] readiness: &str,
        #[case] value: Option<&str>,
    ) -> anyhow::Result<()> {
        let argv = argv_for_prefill_with_readiness(readiness)?;
        assert_eq!(
            argv.windows(2)
                .find(|pair| pair[0] == "--readiness-skin-below-c")
                .map(|pair| pair[1].as_str()),
            value
        );
        Ok(())
    }
}
