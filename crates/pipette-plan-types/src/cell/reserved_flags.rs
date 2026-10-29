use crate::{LlamacppCliStockTools, RuntimeFlags, RuntimeFlagsError};

// A `raw` entry may not carry a flag we type (aliases) or one the
// benchmark/tool owns (reserved). Each cell's denylist concatenates the two.

/// Concatenate two `&'static [&'static str]` consts into one, at compile time.
macro_rules! concat_str_slices {
    ($a:expr, $b:expr) => {{
        const A: &[&str] = $a;
        const B: &[&str] = $b;
        const LEN: usize = A.len() + B.len();
        #[expect(
            clippy::indexing_slicing,
            reason = "const-evaluated: an out-of-bounds index is a compile error, not a panic"
        )]
        const OUT: [&str; LEN] = {
            let mut out = [""; LEN];
            let mut i = 0;
            while i < A.len() {
                out[i] = A[i];
                i += 1;
            }
            let mut j = 0;
            while j < B.len() {
                out[A.len() + j] = B[j];
                j += 1;
            }
            out
        };
        &OUT
    }};
}

/// Long/short spellings of the llama.cpp knobs we type. A `raw` entry using any
/// of these must go through the typed field instead.
const LLAMA_TYPED_ALIASES: &[&str] = &[
    "-t",
    "--threads",
    "-ngl",
    "--n-gpu-layers",
    "--gpu-layers",
    "--mmap",
    "--no-mmap",
    "-mmp",
    // The load-mode spellings and the deprecated booleans it absorbed
    // (`--mlock`, `--direct-io`): once a setting is typed, `raw` must not be a
    // second route to it.
    "-lm",
    "--load-mode",
    "--mlock",
    "-dio",
    "-ndio",
    "--direct-io",
    "--no-direct-io",
    "-fa",
    "--flash-attn",
];
/// vLLM typed-knob aliases. Both prefix-cache spellings: the cell types the
/// setting, not the flag.
const VLLM_TYPED_ALIASES: &[&str] = &[
    "--tensor-parallel-size",
    "-tp",
    "--dtype",
    "--max-model-len",
    "--enable-prefix-caching",
    "--no-enable-prefix-caching",
];
/// sglang typed-knob aliases (see [`LLAMA_TYPED_ALIASES`]). The radix cache is
/// sglang's prefix cache — on by default, `--disable-radix-cache` the lever.
const SGLANG_TYPED_ALIASES: &[&str] = &["--tensor-parallel-size", "-tp", "--disable-radix-cache"];
/// TensorRT Edge-LLM typed-knob aliases: the server's spelling of `precision`.
const TRT_EDGELLM_TYPED_ALIASES: &[&str] = &["--precision"];

/// Flags the llama.cpp stock CLI tools fix per benchmark. The apk/iOS llama
/// runtimes run in-process and use none of them.
impl LlamacppCliStockTools {
    /// Flags every llama-bench benchmark fixes, for cross-run comparability.
    /// `--no-warmup` would move the first kernel builds into the timed rep.
    /// The depth is here, not on decode alone: a `-d` on prefill measures
    /// against a deep KV cache and is recorded as an ordinary prefill.
    const BENCH: &[&str] = &[
        "--output",
        "-o",
        "--model",
        "-m",
        "--n-prompt",
        "-p",
        "--n-gen",
        "-n",
        "--n-depth",
        "-d",
        "--repetitions",
        "-r",
        "--no-warmup",
    ];
    /// Flags `llama-batched-bench` fixes on a batch. `-pps` and `-tgs` change
    /// what the times cover, and the row does not report `-tgs`.
    const BATCHED_BENCH: &[&str] = &[
        "--output-format",
        "--ctx-size",
        "-c",
        "-npp",
        "-ntg",
        "-npl",
        "-pps",
        "-tgs",
    ];
    /// The bench and batched-bench flags. Prefill leaves the depth at
    /// llama-bench's zero default, the measurement it names.
    pub const RESERVED_PREFILL: &[&str] = concat_str_slices!(
        LlamacppCliStockTools::BENCH,
        LlamacppCliStockTools::BATCHED_BENCH
    );
    /// The bench flags, and the batched bench's for a batch. Decode passes
    /// `--n-depth` itself, from the cell's prefill parameter.
    pub const RESERVED_DECODE: &[&str] = concat_str_slices!(
        LlamacppCliStockTools::BENCH,
        LlamacppCliStockTools::BATCHED_BENCH
    );
    /// The bench flags plus `--ctx-size`, which llama-bench lacks: reserving it
    /// turns an unknown-argument exit inside the tool into a plan-time refusal.
    // Not `Self::BENCH`: the consts the macro declares cannot name `Self`.
    pub const RESERVED_MAX_MEMORY: &[&str] =
        concat_str_slices!(LlamacppCliStockTools::BENCH, &["--ctx-size", "-c"]);
    /// Flags llama-server fixes for the latency/eval/VL benchmarks.
    pub const RESERVED_SERVER: &[&str] = &[
        "--model",
        "-m",
        "--mmproj",
        "--host",
        "--port",
        "--no-warmup",
    ];
    /// Speculation flags `vl_spec_decode` derives when speculation is on:
    /// the draft length from `parameter_draft_length`, a separate drafter
    /// from the model spec. Both spellings, upstream and DSpark fork.
    pub const RESERVED_SPECULATIVE_DERIVED: &[&str] = &[
        "--spec-draft-n-max",
        "--draft-max",
        "--draft-n",
        "--draft",
        "--model-draft",
        "-md",
    ];
    /// The slot count `vl_spec_decode` derives from `parameter_batch_size`: a
    /// raw one would run a concurrency the benchmark id does not name, and
    /// split the context the harness sized per slot.
    pub const RESERVED_SLOTS_DERIVED: &[&str] = &["--parallel", "-np"];
    /// What the ASR cells derive on llama-server: thinking off
    /// (`--reasoning off`), which a raw `--reasoning` or budget would undo.
    /// The rest of the `--reasoning-*` family is refused by shape, see
    /// [`Self::is_reasoning_flag`].
    pub const RESERVED_REASONING_DERIVED: &[&str] = &["-rea", "--reasoning", "--reasoning-budget"];
    /// The flags that choose the chat template and what it is passed, which
    /// the ASR cells derive per model family (the recipe): the ask is part of
    /// the prefill the benchmark times, so a plan's own template would time a
    /// different one under the same id. `--chat-template-kwargs` can also
    /// carry `enable_thinking`.
    ///
    /// The client (`pipette-llamacpp`'s `asr::server_args`) refuses the same
    /// set, read through [`Self::flag_name`], in every spelling.
    pub const RESERVED_TEMPLATE_DERIVED: &[&str] = &[
        "--jinja",
        "--no-jinja",
        "--chat-template",
        "--chat-template-file",
        "--chat-template-kwargs",
    ];
    /// The env spellings of the slot count and the reasoning and template
    /// knobs (`LLAMA_ARG_*` in llama.cpp's `common/arg.cpp`), which reach an
    /// ASR server as env forwards: an ASR cell refuses any forward whose name
    /// starts with one, bare (inherited) or `NAME=value`. Builds before b9360
    /// read `--chat-template-kwargs` from `LLAMA_CHAT_TEMPLATE_KWARGS`.
    ///
    /// The client (`pipette-llamacpp`'s `asr::server_args`) refuses the
    /// reasoning and template prefixes.
    pub const RESERVED_ASR_ENV_PREFIXES: &[&str] = &[
        "LLAMA_ARG_N_PARALLEL",
        "LLAMA_ARG_REASONING",
        "LLAMA_ARG_THINK",
        "LLAMA_ARG_JINJA",
        "LLAMA_ARG_CHAT_TEMPLATE",
        "LLAMA_CHAT_TEMPLATE_KWARGS",
    ];
    /// The ASR cells' reserved set: the server's, one slot (the context is
    /// sized for one clip, and llama-server divides it among its slots),
    /// thinking off, and the recipe's template.
    pub const RESERVED_ASR_SERVER: &[&str] = concat_str_slices!(
        concat_str_slices!(
            concat_str_slices!(
                LlamacppCliStockTools::RESERVED_SERVER,
                LlamacppCliStockTools::RESERVED_SLOTS_DERIVED
            ),
            LlamacppCliStockTools::RESERVED_REASONING_DERIVED
        ),
        LlamacppCliStockTools::RESERVED_TEMPLATE_DERIVED
    );

    /// The flag a `raw` token names, as llama.cpp reads it: the name before
    /// any `=`, and a long name with `_` for `-` (llama.cpp accepts both).
    /// `None` for a value (a token not starting with `-`).
    ///
    /// The ASR cells read `raw` through this, and so does the client.
    pub fn flag_name(token: &str) -> Option<String> {
        if !token.starts_with('-') {
            return None;
        }
        let name = token.split_once('=').map_or(token, |(name, _)| name);
        Some(if name.starts_with("--") {
            name.replace('_', "-")
        } else {
            name.to_owned()
        })
    }

    /// Whether a flag name (from [`Self::flag_name`]) is one of llama.cpp's
    /// reasoning flags (`common/arg.cpp`): `-rea` / `--reasoning`, and every
    /// `--reasoning-*` and `--no-reasoning-*` knob (budget, effort, format,
    /// preserve). A shape rule, so a knob added upstream is refused too.
    pub fn is_reasoning_flag(name: &str) -> bool {
        name == "-rea" || name.starts_with("--reasoning") || name.starts_with("--no-reasoning")
    }

    /// Whether an ASR cell refuses this `raw` token: a reasoning flag or a
    /// template flag ([`Self::RESERVED_TEMPLATE_DERIVED`]), in any spelling
    /// llama.cpp reads as one.
    pub fn asr_refuses_raw(token: &str) -> bool {
        Self::flag_name(token).is_some_and(|name| {
            Self::is_reasoning_flag(&name)
                || Self::RESERVED_TEMPLATE_DERIVED.contains(&name.as_str())
        })
    }

    /// Whether an ASR cell refuses an env forward of this name (the part
    /// before any `=`): see [`Self::RESERVED_ASR_ENV_PREFIXES`]. Windows reads
    /// env names in any case, so the match ignores it.
    pub fn asr_refuses_env(name: &str) -> bool {
        let name = name.to_ascii_uppercase();
        Self::RESERVED_ASR_ENV_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
    }
}

