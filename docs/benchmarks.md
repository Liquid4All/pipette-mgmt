# Benchmarks

Each benchmark is a single entry in the catalog identified by a `benchmark_id`
that encodes the type and its parameters (e.g. `prefill_throughput_256`,
`decode_throughput_128_256`, `eval_ifstruct_release_v1_0`,
`vl_throughput_384x512_32_128`). Benchmarks are defined as TOML files in
`{data_dir}/benchmarks/{benchmark_id}.toml` — one file per entry, loaded at
startup. Clients discover benchmarks via the API and submit their measured
values.

The benchmark catalog is public — anyone can browse it without
authentication. Submitting results requires an approved client. See
[authentication.md §4](authentication.md#4-access-matrix) for the access rules.

See [`examples/benchmarks/`](../examples/benchmarks/) for ready-to-use TOML files.

## 1. Overview

| Type | Model type | What it measures |
|------|-----------|------------------|
| [Prefill Throughput](#21-prefill-throughput) | Text | How fast the model processes the input context (time to first token) |
| [Decode Throughput](#22-decode-throughput) | Text | How fast the model generates output tokens after prefill |
| [End-to-End Latency](#23-end-to-end-latency) | Text | Total time from prompt submission to last generated token |
| [Peak Memory Usage](#24-peak-memory-usage) | Text | Peak host, GPU, and NPU memory consumed during inference |
| [Eval (Accuracy)](#25-eval-accuracy) | Text | Model accuracy on a question-answering benchmark |
| [VL Throughput](#26-vl-throughput) | Vision-Language | Prefill, decode, and end-to-end timing for image + text input |
| [VL Peak Memory](#27-vl-peak-memory) | Vision-Language | Peak host + GPU memory for an image + text workload |

**Text** benchmarks (2.1–2.4) use synthetic text-only prompts.
**VL** benchmarks (2.6–2.7) use a synthetic image alongside a text prompt and
require a model with a vision encoder.
**Eval** (2.5) applies to any model type — the server provides the prompts.

### Common rules

These apply to all performance benchmarks (2.1–2.4, 2.6) unless stated
otherwise:

- **Greedy decoding**: always use `temperature = 0.0` for reproducible results.
- **Synthetic inputs**: prompt content and image content do not affect timing.
  Use random tokens or gradient images — only the dimensions matter.
- **Token counting**: when a benchmark specifies "exactly N tokens", N is the
  number of tokens in the user-provided prompt text as counted by the model's
  tokenizer. Do not count BOS, EOS, or other special tokens that the runtime
  adds automatically.
- **Power profile**: on devices with configurable power modes (e.g. Jetson
  `nvpmodel`), set the device to the target power profile before benchmarking
  and document it in `device_name`.
- **EOS handling**: for benchmarks that specify a fixed number of output tokens,
  disable stop tokens (ignore EOS) to ensure the model generates exactly the
  requested count. If the runtime does not support disabling EOS, report the
  actual number of tokens generated — the server will use the reported count
  for metric derivation.
- **Warm-up**: run one full inference and discard the result before measuring.
- **Repetitions**: run the measurement multiple times (5 recommended). Submit
  the **mean** of each timing value. Report the standard deviation of each
  timing field alongside it using the per-field stddev name (see per-type
  submission fields below).
- **Timing**: use runtime-internal timing when available (e.g. `llama-bench`
  built-in measurement, or the `timings` object from `llama-server` responses).
  Only fall back to wall-clock timing when the measurement directly wraps the
  computation with no I/O or HTTP layer in between.
- **Environment**: run benchmarks in a stable thermal state with no competing
  workloads.

### Common submission fields

Every submission includes these fields alongside benchmark-specific data:

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `benchmark_id` | string | yes | Benchmark from the catalog |
| `device_name` | string | yes | Manufacturer product name of the device, including memory/storage SKU when it distinguishes variants (e.g. `iPhone 16 Pro`, `MacBook Pro 16" M3 Max`, `Jetson AGX Orin 64GB`). Do not include software versions or runtime info. |
| `device_form_factor` | string | yes | One of: `phone`, `tablet`, `laptop`, `desktop`, `server`, `embedded` |
| `device_os_name` | string | yes | Operating system (e.g. `iOS`, `iPadOS`, `Android`, `macOS`, `Windows`, `Linux`) |
| `device_os_version` | string | yes | OS version string (e.g. `22.04`, `18.2`) |
| `device_os_build` | string | no | Precise OS build string, finer-grained than `device_os_version` (e.g. iOS `22F76`, macOS `24F74`, Windows `26100.1234`, Android `AP3A.240905.015.A2`, Linux full `uname -r`). Null when unavailable. |
| `device_os_security_patch` | string | no | OS security-patch level where the platform exposes one (currently Android, e.g. `2025-06-01`). Null elsewhere. |
| `device_chip_model` | string | yes | Primary chip — SoC or CPU (e.g. `Apple M4`, `Snapdragon 8 Elite`). On mobile and Apple Silicon this is the SoC that includes the integrated GPU and NPU. On x86 systems this is the CPU model. |
| `device_gpu_model` | string | no | Discrete GPU, not inside the chip (e.g. `NVIDIA RTX 4090`). Null on phones, tablets, and Apple Silicon where the GPU is integrated in the SoC. |
| `device_gpu_vram_bytes` | i64 | no | Discrete GPU VRAM in bytes. Only when `device_gpu_model` is present. |
| `device_npu_model` | string | no | Discrete/external NPU, not inside the chip (e.g. `Hailo-8 26T`, `AWS Inferentia2`). Null when the NPU is integrated in the SoC. |
| `device_npu_vram_bytes` | i64 | no | NPU device memory in bytes. Only when `device_npu_model` is present and the NPU has significant device memory. |
| `device_ram_bytes` | i64 | yes | Total system RAM in bytes |
| `device_battery_level` | i32 | no | Battery charge percent (0–100) at run time. Rejected if outside 0–100. Null when unavailable. |
| `device_power_state` | string (enum) | no | Run-environment power state: `charging` (on external power, battery rising), `not_charging` (on battery, discharging), or `plugged_in_not_charging` (on external power but not adding charge — full or charge-limited). Null when unavailable. |
| `device_power_save_mode` | bool | no | Whether OS low-power / battery-saver mode was active (can cap CPU clocks — distinct from thermal throttling). |
| `device_apple_thermal_state_{before,after}` | list (enum) | no | Apple `ProcessInfo.thermalState`, one per repetition (Apple devices only). See [Thermal telemetry](methodology/thermal-telemetry.md). |
| `device_apple_soc_temp_c_{before,after}` | list&lt;f32&gt; | no | Raw iOS SoC die temperature (fractional °C), one per repetition — iOS-only, gated on the `PIPETTE_PRIVATE_THERMAL` client build. Whole array null when unavailable. See [Thermal telemetry](methodology/thermal-telemetry.md). |
| `device_android_thermal_status_{before,after}` | list (enum) | no | Android `getCurrentThermalStatus()`, one per repetition (Android only). See [Thermal telemetry](methodology/thermal-telemetry.md). |
| `device_android_thermal_headroom_{before,after}` | list&lt;f32&gt; | no | Android `getThermalHeadroom()` per repetition — fraction of thermal envelope in use (higher = worse). See [Thermal telemetry](methodology/thermal-telemetry.md). |
| `device_android_thermal_sensors_{before,after}` | array | no | Android thermal-HAL per-sensor readings, flattened across reps and `iteration`-tagged (privileged). See [Thermal telemetry](methodology/thermal-telemetry.md). |
| `device_linux_thermal_zones_{before,after}` | array | no | Linux `/sys/class/thermal` per-zone readings, flattened across reps and `iteration`-tagged. See [Thermal telemetry](methodology/thermal-telemetry.md). |
| `device_android_cpuset` | string | no | The cpuset cgroup the benchmark process ran under (`/top-app`, `/foreground`, `/moderate`, …). Single-valued per submission (Android only). Surfaces OEM scheduling demotion. |
| `device_android_cpu_affinity_list` | string | no | The process's allowed CPU list (Linux CPU-list syntax, e.g. `0-5`, `0-3,6-7`). Single-valued per submission (Android only). |
| `device_android_cpu_affinity_excludes_top_tier` | bool | no | True when the highest-frequency core tier is absent from the allowed set — the OEM-demotion signal (e.g. a non-top-app service process barred from the prime cores). |
| `model_name` | string | no | Human-facing model identity / grouping key (e.g. `llama-3.2-1b`). Optional — a submission may carry only `model_descriptor`, or both. |
| `model_quant` | string | no | Convenience quantization label for the primary artifact (e.g. `q4_0`). Optional and lossy for multi-artifact models; the authoritative per-piece quant lives in `model_descriptor`. |
| `model_params_total_millions` | i32 | no | Approximate total parameter count in millions (drives RAM/VRAM footprint). Optional; the scorer prefers the catalog value when the model is recognized. See [storage.md § Model catalog](storage.md#model-catalog) for the resolution rules and the catalog format. |
| `model_params_active_millions` | i32 | no | Approximate active parameter count in millions (drives prefill/decode throughput). Optional; populate for MoE / selective-activation architectures. When present, must be `> 0` and `<= model_params_total_millions`. Same resolution as `_total`. |
| `model_descriptor` | string (JSON) | no | Full, lossless model specification as a JSON **string**. Handles single-artifact (MLX) and multi-artifact (llama.cpp VL backbone + projector; audio backbone + encoder + vocoder + tokenizer) models alike. Opaque to the server — its schema is never interpreted; the value is stored as-is except that object keys are sorted and whitespace stripped so pattern search stays stable. Recommended shape: the serialized `pipette-plan-types` `Model` enum variant — see [storage.md § model_descriptor / runtime_descriptor](storage.md#model_descriptor--runtime_descriptor) for the per-variant examples. |
| `model_flags` | string | no | Opaque configuration affecting model behavior. Typically a JSON string going forward (e.g. `{"enable_thinking":true}`), but a plain string is equally valid — never validated or interpreted. Normalized like `model_descriptor` when it parses as JSON (keys sorted, whitespace stripped), stored trimmed-but-verbatim when it doesn't, with a `model_flags_sha256` alongside. |
| `runtime_name` | string | no | Optional, opaque grouping/display label (e.g. `llama.cpp`). The full runtime identity lives in `runtime_descriptor`. |
| `runtime_version` | string | no | Optional, opaque grouping/display label (e.g. `b5000`), stored verbatim. The server never derives, backfills, requires, or interprets it; the full runtime spec (version included) lives in `runtime_descriptor`. |
| `runtime_descriptor` | string (JSON) | no | Full, lossless runtime specification as a JSON **string**, with the version/build baked in. Opaque to the server and normalized like `model_descriptor`; `runtime_name`/`runtime_version` stay as the cheap grouping fields. Recommended shape: the serialized `pipette-plan-types` `Runtime` enum variant — see [storage.md § model_descriptor / runtime_descriptor](storage.md#model_descriptor--runtime_descriptor). |
| `runtime_flags` | string | no | Opaque configuration affecting the runtime itself. Typically a JSON string going forward, but a plain string (e.g. `--n-gpu-layers 999`) is equally valid — never validated or interpreted. Normalized exactly like `model_flags`, with a `runtime_flags_sha256` alongside. |
| `runtime_cpu_variant` | string | no | Runtime-selected CPU kernel variant, interpreted per `runtime_name`. For llama.cpp/ggml: the `ggml-cpu-<tag>` backend variant chosen at load time by feature-dispatch scoring (e.g. `armv8.2_1`, `android_armv8.6_1`, `apple_m2_m3`). Lets result analysis detect when the kernel variant changed. Null when the build ships a single static CPU backend (no runtime dispatch). |

#### Thermal telemetry

Optional, per-platform thermal telemetry — Apple state; Android status +
headroom; Android thermal-HAL per-sensor array; Linux sysfs zones — captured
`before` / `after` / `worst` around the timed region as a **reported
condition**. Full schema, per-platform collection, enums, per-platform examples,
and caveats: **[Thermal telemetry](methodology/thermal-telemetry.md)**.

#### Per-form-factor device field examples

Every `device_*` column shown for each device. `—` means null/absent.

| `device_form_factor` | `device_name` | `device_os_name` | `device_os_version` | `device_chip_model` | `device_gpu_model` | `device_gpu_vram_bytes` | `device_npu_model` | `device_npu_vram_bytes` | `device_ram_bytes` |
|---|---|---|---|---|---|---|---|---|---|
| `phone` | iPhone 16 Pro | iOS | 18.2 | Apple A18 Pro | — | — | — | — | 8589934592 |
| `phone` | Galaxy S25 Ultra | Android | 15 | Snapdragon 8 Elite | — | — | — | — | 12884901888 |
| `tablet` | iPad Pro M4 13" | iPadOS | 18.2 | Apple M4 | — | — | — | — | 17179869184 |
| `laptop` | MacBook Pro 16" M3 Max | macOS | 15.4 | Apple M3 Max | — | — | — | — | 38654705664 |
| `laptop` | Razer Blade 16 | Windows | 11 | Intel i9-14900HX | NVIDIA RTX 4090 Laptop | 17179869184 | — | — | 34359738368 |
| `desktop` | Mac Studio M2 Ultra | macOS | 15.4 | Apple M2 Ultra | — | — | — | — | 206158430208 |
| `desktop` | Custom PC | Linux | Ubuntu 24.04 | AMD Ryzen 9 7950X | NVIDIA RTX 4090 | 25769803776 | — | — | 68719476736 |
| `server` | Dell R750xa | Linux | Ubuntu 22.04 | Intel Xeon w5-3435X | NVIDIA A100 80GB | 85899345920 | — | — | 549755813888 |
| `server` | AWS inf2.xlarge | Linux | Amazon Linux 2023 | Intel Xeon Platinum | — | — | AWS Inferentia2 | 34359738368 | 34359738368 |
| `embedded` | Jetson AGX Orin 64GB | Linux | Ubuntu 22.04 | Jetson AGX Orin | — | — | — | — | 68719476736 |
| `embedded` | Raspberry Pi 5 8GB | Linux | Debian 12 | BCM2712 | — | — | Hailo-8 26T | — | 8589934592 |

**Key rules**: `device_chip_model` is the SoC on mobile/Apple Silicon (includes
integrated GPU and NPU). `device_gpu_model` and `device_npu_model` are only for
discrete accelerators separate from the primary chip. Unified-memory devices
(phones, tablets, Apple Silicon) leave GPU/NPU columns null. Hailo-8 has no
reportable device memory, so `device_npu_vram_bytes` is null even though
`device_npu_model` is set.

`job_id`, `client_id`, `benchmark_type`, and `submitted_at` are injected by the
server — do not send them.

---

## 2. Benchmark Types

### 2.1. Prefill Throughput

Measures how fast the model processes the input context. The reported
`prefill_time_ms` is also the time to first token (TTFT).

#### Parameters

| Field | Type | Description |
|-------|------|-------------|
| `benchmark_type` | string | `prefill_throughput` |
| `parameter_prefill_tokens` | integer | Number of tokens to prefill |

#### Execution

1. Load the model with the target quantization.
2. Construct a prompt of exactly `parameter_prefill_tokens` tokens.
3. Warm up (one prefill + one output token, discard).
4. Prefill the prompt and generate exactly **one** output token. Time this step
   only — from the start of the prefill to the first token produced.
5. Record the elapsed time as `prefill_time_ms`. Repeat and average (see
   [Common rules](#common-rules)).

**Includes**: model forward pass, KV-cache allocation.
**Excludes**: model loading, tokenization, I/O.

#### Submission fields

| Field | Type | Description |
|-------|------|-------------|
| `prefill_time_ms` | f32 | Time to prefill in milliseconds |
| `prefill_time_ms_stddev` | f32 (optional) | Standard deviation of `prefill_time_ms` across repetitions. Optional |

#### Derived metrics

| Metric | Formula | Unit | `value_stddev` |
|--------|---------|------|----------------|
| `ttft` | `prefill_time_ms` | ms | `prefill_time_ms_stddev` |
| `prefill_throughput` | `parameter_prefill_tokens / prefill_time_ms * 1000` | tokens/sec | `prefill_throughput * prefill_time_ms_stddev / prefill_time_ms` |

---

### 2.2. Decode Throughput

Measures how fast the model generates output tokens in isolation. The prompt is
prefilled first to put the KV-cache into a realistic state, but only the decode
phase is timed.

#### Parameters

| Field | Type | Description |
|-------|------|-------------|
| `benchmark_type` | string | `decode_throughput` |
| `parameter_prefill_tokens` | integer | Number of tokens to prefill (sets KV-cache size) |
| `parameter_decode_tokens` | integer | Number of tokens to generate |

#### Execution

1. Load the model with the target quantization.
2. Construct a prompt of exactly `parameter_prefill_tokens` tokens.
3. Warm up (full prefill + decode cycle, discard).
4. Prefill the prompt (do **not** time this step).
5. Generate exactly `parameter_decode_tokens` tokens. Time this step only —
   from the first decode token to the last.
6. Record the elapsed time as `decode_time_ms`. Repeat and average (see
   [Common rules](#common-rules)).

**Includes**: all decode forward passes.
**Excludes**: prefill, model loading, tokenization, I/O.

#### Submission fields

| Field | Type | Description |
|-------|------|-------------|
| `decode_time_ms` | f32 | Time to decode in milliseconds |
| `decode_time_ms_stddev` | f32 (optional) | Standard deviation of `decode_time_ms` across repetitions. Optional |

#### Derived metrics

| Metric | Formula | Unit | `value_stddev` |
|--------|---------|------|----------------|
| `decode_throughput` | `parameter_decode_tokens / decode_time_ms * 1000` | tokens/sec | `decode_throughput * decode_time_ms_stddev / decode_time_ms` |

---

### 2.3. End-to-End Latency

Measures the total wall-clock time from submitting a prompt to receiving the
last generated token. This captures the full user-facing latency including
prefill, all decode steps, and any runtime overhead.

#### Parameters

| Field | Type | Description |
|-------|------|-------------|
| `benchmark_type` | string | `end_to_end_latency` |
| `parameter_prefill_tokens` | integer | Number of tokens to prefill |
| `parameter_decode_tokens` | integer | Number of tokens to generate |

#### Execution

1. Load the model with the target quantization.
2. Construct a prompt of exactly `parameter_prefill_tokens` tokens.
3. Warm up (full prefill + decode cycle, discard).
4. Prefill the prompt and generate exactly `parameter_decode_tokens` tokens.
   Time the entire operation — from prompt submission to last token.
5. Record the elapsed time as `total_time_ms`. Repeat and average (see
   [Common rules](#common-rules)).

**Includes**: prefill, all decode forward passes, KV-cache management, runtime
scheduling overhead.
**Excludes**: model loading, tokenization, detokenization.

#### Submission fields

| Field | Type | Description |
|-------|------|-------------|
| `total_time_ms` | f32 | Total wall-clock time in milliseconds |
| `total_time_ms_stddev` | f32 (optional) | Standard deviation of `total_time_ms` across repetitions. Optional |

#### Derived metrics

| Metric | Formula | Unit | `value_stddev` |
|--------|---------|------|----------------|
| `end_to_end_latency` | `total_time_ms` | ms | `total_time_ms_stddev` |

---

### 2.4. Peak Memory Usage

Measures peak memory consumption while prefilling a prompt of a given length,
split by compute path (host CPU vs GPU vs NPU). Each bucket is the
**independently-sampled peak of a distinct counter** — there is no
cross-subtraction and the buckets do not partition the process footprint.
Memory usage is deterministic for a given context length and model, so no
warm-up or repetitions are needed.

On unified-memory devices (Apple Silicon, modern integrated graphics) the
host and GPU peaks can *overlap*: Metal allocations live inside
`phys_footprint`, so `max_host_bytes + max_gpu_bytes` may exceed the OS-level
process peak. On discrete-pool devices the counters correspond to physically
separate pools and do not overlap. Either way, each bucket is its own
dimension's peak; do not sum them and expect `process_total_peak`.

#### Parameters

| Field | Type | Description |
|-------|------|-------------|
| `benchmark_type` | string | `max_memory_usage` |
| `parameter_prefill_tokens` | integer | Number of tokens to prefill |

#### Execution

1. Load the model with the target quantization.
2. Construct a prompt of exactly `parameter_prefill_tokens` tokens.
3. Prefill the prompt and generate one output token.
4. Record the peak attributed to each compute path. The full methodology
   (probe architecture, host counter sources, sidecar OS-counter
   observations) is specified in
   [`methodology/peak-memory.md`](methodology/peak-memory.md). Summary of
   what is implemented today in `pipette-clients`:
   - **Host (CPU)**: peak of the kernel host counter, sampled
     independently — no GPU/NPU subtraction.
     - macOS (llama.cpp, mlx-lm): `phys_footprint` polled at 20 ms via
       `proc_pid_rusage(RUSAGE_INFO_V4)`.
     - Windows (llama.cpp): `GetProcessMemoryInfo PeakWorkingSetSize`
       read post-exit.
     - Android (llama.cpp): `wait4 ru_maxrss × 1024` from toybox
       `time -v`.
     - iOS: `phys_footprint` peak with a *baseline* subtraction (idle
       memory before the run) — the only place any subtraction is
       applied, and it is host-on-host, not host-on-GPU.
   - **GPU**: peak of the runtime's in-process allocator, sampled
     independently from the host counter. Today: Metal
     `[MTLDevice currentAllocatedSize]` via a DYLD-injected `peakmtl`
     shim on macOS; Vulkan layer (`VK_LAYER_pipette_peakvk`) on the
     Vulkan flavor of llama.cpp on Windows. Reported as `null` when no
     in-process probe is wired up for the runtime — OS-attribution
     counters (PDH, DRM `fdinfo`, `nvidia-smi`) are surfaced as sidecar
     diagnostic data in `extras.json`, **not** as `max_gpu_bytes`.
   - **NPU**: reserved. No client implementation exists yet; always
     `null`.
5. Report observed peak values (not delta from a baseline, except for the
   iOS host-counter baseline noted above).

**Includes**: model weights, KV-cache, activations, runtime buffers, runtime
and driver overhead.
**Excludes**: OS overhead, unrelated processes.

#### Submission fields

| Field | Type | Description |
|-------|------|-------------|
| `max_host_bytes` | i64 | Peak host (CPU) memory in bytes |
| `max_gpu_bytes` | i64 or null | Peak GPU memory in bytes (null if no GPU involved) |
| `max_npu_bytes` | i64 or null | Peak NPU memory in bytes (null if no NPU involved) |

The wire-name aliases `max_ram_bytes` / `max_vram_bytes` are also
accepted for backward compatibility with current `pipette-clients`
builds, which still emit those names. New code should use the
`max_*_bytes` spelling.

> **No partition invariant.** Each bucket is the peak of an
> independently-sampled counter. `max_host_bytes + max_gpu_bytes` may
> exceed `phys_footprint_peak` on unified-memory systems (GPU
> allocations live inside `phys_footprint`); on discrete-pool systems
> the two counters describe disjoint physical pools. Do not derive any
> bucket by subtraction. See
> [`methodology/peak-memory.md`](methodology/peak-memory.md).

#### Derived metrics

| Metric | Formula | Unit |
|--------|---------|------|
| `max_host_usage` | `max_host_bytes` | bytes |
| `max_gpu_usage` | `max_gpu_bytes` | bytes (row omitted when null) |
| `max_npu_usage` | `max_npu_bytes` | bytes (row omitted when null) |

---

### 2.5. Eval (Accuracy)

Measures model accuracy on a question-answering benchmark. The server provides
all prompts via the API so the device has no external dependencies at runtime.
The device generates completions and submits them; scoring happens server-side.

No warm-up or repetitions are needed.

#### Parameters

| Field | Type | Description |
|-------|------|-------------|
| `benchmark_type` | string | `eval` |
| `parameter_eval_id` | string | Eval identifier on the upstream evals server |
| `parameter_dataset_name` | string | Dataset name for the eval |
| `parameter_max_tokens` | integer | Maximum tokens for generation |
| `parameter_mcq_choices` | array of strings or absent | Allowed choices for multiple-choice prompts |

#### Execution

1. Load the model with the target quantization.
2. Fetch the benchmark via `GET /benchmarks/{benchmark_id}`. The response
   includes a `samples` array. Each sample has an `id` and `messages`.
3. For each sample:
   - Apply the model's chat template to the `messages` array. If using an
     OpenAI-compatible API (e.g. `/v1/chat/completions`), pass `messages`
     directly — the server applies the template. If running inference
     directly, apply the template manually before tokenizing.
   - Use the sampling temperature the client assigns for this eval. **The
     catalog has no temperature field** — temperature is a client-side policy
     keyed on `parameter_eval_id`. Most evals use greedy decoding
     (`temperature = 0.0`); some sample at `0.6`. See the per-eval articles
     under [§ Supported evals](#supported-evals) for which.
   - Set `max_tokens` to `parameter_max_tokens`.
   - If `parameter_mcq_choices` is present, use constrained generation to
     restrict output to a **single token** from the allowed choices (e.g.
     `["A", "B", "C", "D"]`). The runtime should mask logits so only choice
     tokens are sampled. Set `max_tokens = 1` regardless of
     `parameter_max_tokens`. If the runtime does not support logit masking,
     generate freely and submit the raw completion — the server will attempt
     to extract the answer.
   - Collect the generated text as `completion`.
4. Submit all completions in a single `POST /benchmarks` request.

Submit completions for **all** samples, even if the model produces empty or
nonsensical output. Missing samples are scored as incorrect. The order of
completions does not matter, but **each `id` must appear at most once** —
submissions with duplicate ids are rejected at the gateway with `400`.

If the local runtime crashes mid-completion for a specific sample (e.g.
[pipette-clients#103](https://github.com/Liquid4All/pipette-clients/pull/103)),
submit the sample with `completion: ""`, `failed: true`, and an optional
human-readable `failed_reason`. The mgmt server forwards every
completion to the scoring service — including failed ones — but
strips the `failed` / `failed_reason` fields from the wire request;
the scorer almost always returns `is_correct: false` for empty
completions, which is what we want.

#### Supported evals

The eval benchmarks, defined under
[`examples/benchmarks/`](../examples/benchmarks/). Each has its own methodology
article:

- [`eval_ifbench_2026.06.1`](methodology/ifbench-2026.06.1.md)
- [`eval_ifstruct_release_v1_0`](methodology/ifstruct-release_v1_0.md)
- [`eval_gpqa_diamond_2026.06.1`](methodology/gpqa_diamond-2026.06.1.md)
- [`eval_math_500_2026.06.1`](methodology/math_500-2026.06.1.md)

#### Submission fields

| Field | Type | Description |
|-------|------|-------------|
| `completions` | array | One entry per sample; ids must be unique within the array |
| `completions[].id` | string | Sample ID from the benchmark |
| `completions[].completion` | string | Model-generated text; empty when `failed: true` |
| `completions[].failed` | boolean (optional) | `true` if the local runtime crashed for this sample. Defaults to `false`; elided from the wire when unset |
| `completions[].failed_reason` | string (optional) | Free-form, human-readable description of the failure. Elided from the wire when unset |

#### Derived metrics

| Metric | Formula | Unit |
|--------|---------|------|
| `accuracy` | `correct / total` | ratio |

`accuracy` is computed over every scored sample, including failed
ones (which are almost always counted as wrong). Consumers that want
an "accuracy over samples that could actually be evaluated" rate
compute it themselves from the `samples_failed` key in the warehouse
row's `eval_metadata` column (see
[storage.md § Per-run metadata](storage.md#per-run-metadata-eval_metadata)).

Scoring also produces per-sample results (prompt, completion,
correct/incorrect, and the re-injected `failed` / `failed_reason`
metadata) stored as Parquet — see
[storage.md § Eval sample results](storage.md#6-eval-sample-results-contract).

---

### 2.6. VL Throughput

Measures vision-language model inference throughput. The device processes a
synthetic image at a specified resolution alongside a text prompt, then
generates output tokens. The client reports prompt processing time (covering
vision encoding + LLM prefill) and generation time separately, along with the
actual number of prompt tokens processed. The prompt token count is
model-dependent due to differences in vision encoder tiling and token
compression strategies.

#### Parameters

| Field | Type | Description |
|-------|------|-------------|
| `benchmark_type` | string | `vl_throughput` |
| `parameter_image_width` | integer | Image width in pixels |
| `parameter_image_height` | integer | Image height in pixels |
| `parameter_text_tokens` | integer | Number of text tokens in the prompt (0 = image-only) |
| `parameter_decode_tokens` | integer | Number of tokens to generate |
| `parameter_num_images` | integer | Images packed into one prompt (default 1; `>1` emulates multi-frame / video) |

**Multiple images (`parameter_num_images`).** `num_images` copies of the
synthetic image are attached as separate parts of a single user message, so the
prompt is `[optional text] + N × [image]` — emulating multi-frame / video
inputs. Each image expands into its own block of image tokens, so the prompt
grows ≈ `N ×` the single-image token count and the context is sized accordingly.
`num_images = 1` is the ordinary single-image case; `num_images = 0` is used only
internally as the text-only baseline for the `image_tokens` measurement. The
per-model image-token counts and the runtime flags that control them are
documented in the client repo's `docs/methodology/vl-image-tokens.md`.

#### Execution

1. Load the VL model with the target quantization (both LLM weights and vision
   encoder / mmproj weights).
2. Generate a synthetic PNG image of exactly `parameter_image_width` x
   `parameter_image_height` pixels, and attach `parameter_num_images` copies to
   the prompt.
3. Construct a text prompt of exactly `parameter_text_tokens` tokens.
4. Warm up (full end-to-end inference, discard).
5. Run inference with `max_tokens` = `parameter_decode_tokens`. Record timings
   and repeat (see [Common rules](#common-rules)).

#### Submission fields

| Field | Type | Description |
|-------|------|-------------|
| `prompt_tokens` | i32 | Actual tokens processed during the prompt phase (image + text, as reported by the runtime) |
| `image_tokens` | i32 (optional) | Image-only tokens = `prompt_tokens` minus the same prompt with the image removed (image embedding + markers). Optional |
| `prompt_ms` | f32 | Prompt processing time in milliseconds (vision encoding + LLM prefill) |
| `prompt_ms_stddev` | f32 (optional) | Standard deviation of `prompt_ms` across repetitions. Optional |
| `predicted_ms` | f32 | Generation time in milliseconds |
| `predicted_ms_stddev` | f32 (optional) | Standard deviation of `predicted_ms` across repetitions |

VL submissions should carry the optional common field `model_descriptor`
(see [Common submission fields](#common-submission-fields)) — the full model
specification, including the multimodal projector artifact and its precision.
`model_descriptor` is what uniquely identifies a VL model configuration; `model_name`
and `model_quant` remain lossy convenience fields for grouping/display.

#### Derived metrics

| Metric | Formula | Unit | `value_stddev` |
|--------|---------|------|----------------|
| `ttft` | `prompt_ms` | ms | `prompt_ms_stddev` |
| `prefill_throughput` | `prompt_tokens / prompt_ms * 1000` | tokens/sec | `prefill_throughput * prompt_ms_stddev / prompt_ms` |
| `decode_throughput` | `parameter_decode_tokens / predicted_ms * 1000` | tokens/sec | `decode_throughput * predicted_ms_stddev / predicted_ms` |
| `e2e_latency` | `prompt_ms + predicted_ms` | ms | `sqrt(prompt_ms_stddev² + predicted_ms_stddev²)` |

The measured token counts are **observations, not metrics** — they are
model-dependent workload facts (a 512px image is ~258 tokens on one encoder,
~1282 on another), not performance results. Each scored row carries them as
observation columns rather than as their own metric rows:

| Observation column | Source | Unit |
|--------------------|--------|------|
| `observation_vl_throughput_prefill_tokens` | `prompt_tokens` | tokens |
| `observation_vl_throughput_image_tokens` | `image_tokens` (when submitted) | tokens |

---

### 2.7. VL Peak Memory

Measures peak host, GPU, and NPU memory for a single vision-language image
workload — the LLM weights plus the resident vision encoder/projector plus the
image-token KV cache. Reports the same fields, counter semantics, and derived
metrics as [Peak Memory Usage](#24-peak-memory-usage): each bucket is the
independently-sampled peak of a distinct counter with no cross-subtraction, and
on unified-memory devices (Apple Silicon, integrated graphics) the host and GPU
peaks can overlap, so `max_host_bytes + max_gpu_bytes` may exceed the OS-level
process peak. NPU is reserved (no client implementation yet), exactly as in the
text benchmark. Only the workload differs — one image through the vision tower
with a single decode step, context sized to the exact workload with no floor so
the KV cache does not inflate the peak. Full methodology in the client repo's
`docs/methodology/vl-max-memory.md`.

#### Parameters

| Field | Type | Description |
|-------|------|-------------|
| `benchmark_type` | string | `vl_max_memory` |
| `parameter_image_width` | integer | Image width in pixels |
| `parameter_image_height` | integer | Image height in pixels |
| `parameter_text_tokens` | integer | Text tokens in the prompt (0 = image-only) |
| `parameter_num_images` | integer | Images packed into one prompt (default 1; `>1` = multi-frame) |

#### Submission fields

Same as [Peak Memory Usage](#24-peak-memory-usage): `max_host_bytes`,
`max_gpu_bytes` (optional), `max_npu_bytes` (optional). Include `model_descriptor`,
since the vision encoder / projector precision it records changes resident size.

#### Derived metrics

| Metric | Formula | Unit |
|--------|---------|------|
| `max_host_usage` | `max_host_bytes` | bytes |
| `max_gpu_usage` | `max_gpu_bytes` | bytes (row omitted when null) |
| `max_npu_usage` | `max_npu_bytes` | bytes (row omitted when null) |

Same no-partition invariant as [Peak Memory Usage](#24-peak-memory-usage): the
buckets are independently-sampled counter peaks and must not be derived by
subtraction.

---

## 3. Appendix

### 3.1. Parquet storage schema

Every metric row in the warehouse Parquet files includes these common columns.
Per-benchmark parameter columns are nullable — only populated for the relevant
benchmark type.

#### Common columns

| Column | Type | Nullable |
|--------|------|----------|
| `result_id` | string | no |
| `benchmark_id` | string | no |
| `benchmark_type` | string | no |
| `metric` | string | no |
| `client_id` | string | no |
| `device_name` | string | no |
| `device_form_factor` | string | no |
| `device_os_name` | string | no |
| `device_os_version` | string | no |
| `device_os_build` | string | yes |
| `device_os_security_patch` | string | yes |
| `device_chip_model` | string | no |
| `device_gpu_model` | string | yes |
| `device_gpu_vram_bytes` | int64 | yes |
| `device_npu_model` | string | yes |
| `device_npu_vram_bytes` | int64 | yes |
| `device_ram_bytes` | int64 | no |
| `device_battery_level` | int32 | yes |
| `device_power_state` | string | yes |
| `device_power_save_mode` | bool | yes |
| `device_android_cpuset` | string | yes |
| `device_android_cpu_affinity_list` | string | yes |
| `device_android_cpu_affinity_excludes_top_tier` | bool | yes |
| `device_apple_thermal_state_before` | list&lt;string&gt; | yes |
| `device_apple_thermal_state_after` | list&lt;string&gt; | yes |
| `device_apple_soc_temp_c_before` | list&lt;f32&gt; | yes |
| `device_apple_soc_temp_c_after` | list&lt;f32&gt; | yes |
| `device_android_thermal_status_before` | list&lt;string&gt; | yes |
| `device_android_thermal_status_after` | list&lt;string&gt; | yes |
| `device_android_thermal_headroom_before` | list&lt;f32&gt; | yes |
| `device_android_thermal_headroom_after` | list&lt;f32&gt; | yes |
| `device_android_thermal_sensors_before` | list&lt;struct&gt; | yes |
| `device_android_thermal_sensors_after` | list&lt;struct&gt; | yes |
| `device_linux_thermal_zones_before` | list&lt;struct&gt; | yes |
| `device_linux_thermal_zones_after` | list&lt;struct&gt; | yes |
| `model_name` | string | yes |
| `model_quant` | string | yes |
| `model_params_total_millions` | int32 | yes |
| `model_params_active_millions` | int32 | yes |
| `model_flags` | string | yes |
| `runtime_name` | string | yes |
| `runtime_version` | string | yes |
| `runtime_flags` | string | yes |
| `runtime_cpu_variant` | string | yes |
| `value` | f32 | no |
| `unit` | string | no |
| `submitted_at` | timestamp_utc | no |
| `scored_at` | timestamp_utc | no |
| `value_stddev` | f32 | yes |
| `score_runtime_version` | string | yes |
| `eval_metadata` | string | yes |
| `model_descriptor` | string | yes |
| `runtime_descriptor` | string | yes |
| `benchmark_flags` | string | yes |
| `model_descriptor_sha256` | string | yes |
| `runtime_descriptor_sha256` | string | yes |
| `benchmark_flags_sha256` | string | yes |
| `client_version` | string | yes |
| `model_flags_sha256` | string | yes |
| `runtime_flags_sha256` | string | yes |

`model_descriptor` / `runtime_descriptor` are canonical JSON strings (object keys sorted,
whitespace stripped) — the full, lossless model/runtime specifications. They are
stored verbatim after canonicalization and never interpreted by the server;
slice them with JSON-path or (thanks to canonicalization) stable `LIKE`
patterns. `model_descriptor_sha256` / `runtime_descriptor_sha256` /
`benchmark_flags_sha256` are the hex sha256 of the respective canonical string,
computed mgmt-side at submission time as a stable id (null when the value is
absent).

`benchmark_flags` is the canonical JSON of the harness configuration the run
resolved to — readiness gating, timeouts, loop detection — normalized the same
way. Group on `benchmark_flags_sha256` to compare only runs measured under the
same conditions; a waived thermal gate is otherwise invisible in the row. See
[storage.md § benchmark_flags](storage.md#benchmark_flags).

`model_flags` / `runtime_flags` are normalized the same way *when they parse as
JSON*, each with a `_sha256` alongside. Unlike the columns above they are also
documented to accept a plain string (`--n-gpu-layers 999`), which is stored
trimmed and hashed as-is rather than rejected — so the hash is a content id for
whatever spelling the client uses, and two clients that send the same JSON with
different key order or spacing land in one bucket. A top-level empty object
collapses to NULL, so "nothing reported" has one spelling. Historical rows are
brought to the current rules by
[`fix-canonical`](cli.md#pipette-mgmt-fix-canonical).

`client_version` is the version of the client build that produced the run — the
harness, not the inference runtime it drove (`runtime_version`). Opaque to the
server: never parsed or ordered, so any versioning scheme a client uses is
fine. Null for clients that don't report it. Use it to attribute a shift in the
numbers to a harness change rather than to the device or the runtime; the two
version columns move independently, and the same `runtime_version` measured by
two client versions is not necessarily measured the same way.

`eval_metadata` is a JSON-encoded `{key: value}` object holding
per-run metadata that doesn't belong on the metric axis — currently
`{"samples_failed": N}` for eval submissions with any client-side
failures. The same blob is denormalized onto every row of the
submission. See
[storage.md § Per-run metadata](storage.md#per-run-metadata-eval_metadata).

#### Thermal list-element schema

The `device_android_thermal_sensors_*` and `device_linux_thermal_zones_*`
`List<Struct>` element fields (`iteration`, `type`, `name`, `celsius`,
`throttling_status`) are documented in
[Thermal telemetry § Field reference](methodology/thermal-telemetry.md#field-reference).

#### Per-benchmark parameter columns

| Column | Type | Used by |
|--------|------|---------|
| `parameter_prefill_tokens` | int32 | prefill_throughput, decode_throughput, end_to_end_latency, max_memory_usage |
| `parameter_decode_tokens` | int32 | decode_throughput, end_to_end_latency, vl_throughput |
| `parameter_eval_id` | string | eval |
| `parameter_image_width` | int32 | vl_throughput, vl_max_memory |
| `parameter_image_height` | int32 | vl_throughput, vl_max_memory |
| `parameter_text_tokens` | int32 | vl_throughput, vl_max_memory |
| `parameter_num_images` | int32 | vl_throughput, vl_max_memory |

#### Per-benchmark observation columns

Measured workload facts (not parameters, not metrics). Nullable — only
populated for the relevant benchmark type.

| Column | Type | Used by |
|--------|------|---------|
| `observation_vl_throughput_prefill_tokens` | int32 | vl_throughput |
| `observation_vl_throughput_image_tokens` | int32 | vl_throughput |

### 3.2. Glossary

| Term | Definition |
|------|------------|
| **Prefill** | The phase where the model processes the entire input prompt in one forward pass, populating the key-value cache. |
| **Decode** | The phase where the model generates output tokens one at a time, each conditioned on the KV-cache and previously generated tokens. |
| **KV-cache** | Key-value cache — stores intermediate attention states from the prefill phase so they don't need to be recomputed during decode. |
| **TTFT** | Time to first token — the latency from prompt submission to the first output token. Equal to the prefill time. |
| **Greedy decoding** | Deterministic generation strategy (`temperature = 0.0`) that always picks the highest-probability token. Required for reproducible results. |
| **Quantization** | Reducing model weight precision (e.g. from 16-bit to 4-bit) to lower memory usage and increase speed. Common formats: `q4_0`, `q8_0`, `f16`. |
| **mmproj** | The vision encoder projection weights that map image features into the LLM's embedding space. Loaded alongside the LLM for VL models. |
| **RSS** | Resident Set Size — the amount of physical RAM a process is currently using. |
| **MCQ** | Multiple-choice question — an eval format where the model must select one of a fixed set of answer choices (e.g. A, B, C, D). |