/// llama-server knobs we type (`ctx_size`, `no_cache`) and the reasoning flags;
/// server-only, so not on the bench denylists. The reasoning flags are closed on
/// every server cell: raw, they would toggle thinking past the model flags'
/// `enable_thinking`, which the client applies.
const LLAMA_SERVER_TYPED_ALIASES: &[&str] = &[
    "-c",
    "--ctx-size",
    "--no-cache-prompt",
    "--reasoning",
    "--reasoning-budget",
    // Untyped, but `none` inlines thinking into the content the scorers parse.
    "--reasoning-format",
];
/// llama-server flags that would change what a vlm_eval generates, or how its
/// reply is read, behind its catalog entry: every sampler and its order, the
/// length cap, grammars, the chat template and its kwargs, thinking, special
/// tokens in the reply, the projector's image budget, and the presets that set
/// any of these. The entry and its recipe own all of them (a request sets most
/// samplers itself, but a server flag is the default for each one it leaves
/// out), so a score is always the recipe's.
const LLAMA_VLM_EVAL_RECIPE_OWNED: &[&str] = &[
    "--samplers",
    "--sampler-seq",
    "--sampling-seq",
    "-s",
    "--seed",
    "--ignore-eos",
    "--temp",
    "--temperature",
    "--top-k",
    "--top-p",
    "--min-p",
    "--top-nsigma",
    "--top-n-sigma",
    "--xtc-probability",
    "--xtc-threshold",
    "--typical",
    "--typical-p",
    "--repeat-last-n",
    "--repeat-penalty",
    "--presence-penalty",
    "--frequency-penalty",
    "--dry-multiplier",
    "--dry-base",
    "--dry-allowed-length",
    "--dry-penalty-last-n",
    "--dry-sequence-breaker",
    "--adaptive-target",
    "--adaptive-decay",
    "--dynatemp-range",
    "--dynatemp-exp",
    "--mirostat",
    "--mirostat-lr",
    "--mirostat-ent",
    "-l",
    "--logit-bias",
    "--grammar",
    "--grammar-file",
    "-j",
    "--json-schema",
    "-jf",
    "--json-schema-file",
    "-bs",
    "--backend-sampling",
    "-n",
    "--predict",
    "--n-predict",
    "-r",
    "--reverse-prompt",
    "-sp",
    "--special",
    "--jinja",
    "--no-jinja",
    "--chat-template",
    "--chat-template-file",
    "--chat-template-kwargs",
    "--no-prefill-assistant",
    "--skip-chat-parsing",
    "--no-skip-chat-parsing",
    "-sys",
    "--system-prompt",
    "-sysf",
    "--system-prompt-file",
    // Any GGUF metadata, the chat template included.
    "--override-kv",
    "-rea",
    "--reasoning-effort",
    "--reasoning-budget-message",
    "--reasoning-preserve",
    "--no-reasoning-preserve",
    "--image-min-tokens",
    "--image-max-tokens",
    "--video-fps",
    "--video-timestamp-interval",
    // Another model, projector or adapter than the one the recipe covers.
    "-mm",
    "-mmu",
    "--mmproj-url",
    "--no-mmproj",
    "--no-mmproj-auto",
    "-hf",
    "-hfr",
    "--hf-repo",
    "-hff",
    "--hf-file",
    "-mu",
    "--model-url",
    "-dr",
    "--docker-repo",
    "--models-dir",
    "--models-preset",
    "--lora",
    "--lora-scaled",
    "--lora-init-without-apply",
    "--control-vector",
    "--control-vector-scaled",
    "--control-vector-layer-range",
    "--dflash",
    // The model's positions: its attention, not its speed.
    "--rope-scaling",
    "--rope-scale",
    "--rope-freq-base",
    "--rope-freq-scale",
    "--yarn-orig-ctx",
    "--yarn-ext-factor",
    "--yarn-attn-factor",
    "--yarn-beta-slow",
    "--yarn-beta-fast",
    // What a sample sees once its context fills.
    "--context-shift",
    "--no-context-shift",
    "--keep",
    "--embd-gemma-default",
    "--fim-qwen-1.5b-default",
    "--fim-qwen-3b-default",
    "--fim-qwen-7b-default",
    "--fim-qwen-7b-spec",
    "--fim-qwen-14b-spec",
    "--fim-qwen-30b-default",
    "--gpt-oss-20b-default",
    "--gpt-oss-120b-default",
    "--vision-gemma-4b-default",
    "--vision-gemma-12b-default",
];
/// The prefix of every environment variable llama.cpp reads an option from.
const LLAMA_ARG_PREFIX: &str = "LLAMA_ARG_";
/// The variable builds before b9360 read `--chat-template-kwargs` from.
const LLAMA_CHAT_TEMPLATE_KWARGS_ENV: &str = "LLAMA_CHAT_TEMPLATE_KWARGS";
/// What a vlm_eval cell's `raw` may not carry that it reports as reserved: the
/// server's own flags and the recipe's.
const RESERVED_LLAMA_VLM_EVAL: &[&str] = concat_str_slices!(
    LlamacppCliStockTools::RESERVED_SERVER,
    LLAMA_VLM_EVAL_RECIPE_OWNED
);
/// An asr_eval's recipe owns what a vlm_eval's does, and its beam search too
/// (pipette's ASR build of llama.cpp): the client starts the server with the
/// entry's beams, chat template, template kwargs and EOT override per prompt
/// profile. It also owns the slot split and the per-slot limit it sizes the
/// context for (`--parallel`, and the ASR build's `--kv-unified-per-slot`),
/// which the client refuses in `raw` too, so no job is queued that a device
/// would refuse.
const LLAMA_ASR_EVAL_RECIPE_OWNED: &[&str] = concat_str_slices!(
    LLAMA_VLM_EVAL_RECIPE_OWNED,
    &[
        "--beam-search",
        "-np",
        "--parallel",
        "--kv-unified-per-slot"
    ]
);
const RESERVED_LLAMA_ASR_EVAL: &[&str] = concat_str_slices!(
    LlamacppCliStockTools::RESERVED_SERVER,
    LLAMA_ASR_EVAL_RECIPE_OWNED
);
/// vLLM launch flags the runtime fixes itself.
const VLLM_RESERVED: &[&str] = &["--model", "--host", "--port"];
/// vLLM flags that would change what a vlm_eval generates, or what it sees,
/// behind its catalog entry: the sampling defaults (the runner fixes them at
/// `--generation-config vllm`, so every knob a request leaves out is neutral),
/// the chat template, its default kwargs and how a multimodal prompt is laid
/// out, the image processor's kwargs and whether a request may set them (the
/// recipe's image budget, which the runner sends), another tokenizer, revision
/// or config than the model's, weights quantized or loaded otherwise than the
/// repo publishes them or run by another implementation, a quantized KV
/// cache, attention without the model's sliding window, a server seed, logits
/// processors, adapters and drafters, a parser that would take the reply apart
/// (the reasoning, a structured output), and a YAML file that would set any of
/// these out of sight of the argv.
const VLLM_VLM_EVAL_RECIPE_OWNED: &[&str] = &[
    "--config",
    "--generation-config",
    "--override-generation-config",
    "--chat-template",
    "--chat-template-content-format",
    "--default-chat-template-kwargs",
    "--interleave-mm-strings",
    "--mm-processor-kwargs",
    "--trust-request-mm-kwargs",
    "--media-io-kwargs",
    "--hf-overrides",
    "--hf-config-path",
    "--tokenizer",
    "--tokenizer-mode",
    "--revision",
    "--tokenizer-revision",
    "--code-revision",
    "--quantization",
    "-q",
    "--quantization-config",
    "--load-format",
    "--model-loader-extra-config",
    // pipette serves the repo from its own store by path, so this changes
    // nothing it loads today; reserved so no cell can point the server at a
    // pre-seeded copy of the weights.
    "--download-dir",
    "--model-impl",
    "--kv-cache-dtype",
    "--calculate-kv-scales",
    "--disable-sliding-window",
    "--seed",
    "--logits-processors",
    "--logits-processor-pattern",
    "--enable-lora",
    "--lora-modules",
    "--speculative-config",
    "--speculative-model",
    "--num-speculative-tokens",
    "--reasoning-parser",
    "--reasoning-parser-plugin",
    "--enable-reasoning",
    "--structured-outputs-config",
    "--guided-decoding-backend",
];
/// What a vLLM vlm_eval cell's `raw` may not carry that it reports as
/// reserved: the server's own flags and the recipe's.
const RESERVED_VLLM_VLM_EVAL: &[&str] =
    concat_str_slices!(VLLM_RESERVED, VLLM_VLM_EVAL_RECIPE_OWNED);
/// Everything a vLLM vlm_eval cell's `raw` may carry: how fast the server
/// runs, how much memory it takes, how it is split over devices and what it
/// logs. vLLM's command line is too wide, and grows too fast, to close a
/// recipe by naming what may not be set: the reserved list above only names
/// the commonest in its refusal. A flag not here is refused.
const VLLM_VLM_EVAL_RAW_ALLOWED: &[&str] = &[
    // Loading a repo whose processor or model is its own code.
    "--trust-remote-code",
    // Memory.
    "--gpu-memory-utilization",
    "--kv-cache-memory-bytes",
    "--swap-space",
    "--cpu-offload-gb",
    "--block-size",
    "--num-gpu-blocks-override",
    // Scheduling and batching.
    "--max-num-seqs",
    "--max-num-batched-tokens",
    "--max-num-partial-prefills",
    "--max-long-partial-prefills",
    "--long-prefill-token-threshold",
    "--enable-chunked-prefill",
    "--no-enable-chunked-prefill",
    "--async-scheduling",
    "--no-async-scheduling",
    // Kernels and graphs.
    "--enforce-eager",
    "--compilation-config",
    "--attention-backend",
    // Multimodal processing caches and the encoder's split over devices.
    // `--limit-mm-per-prompt` is not here: besides a count, it takes per-image
    // sizes and frame counts.
    "--mm-processor-cache-gb",
    "--mm-processor-cache-type",
    "--mm-encoder-tp-mode",
    "--skip-mm-profiling",
    // Weight loading.
    "--safetensors-load-strategy",
    "--max-parallel-loading-workers",
    // Devices.
    "--pipeline-parallel-size",
    "--data-parallel-size",
    "--distributed-executor-backend",
    "--enable-expert-parallel",
    "--disable-custom-all-reduce",
    // Logging.
    "--disable-log-stats",
    "--enable-log-requests",
    "--disable-log-requests",
    "--max-log-len",
    "--uvicorn-log-level",
    "--disable-uvicorn-access-log",
];
/// The environment a vLLM vlm_eval cell's `envs` may set, by exact name: which
/// devices it sees, its threads, cache locations and logging. vLLM also reads
/// `VLLM_*` variables that redirect the model to other weights, load plugins,
/// or pick kernels and sampling paths, none of which a recipe can vouch for.
const VLLM_VLM_EVAL_ENV_ALLOWED: &[&str] = &[
    "CUDA_VISIBLE_DEVICES",
    "HIP_VISIBLE_DEVICES",
    "ROCR_VISIBLE_DEVICES",
    "OMP_NUM_THREADS",
    "TMPDIR",
    "HF_HUB_OFFLINE",
    "VLLM_LOGGING_LEVEL",
    "VLLM_CACHE_ROOT",
    "VLLM_WORKER_MULTIPROC_METHOD",
    "TRITON_CACHE_DIR",
    "TORCHINDUCTOR_CACHE_DIR",
    // How the collective library talks between devices, and what it logs to
    // the console: not its plugins, config files or log files.
    "NCCL_DEBUG",
    "NCCL_DEBUG_SUBSYS",
    "NCCL_SOCKET_IFNAME",
    "NCCL_P2P_DISABLE",
    "NCCL_IB_DISABLE",
    "NCCL_SHM_DISABLE",
];
/// sglang launch flags the runtime fixes itself.
const SGLANG_RESERVED: &[&str] = &["--model-path", "--host", "--port"];
/// TensorRT Edge-LLM launch flags the runtime fixes: the GGUF pair it builds
/// from, the address, the compile cache the engines live in, and the build.
const TRT_EDGELLM_RESERVED: &[&str] = &[
    "--model",
    "-m",
    "--mmproj",
    "--host",
    "--port",
    "--cache-dir",
    "--build-only",
];
/// The slot cap the timing cells derive from `parameter_batch_size`. Not on
/// eval: its `parallel` is a client-side fan-out, and the server cap there is
/// the operator's business.
const VLLM_BATCH_DERIVED: &[&str] = &["--max-num-seqs"];
const SGLANG_BATCH_DERIVED: &[&str] = &["--max-running-requests"];
/// The timing cells' reserved sets: the launch flags plus the slot cap.
const VLLM_TIMING_RESERVED: &[&str] = concat_str_slices!(VLLM_RESERVED, VLLM_BATCH_DERIVED);
const SGLANG_TIMING_RESERVED: &[&str] = concat_str_slices!(SGLANG_RESERVED, SGLANG_BATCH_DERIVED);
/// The cache-report flag `vl_spec_decode` always derives on sglang: a turn is
/// refused on cached prompt tokens, which sglang counts only with this flag.
/// Reserved so restating it cannot make identical runs record different argv.
const SGLANG_CACHE_REPORT_DERIVED: &[&str] = &["--enable-cache-report"];
/// sglang speculation flags `vl_spec_decode` derives with speculation on. With
/// it off the whole family is refused by shape, see
/// [`is_sglang_speculation_flag`].
const SGLANG_SPECULATIVE_DERIVED: &[&str] = &[
    "--speculative-algorithm",
    "--speculative-draft-model-path",
    "--speculative-dspark-block-size",
    "--speculative-num-draft-tokens",
];

const DENY_LLAMA_PREFILL: &[&str] =
    concat_str_slices!(LLAMA_TYPED_ALIASES, LlamacppCliStockTools::RESERVED_PREFILL);
const DENY_LLAMA_DECODE: &[&str] =
    concat_str_slices!(LLAMA_TYPED_ALIASES, LlamacppCliStockTools::RESERVED_DECODE);
const DENY_LLAMA_MAXMEM: &[&str] = concat_str_slices!(
    LLAMA_TYPED_ALIASES,
    LlamacppCliStockTools::RESERVED_MAX_MEMORY
);
const DENY_LLAMA_SERVER: &[&str] = concat_str_slices!(
    concat_str_slices!(LLAMA_TYPED_ALIASES, LlamacppCliStockTools::RESERVED_SERVER),
    LLAMA_SERVER_TYPED_ALIASES
);
/// The reasoning names an ASR cell reserves that every server cell does not
/// already deny as typed aliases ([`LLAMA_SERVER_TYPED_ALIASES`]).
const ASR_REASONING_BEYOND_SERVER: &[&str] = &["-rea"];
/// The ASR cells': new, so no stored plan carries a raw slot flag to keep.
const DENY_LLAMA_ASR_SERVER: &[&str] = concat_str_slices!(
    concat_str_slices!(
        concat_str_slices!(
            DENY_LLAMA_SERVER,
            LlamacppCliStockTools::RESERVED_SLOTS_DERIVED
        ),
        ASR_REASONING_BEYOND_SERVER
    ),
    LlamacppCliStockTools::RESERVED_TEMPLATE_DERIVED
);

const DENY_LLAMA_VLM_EVAL: &[&str] =
    concat_str_slices!(DENY_LLAMA_SERVER, LLAMA_VLM_EVAL_RECIPE_OWNED);
const DENY_LLAMA_ASR_EVAL: &[&str] =
    concat_str_slices!(DENY_LLAMA_SERVER, LLAMA_ASR_EVAL_RECIPE_OWNED);
const DENY_VLLM: &[&str] = concat_str_slices!(VLLM_TYPED_ALIASES, VLLM_RESERVED);
const DENY_VLLM_VLM_EVAL: &[&str] = concat_str_slices!(VLLM_TYPED_ALIASES, RESERVED_VLLM_VLM_EVAL);
const DENY_SGLANG: &[&str] = concat_str_slices!(SGLANG_TYPED_ALIASES, SGLANG_RESERVED);
const DENY_VLLM_TIMING: &[&str] = concat_str_slices!(VLLM_TYPED_ALIASES, VLLM_TIMING_RESERVED);
const DENY_SGLANG_TIMING: &[&str] =
    concat_str_slices!(SGLANG_TYPED_ALIASES, SGLANG_TIMING_RESERVED);
const DENY_TRT_EDGELLM: &[&str] =
    concat_str_slices!(TRT_EDGELLM_TYPED_ALIASES, TRT_EDGELLM_RESERVED);

impl RuntimeFlags {
    /// Flag names a `raw` entry on this cell may not carry: the tool's
    /// typed-knob aliases plus the benchmark/tool's reserved flags.
    /// The only flags this cell's `raw` may carry, for a cell that names
    /// them rather than the flags it may not ([`VLLM_VLM_EVAL_RAW_ALLOWED`]):
    /// the vLLM vlm_eval and asr_eval cells.
    pub fn raw_allowlist(&self) -> Option<&'static [&'static str]> {
        matches!(
            self,
            RuntimeFlags::VlmEvalUvVllmTorch { .. } | RuntimeFlags::AsrEvalUvVllmTorch { .. }
        )
        .then_some(VLLM_VLM_EVAL_RAW_ALLOWED)
    }

    pub fn raw_denylist(&self) -> &'static [&'static str] {
        match self {
            RuntimeFlags::PrefillThroughputLlamacppCliStockToolsGgufText { .. } => {
                DENY_LLAMA_PREFILL
            }
            RuntimeFlags::DecodeThroughputLlamacppCliStockToolsGgufText { .. } => DENY_LLAMA_DECODE,
            RuntimeFlags::MaxMemoryUsageLlamacppCliStockToolsGgufText { .. } => DENY_LLAMA_MAXMEM,
            // `--parallel` stays allowed though eval types it: stored plans
            // carry it in `raw`, and denying it would fail their deserialize.
            // The client rejects only the typed-plus-raw combination.
            RuntimeFlags::EndToEndLatencyLlamacppCliStockToolsGgufText { .. }
            | RuntimeFlags::EvalLlamacppCliStockToolsGgufText { .. }
            | RuntimeFlags::VlThroughputLlamacppCliStockToolsGgufVision { .. }
            | RuntimeFlags::VlMaxMemoryLlamacppCliStockToolsGgufVision { .. }
            | RuntimeFlags::VlSpecDecodeLlamacppCliStockToolsGgufVision { .. }
            | RuntimeFlags::VlSpecDecodeLlamacppCliStockToolsGgufVisionSpeculative { .. } => {
                DENY_LLAMA_SERVER
            }
            RuntimeFlags::AsrThroughputLlamacppCliStockToolsGgufVision { .. }
            | RuntimeFlags::AsrMaxMemoryLlamacppCliStockToolsGgufVision { .. } => {
                DENY_LLAMA_ASR_SERVER
            }
            RuntimeFlags::VlmEvalLlamacppCliStockToolsGgufVision { .. } => DENY_LLAMA_VLM_EVAL,
            RuntimeFlags::AsrEvalLlamacppCliStockToolsGgufVision { .. } => DENY_LLAMA_ASR_EVAL,
            RuntimeFlags::PrefillThroughputDockerVllmTorch { .. }
            | RuntimeFlags::DecodeThroughputDockerVllmTorch { .. }
            | RuntimeFlags::EndToEndLatencyDockerVllmTorch { .. }
            | RuntimeFlags::PrefillThroughputUvVllmTorch { .. }
            | RuntimeFlags::DecodeThroughputUvVllmTorch { .. }
            | RuntimeFlags::EndToEndLatencyUvVllmTorch { .. } => DENY_VLLM_TIMING,
            RuntimeFlags::EvalDockerVllmTorch { .. }
            | RuntimeFlags::MaxMemoryUsageDockerVllmTorch { .. }
            | RuntimeFlags::EvalUvVllmTorch { .. }
            | RuntimeFlags::MaxMemoryUsageUvVllmTorch { .. } => DENY_VLLM,
            RuntimeFlags::VlmEvalUvVllmTorch { .. } | RuntimeFlags::AsrEvalUvVllmTorch { .. } => {
                DENY_VLLM_VLM_EVAL
            }
            RuntimeFlags::PrefillThroughputDockerSglangTorch { .. }
            | RuntimeFlags::DecodeThroughputDockerSglangTorch { .. }
            | RuntimeFlags::EndToEndLatencyDockerSglangTorch { .. }
            | RuntimeFlags::PrefillThroughputUvSglangTorch { .. }
            | RuntimeFlags::DecodeThroughputUvSglangTorch { .. }
            | RuntimeFlags::EndToEndLatencyUvSglangTorch { .. } => DENY_SGLANG_TIMING,
            RuntimeFlags::EvalDockerSglangTorch { .. }
            | RuntimeFlags::MaxMemoryUsageDockerSglangTorch { .. }
            | RuntimeFlags::EvalUvSglangTorch { .. }
            | RuntimeFlags::MaxMemoryUsageUvSglangTorch { .. }
            | RuntimeFlags::VlSpecDecodeDockerSglangTorch { .. }
            | RuntimeFlags::VlSpecDecodeDockerSglangTorchSpeculative { .. }
            | RuntimeFlags::VlSpecDecodeUvSglangTorch { .. }
            | RuntimeFlags::VlSpecDecodeUvSglangTorchSpeculative { .. } => DENY_SGLANG,
            RuntimeFlags::PrefillThroughputLlamacppIosPipetteGgufText { .. }
            | RuntimeFlags::DecodeThroughputLlamacppIosPipetteGgufText { .. }
            | RuntimeFlags::MaxMemoryUsageLlamacppIosPipetteGgufText { .. }
            | RuntimeFlags::EndToEndLatencyLlamacppIosPipetteGgufText { .. }
            | RuntimeFlags::EvalLlamacppIosPipetteGgufText { .. }
            | RuntimeFlags::VlThroughputLlamacppIosPipetteGgufVision { .. }
            | RuntimeFlags::VlMaxMemoryLlamacppIosPipetteGgufVision { .. }
            | RuntimeFlags::AsrThroughputLlamacppIosPipetteGgufVision { .. }
            | RuntimeFlags::AsrMaxMemoryLlamacppIosPipetteGgufVision { .. }
            | RuntimeFlags::VlSpecDecodeLlamacppIosPipetteGgufVision { .. }
            | RuntimeFlags::VlSpecDecodeLlamacppIosPipetteGgufVisionSpeculative { .. }
            | RuntimeFlags::PrefillThroughputLlamacppApkPipetteGgufText { .. }
            | RuntimeFlags::DecodeThroughputLlamacppApkPipetteGgufText { .. }
            | RuntimeFlags::MaxMemoryUsageLlamacppApkPipetteGgufText { .. }
            | RuntimeFlags::EndToEndLatencyLlamacppApkPipetteGgufText { .. }
            | RuntimeFlags::EvalLlamacppApkPipetteGgufText { .. }
            | RuntimeFlags::VlThroughputLlamacppApkPipetteGgufVision { .. }
            | RuntimeFlags::PrefillThroughputMlxIosPipetteMlx { .. }
            | RuntimeFlags::DecodeThroughputMlxIosPipetteMlx { .. }
            | RuntimeFlags::MaxMemoryUsageMlxIosPipetteMlx { .. }
            | RuntimeFlags::EndToEndLatencyMlxIosPipetteMlx { .. }
            | RuntimeFlags::EvalMlxIosPipetteMlx { .. }
            | RuntimeFlags::VlSpecDecodeMlxMacosPipetteMlx { .. }
            | RuntimeFlags::VlSpecDecodeMlxMacosPipetteMlxSpeculative { .. } => &[],
            RuntimeFlags::PrefillThroughputUvOpenvinoOpenvino { .. }
            | RuntimeFlags::DecodeThroughputUvOpenvinoOpenvino { .. }
            | RuntimeFlags::EndToEndLatencyUvOpenvinoOpenvino { .. }
            | RuntimeFlags::MaxMemoryUsageUvOpenvinoOpenvino { .. }
            | RuntimeFlags::PrefillThroughputOvmsOpenvino { .. }
            | RuntimeFlags::DecodeThroughputOvmsOpenvino { .. }
            | RuntimeFlags::EndToEndLatencyOvmsOpenvino { .. }
            | RuntimeFlags::MaxMemoryUsageOvmsOpenvino { .. } => &[],
            RuntimeFlags::VlThroughputTrtEdgellmGgufVision { .. }
            | RuntimeFlags::VlMaxMemoryTrtEdgellmGgufVision { .. } => DENY_TRT_EDGELLM,
        }
    }

    /// The reserved subset of [`Self::raw_denylist`]; the split picks which
    /// `raw` rejection reason is reported.
    fn tool_reserved(&self) -> &'static [&'static str] {
        match self {
            RuntimeFlags::PrefillThroughputLlamacppCliStockToolsGgufText { .. } => {
                LlamacppCliStockTools::RESERVED_PREFILL
            }
            RuntimeFlags::DecodeThroughputLlamacppCliStockToolsGgufText { .. } => {
                LlamacppCliStockTools::RESERVED_DECODE
            }
            RuntimeFlags::MaxMemoryUsageLlamacppCliStockToolsGgufText { .. } => {
                LlamacppCliStockTools::RESERVED_MAX_MEMORY
            }
            RuntimeFlags::EndToEndLatencyLlamacppCliStockToolsGgufText { .. }
            | RuntimeFlags::EvalLlamacppCliStockToolsGgufText { .. }
            | RuntimeFlags::VlThroughputLlamacppCliStockToolsGgufVision { .. }
            | RuntimeFlags::VlMaxMemoryLlamacppCliStockToolsGgufVision { .. }
            | RuntimeFlags::VlSpecDecodeLlamacppCliStockToolsGgufVision { .. }
            | RuntimeFlags::VlSpecDecodeLlamacppCliStockToolsGgufVisionSpeculative { .. } => {
                LlamacppCliStockTools::RESERVED_SERVER
            }
            RuntimeFlags::AsrThroughputLlamacppCliStockToolsGgufVision { .. }
            | RuntimeFlags::AsrMaxMemoryLlamacppCliStockToolsGgufVision { .. } => {
                LlamacppCliStockTools::RESERVED_ASR_SERVER
            }
            RuntimeFlags::VlmEvalLlamacppCliStockToolsGgufVision { .. } => RESERVED_LLAMA_VLM_EVAL,
            RuntimeFlags::AsrEvalLlamacppCliStockToolsGgufVision { .. } => RESERVED_LLAMA_ASR_EVAL,
            RuntimeFlags::PrefillThroughputDockerVllmTorch { .. }
            | RuntimeFlags::DecodeThroughputDockerVllmTorch { .. }
            | RuntimeFlags::EndToEndLatencyDockerVllmTorch { .. }
            | RuntimeFlags::PrefillThroughputUvVllmTorch { .. }
            | RuntimeFlags::DecodeThroughputUvVllmTorch { .. }
            | RuntimeFlags::EndToEndLatencyUvVllmTorch { .. } => VLLM_TIMING_RESERVED,
            RuntimeFlags::EvalDockerVllmTorch { .. }
            | RuntimeFlags::MaxMemoryUsageDockerVllmTorch { .. }
            | RuntimeFlags::EvalUvVllmTorch { .. }
            | RuntimeFlags::MaxMemoryUsageUvVllmTorch { .. } => VLLM_RESERVED,
            RuntimeFlags::VlmEvalUvVllmTorch { .. } | RuntimeFlags::AsrEvalUvVllmTorch { .. } => {
                RESERVED_VLLM_VLM_EVAL
            }
            RuntimeFlags::PrefillThroughputDockerSglangTorch { .. }
            | RuntimeFlags::DecodeThroughputDockerSglangTorch { .. }
            | RuntimeFlags::EndToEndLatencyDockerSglangTorch { .. }
            | RuntimeFlags::PrefillThroughputUvSglangTorch { .. }
            | RuntimeFlags::DecodeThroughputUvSglangTorch { .. }
            | RuntimeFlags::EndToEndLatencyUvSglangTorch { .. } => SGLANG_TIMING_RESERVED,
            RuntimeFlags::EvalDockerSglangTorch { .. }
            | RuntimeFlags::MaxMemoryUsageDockerSglangTorch { .. }
            | RuntimeFlags::EvalUvSglangTorch { .. }
            | RuntimeFlags::MaxMemoryUsageUvSglangTorch { .. }
            | RuntimeFlags::VlSpecDecodeDockerSglangTorch { .. }
            | RuntimeFlags::VlSpecDecodeDockerSglangTorchSpeculative { .. }
            | RuntimeFlags::VlSpecDecodeUvSglangTorch { .. }
            | RuntimeFlags::VlSpecDecodeUvSglangTorchSpeculative { .. } => SGLANG_RESERVED,
            RuntimeFlags::PrefillThroughputLlamacppIosPipetteGgufText { .. }
            | RuntimeFlags::DecodeThroughputLlamacppIosPipetteGgufText { .. }
            | RuntimeFlags::MaxMemoryUsageLlamacppIosPipetteGgufText { .. }
            | RuntimeFlags::EndToEndLatencyLlamacppIosPipetteGgufText { .. }
            | RuntimeFlags::EvalLlamacppIosPipetteGgufText { .. }
            | RuntimeFlags::VlThroughputLlamacppIosPipetteGgufVision { .. }
            | RuntimeFlags::VlMaxMemoryLlamacppIosPipetteGgufVision { .. }
            | RuntimeFlags::AsrThroughputLlamacppIosPipetteGgufVision { .. }
            | RuntimeFlags::AsrMaxMemoryLlamacppIosPipetteGgufVision { .. }
            | RuntimeFlags::VlSpecDecodeLlamacppIosPipetteGgufVision { .. }
            | RuntimeFlags::VlSpecDecodeLlamacppIosPipetteGgufVisionSpeculative { .. }
            | RuntimeFlags::PrefillThroughputLlamacppApkPipetteGgufText { .. }
            | RuntimeFlags::DecodeThroughputLlamacppApkPipetteGgufText { .. }
            | RuntimeFlags::MaxMemoryUsageLlamacppApkPipetteGgufText { .. }
            | RuntimeFlags::EndToEndLatencyLlamacppApkPipetteGgufText { .. }
            | RuntimeFlags::EvalLlamacppApkPipetteGgufText { .. }
            | RuntimeFlags::VlThroughputLlamacppApkPipetteGgufVision { .. }
            | RuntimeFlags::PrefillThroughputMlxIosPipetteMlx { .. }
            | RuntimeFlags::DecodeThroughputMlxIosPipetteMlx { .. }
            | RuntimeFlags::MaxMemoryUsageMlxIosPipetteMlx { .. }
            | RuntimeFlags::EndToEndLatencyMlxIosPipetteMlx { .. }
            | RuntimeFlags::EvalMlxIosPipetteMlx { .. }
            | RuntimeFlags::VlSpecDecodeMlxMacosPipetteMlx { .. }
            | RuntimeFlags::VlSpecDecodeMlxMacosPipetteMlxSpeculative { .. } => &[],
            RuntimeFlags::PrefillThroughputUvOpenvinoOpenvino { .. }
            | RuntimeFlags::DecodeThroughputUvOpenvinoOpenvino { .. }
            | RuntimeFlags::EndToEndLatencyUvOpenvinoOpenvino { .. }
            | RuntimeFlags::MaxMemoryUsageUvOpenvinoOpenvino { .. }
            | RuntimeFlags::PrefillThroughputOvmsOpenvino { .. }
            | RuntimeFlags::DecodeThroughputOvmsOpenvino { .. }
            | RuntimeFlags::EndToEndLatencyOvmsOpenvino { .. }
            | RuntimeFlags::MaxMemoryUsageOvmsOpenvino { .. } => &[],
            RuntimeFlags::VlThroughputTrtEdgellmGgufVision { .. }
            | RuntimeFlags::VlMaxMemoryTrtEdgellmGgufVision { .. } => TRT_EDGELLM_RESERVED,
        }
    }

    /// Reject any `raw` entry that names a flag this cell owns, in the forms
    /// [`deny_match`] matches. `speculation` is the run's resolved drafter
    /// state.
    pub(super) fn validate_raw(&self, speculation: bool) -> Result<(), RuntimeFlagsError> {
        check_raw(self.raw(), self.raw_denylist(), self.tool_reserved())?;
        if let Some(allowed) = self.raw_allowlist() {
            check_allowed(self.raw(), allowed, self.axes().0)?;
        }
        // An ASR server runs with thinking off and the recipe's template, so
        // every reasoning or template knob is either an override or a no-op
        // the row would record as a difference. Read as llama.cpp reads it
        // (`_` for `-`, `=value`), as the client does.
        if self.is_asr_cell()
            && let Some(flag) = self
                .raw()
                .iter()
                .find(|t| LlamacppCliStockTools::asr_refuses_raw(t))
        {
            return Err(RuntimeFlagsError::RawReservedFlag { flag: flag.clone() });
        }
        // The cells whose catalog entry carries a recipe.
        let recipe_cell = if matches!(
            self,
            RuntimeFlags::VlmEvalLlamacppCliStockToolsGgufVision { .. }
        ) {
            Some(crate::BenchmarkType::VlmEval)
        } else if matches!(
            self,
            RuntimeFlags::AsrEvalLlamacppCliStockToolsGgufVision { .. }
        ) {
            Some(crate::BenchmarkType::AsrEval)
        } else {
            None
        };
        // A drafter changes how a sampled decode draws its tokens: the recipe's
        // decode is the model's own, in every speculation flag's spelling.
        if recipe_cell.is_some()
            && let Some(flag) = self.raw().iter().find(|t| is_speculation_flag(t))
        {
            return Err(RuntimeFlagsError::RawReservedFlag { flag: flag.clone() });
        }
        // llama.cpp reads an option from `LLAMA_ARG_<NAME>` before its argv, so
        // on a recipe's cell every one is a second route to what the recipe
        // owns. Builds before b9360 read the template kwargs from
        // `LLAMA_CHAT_TEMPLATE_KWARGS`, which an asr_eval's recipe owns too.
        if let Some(benchmark) = recipe_cell
            && let Some(env) = self.envs().iter().find(|env| {
                let name = env.name();
                name.get(..LLAMA_ARG_PREFIX.len())
                    .is_some_and(|prefix| prefix.eq_ignore_ascii_case(LLAMA_ARG_PREFIX))
                    || (benchmark == crate::BenchmarkType::AsrEval
                        && name.eq_ignore_ascii_case(LLAMA_CHAT_TEMPLATE_KWARGS_ENV))
            })
        {
            return Err(RuntimeFlagsError::LlamaArgEnv {
                benchmark,
                name: env.name().to_owned(),
            });
        }
        // Per engine: the flags derived with speculation on, and what a
        // speculation flag looks like with it off.
        let llama: Option<&[String]> = match self {
            RuntimeFlags::VlSpecDecodeLlamacppCliStockToolsGgufVision(
                crate::VlSpecDecodeLlamacppCliStockTools { raw, .. },
            )
            | RuntimeFlags::VlSpecDecodeLlamacppCliStockToolsGgufVisionSpeculative(
                crate::VlSpecDecodeLlamacppCliStockTools { raw, .. },
            ) => Some(raw),
            RuntimeFlags::VlSpecDecodeDockerSglangTorch { .. }
            | RuntimeFlags::VlSpecDecodeDockerSglangTorchSpeculative { .. }
            | RuntimeFlags::VlSpecDecodeUvSglangTorch { .. }
            | RuntimeFlags::VlSpecDecodeUvSglangTorchSpeculative { .. } => None,
            RuntimeFlags::PrefillThroughputLlamacppCliStockToolsGgufText { .. }
            | RuntimeFlags::DecodeThroughputLlamacppCliStockToolsGgufText { .. }
            | RuntimeFlags::MaxMemoryUsageLlamacppCliStockToolsGgufText { .. }
            | RuntimeFlags::EndToEndLatencyLlamacppCliStockToolsGgufText { .. }
            | RuntimeFlags::EvalLlamacppCliStockToolsGgufText { .. }
            | RuntimeFlags::VlThroughputLlamacppCliStockToolsGgufVision { .. }
            | RuntimeFlags::AsrThroughputLlamacppCliStockToolsGgufVision { .. }
            | RuntimeFlags::VlmEvalLlamacppCliStockToolsGgufVision { .. }
            | RuntimeFlags::AsrEvalLlamacppCliStockToolsGgufVision { .. }
            | RuntimeFlags::VlMaxMemoryLlamacppCliStockToolsGgufVision { .. }
            | RuntimeFlags::AsrMaxMemoryLlamacppCliStockToolsGgufVision { .. }
            | RuntimeFlags::PrefillThroughputDockerVllmTorch { .. }
            | RuntimeFlags::DecodeThroughputDockerVllmTorch { .. }
            | RuntimeFlags::EndToEndLatencyDockerVllmTorch { .. }
            | RuntimeFlags::EvalDockerVllmTorch { .. }
            | RuntimeFlags::MaxMemoryUsageDockerVllmTorch { .. }
            | RuntimeFlags::PrefillThroughputUvVllmTorch { .. }
            | RuntimeFlags::DecodeThroughputUvVllmTorch { .. }
            | RuntimeFlags::EndToEndLatencyUvVllmTorch { .. }
            | RuntimeFlags::EvalUvVllmTorch { .. }
            | RuntimeFlags::VlmEvalUvVllmTorch { .. }
            | RuntimeFlags::AsrEvalUvVllmTorch { .. }
            | RuntimeFlags::MaxMemoryUsageUvVllmTorch { .. }
            | RuntimeFlags::PrefillThroughputDockerSglangTorch { .. }
            | RuntimeFlags::DecodeThroughputDockerSglangTorch { .. }
            | RuntimeFlags::EndToEndLatencyDockerSglangTorch { .. }
            | RuntimeFlags::EvalDockerSglangTorch { .. }
            | RuntimeFlags::MaxMemoryUsageDockerSglangTorch { .. }
            | RuntimeFlags::PrefillThroughputUvSglangTorch { .. }
            | RuntimeFlags::DecodeThroughputUvSglangTorch { .. }
            | RuntimeFlags::EndToEndLatencyUvSglangTorch { .. }
            | RuntimeFlags::EvalUvSglangTorch { .. }
            | RuntimeFlags::MaxMemoryUsageUvSglangTorch { .. }
            | RuntimeFlags::PrefillThroughputLlamacppIosPipetteGgufText { .. }
            | RuntimeFlags::DecodeThroughputLlamacppIosPipetteGgufText { .. }
            | RuntimeFlags::MaxMemoryUsageLlamacppIosPipetteGgufText { .. }
            | RuntimeFlags::EndToEndLatencyLlamacppIosPipetteGgufText { .. }
            | RuntimeFlags::EvalLlamacppIosPipetteGgufText { .. }
            | RuntimeFlags::VlThroughputLlamacppIosPipetteGgufVision { .. }
            | RuntimeFlags::VlMaxMemoryLlamacppIosPipetteGgufVision { .. }
            | RuntimeFlags::AsrThroughputLlamacppIosPipetteGgufVision { .. }
            | RuntimeFlags::AsrMaxMemoryLlamacppIosPipetteGgufVision { .. }
            | RuntimeFlags::VlSpecDecodeLlamacppIosPipetteGgufVision { .. }
            | RuntimeFlags::VlSpecDecodeLlamacppIosPipetteGgufVisionSpeculative { .. }
            | RuntimeFlags::VlSpecDecodeMlxMacosPipetteMlx { .. }
            | RuntimeFlags::VlSpecDecodeMlxMacosPipetteMlxSpeculative { .. }
            | RuntimeFlags::PrefillThroughputLlamacppApkPipetteGgufText { .. }
            | RuntimeFlags::DecodeThroughputLlamacppApkPipetteGgufText { .. }
            | RuntimeFlags::MaxMemoryUsageLlamacppApkPipetteGgufText { .. }
            | RuntimeFlags::EndToEndLatencyLlamacppApkPipetteGgufText { .. }
            | RuntimeFlags::EvalLlamacppApkPipetteGgufText { .. }
            | RuntimeFlags::VlThroughputLlamacppApkPipetteGgufVision { .. }
            | RuntimeFlags::PrefillThroughputMlxIosPipetteMlx { .. }
            | RuntimeFlags::DecodeThroughputMlxIosPipetteMlx { .. }
            | RuntimeFlags::MaxMemoryUsageMlxIosPipetteMlx { .. }
            | RuntimeFlags::EndToEndLatencyMlxIosPipetteMlx { .. }
            | RuntimeFlags::EvalMlxIosPipetteMlx { .. }
            | RuntimeFlags::PrefillThroughputUvOpenvinoOpenvino { .. }
            | RuntimeFlags::DecodeThroughputUvOpenvinoOpenvino { .. }
            | RuntimeFlags::EndToEndLatencyUvOpenvinoOpenvino { .. }
            | RuntimeFlags::MaxMemoryUsageUvOpenvinoOpenvino { .. }
            | RuntimeFlags::PrefillThroughputOvmsOpenvino { .. }
            | RuntimeFlags::DecodeThroughputOvmsOpenvino { .. }
            | RuntimeFlags::EndToEndLatencyOvmsOpenvino { .. }
            | RuntimeFlags::MaxMemoryUsageOvmsOpenvino { .. }
            | RuntimeFlags::VlThroughputTrtEdgellmGgufVision { .. }
            | RuntimeFlags::VlMaxMemoryTrtEdgellmGgufVision { .. } => return Ok(()),
        };
        let (raw, derived, is_spec_flag): (&[String], &[&str], SpeculationFlagRule) = match llama {
            Some(raw) => (
                raw,
                LlamacppCliStockTools::RESERVED_SPECULATIVE_DERIVED,
                is_speculation_flag,
            ),
            None => (
                self.raw(),
                SGLANG_SPECULATIVE_DERIVED,
                is_sglang_speculation_flag,
            ),
        };
        // The slot count is `parameter_batch_size`, on both conditions: the
        // benchmark id names the concurrency, so a cell may not restate it.
        let slots = LlamacppCliStockTools::RESERVED_SLOTS_DERIVED;
        if let Some(flag) = raw.iter().find(|t| deny_match(slots, t).is_some()) {
            return Err(RuntimeFlagsError::RawReservedFlag { flag: flag.clone() });
        }
        // Likewise on both conditions, on the engine that needs asking:
        // llama-server's `timings` always carry `cache_n`.
        if llama.is_none()
            && let Some(flag) = raw
                .iter()
                .find(|t| deny_match(SGLANG_CACHE_REPORT_DERIVED, t).is_some())
        {
            return Err(RuntimeFlagsError::RawReservedFlag { flag: flag.clone() });
        }
        if speculation {
            match raw.iter().find(|t| deny_match(derived, t).is_some()) {
                Some(flag) => Err(RuntimeFlagsError::RawReservedFlag { flag: flag.clone() }),
                None => Ok(()),
            }
        } else {
            // A speculation flag would turn the baseline into the thing it is
            // the baseline for.
            match raw.iter().find(|t| is_spec_flag(t)) {
                Some(flag) => Err(RuntimeFlagsError::RawSpeculativeFlagWithSpeculationOff {
                    flag: flag.clone(),
                }),
                None => Ok(()),
            }
        }
    }
}

impl RuntimeFlags {
    /// The two ASR cells, whose server the client derives per model family.
    fn is_asr_cell(&self) -> bool {
        matches!(
            self,
            RuntimeFlags::AsrThroughputLlamacppCliStockToolsGgufVision { .. }
                | RuntimeFlags::AsrMaxMemoryLlamacppCliStockToolsGgufVision { .. }
        )
    }

    /// Reject an env forward this cell owns: on an ASR cell, the `LLAMA_ARG_*`
    /// spellings of the reasoning and template knobs
    /// ([`LlamacppCliStockTools::RESERVED_ASR_ENV_PREFIXES`]), bare or with a
    /// value. On the vLLM vlm_eval cell, a name off its allowlist
    /// ([`VLLM_VLM_EVAL_ENV_ALLOWED`]). Every other cell's forwards are its
    /// own.
    pub(super) fn validate_envs(&self) -> Result<(), RuntimeFlagsError> {
        if let RuntimeFlags::VlmEvalUvVllmTorch { envs, .. }
        | RuntimeFlags::AsrEvalUvVllmTorch { envs, .. } = self
        {
            return match envs
                .iter()
                .find(|env| !VLLM_VLM_EVAL_ENV_ALLOWED.contains(&env.name()))
            {
                Some(env) => Err(RuntimeFlagsError::EnvNotAllowed {
                    name: env.name().to_owned(),
                    benchmark: self.axes().0,
                }),
                None => Ok(()),
            };
        }
        if !self.is_asr_cell() {
            return Ok(());
        }
        match self
            .envs()
            .iter()
            .find(|env| LlamacppCliStockTools::asr_refuses_env(env.name()))
        {
            Some(env) => Err(RuntimeFlagsError::ReservedEnvForward {
                name: env.name().to_owned(),
            }),
            None => Ok(()),
        }
    }
}

/// Reject any `tokens` entry naming a flag in `deny`, reporting it as reserved
/// when `reserved` holds it and as a typed-knob alias otherwise.
fn check_raw(tokens: &[String], deny: &[&str], reserved: &[&str]) -> Result<(), RuntimeFlagsError> {
    tokens.iter().try_for_each(|token| {
        let Some(denied) = deny_match(deny, token) else {
            return Ok(());
        };
        Err(if reserved.contains(&denied) {
            RuntimeFlagsError::RawReservedFlag {
                flag: token.clone(),
            }
        } else {
            RuntimeFlagsError::RawAliasesTypedKnob {
                flag: token.clone(),
            }
        })
    })
}

/// Reject any `tokens` flag `allowed` does not name: an exact name (in either
/// spelling of a long one, `=value` dropped), or a key inside an allowed JSON
/// flag (`--compilation-config.mode 3`, `+` to append to a list). A token that
/// is not a flag is a value, and passes. Nothing else does: no abbreviation,
/// no short form, no flag this list does not hold.
fn check_allowed(
    tokens: &[String],
    allowed: &[&str],
    benchmark: crate::BenchmarkType,
) -> Result<(), RuntimeFlagsError> {
    tokens.iter().try_for_each(|token| {
        if !token.starts_with('-') {
            return Ok(());
        }
        let name = flag_name(token);
        let base = name
            .split('.')
            .next()
            .unwrap_or(&name)
            .trim_end_matches('+');
        if allowed.contains(&base) {
            Ok(())
        } else {
            Err(RuntimeFlagsError::RawNotAllowed {
                flag: token.clone(),
                benchmark,
            })
        }
    })
}

/// The shape rule an engine uses to recognise a speculation flag in `raw`.
type SpeculationFlagRule = fn(&str) -> bool;

/// Whether a llama-server argv token turns on or tunes speculation: any
/// `--spec-*` or `--*draft*` long flag, or a draft short form.
///
/// Not a list: the measured fork adds its own speculation flags, and a list
/// would let each new one quietly make a baseline row speculative.
fn is_speculation_flag(token: &str) -> bool {
    const SHORT: &[&str] = &["-md", "-ngld", "-cd", "-devd", "-td", "-tbd"];
    let name = flag_name(token);
    match name.strip_prefix("--") {
        Some(long) => long.starts_with("spec-") || long.contains("draft"),
        None => SHORT.contains(&name.as_ref()),
    }
}

/// Whether an sglang argv token turns on or tunes speculation: the whole
/// `--speculative-*` family, which is how `launch_server` spells every one of
/// them. A shape rule for the same reason [`is_speculation_flag`] is.
fn is_sglang_speculation_flag(token: &str) -> bool {
    flag_name(token).starts_with("--speculative-")
}

/// The flag `token` names, as an engine reads it: the part before any `=`, and
/// in a long flag each `_` read as `-`, which llama.cpp and vLLM both do
/// (`--top_k` is `--top-k`), so an underscored spelling names the same flag.
fn flag_name(token: &str) -> std::borrow::Cow<'_, str> {
    let name = token.split('=').next().unwrap_or(token);
    if name.starts_with("--") && name.contains('_') {
        std::borrow::Cow::Owned(name.replace('_', "-"))
    } else {
        std::borrow::Cow::Borrowed(name)
    }
}

/// The denylisted flag `token` matches, if any: an exact name, in either
/// spelling of a long one (`--top_k` is `--top-k`, see [`flag_name`]), a
/// `--flag=value` (keyed on the pre-`=` name), or a short flag glued to a
/// number (`-t8`, `-ngl99`, `-c4096`).
pub(super) fn deny_match<'a>(deny: &[&'a str], token: &str) -> Option<&'a str> {
    let name = flag_name(token);
    if let Some(d) = deny.iter().copied().find(|&d| d == name) {
        return Some(d);
    }
    deny.iter().copied().find(|&d| {
        d.starts_with('-')
            && !d.starts_with("--")
            && token
                .strip_prefix(d)
                .and_then(|value| value.bytes().next())
                .is_some_and(|b| b.is_ascii_digit())
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use rstest::rstest;
    use strum::EnumCount as _;

    use super::*;
    use crate::RuntimeFlagRef;
    use crate::cell::runtime_flags::fixtures::{
        REGULAR_CELL, SGLANG_REGULAR_CELL, SGLANG_SPEC_CELL, SPEC_CELL, axes_wire, parse, refusal,
    };

    const DENY: &[&str] = &["-t", "--threads", "-ngl", "-n", "-c"];

    #[rstest]
    #[case::exact_long("--threads", Some("--threads"))]
    #[case::exact_short("-t", Some("-t"))]
    #[case::long_with_value("--threads=8", Some("--threads"))]
    #[case::short_with_value("-t=8", Some("-t"))]
    #[case::glued_short("-t8", Some("-t"))]
    #[case::glued_multi_letter("-ngl99", Some("-ngl"))]
    #[case::glued_context("-c4096", Some("-c"))]
    // `-n` prefixes `-ngl`: only a digit after the name glues a value to it.
    #[case::glued_prefix_of_another("-n128", Some("-n"))]
    #[case::long_underscored_names_no_other("--threads_x", None)]
    #[case::a_longer_long_name("--threadsX", None)]
    #[case::long_names_take_no_glued_value("--threads8", None)]
    #[case::short_followed_by_a_letter("-tx", None)]
    #[case::a_bare_value("8", None)]
    fn deny_match_forms(#[case] token: &str, #[case] expected: Option<&str>) {
        assert_eq!(deny_match(DENY, token), expected);
    }

    #[rstest]
    #[case::llama_spec_long("--spec-type", true, false)]
    #[case::llama_spec_with_value("--spec-draft-n-min=0", true, false)]
    #[case::llama_draft_anywhere("--gpu-layers-draft", true, false)]
    #[case::llama_draft_model("--model-draft", true, false)]
    #[case::llama_draft_short("-md", true, false)]
    #[case::llama_draft_threads_short("-td", true, false)]
    #[case::llama_draft_short_with_value("-ngld=99", true, false)]
    #[case::sglang_family("--speculative-algorithm", false, true)]
    #[case::sglang_family_with_value("--speculative-num-steps=3", false, true)]
    #[case::llama_spec_underscored("--spec_type", true, false)]
    #[case::llama_draft_underscored("--model_draft=/d.gguf", true, false)]
    #[case::sglang_family_underscored("--speculative_algorithm", false, true)]
    #[case::sglang_family_needs_its_dash("--speculative", false, false)]
    #[case::a_server_flag("--model", false, false)]
    #[case::a_short_flag_beside_a_draft_one("-t", false, false)]
    #[case::the_depth_is_not_a_draft("-d", false, false)]
    #[case::the_slot_count("--parallel", false, false)]
    #[case::a_bare_value("draft", false, false)]
    fn speculation_flag_shapes(#[case] token: &str, #[case] llama: bool, #[case] sglang: bool) {
        assert_eq!(is_speculation_flag(token), llama, "llama.cpp: {token}");
        assert_eq!(is_sglang_speculation_flag(token), sglang, "sglang: {token}");
    }

    /// With speculation off the shape is the only guard, so a derived flag
    /// without it would slip into a baseline run.
    #[test]
    fn every_derived_speculation_flag_has_its_engines_shape() {
        LlamacppCliStockTools::RESERVED_SPECULATIVE_DERIVED
            .iter()
            .for_each(|flag| assert!(is_speculation_flag(flag), "llama.cpp: {flag}"));
        SGLANG_SPECULATIVE_DERIVED
            .iter()
            .for_each(|flag| assert!(is_sglang_speculation_flag(flag), "sglang: {flag}"));
    }

    /// Checked against the llama.cpp prefill cell's lists. The error quotes
    /// the authored token, not the name it matched.
    #[rstest]
    #[case::alias(&["-t", "8"], Err(RuntimeFlagsError::RawAliasesTypedKnob { flag: "-t".to_owned() }))]
    #[case::reserved(
        &["--repetitions", "5"],
        Err(RuntimeFlagsError::RawReservedFlag { flag: "--repetitions".to_owned() })
    )]
    #[case::glued_alias(&["-ngl99"], Err(RuntimeFlagsError::RawAliasesTypedKnob { flag: "-ngl99".to_owned() }))]
    #[case::glued_reserved(&["-r5"], Err(RuntimeFlagsError::RawReservedFlag { flag: "-r5".to_owned() }))]
    #[case::the_tools_warm_up(
        &["--no-warmup"],
        Err(RuntimeFlagsError::RawReservedFlag { flag: "--no-warmup".to_owned() })
    )]
    #[case::the_first_offender_is_reported(
        &["--swa-full", "--mlock", "-r", "1"],
        Err(RuntimeFlagsError::RawAliasesTypedKnob { flag: "--mlock".to_owned() })
    )]
    #[case::batched_bench_reserved(
        &["-npl", "8"],
        Err(RuntimeFlagsError::RawReservedFlag { flag: "-npl".to_owned() })
    )]
    #[case::nothing_denied(&["--ubatch-size", "256"], Ok(()))]
    fn check_raw_splits_reserved_from_aliases(
        #[case] tokens: &[&str],
        #[case] expected: Result<(), RuntimeFlagsError>,
    ) {
        let tokens: Vec<String> = tokens.iter().map(|t| (*t).to_owned()).collect();
        assert_eq!(
            check_raw(
                &tokens,
                DENY_LLAMA_PREFILL,
                LlamacppCliStockTools::RESERVED_PREFILL
            ),
            expected
        );
    }

    /// One knob-less cell per `RuntimeFlags` variant, routed from its flat ref.
    fn every_cell() -> anyhow::Result<Vec<RuntimeFlags>> {
        let cells: Vec<RuntimeFlags> = crate::cell::every_triple()
            .filter_map(|(benchmark, runtime, model)| {
                RuntimeFlags::try_from(RuntimeFlagRef::new(benchmark, runtime, model)).ok()
            })
            .collect();
        anyhow::ensure!(
            cells.len() == RuntimeFlags::COUNT,
            "routed {} cells, the enum has {}",
            cells.len(),
            RuntimeFlags::COUNT
        );
        Ok(cells)
    }

    /// A reserved name outside the denylist would never be refused. A name
    /// denied twice would make the reported reason depend on list order.
    #[test]
    fn every_cells_reserved_set_is_within_its_denylist() -> anyhow::Result<()> {
        for cell in every_cell()? {
            let (deny, axes) = (cell.raw_denylist(), cell.axes());
            for flag in cell.tool_reserved() {
                anyhow::ensure!(
                    deny.contains(flag),
                    "{axes:?}: {flag} is reserved, not denied"
                );
            }
            let distinct: HashSet<&str> = deny.iter().copied().collect();
            anyhow::ensure!(distinct.len() == deny.len(), "{axes:?} denies a name twice");
        }
        Ok(())
    }

    #[rstest]
    #[case::prefill_adds_the_batched_bench(
        LlamacppCliStockTools::RESERVED_PREFILL,
        &[LlamacppCliStockTools::BENCH, LlamacppCliStockTools::BATCHED_BENCH]
    )]
    #[case::decode_is_the_prefill_set(LlamacppCliStockTools::RESERVED_DECODE, &[LlamacppCliStockTools::RESERVED_PREFILL])]
    #[case::max_memory_adds_the_context_size(
        LlamacppCliStockTools::RESERVED_MAX_MEMORY,
        &[LlamacppCliStockTools::BENCH, &["--ctx-size", "-c"]]
    )]
    #[case::vllm_timing_reserved(VLLM_TIMING_RESERVED, &[VLLM_RESERVED, VLLM_BATCH_DERIVED])]
    #[case::sglang_timing_reserved(SGLANG_TIMING_RESERVED, &[SGLANG_RESERVED, SGLANG_BATCH_DERIVED])]
    #[case::deny_llama_prefill(
        DENY_LLAMA_PREFILL,
        &[LLAMA_TYPED_ALIASES, LlamacppCliStockTools::RESERVED_PREFILL]
    )]
    #[case::deny_llama_decode(
        DENY_LLAMA_DECODE,
        &[LLAMA_TYPED_ALIASES, LlamacppCliStockTools::RESERVED_DECODE]
    )]
    #[case::deny_llama_max_memory(
        DENY_LLAMA_MAXMEM,
        &[LLAMA_TYPED_ALIASES, LlamacppCliStockTools::RESERVED_MAX_MEMORY]
    )]
    #[case::deny_llama_server(
        DENY_LLAMA_SERVER,
        &[LLAMA_TYPED_ALIASES, LlamacppCliStockTools::RESERVED_SERVER, LLAMA_SERVER_TYPED_ALIASES]
    )]
    #[case::asr_server_reserved(
        LlamacppCliStockTools::RESERVED_ASR_SERVER,
        &[
            LlamacppCliStockTools::RESERVED_SERVER,
            LlamacppCliStockTools::RESERVED_SLOTS_DERIVED,
            LlamacppCliStockTools::RESERVED_REASONING_DERIVED,
            LlamacppCliStockTools::RESERVED_TEMPLATE_DERIVED
        ]
    )]
    #[case::deny_llama_asr_server(
        DENY_LLAMA_ASR_SERVER,
        &[
            DENY_LLAMA_SERVER,
            LlamacppCliStockTools::RESERVED_SLOTS_DERIVED,
            ASR_REASONING_BEYOND_SERVER,
            LlamacppCliStockTools::RESERVED_TEMPLATE_DERIVED
        ]
    )]
    #[case::deny_llama_vlm_eval(
        DENY_LLAMA_VLM_EVAL,
        &[DENY_LLAMA_SERVER, LLAMA_VLM_EVAL_RECIPE_OWNED]
    )]
    #[case::reserved_llama_vlm_eval(
        RESERVED_LLAMA_VLM_EVAL,
        &[LlamacppCliStockTools::RESERVED_SERVER, LLAMA_VLM_EVAL_RECIPE_OWNED]
    )]
    #[case::deny_vllm(DENY_VLLM, &[VLLM_TYPED_ALIASES, VLLM_RESERVED])]
    #[case::deny_vllm_timing(DENY_VLLM_TIMING, &[VLLM_TYPED_ALIASES, VLLM_TIMING_RESERVED])]
    #[case::deny_sglang(DENY_SGLANG, &[SGLANG_TYPED_ALIASES, SGLANG_RESERVED])]
    #[case::deny_sglang_timing(DENY_SGLANG_TIMING, &[SGLANG_TYPED_ALIASES, SGLANG_TIMING_RESERVED])]
    #[case::deny_trt_edgellm(DENY_TRT_EDGELLM, &[TRT_EDGELLM_TYPED_ALIASES, TRT_EDGELLM_RESERVED])]
    fn reserved_lists_are_their_pieces(#[case] list: &[&str], #[case] pieces: &[&[&str]]) {
        assert_eq!(list, pieces.concat());
    }

    /// Whether speculation is on comes from the model and the flag together,
    /// never the model alone.
    #[rstest]
    #[case::spec_type_on_regular(REGULAR_CELL, "raw = [\"--spec-type\"]", "speculation is off")]
    #[case::draft_max_on_regular(REGULAR_CELL, "raw = [\"--draft-max\"]", "speculation is off")]
    #[case::short_drafter_on_regular(REGULAR_CELL, "raw = [\"-md\"]", "speculation is off")]
    #[case::spec_type_with_speculation_off(
        SPEC_CELL,
        "speculative_enabled = false\nraw = [\"--spec-type\"]",
        "speculation is off"
    )]
    #[case::draft_layers_with_speculation_off(
        SPEC_CELL,
        "speculative_enabled = false\nraw = [\"--gpu-layers-draft\"]",
        "speculation is off"
    )]
    #[case::derived_length(SPEC_CELL, "raw = [\"--spec-draft-n-max\"]", "is reserved")]
    #[case::derived_length_upstream(SPEC_CELL, "raw = [\"--draft-max=4\"]", "is reserved")]
    #[case::derived_drafter(SPEC_CELL, "raw = [\"-md\"]", "is reserved")]
    #[case::server_reserved(SPEC_CELL, "raw = [\"--model\"]", "is reserved")]
    // The slot count is `parameter_batch_size`, whichever condition the run is.
    #[case::derived_slots(SPEC_CELL, "raw = [\"--parallel\", \"4\"]", "is reserved")]
    #[case::derived_slots_glued(REGULAR_CELL, "raw = [\"-np4\"]", "is reserved")]
    #[case::derived_slots_with_speculation_off(
        SPEC_CELL,
        "speculative_enabled = false\nraw = [\"-np\", \"4\"]",
        "is reserved"
    )]
    // SGLang spells the whole family `--speculative-*`: any of it is refused
    // with speculation off, and the derived three with it on.
    #[case::sglang_algorithm_on_regular(
        SGLANG_REGULAR_CELL,
        "raw = [\"--speculative-algorithm\", \"DSPARK\"]",
        "speculation is off"
    )]
    #[case::sglang_backend_with_speculation_off(
        SGLANG_SPEC_CELL,
        "speculative_enabled = false\nraw = [\"--speculative-draft-attention-backend\", \"flashinfer\"]",
        "speculation is off"
    )]
    #[case::sglang_derived_drafter(
        SGLANG_SPEC_CELL,
        "raw = [\"--speculative-draft-model-path=/x\"]",
        "is reserved"
    )]
    #[case::sglang_derived_block(
        SGLANG_SPEC_CELL,
        "raw = [\"--speculative-dspark-block-size\", \"9\"]",
        "is reserved"
    )]
    #[case::sglang_model_path_reserved(SGLANG_SPEC_CELL, "raw = [\"--model-path\"]", "is reserved")]
    #[case::sglang_cache_report(
        SGLANG_SPEC_CELL,
        "raw = [\"--enable-cache-report\"]",
        "is reserved"
    )]
    #[case::sglang_cache_report_with_speculation_off(
        SGLANG_REGULAR_CELL,
        "raw = [\"--enable-cache-report\"]",
        "is reserved"
    )]
    fn raw_is_policed_by_whether_speculation_is_on(
        #[case] cell: &str,
        #[case] knob: &str,
        #[case] expected: &str,
    ) {
        let message = refusal(cell, knob);
        assert!(message.contains(expected), "{knob}: {message:?}");
    }

    /// The draft attention backend is the cell's own: not derived, so not
    /// reserved, and it round-trips.
    #[test]
    fn an_sglang_spec_cell_keeps_its_own_speculation_flags() -> anyhow::Result<()> {
        let f = parse(&format!(
            "{SGLANG_SPEC_CELL}raw = [\"--mem-fraction-static\", \"0.60\", \"--speculative-draft-attention-backend\", \"flashinfer\"]"
        ))?;
        assert!(matches!(
            f,
            RuntimeFlags::VlSpecDecodeUvSglangTorchSpeculative { .. }
        ));
        assert_eq!(parse(&toml::to_string(&f)?)?, f);
        assert_eq!(
            f.submission_value()["raw"],
            serde_json::json!([
                "--mem-fraction-static",
                "0.60",
                "--speculative-draft-attention-backend",
                "flashinfer"
            ])
        );
        Ok(())
    }

    /// Stored plans and results fan out an eval with a raw slot flag, so typing
    /// `parallel` must not make them refuse to deserialize.
    #[rstest]
    #[case::long_form("[\"--parallel\", \"8\"]")]
    #[case::short_form("[\"-np\", \"8\"]")]
    #[case::glued_short_form("[\"-np8\"]")]
    fn raw_parallel_still_parses_on_every_llama_server_cell(
        #[case] raw: &str,
    ) -> anyhow::Result<()> {
        let wire = |benchmark: &str, model: &str| {
            format!(
                "{}raw = {raw}\n",
                axes_wire(benchmark, "llamacpp_cli_stock_tools", model)
            )
        };
        let eval = parse(&wire("eval", "gguf_text"))?;
        anyhow::ensure!(
            eval.raw()
                .iter()
                .any(|token| token.contains("np") || token.contains("parallel")),
            "the raw slot flag must survive parsing verbatim: {:?}",
            eval.raw()
        );
        parse(&wire("end_to_end_latency", "gguf_text"))?;
        parse(&wire("vl_throughput", "gguf_vision"))?;
        Ok(())
    }

    /// An ASR cell's context is sized for one clip and its server runs with
    /// thinking off and the recipe's template, so a raw slot count, reasoning
    /// or template flag is refused on both ASR families, in every spelling.
    /// Other llama-server families keep a raw slot count (see
    /// `raw_parallel_still_parses_on_every_llama_server_cell`).
    #[rstest]
    fn an_asr_cell_refuses_raw_slots_and_reasoning(
        #[values("asr_throughput", "asr_max_memory")] benchmark: &str,
        #[values(
            "[\"--parallel\", \"2\"]",
            "[\"--parallel=2\"]",
            "[\"-np\", \"2\"]",
            "[\"-np2\"]",
            "[\"--reasoning\", \"on\"]",
            "[\"--reasoning=auto\"]",
            "[\"-rea\", \"on\"]",
            "[\"--reasoning-budget\", \"-1\"]",
            "[\"--reasoning-budget=-1\"]",
            "[\"--reasoning_budget=-1\"]",
            "[\"--reasoning-format\", \"none\"]",
            "[\"--reasoning-effort\", \"high\"]",
            "[\"--no-reasoning-preserve\"]",
            "[\"--no_reasoning_preserve\"]",
            "[\"--no_reasoning-format\"]",
            "[\"-rea=on\"]",
            "[\"--batch-size\", \"2048\", \"-rea\", \"on\"]",
            // The recipe's template, in every spelling llama.cpp reads.
            "[\"--jinja\"]",
            "[\"--no-jinja\"]",
            "[\"--no_jinja\"]",
            "[\"--chat-template\", \"chatml\"]",
            "[\"--chat-template=chatml\"]",
            "[\"--chat_template\", \"chatml\"]",
            "[\"--chat_template=chatml\"]",
            "[\"--chat-template-file\", \"custom.jinja\"]",
            "[\"--chat-template-file=custom.jinja\"]",
            "[\"--chat_template_file=t.jinja\"]",
            "[\"--chat-template_file\", \"t.jinja\"]",
            "[\"--chat-template-kwargs\", \"{}\"]",
            "[\"--chat-template-kwargs={}\"]",
            "[\"--chat_template_kwargs={}\"]",
            "[\"--batch-size\", \"2048\", \"--jinja\"]"
        )]
        raw: &str,
    ) {
        let wire = format!(
            "{}raw = {raw}\n",
            axes_wire(benchmark, "llamacpp_cli_stock_tools", "gguf_vision")
        );
        let error = parse(&wire)
            .err()
            .map(|e| format!("{e}"))
            .unwrap_or_else(|| format!("{raw} reached an {benchmark} raw unchallenged"));
        // Refused either as this cell's reservation or, for the reasoning
        // names every server cell types, as a typed knob's alias.
        assert!(
            error.contains("reserved by the benchmark/tool")
                || error.contains("aliases a typed knob"),
            "{benchmark} {raw}: {error}"
        );
    }

    /// The control: a raw knob the ASR cells do not own still passes, and the
    /// legacy llama-server families keep a raw template flag as they always
    /// had (a raw reasoning flag is refused on every server cell, as a typed
    /// knob's alias).
    #[rstest]
    #[case::asr_throughput_batch("asr_throughput", "gguf_vision", "[\"--batch-size\", \"2048\"]")]
    #[case::asr_max_memory_ubatch("asr_max_memory", "gguf_vision", "[\"--ubatch-size\", \"512\"]")]
    // A value is not a flag, even one spelled like a template's name.
    #[case::asr_template_named_value("asr_throughput", "gguf_vision", "[\"--alias\", \"jinja\"]")]
    #[case::vl_max_memory_parallel("vl_max_memory", "gguf_vision", "[\"--parallel\", \"2\"]")]
    #[case::vl_throughput_jinja("vl_throughput", "gguf_vision", "[\"--jinja\"]")]
    #[case::vl_max_memory_template_file(
        "vl_max_memory",
        "gguf_vision",
        "[\"--chat-template-file\", \"t.jinja\"]"
    )]
    #[case::vl_spec_decode_kwargs(
        "vl_spec_decode",
        "gguf_vision",
        "[\"--chat-template-kwargs\", \"{}\"]"
    )]
    #[case::eval_no_jinja("eval", "gguf_text", "[\"--no-jinja\"]")]
    #[case::e2e_chat_template("end_to_end_latency", "gguf_text", "[\"--chat_template=chatml\"]")]
    fn the_asr_refusal_is_the_asr_cells_alone(
        #[case] benchmark: &str,
        #[case] model: &str,
        #[case] raw: &str,
    ) -> anyhow::Result<()> {
        parse(&format!(
            "{}raw = {raw}\n",
            axes_wire(benchmark, "llamacpp_cli_stock_tools", model)
        ))?;
        Ok(())
    }

    /// The env spellings of the slot count and the reasoning and template
    /// knobs are refused on both ASR families, bare (inherited) or with a
    /// value.
    #[rstest]
    fn an_asr_cell_refuses_slot_reasoning_and_template_envs(
        #[values("asr_throughput", "asr_max_memory")] benchmark: &str,
        #[values(
            "LLAMA_ARG_N_PARALLEL=4",
            "LLAMA_ARG_N_PARALLEL",
            "LLAMA_ARG_REASONING",
            "LLAMA_ARG_REASONING=on",
            "LLAMA_ARG_REASONING_BUDGET=-1",
            "LLAMA_ARG_REASONING_EFFORT=high",
            "LLAMA_ARG_THINK",
            "LLAMA_ARG_THINK=1",
            "LLAMA_ARG_THINK_BUDGET=0",
            "LLAMA_ARG_JINJA",
            "LLAMA_ARG_JINJA=0",
            "LLAMA_ARG_CHAT_TEMPLATE=chatml",
            "LLAMA_ARG_CHAT_TEMPLATE_FILE",
            "LLAMA_ARG_CHAT_TEMPLATE_FILE=t.jinja",
            "LLAMA_ARG_CHAT_TEMPLATE_KWARGS={}",
            "LLAMA_CHAT_TEMPLATE_KWARGS={}",
            "llama_arg_chat_template=chatml"
        )]
        env: &str,
    ) {
        let wire = format!(
            "{}envs = [\"GGML_METAL_NO_RESIDENCY=1\", \"{env}\"]\n",
            axes_wire(benchmark, "llamacpp_cli_stock_tools", "gguf_vision"),
        );
        let name = env.split_once('=').map_or(env, |(name, _)| name);
        let error = parse(&wire)
            .err()
            .map(|e| format!("{e}"))
            .unwrap_or_else(|| format!("{env} reached an {benchmark} env unchallenged"));
        let expected = RuntimeFlagsError::ReservedEnvForward {
            name: name.to_owned(),
        }
        .to_string();
        assert!(error.contains(&expected), "{benchmark} {env}: {error}");
    }

    /// The control: an ASR cell keeps the env forwards it does not own (the
    /// `GGML_*` backend knobs, an unrelated `LLAMA_ARG_*`), and the other
    /// llama-server families keep the slot, reasoning and template ones.
    #[rstest]
    #[case::asr_throughput_backend("asr_throughput", "gguf_vision", "GGML_METAL_NO_RESIDENCY=1")]
    #[case::asr_max_memory_other_arg("asr_max_memory", "gguf_vision", "LLAMA_ARG_BATCH=2048")]
    #[case::vl_throughput_parallel("vl_throughput", "gguf_vision", "LLAMA_ARG_N_PARALLEL=4")]
    #[case::asr_prefix_must_lead("asr_throughput", "gguf_vision", "MY_LLAMA_ARG_JINJA=1")]
    #[case::vl_throughput_jinja("vl_throughput", "gguf_vision", "LLAMA_ARG_JINJA=0")]
    #[case::vl_max_memory_think("vl_max_memory", "gguf_vision", "LLAMA_ARG_THINK")]
    #[case::eval_template("eval", "gguf_text", "LLAMA_ARG_CHAT_TEMPLATE=chatml")]
    #[case::e2e_reasoning("end_to_end_latency", "gguf_text", "LLAMA_ARG_REASONING_BUDGET=0")]
    fn the_asr_env_refusal_is_the_asr_cells_alone(
        #[case] benchmark: &str,
        #[case] model: &str,
        #[case] env: &str,
    ) -> anyhow::Result<()> {
        let f = parse(&format!(
            "{}envs = [\"{env}\"]\n",
            axes_wire(benchmark, "llamacpp_cli_stock_tools", model)
        ))?;
        anyhow::ensure!(f.envs().len() == 1, "{benchmark}: {:?}", f.envs());
        Ok(())
    }

    /// The client reads `raw` through this, so its shape is the contract: a
    /// long name with `_` for `-`, the name before `=`.
    #[rstest]
    #[case::long("--chat-template", Some("--chat-template"))]
    #[case::long_underscores("--chat_template_file", Some("--chat-template-file"))]
    #[case::long_with_value("--chat_template_kwargs={\"a_b\":1}", Some("--chat-template-kwargs"))]
    #[case::short_keeps_its_spelling("-rea", Some("-rea"))]
    #[case::short_with_value("-rea=on", Some("-rea"))]
    #[case::a_value("chatml", None)]
    fn flag_name_reads_as_llama_cpp_does(#[case] token: &str, #[case] expected: Option<&str>) {
        assert_eq!(
            LlamacppCliStockTools::flag_name(token).as_deref(),
            expected,
            "{token}"
        );
    }

    #[rstest]
    // A typed-knob alias points the author at the field.
    #[case("prefill_throughput", "-t", "aliases a typed knob")]
    // A benchmark/tool-reserved flag can't be overridden at all.
    #[case(
        "prefill_throughput",
        "--repetitions",
        "reserved by the benchmark/tool"
    )]
    fn raw_rejection_distinguishes_alias_from_reserved(
        #[case] benchmark: &str,
        #[case] flag: &str,
        #[case] expected: &str,
    ) {
        let wire = format!(
            "runtime_type = \"llamacpp_cli_stock_tools\"\nmodel_type = \"gguf_text\"\nbenchmark_type = \"{benchmark}\"\nraw = [\"{flag}\"]"
        );
        let result = parse(&wire);
        assert!(result.is_err());
        if let Err(e) = result {
            assert!(format!("{e}").contains(expected), "got: {e}");
        }
    }

    /// Eval and max-memory keep the slot cap: eval's `parallel` is a
    /// client-side fan-out.
    #[rstest]
    #[case::vllm_docker_prefill("docker_vllm", "prefill_throughput", "--max-num-seqs", true)]
    #[case::vllm_uv_decode("uv_vllm", "decode_throughput", "--max-num-seqs=4", true)]
    #[case::vllm_docker_e2e("docker_vllm", "end_to_end_latency", "--max-num-seqs", true)]
    #[case::sglang_docker_prefill(
        "docker_sglang",
        "prefill_throughput",
        "--max-running-requests",
        true
    )]
    #[case::sglang_uv_decode("uv_sglang", "decode_throughput", "--max-running-requests=4", true)]
    #[case::sglang_uv_e2e("uv_sglang", "end_to_end_latency", "--max-running-requests", true)]
    #[case::vllm_eval_keeps_it("docker_vllm", "eval", "--max-num-seqs", false)]
    #[case::sglang_eval_keeps_it("uv_sglang", "eval", "--max-running-requests", false)]
    #[case::vllm_max_memory_keeps_it("uv_vllm", "max_memory_usage", "--max-num-seqs", false)]
    fn the_slot_cap_is_reserved_on_the_timing_cells(
        #[case] runtime: &str,
        #[case] benchmark: &str,
        #[case] flag: &str,
        #[case] reserved: bool,
    ) {
        let wire = format!(
            "runtime_type = \"{runtime}\"\nmodel_type = \"torch\"\nbenchmark_type = \"{benchmark}\"\nraw = [\"{flag}\", \"4\"]"
        );
        let result = parse(&wire);
        if reserved {
            let error = result
                .err()
                .map(|e| format!("{e}"))
                .unwrap_or_else(|| format!("`{flag}` reached raw unchallenged"));
            assert!(
                error.contains("reserved by the benchmark/tool"),
                "`{flag}` on {benchmark}: {error}"
            );
        } else {
            assert!(result.is_ok(), "`{flag}` on {benchmark}: {result:?}");
        }
    }

    /// A vlm_eval's sampling, length, template, thinking and image budget are
    /// its catalog entry's: none reaches the server through `raw`, in any
    /// spelling. The same flags stay open on a timing cell, which samples
    /// nothing.
    #[rstest]
    #[case::temperature("[\"--temp\", \"0.6\"]")]
    #[case::temperature_long("[\"--temperature=0.6\"]")]
    #[case::top_k("[\"--top-k\", \"20\"]")]
    #[case::sampler_order("[\"--samplers\", \"top_k;temperature\"]")]
    #[case::penalty_window("[\"--repeat-last-n\", \"64\"]")]
    #[case::dynamic_temperature("[\"--dynatemp-range\", \"0.5\"]")]
    #[case::ignore_eos("[\"--ignore-eos\"]")]
    #[case::seed_glued("[\"-s0\"]")]
    #[case::length_glued("[\"-n32\"]")]
    #[case::length_long("[\"--n-predict\", \"32\"]")]
    #[case::logit_bias_short("[\"-l\", \"15+1\"]")]
    #[case::grammar("[\"--grammar-file\", \"g.gbnf\"]")]
    #[case::template_file("[\"--chat-template-file\", \"t.jinja\"]")]
    #[case::template_kwargs("[\"--chat-template-kwargs\", \"{}\"]")]
    #[case::jinja_off("[\"--no-jinja\"]")]
    #[case::thinking_short("[\"-rea\", \"on\"]")]
    #[case::thinking_effort("[\"--reasoning-effort\", \"high\"]")]
    #[case::special_tokens("[\"-sp\"]")]
    #[case::image_budget("[\"--image-max-tokens\", \"280\"]")]
    #[case::preset("[\"--vision-gemma-4b-default\"]")]
    #[case::schema_file("[\"-jf\", \"s.json\"]")]
    #[case::reasoning_into_content("[\"--skip-chat-parsing\"]")]
    #[case::metadata_override("[\"--override-kv\", \"tokenizer.chat_template=str:x\"]")]
    #[case::reasoning_preserve("[\"--reasoning-preserve\"]")]
    #[case::another_projector("[\"-mm\", \"other.gguf\"]")]
    #[case::a_hub_model("[\"--hf_repo\", \"org/model\"]")]
    #[case::an_adapter("[\"--lora\", \"a.gguf\"]")]
    #[case::a_control_vector("[\"--control-vector-scaled\", \"v.gguf\", \"0.5\"]")]
    #[case::rope("[\"--rope-freq-base\", \"1000000\"]")]
    #[case::context_shift("[\"--context-shift\"]")]
    #[case::a_drafter("[\"--spec-type\", \"ngram\"]")]
    #[case::a_draft_model_short("[\"-md\", \"d.gguf\"]")]
    // llama.cpp reads `_` as `-` in a long flag: the same flags, spelled so.
    #[case::top_k_underscored("[\"--top_k\", \"40\"]")]
    #[case::penalty_window_underscored("[\"--repeat_last_n=64\"]")]
    #[case::template_file_underscored("[\"--chat_template_file\", \"t.jinja\"]")]
    #[case::image_budget_underscored("[\"--image_max_tokens\", \"280\"]")]
    fn a_vlm_eval_cells_recipe_is_closed_to_raw(#[case] raw: &str) -> anyhow::Result<()> {
        let wire = |benchmark: &str| {
            format!(
                "{}raw = {raw}\n",
                axes_wire(benchmark, "llamacpp_cli_stock_tools", "gguf_vision")
            )
        };
        let error = parse(&wire("vlm_eval"))
            .err()
            .map(|e| e.to_string())
            .unwrap_or_else(|| format!("{raw} reached a vlm_eval's raw unchallenged"));
        assert!(
            error.contains("reserved by the benchmark/tool"),
            "{raw}: {error}"
        );
        parse(&wire("vl_throughput"))?;
        Ok(())
    }

    /// An asr_eval cell's recipe is closed to `raw` as a vlm_eval's is, and its
    /// beam search, chat template and template kwargs, which the client sets
    /// per prompt profile, too; a timing cell keeps them open.
    #[rstest]
    #[case::beams("[\"--beam-search\", \"3\"]")]
    #[case::beams_underscored("[\"--beam_search=3\"]")]
    #[case::template_file("[\"--chat-template-file\", \"voxtral.jinja\"]")]
    #[case::template_kwargs("[\"--chat-template-kwargs\", \"{}\"]")]
    #[case::thinking("[\"--reasoning\", \"on\"]")]
    #[case::eot_override("[\"--override-kv\", \"tokenizer.ggml.eot_token_id=int:151704\"]")]
    #[case::temperature("[\"--temp\", \"0.6\"]")]
    #[case::length("[\"-n\", \"32\"]")]
    #[case::another_projector("[\"-mm\", \"other.gguf\"]")]
    #[case::a_drafter("[\"--spec-type\", \"ngram\"]")]
    #[case::slots("[\"-np\", \"2\"]")]
    #[case::slots_long("[\"--parallel=2\"]")]
    #[case::per_slot_limit("[\"--kv-unified-per-slot\", \"1024\"]")]
    fn an_asr_eval_cells_recipe_is_closed_to_raw(#[case] raw: &str) -> anyhow::Result<()> {
        let wire = |benchmark: &str| {
            format!(
                "{}raw = {raw}\n",
                axes_wire(benchmark, "llamacpp_cli_stock_tools", "gguf_vision")
            )
        };
        let error = parse(&wire("asr_eval"))
            .err()
            .map(|e| e.to_string())
            .unwrap_or_else(|| format!("{raw} reached an asr_eval's raw unchallenged"));
        // `--reasoning` is refused on every llama.cpp cell, as a typed knob's
        // spelling; the rest as the recipe's.
        assert!(
            error.contains("reserved by the benchmark/tool")
                || error.contains("aliases a typed knob"),
            "{raw}: {error}"
        );
        if raw != "[\"--reasoning\", \"on\"]" {
            parse(&wire("vl_throughput"))?;
        }
        Ok(())
    }

    /// The beam search is the asr_eval recipe's alone: a vlm_eval cell, run
    /// on a stock llama.cpp, does not reserve it.
    #[test]
    fn beam_search_is_reserved_on_the_asr_eval_cell_only() -> anyhow::Result<()> {
        parse(&format!(
            "{}raw = [\"--beam-search\", \"3\"]\n",
            axes_wire("vlm_eval", "llamacpp_cli_stock_tools", "gguf_vision")
        ))?;
        Ok(())
    }

    /// An asr_eval cell refuses llama.cpp option variables as a vlm_eval does,
    /// and names its own cell.
    #[test]
    fn an_asr_eval_cell_refuses_llama_option_envs() -> anyhow::Result<()> {
        let error = parse(&format!(
            "{}envs = [\"LLAMA_ARG_TOP_K=40\"]\n",
            axes_wire("asr_eval", "llamacpp_cli_stock_tools", "gguf_vision")
        ))
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
        assert!(
            error.contains("sets a llama.cpp option on a asr_eval cell"),
            "{error}"
        );
        parse(&format!(
            "{}envs = [\"GGML_CUDA_GRAPHS=0\"]\nraw = [\"-ub\", \"512\"]\n",
            axes_wire("asr_eval", "llamacpp_cli_stock_tools", "gguf_vision")
        ))?;
        Ok(())
    }

    /// The template kwargs' older variable reaches an asr_eval's server as
    /// the recipe's own kwargs do, bare or set, in any case. The refusal is
    /// asr_eval's alone: a vlm_eval cell is unchanged.
    #[rstest]
    #[case::set("LLAMA_CHAT_TEMPLATE_KWARGS={\\\"enable_thinking\\\":false}")]
    #[case::inherited("LLAMA_CHAT_TEMPLATE_KWARGS")]
    #[case::lowercase("llama_chat_template_kwargs={}")]
    fn an_asr_eval_cell_refuses_the_template_kwargs_env(#[case] env: &str) -> anyhow::Result<()> {
        let wire = |benchmark: &str| {
            format!(
                "{}envs = [\"{env}\"]\n",
                axes_wire(benchmark, "llamacpp_cli_stock_tools", "gguf_vision")
            )
        };
        let error = parse(&wire("asr_eval"))
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(
            error.contains("sets a llama.cpp option on a asr_eval cell"),
            "{env}: {error}"
        );
        parse(&wire("vlm_eval"))?;
        Ok(())
    }

    /// llama.cpp reads every option from a `LLAMA_ARG_<NAME>` variable as well,
    /// before its argv, so none reaches a vlm_eval's server, in any case; what
    /// is not an option variable (a backend's own) stays the cell's, and other
    /// cells keep theirs.
    #[rstest]
    #[case::sampler("LLAMA_ARG_TOP_K=40", true)]
    #[case::image_budget("LLAMA_ARG_IMAGE_MAX_TOKENS=32", true)]
    #[case::reply_parsing("llama_arg_skip_chat_parsing=1", true)]
    #[case::backend_knob("GGML_CUDA_GRAPHS=0", false)]
    fn a_vlm_eval_cell_refuses_llama_option_envs(
        #[case] env: &str,
        #[case] refused: bool,
    ) -> anyhow::Result<()> {
        let wire = |benchmark: &str| {
            format!(
                "{}envs = [\"{env}\"]\n",
                axes_wire(benchmark, "llamacpp_cli_stock_tools", "gguf_vision")
            )
        };
        let result = parse(&wire("vlm_eval"));
        if refused {
            let error = result.err().map(|e| e.to_string()).unwrap_or_default();
            assert!(
                error.contains("sets a llama.cpp option on a vlm_eval cell"),
                "{env}: {error}"
            );
        } else {
            result?;
        }
        parse(&wire("vl_throughput"))?;
        Ok(())
    }

    /// What only moves speed or memory stays the cell's to set.
    #[rstest]
    #[case::micro_batch("[\"-ub\", \"512\"]")]
    #[case::kv_cache_type("[\"--cache-type-k\", \"q8_0\"]")]
    #[case::projector_on_cpu("[\"--no-mmproj-offload\"]")]
    #[case::slots("[\"--parallel\", \"4\"]")]
    fn a_vlm_eval_cell_keeps_its_performance_flags(#[case] raw: &str) -> anyhow::Result<()> {
        parse(&format!(
            "{}raw = {raw}\n",
            axes_wire("vlm_eval", "llamacpp_cli_stock_tools", "gguf_vision")
        ))?;
        Ok(())
    }

    /// On vLLM a vlm_eval's sampling defaults, template, thinking default,
    /// image budget, tokenizer, seed, logits processors, adapters and drafters
    /// are its catalog entry's, in any spelling. The text eval cell keeps every
    /// one of them: it is not this recipe's.
    #[rstest]
    #[case::generation_config("[\"--generation-config\", \"auto\"]")]
    #[case::generation_config_glued("[\"--generation-config=auto\"]")]
    #[case::override_generation_config("[\"--override-generation-config\", \"{}\"]")]
    #[case::chat_template("[\"--chat-template\", \"t.jinja\"]")]
    #[case::chat_template_underscored("[\"--chat_template\", \"t.jinja\"]")]
    #[case::thinking_default("[\"--default-chat-template-kwargs\", \"{}\"]")]
    #[case::image_budget("[\"--mm-processor-kwargs\", \"{}\"]")]
    #[case::config_override("[\"--hf-overrides\", \"{}\"]")]
    #[case::tokenizer("[\"--tokenizer\", \"o/t\"]")]
    #[case::revision("[\"--revision\", \"main\"]")]
    #[case::seed("[\"--seed\", \"1\"]")]
    #[case::logits_processor("[\"--logits-processors\", \"m:P\"]")]
    #[case::adapter("[\"--enable-lora\"]")]
    #[case::drafter("[\"--speculative-config\", \"{}\"]")]
    #[case::config_file("[\"--config\", \"serve.yaml\"]")]
    #[case::quantization("[\"--quantization\", \"fp8\"]")]
    #[case::quantization_short("[\"-q\", \"fp8\"]")]
    #[case::load_format("[\"--load-format\", \"dummy\"]")]
    #[case::kv_cache_dtype("[\"--kv-cache-dtype\", \"fp8\"]")]
    #[case::sliding_window("[\"--disable-sliding-window\"]")]
    #[case::interleaved_prompt("[\"--interleave-mm-strings\"]")]
    #[case::request_kwargs("[\"--trust-request-mm-kwargs\"]")]
    #[case::download_dir("[\"--download-dir\", \"/seeded\"]")]
    #[case::implementation("[\"--model-impl\", \"transformers\"]")]
    #[case::reasoning_parser("[\"--reasoning-parser\", \"qwen3\"]")]
    #[case::structured_outputs("[\"--structured-outputs-config\", \"{}\"]")]
    fn a_vllm_vlm_eval_cells_recipe_is_closed_to_raw(
        #[values("vlm_eval", "asr_eval")] cell: &str,
        #[case] raw: &str,
    ) -> anyhow::Result<()> {
        let wire =
            |benchmark: &str| format!("{}raw = {raw}\n", axes_wire(benchmark, "uv_vllm", "torch"));
        let error = parse(&wire(cell))
            .err()
            .map(|e| e.to_string())
            .unwrap_or_else(|| format!("{raw} reached a {cell}'s raw unchallenged"));
        assert!(
            error.contains("reserved by the benchmark/tool"),
            "{raw}: {error}"
        );
        parse(&wire("eval"))?;
        Ok(())
    }

    /// What only moves speed or memory stays a vLLM vlm_eval cell's to set, as
    /// on the text eval cell; the typed knobs keep their typed route.
    #[rstest]
    #[case::trust_remote_code("[\"--trust-remote-code\"]")]
    #[case::memory("[\"--gpu-memory-utilization\", \"0.9\"]")]
    #[case::slots("[\"--max-num-seqs\", \"64\"]")]
    #[case::load_strategy("[\"--safetensors-load-strategy\", \"prefetch\"]")]
    #[case::compilation_key("[\"--compilation-config.cudagraph_mode\", \"NONE\"]")]
    #[case::underscored("[\"--gpu_memory_utilization=0.85\"]")]
    #[case::logging("[\"--disable-log-stats\"]")]
    #[case::eager("[\"--enforce-eager\"]")]
    #[case::batched_tokens("[\"--max-num-batched-tokens\", \"32768\"]")]
    #[case::processor_cache("[\"--mm-processor-cache-gb\", \"0\"]")]
    fn a_vllm_vlm_eval_cell_keeps_its_performance_flags(
        #[values("vlm_eval", "asr_eval")] cell: &str,
        #[case] raw: &str,
    ) -> anyhow::Result<()> {
        parse(&format!(
            "{}raw = {raw}\n",
            axes_wire(cell, "uv_vllm", "torch")
        ))?;
        Ok(())
    }

    /// vLLM sets a flag under more names than its own: none of them reaches a
    /// vlm_eval's recipe. The text eval cell's checks are unchanged.
    #[rstest]
    #[case::json_key("[\"--mm-processor-kwargs.max_soft_tokens\", \"280\"]")]
    #[case::json_key_glued("[\"--mm_processor_kwargs.max_soft_tokens=280\"]")]
    #[case::json_list("[\"--logits-processors+\", \"m:P\"]")]
    #[case::abbreviation("[\"--generation-conf\", \"auto\"]")]
    #[case::abbreviated_json_key("[\"--mm-processor-kw.max_soft_tokens\", \"280\"]")]
    #[case::negation("[\"--no-trust-request-mm-kwargs\"]")]
    #[case::short_glued("[\"-qfp8\"]")]
    #[case::typed_abbreviation("[\"--max-model\", \"4096\"]")]
    fn a_vllm_vlm_eval_cell_reads_flags_as_vllm_does(
        #[values("vlm_eval", "asr_eval")] cell: &str,
        #[case] raw: &str,
    ) -> anyhow::Result<()> {
        let error = parse(&format!(
            "{}raw = {raw}\n",
            axes_wire(cell, "uv_vllm", "torch")
        ))
        .err()
        .map(|e| e.to_string())
        .unwrap_or_else(|| format!("{raw} reached a {cell}'s raw unchallenged"));
        assert!(
            error.contains("reserved by the benchmark/tool")
                || error.contains("typed")
                || error.contains(&format!("not one a {cell} cell on vLLM may set")),
            "{raw}: {error}"
        );
        Ok(())
    }

    /// A flag the allowlist does not hold is refused, named or not in the
    /// reserved list: these change the model, its inputs or its sampling.
    /// The text eval cell takes each of them, as it did.
    #[rstest]
    #[case::vision_attention_dtype("[\"--mm-encoder-attn-dtype\", \"fp8\"]")]
    #[case::model_class("[\"--model-class-overrides\", \"{}\"]")]
    #[case::io_processor("[\"--io-processor-plugin\", \"p\"]")]
    #[case::gumbel("[\"--use-fp64-gumbel\"]")]
    #[case::short_optimization_level("[\"-O3\"]")]
    #[case::abbreviated_allowed("[\"--gpu-memory\", \"0.9\"]")]
    #[case::image_limit_with_sizes("[\"--limit-mm-per-prompt\", \"{}\"]")]
    fn a_vllm_vlm_eval_cell_takes_only_its_allowlist(
        #[values("vlm_eval", "asr_eval")] cell: &str,
        #[case] raw: &str,
    ) -> anyhow::Result<()> {
        let wire =
            |benchmark: &str| format!("{}raw = {raw}\n", axes_wire(benchmark, "uv_vllm", "torch"));
        let error = parse(&wire(cell))
            .err()
            .map(|e| e.to_string())
            .unwrap_or_else(|| format!("{raw} reached a {cell}'s raw unchallenged"));
        assert!(
            error.contains(&format!("not one a {cell} cell on vLLM may set")),
            "{raw}: {error}"
        );
        parse(&wire("eval"))?;
        Ok(())
    }

    #[rstest]
    #[case::length("[\"--max-model-len\", \"40960\"]")]
    #[case::prefix_cache("[\"--no-enable-prefix-caching\"]")]
    fn a_vllm_vlm_eval_cells_typed_knobs_are_closed_to_raw(
        #[values("vlm_eval", "asr_eval")] cell: &str,
        #[case] raw: &str,
    ) -> anyhow::Result<()> {
        let error = parse(&format!(
            "{}raw = {raw}\n",
            axes_wire(cell, "uv_vllm", "torch")
        ))
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
        assert!(error.contains("typed"), "{raw}: {error}");
        Ok(())
    }

    /// A vLLM vlm_eval cell forwards only the environment on its allowlist;
    /// the text eval cell forwards anything, as it did.
    #[rstest]
    #[case::devices("ROCR_VISIBLE_DEVICES=0", true)]
    #[case::inherited_devices("HIP_VISIBLE_DEVICES", true)]
    #[case::collectives("NCCL_DEBUG=INFO", true)]
    #[case::logging("VLLM_LOGGING_LEVEL=DEBUG", true)]
    #[case::redirect("VLLM_MODEL_REDIRECT_PATH=/w", false)]
    #[case::plugins("VLLM_PLUGINS=x", false)]
    #[case::kernel("VLLM_ROCM_USE_AITER=1", false)]
    #[case::collective_plugin("NCCL_NET_PLUGIN=x", false)]
    #[case::collective_env_plugin("NCCL_ENV_PLUGIN=x", false)]
    #[case::collective_log_file("NCCL_DEBUG_FILE=/tmp/x", false)]
    #[case::rccl("RCCL_MSCCL_ENABLE=1", false)]
    #[case::token("HF_TOKEN=x", false)]
    fn a_vllm_vlm_eval_cell_forwards_only_its_allowlist(
        #[values("vlm_eval", "asr_eval")] cell: &str,
        #[case] env: &str,
        #[case] allowed: bool,
    ) -> anyhow::Result<()> {
        let wire = |benchmark: &str| {
            format!(
                "{}envs = [\"{env}\"]\n",
                axes_wire(benchmark, "uv_vllm", "torch")
            )
        };
        let parsed = parse(&wire(cell));
        if allowed {
            parsed?;
        } else {
            let error = parsed.err().map(|e| e.to_string()).unwrap_or_default();
            assert!(
                error.contains(&format!("not one a {cell} cell on vLLM may set")),
                "{env}: {error}"
            );
        }
        parse(&wire("eval"))?;
        Ok(())
    }

    /// An open spelling would let the argv carry a load mode the recorded
    /// `runtime_flags` never mentions.
    #[rstest]
    #[case::load_mode_short("-lm")]
    #[case::load_mode_long("--load-mode")]
    #[case::mmap_short("-mmp")]
    #[case::mmap_long("--mmap")]
    #[case::no_mmap("--no-mmap")]
    #[case::mlock("--mlock")]
    #[case::direct_io_short("-dio")]
    #[case::direct_io_long("--direct-io")]
    #[case::no_direct_io_short("-ndio")]
    #[case::no_direct_io_long("--no-direct-io")]
    fn every_load_flag_spelling_is_closed_to_raw(#[case] flag: &str) {
        let wire = format!(
            "runtime_type = \"llamacpp_cli_stock_tools\"\nmodel_type = \"gguf_text\"\nbenchmark_type = \"prefill_throughput\"\nraw = [\"{flag}\"]"
        );
        let error = parse(&wire)
            .err()
            .map(|e| format!("{e}"))
            .unwrap_or_else(|| format!("`{flag}` reached raw unchallenged"));
        assert!(
            error.contains("aliases a typed knob"),
            "`{flag}` should point the author at the typed knob, got: {error}"
        );
    }

    /// A batch runs on `llama-batched-bench`, which sizes the context to the
    /// batch and reads the workload off its own flags, so neither prefill nor
    /// decode takes them in `raw`.
    #[rstest]
    #[case::prefill_context("prefill_throughput", "--ctx-size")]
    #[case::prefill_context_short("prefill_throughput", "-c")]
    #[case::prefill_batch("prefill_throughput", "-npl")]
    #[case::prefill_shared_prompt("prefill_throughput", "-pps")]
    #[case::decode_generation("decode_throughput", "-ntg")]
    #[case::decode_one_sequence_at_a_time("decode_throughput", "-tgs")]
    #[case::decode_output_format("decode_throughput", "--output-format")]
    fn the_batched_bench_flags_are_reserved(#[case] benchmark: &str, #[case] flag: &str) {
        let wire = format!(
            "runtime_type = \"llamacpp_cli_stock_tools\"\nmodel_type = \"gguf_text\"\nbenchmark_type = \"{benchmark}\"\nraw = [\"{flag}\"]"
        );
        let error = parse(&wire)
            .err()
            .map(|e| format!("{e}"))
            .unwrap_or_else(|| format!("`{flag}` reached raw unchallenged"));
        assert!(
            error.contains("reserved by the benchmark/tool"),
            "`{flag}` on {benchmark}: {error}"
        );
    }

    /// The client renders these flags from the typed knobs; a raw copy would
    /// collide.
    #[rstest]
    #[case::max_model_len(r#"raw = ["--max-model-len", "4096"]"#)]
    #[case::max_model_len_glued(r#"raw = ["--max-model-len=4096"]"#)]
    #[case::prefix_cache_on(r#"raw = ["--enable-prefix-caching"]"#)]
    #[case::prefix_cache_off(r#"raw = ["--no-enable-prefix-caching"]"#)]
    fn vllm_raw_cannot_shadow_the_typed_server_knobs(#[case] raw: &str) {
        let result = parse(&format!(
            "runtime_type = \"docker_vllm\"\nmodel_type = \"torch\"\nbenchmark_type = \"eval\"\n{raw}"
        ));
        assert!(result.is_err());
        if let Err(e) = result {
            assert!(format!("{e}").contains("aliases a typed knob"), "got: {e}");
        }
    }

    /// The harness owns the GGUF pair, the address and the engine cache, and
    /// `precision` is typed; the robo settings are the server's own and pass.
    #[rstest]
    #[case::model("vl_throughput", r#"["--model", "/m.gguf"]"#, "reserved")]
    #[case::model_short_glued("vl_throughput", r#"["-m=/m.gguf"]"#, "reserved")]
    #[case::mmproj("vl_max_memory", r#"["--mmproj", "/mm.gguf"]"#, "reserved")]
    #[case::port("vl_throughput", r#"["--port=8080"]"#, "reserved")]
    #[case::cache_dir("vl_max_memory", r#"["--cache_dir", "/tmp/e"]"#, "reserved")]
    #[case::precision("vl_throughput", r#"["--precision=bf16"]"#, "aliases a typed knob")]
    #[case::robo(
        "vl_throughput",
        r#"["--robo-state", "0.1,0.2", "--robo-embodiment", "0"]"#,
        ""
    )]
    fn trt_edgellm_raw_keeps_off_what_the_harness_owns(
        #[case] benchmark: &str,
        #[case] raw: &str,
        #[case] refusal: &str,
    ) {
        let wire = format!(
            "{}raw = {raw}\n",
            axes_wire(benchmark, "trt_edgellm", "gguf_vision")
        );
        let error = parse(&wire)
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        if refusal.is_empty() {
            assert!(error.is_empty(), "{raw} on {benchmark}: {error}");
        } else {
            assert!(error.contains(refusal), "{raw} on {benchmark}: {error:?}");
        }
    }
}
