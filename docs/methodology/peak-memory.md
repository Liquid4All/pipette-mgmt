# Peak Memory Usage — Measurement Methodology

This document specifies *how* clients should measure the values reported by the
`max_memory_usage` benchmark. The benchmark itself is defined in
[benchmarks.md §2.4](../benchmarks.md#24-peak-memory-usage); this document
covers the operational details that the spec deliberately leaves open.

## 1. Reported fields

`max_memory_usage` submissions carry up to three integer byte counts.
**Each is the peak of an independently-sampled counter** — no
cross-subtraction, no derived values, no partition assumption.

| Field | What it measures |
|---|---|
| `max_host_bytes` | Peak of the OS-level whole-process resident-set counter (the kernel's lifetime-max where available; sampled where that counter is unsuitable — e.g. iOS, whose lifetime-max is sticky across cells in a long-lived in-process app, see the per-platform docs). Always populated. Per-OS counter API in the per-platform docs linked from §3. |
| `max_gpu_bytes` | Peak of the GPU memory the workload holds, reported **only when the OS accounts for GPU memory as a pool separate from the process resident set** (see §1.2, *Unified vs. split memory*). Source is then either an **in-process allocator probe** (allocator-level) or an **OS-attribution counter** (OS-attribution level, sees allocator + driver state). Which one and why is platform-specific — see §2 and the per-platform docs. `null` on truly-unified platforms (no separate pool to report), on CPU-only flavors, and on GPU-capable platforms where no probe has landed yet. |
| `max_npu_bytes` | Peak of an NPU runtime's allocator. `null` when not used or no probe exists. Reserved for future backends. |

`max_host_bytes` is a **process-level** number — it counts everything
the kernel attributes to this process, including loaded shared
libraries, the binary's text/data, libc heap, stack, and (on
unified-memory devices) GPU allocations. `max_gpu_bytes` is an
**allocator-level** number, scoped to the GPU runtime's bookkeeping —
and is reported *only on platforms that account GPU memory as a separate
pool*; on a truly-unified platform it is `null`. The two are read from
different sources and may overlap; see §1.1.

Submissions also carry a few diagnostic fields that are *not* part of the
scored metrics but are surfaced via `extras.json`:

| Field | What it measures |
|---|---|
| `host_method` | Identifies the kernel counter `host_bytes` was read from. Self-describing enum values; current platforms in the per-platform docs. |
| `gpu_method` | Identifies the probe (or OS counter) that produced `gpu_bytes`. `null` when `gpu_bytes` is `null`. |
| `os_counter_observations` | Zero or more sidecar readings from OS-attribution counters captured as diagnostic data. Sources and field labels in the per-platform docs linked from §3. |

### 1.1. How to interpret the fields

The fields are independent counter peaks, not a partition. Consumer-side
reasoning depends on **how the platform accounts for GPU memory** — a
per-platform property the consumer reads from the result's platform and
runtime (see the producer rule in §1.2 and the per-platform docs). There are
two cases:

- **Unified (host already includes GPU)** — e.g. Apple Silicon, where
  `max_gpu_bytes` is `null` and GPU allocations are billed to
  `max_host_bytes`. Or a *virtual* split (e.g. Windows WDDM) where
  `max_gpu_bytes` is populated but the host counter still bills those bytes
  to the process, so `max_gpu_bytes` is a subset of `max_host_bytes`.
- **Discrete / physical split** — host and GPU (VRAM) are separate pools; the
  two counters are disjoint.

| Question | Unified / host-includes-GPU | Discrete / disjoint pools |
|---|---|---|
| "Will this fit on a device with N bytes of total memory?" | `max_host_bytes ≤ N` (host already includes GPU; do not add) | `max_host_bytes + max_gpu_bytes ≤ N` (separate pools, sum them) |
| "How much memory does the GPU allocator manage?" | `max_gpu_bytes` (subset of `max_host_bytes`; `null` on a truly-unified platform) | `max_gpu_bytes` (disjoint from `max_host_bytes`) |
| "How much non-GPU host memory?" | `max_host_bytes − max_gpu_bytes` (if you want it; the wire schema doesn't pre-compute this because the two peaks may not be temporally co-located) | `max_host_bytes` (host is non-GPU by physical structure) |

The wire schema does **not** pre-compute "host minus gpu" because that
quantity is ambiguous when the two peaks occur at different moments
(e.g. host counter peaks during model load while GPU allocator peaks
during the bench). Consumers that genuinely need a non-GPU subtotal can
compute it from the two reported values, but should treat it as
`≤ max_host_bytes − max_gpu_bytes` rather than `=`.

### 1.2. Unified vs. split memory (the producer rule)

§1.1 tells a *consumer* how to read the fields. This is the *producer*
rule that decides which fields a platform populates in the first place: a
platform reports memory by **how the OS accounts for it, not by the
physical layout of the silicon**.

- **No split (truly unified).** The OS bills GPU allocations to the
  single whole-process resident counter and exposes no separate per-process
  GPU-memory accounting. Report the whole cost as `max_host_bytes` and set
  `max_gpu_bytes = null`. There is no second pool to fit into, so the host
  footprint *is* the device-fit number. This is the Apple-Silicon case: iOS,
  and macOS on M1–M5. Metal allocations live inside `phys_footprint`, with no
  driver-managed carve-out the host counter cannot see.
- **Split (virtual or physical).** The platform accounts host and GPU
  memory separately — a discrete GPU's own VRAM (physical split), or a
  driver/OS *virtual* carve-out on shared silicon that is tracked as its
  own pool (e.g. Windows WDDM "GPU Process Memory"). Report each field. On a
  *virtual* split the host counter still bills those GPU bytes to the process
  (`max_gpu_bytes` is a subset of `max_host_bytes` → don't add); on a
  *discrete* GPU the two are disjoint pools (separate limits). The consumer
  applies this from the result's platform/runtime — see §1.1.

On a unified device, the runtime's GPU-allocator high-water mark (Metal
`[MTLDevice currentAllocatedSize]`, MLX `peakMemory`) is a **diagnostic**
of how much of the footprint the GPU runtime holds — useful in captured
runtime logs — **not** a reported `max_gpu_bytes`. Reporting it as
`max_gpu_bytes` would label a subset of the host footprint as if it were a
second capacity dimension.

### 1.3. Invariants

- **`max_host_bytes` is always populated.** Every supported platform has
  a host counter; it never reads as `null`.
- **`max_gpu_bytes` is `null` when the platform has no separate GPU pool
  to report** — either truly-unified memory (§1.2; the cost is already in
  `max_host_bytes`) or no probe on a split platform. It is never `0`: zero
  would mean "used the GPU and it allocated nothing measurable" —
  operationally unreachable for transformer inference, so prefer `null`.
- **`max_npu_bytes` is `null` until a backend lands.** No NPU probe
  exists today.
- **No partition or summation invariant.** When `max_gpu_bytes` is
  non-`null`, whether it is a subset of `max_host_bytes` (virtual split) or
  disjoint from it (discrete GPU) depends on the platform. There is no
  cross-platform algebraic relationship — use the platform's memory model
  (§1.1) to decide how to combine them.

## 2. Measurement architecture

There are three concurrent measurement channels. Each answers a
different question and feeds a different wire field:

| channel | answers | populates |
|---|---|---|
| ① in-process probe (parent-injected code that runs inside the child) | "what did the runtime's allocator hand out?" | `max_gpu_bytes`, `gpu_method` |
| ② host counter (parent reads the kernel's per-process resident-set ledger) | "what's the process's peak resident set?" | `max_host_bytes`, `host_method` |
| ③ OS attribution (parent polls per-PID kernel bookkeeping for GPU memory) | "what bytes does the OS bill to this PID?" | `max_gpu_bytes` *or* `os_counter_observations`, depending on whether the platform's counter is reliable enough to be primary — see §2.3 |

Per-channel mechanics (injection method, syscall names, counter APIs)
are platform-specific and live in the per-platform docs linked from §3.

### 2.1. In-process probes vs OS-attribution counters as `max_gpu_bytes` source

On a *split* platform, `max_gpu_bytes` can in principle be sourced from
either an in-process probe (runtime-API-specific code injected into the
child) or from an OS-attribution counter (per-PID kernel bookkeeping read
from the parent). The two answer subtly different questions — see §2.3 —
and the right choice depends on what each platform supplies. Per-platform
rationale, counter choice, and trade-offs live in the per-platform docs
linked from §3. On a truly-unified platform there is no separate GPU pool
to source from, so `max_gpu_bytes` stays `null` regardless (§1.2).

The same in-process-probe machinery is also used on unified platforms
(macOS, iOS) to capture the GPU-allocator high-water mark as a **logged
diagnostic** — see §1.2 — rather than as a reported `max_gpu_bytes`. When a
probe drives a field or a diagnostic, it follows a shared wire contract:

1. Parent creates a tempfile (RAII via `tempfile::TempDir`) and passes its path
   to the child via the `PIPETTE_MEMPROBE_OUT` environment variable.
2. Probe (constructor / layer-init / first-call hook) reads the env var; if
   absent, the probe is dormant (no-op for the rest of the run).
3. On every peak grow, the probe writes `key=value\n` lines to the file using
   `O_TRUNC` — last write wins. This survives `_exit()`, `SIGKILL`, and
   `__attribute__((destructor))`-skipping exits because the file always
   contains the latest peak.
4. After the child exits, the parent reads the file and parses the snapshot.

Probes are runtime-API-specific, **not OS-specific**. On Apple every GPU
runtime funnels through `MTLDevice`, so the Metal shim covers Metal, MPS
Graph, CoreML, MLX, PyTorch MPS, and any future Apple stack. Where an
OS-attribution counter is the chosen path (Windows PDH), the same
universality comes "for free" — every GPU API that runs through WDDM gets
counted, no per-API code required.

### 2.2. Host counters

Every supported OS exposes a kernel-maintained lifetime-max resident-set
counter for a process. This is read **directly as `max_host_bytes`** with
no subtraction. Cheap, always available, and named per-OS in the
per-platform docs linked from §3.

`max_host_bytes` and (when present) `max_gpu_bytes` are independently
sampled peaks of two distinct counters. On a truly-unified platform there
is no separate GPU pool to sample, so `max_gpu_bytes` is `null` and the
host counter already subsumes the GPU allocations (§1.2). On a *virtual*
split `max_host_bytes` includes the bytes counted by `max_gpu_bytes`; on
systems with separate physical pools they're disjoint. The wire schema does
**not** subtract: each value stands on its own, and the consumer relates
them from the platform's memory model (see §1.1).

### 2.3. OS-attribution counters: primary, sidecar, or absent

This section applies only to *split* platforms, where a separate GPU pool
exists to report. (On a truly-unified platform `max_gpu_bytes` is `null`
by the producer rule in §1.2, so there is nothing to source.) Most split
platforms expose per-PID GPU memory counters at the OS level; whether one
is suitable as the primary `max_gpu_bytes` source depends on its
properties:

- **Stable, GPU-API-agnostic, available at any process integrity
  level**: promote to primary `max_gpu_bytes`. One platform (Windows)
  satisfies this today.
- **Available but with caveats** (counts mixed with non-relevant
  buffers; lags the allocator's high-water mark; misses entire runtime
  APIs): captured as **sidecar diagnostic data** in `extras.json`
  under `os_counter_observations`, not promoted to primary.
- **Absent on a split platform**: rely solely on the in-process probe.

Per-platform counter selection, sidecar field labels, and known caveats
are in the per-platform docs linked from §3.

### 2.4. Workload phases captured

The `max_memory_usage` benchmark runs the runtime through both the **prefill**
and **decode** phases so that allocations specific to either phase are billed
to the reported peak.

Concretely, the bench is invoked with `--n-prompt N --n-gen 1` (llama.cpp) or
`max_tokens=1` (MLX `stream_generate`):

| Phase | What gets allocated | Captured by prefill alone (`--n-gen 0`)? |
|---|---|---|
| Model load | weights (mmap), tensor metadata | yes |
| Compute graph setup | scratch buffer sized for `--ctx-size` | yes |
| Prefill (N tokens, batched) | batched-attention kernels, large intermediate tensors | yes |
| **Decode (1 token)** | **single-token attention kernels (separate Metal pipeline state objects), sampling buffers (vocab-sized logits + top-k scratch)** | **no — only allocated on first decode** |
| KV cache | pre-allocated to `--ctx-size` at load | yes |

Decode-specific allocations are *additive*: once created on the first decode
they persist for the run, raising both the steady state and the peak above
what prefill alone would observe. For a 350M-param model the gap is roughly
5–20 MB out of the ~600–1500 MB total — small in percentage terms, but it's a
**systematic underreport** if `--n-gen 0` is used.

`--n-gen 1` is sufficient: kernels and sampling buffers are created on first
use and reused for any subsequent decode, so generating more than one token
adds wall-clock cost without raising the measured peak. The benchmark uses
`--n-gen 1` to stay close to minimum runtime while still exercising both
phases.

Higher-token decode is unnecessary here but appropriate for the
`decode_throughput` benchmark, which has different goals.

### 2.5. Runtime allocator accounting vs process-level RSS

The runtime (llama.cpp, MLX, etc.) prints its own buffer-size logs at load
and bench time. Those numbers do **not** equal what we report in the wire
schema, and the difference is intentional — they answer different questions:

| Source | Question answered | What it counts |
|---|---|---|
| **Runtime's announced sum** (`load_tensors: ... model buffer size = …`, `llama_kv_cache: ... KV buffer size = …`, `sched_reserve: ... compute buffer size = …`) | "How much memory did *the runtime's allocator* deliberately allocate?" | Buffers the runtime explicitly requests through its backend's `buft_alloc` (model weights, KV cache, compute scratch, output buffer). |
| **`max_host_bytes` / `max_gpu_bytes`** (this spec) | "How much physical memory does the OS / GPU driver attribute to *this process*?" | Everything the runtime allocates **plus**: the binary's `.text` / `.data` segments, dynamically-loaded `.so`s, libc heap allocations not routed through the backend allocator, dynamic linker, stack, transient init buffers, driver-internal state. |

The runtime's number is a strict subset of what we report. Per-platform
empirical numbers showing the gap are in the per-platform docs linked
from §3, and the cross-platform comparison is in §5.

#### 2.5.1. Why we report process-level

Consumers of `max_host_bytes` / `max_gpu_bytes` are answering questions
like:

- "Can this device fit this model?" (8 GiB device → does the *whole
  process*, including runtime libraries and heap, fit in 8 GiB?)
- "Rank devices by efficiency under this workload."
- "Did this build regress memory between commits?"

For all three, the inclusive process-level number is the right one — a
device with 200 MiB of free RAM can't fit a model that the runtime's
allocator says is 180 MiB if the runtime + libraries + heap eat another
30 MiB. Reporting the allocator-side sum would systematically understate
real memory cost.

#### 2.5.2. Where to find the runtime's view

The runtime's own buffer-size logs are captured verbatim into the bench
result's `extras.json` under `outcome.stderr`. An operator who wants the
allocator-side breakdown — e.g., model vs KV cache vs compute scratch —
can grep the stderr for `load_tensors:`, `llama_kv_cache:`,
`sched_reserve:`, etc. The wire-schema field gives the *total*; the
captured stderr gives the *breakdown*.

## 3. Per-platform probes

Each platform's measurement details — counter choice, probe internals,
empirical reference numbers, caveats, and sidecar observations — lives in
its own document:

| Platform | Status | Document |
|---|---|---|
| macOS (Apple Silicon M1–M5, Metal) | ✅ implemented; host-only (unified, `max_gpu_bytes = null`) | [peak-memory-macos.md](peak-memory-macos.md) |
| iOS (Apple Silicon, Metal/MLX) | ✅ implemented; host-only (unified, `max_gpu_bytes = null`) | [peak-memory-ios.md](peak-memory-ios.md) |
| Linux | ⌛ host-only; GPU probes proposed | [peak-memory-linux.md](peak-memory-linux.md) |
| Windows (Vulkan / HIP / SYCL / ARM64-CPU) | ✅ implemented | [peak-memory-windows.md](peak-memory-windows.md) |
| Android (arm64-v8a CPU) | ✅ host implemented; GPU intentionally null | [peak-memory-android.md](peak-memory-android.md) |

The cross-platform empirical comparison (same model, same flags, three
context lengths) for the platforms with published reference numbers
(macOS, Windows, Android) is in §5 below.

## 4. Implementation status

Both clients consume the shared `pipette-memprobe` crate
(`crates/pipette-memprobe/` in the pipette-clients workspace), so the
host-peak measurement, the Probe trait, the Metal probe, the OS-counter
sidecar, and the host-vs-accelerator reduction (`split_memory`) live in
one place. See the crate's `README.md` for design rationale, per-platform
implementation details, and known caveats.

| Runtime / OS | `max_host_bytes` source | `max_gpu_bytes` source | Notes / Status |
|---|---|---|---|
| pipette-llamacpp on macOS (Metal) | `phys_footprint` peak (child `proc_pid_rusage` `RUSAGE_INFO_V4`) | `null` — Apple Silicon unified, no host/GPU split (§1.2). The `MetalProbe` DYLD shim `[MTLDevice currentAllocatedSize]` peak is kept as a logged diagnostic | ✅ implemented |
| pipette-mlx on macOS (Metal) | `phys_footprint` peak (child `proc_pid_rusage` `RUSAGE_INFO_V4`) | `null` — unified, no host/GPU split (§1.2). Both the `MetalProbe` peak and `mx.get_peak_memory()` are kept as logged diagnostics | ✅ implemented |
| pipette-llamacpp on **iOS** (Metal) | in-process `task_vm_info.phys_footprint` sampler across a fresh model-load + prefill + 1-decode bracket | `null` — unified, no host/GPU split (§1.2). The Metal `currentAllocatedSize` peak is a logged diagnostic | ✅ implemented |
| pipette-mlx on **iOS** (Metal) | in-process `task_vm_info.phys_footprint` sampler across a fresh model-load + prefill + 1-decode bracket | `null` — unified, no host/GPU split (§1.2). MLX `peakMemory` is a logged diagnostic | ✅ implemented |
| pipette-llamacpp on **Windows** (Vulkan / HIP / SYCL) | PSAPI `PeakWorkingSetSize` | PDH `\GPU Process Memory(...)\Total Committed` | ✅ implemented — one path covers every GPU runtime WDDM tracks |
| pipette-llamacpp on Windows (ARM64-CPU) | PSAPI `PeakWorkingSetSize` | `null` (no GPU) | ✅ implemented |
| pipette-llamacpp on **Android** arm64-v8a CPU | `wait4 ru_maxrss × 1024` via toybox `time -v` | `null` (Mali has no stable per-PID ioctl) | ✅ implemented |
| pipette-llamacpp on Linux (CPU) | `wait4 ru_maxrss × 1024` | `null` (no GPU runtime) | ⌛ pending; same dispatcher pattern as Mac/Win/Android |
| pipette-llamacpp on Linux (Vulkan) | `wait4 ru_maxrss × 1024` | `null` (Vulkan probe pending) | ⌛ host-only; DRM fdinfo + `nvidia_smi` sidecar specified in [peak-memory-linux.md](peak-memory-linux.md) |

When `max_gpu_bytes` is `null` on a flavor that demonstrably uses a GPU
(Vulkan / HIP / SYCL on Linux/Windows), submissions are still accepted;
readers should treat the field as "not measured by an in-process probe"
rather than "zero," and consult `os_counter_observations` in `extras.json`
for the closest available signal.

Cross-runtime consistency notes for the two macOS clients (pipette-llamacpp
and pipette-mlx) are in [peak-memory-macos.md](peak-memory-macos.md#6-cross-runtime-consistency).

## 5. Cross-platform empirical reference (LFM2-350M-Q4_K_M)

Same model (LiquidAI/LFM2-350M-GGUF, Q4_K_M, 350 M parameters), same
`llama-bench --output json --mmap 0 --n-prompt N --n-gen 1 -r 1` shape.
Validated on production benchmark hosts; raw numbers from
`payload.json` (`max_ram_bytes` / `max_vram_bytes` wire fields).

### 5.1. macOS (Apple Silicon, Metal)

llama.cpp b9058 macos-arm64, Metal backend, all 17 layers offloaded.
macOS is unified (Apple Silicon M1–M5), so the only **reported** field is
`max_host_bytes`; `max_gpu_bytes = null`. The Metal column is the logged
**diagnostic** (`[MTLDevice currentAllocatedSize]` peak), not a reported
field:

| Ctx  | `max_host_bytes` (`phys_footprint`) | Metal `currentAllocatedSize` peak *(diagnostic)* | Σ announced `MTL0` buffers *(diagnostic)* |
|-----:|------------------------------------:|-------------------------------------------------:|------------------------------------------:|
| 256  | 530.69 MiB                          | 290.71 MiB                                        | 289.74 MiB                                |
| 1024 | 536.66 MiB                          | 371.47 MiB                                        | 370.50 MiB                                |
| 2048 | 552.71 MiB                          | 375.47 MiB                                        | 374.50 MiB                                |

The reported number is `max_host_bytes` (`phys_footprint`), which on UMA
Apple Silicon already subsumes the Metal-allocated pages — there is no
separate pool, so `max_gpu_bytes = null` (§1.2). As a diagnostic, the Metal
allocator peak is consistently
+0.97 MiB above the runtime's announced `MTL0` sum across all three
context lengths (Metal driver state: default command queue, residency-set
scratch, pipeline-state objects); it is logged for auditing, not reported.

### 5.2. Windows (AMD Strix Halo iGPU, Vulkan)

AMD Ryzen AI 9 HX 370 + Radeon 890M, AMDVLK driver
32.0.23002.1006, llama.cpp b9058 win-vulkan-x64, all layers offloaded
to `Vulkan0`:

| Ctx  | `max_host_bytes` (PSAPI `PeakWorkingSetSize`) | `max_gpu_bytes` (PDH `Total Committed`) | Σ announced Vulkan buffers |
|-----:|----------------------------------------------:|----------------------------------------:|---------------------------:|
| 256  | 152.85 MiB                                    | 375.95 MiB                              | 342.75 MiB                 |
| 1024 | 159.51 MiB                                    | 458.27 MiB                              | 423.24 MiB                 |
| 2048 | 158.04 MiB                                    | 481.68 MiB                              | 437.24 MiB                 |

Windows host and GPU are separate pools — PSAPI doesn't count
GPU-driver-managed memory. PDH `Total Committed` runs +33 → +44 MiB
above the Vulkan-announced sum across n=256→2048: WDDM driver-state
attribution (command-buffer pools, descriptor tables, paging entries,
residency scratch) that the OS bills to the process but the Vulkan
API doesn't expose. The Δ grows mildly with the number / size of
allocations (more command buffers, larger descriptor pools, more
per-allocation tracking metadata) — it's not a strict constant. See
[peak-memory-windows.md](peak-memory-windows.md#2-gpu-total-pdh-gpu-process-memorypid__total-committed)
for the per-counter breakdown that led to picking `Total Committed`
over `Dedicated`/`Shared` independently, and for the multi-process
attribution verification.

### 5.3. Android (Samsung S25 Ultra, Snapdragon 8 Elite, CPU)

`bench-tools-20260415-38cc8e3fd` android-arm64-v8a, CPU-only build
(no Vulkan/HIP Android target today), `armv8.6_1` CPU backend:

| Ctx  | `max_host_bytes` (toybox `time -v` Max RSS) | `max_gpu_bytes` | Σ announced runtime buffers |
|-----:|--------------------------------------------:|----------------:|----------------------------:|
| 256  | 348.83 MiB                                  | null            | 338.24 MiB                  |
| 1024 | 367.95 MiB                                  | null            | 413.24 MiB                  |
| 2048 | 430.34 MiB                                  | null            | 425.24 MiB                  |

`max_host_bytes` at n=1024 is *below* the runtime's announced sum
(367.95 < 413.24): the virtual-reservations-never-faulted case
documented in §2.5. `sched_reserve` sizes the compute scratch for
worst-case (132 MiB on this run) but at n=1024 only a fraction of
those pages get faulted in. `wait4 ru_maxrss` measures resident-set
high water, not the runtime's reservation. At n=2048 the workload
touches the full reservation and the two figures realign.

### 5.4. Reading the table across platforms

Same workload, three different measurements. What to take away:

- **Mac/iOS UMA (unified, `max_gpu_bytes = null`)**: only
  `max_host_bytes` is reported and it already subsumes the GPU
  allocations (§1.2). To answer "fits on a Mac/iPhone with N GB unified
  memory?", compare `max_host_bytes` to N. There is no second field to
  add; the Metal/MLX allocator peak is a logged diagnostic only.
- **Windows (separate host/GPU pools)**:
  `max_host_bytes` and `max_gpu_bytes` measure disjoint memory pools
  by physical structure. On discrete GPUs, sum them for the "fit on
  device" question. On UMA Windows, the two slightly overlap (the
  `Shared Usage` component of `max_gpu_bytes` and the `Vulkan_Host`
  pages PSAPI sees) — operators wanting a precise UMA total should
  parse the runtime's announced breakdown from captured stderr.
- **Android CPU-only**: `max_gpu_bytes` is `null`. `max_host_bytes`
  is the full memory footprint; treat the runtime's announced sum
  (`extras.json` → `outcome.stderr`) as a complementary signal of
  how much was reserved vs how much was actually faulted in.

### 5.5. What each platform's GPU-allocator Δ to the runtime's announcement means

On Windows the GPU-allocator counter is the reported `max_gpu_bytes`; on
macOS it is the **logged diagnostic** Metal peak (§1.2), not a reported
field. The Δ to the runtime's announced buffer sum is informative in both
cases:

| Platform | GPU-allocator peak vs runtime announced (n=256→2048) | What the Δ represents |
|---|---|---|
| macOS (Metal, *diagnostic only*) | ≈+1.0 MiB (0.97 MiB), **constant** across context lengths | Metal driver state: default command queue, internal heaps, residency-set objects — outside `MTLBuffer` accounting but inside the Metal allocator's reach |
| Windows (PDH Total Committed, *reported `max_gpu_bytes`*) | +33 → +44 MiB, **grows mildly** with allocation count / context length | WDDM driver state: command-buffer pools, descriptor heaps, paging-table entries, shader-bytecode cache — outside the GPU API surface entirely, attributed by the OS |
| Android | n/a (`max_gpu_bytes = null`) | — |

The two platforms behave differently here: Mac's Δ is bit-stable across
context lengths (the same ~1.0 MiB driver-state floor regardless of KV
cache size), while Windows' Δ grows by ~11 MiB across the same range as
WDDM allocates more command-buffer / descriptor / paging metadata to
back the larger allocation count. Operators tracking memory regressions
should diff a given counter against itself across runs, not against the
runtime's announced sum — and on macOS this is a diagnostic series, not
the reported `max_gpu_bytes` (which is `null`).

The Windows Δ is also **strictly per-process**: when two processes share
the GPU, each pays its full driver-state overhead independently (no
amortization, no cross-process sharing). See
[peak-memory-windows.md](peak-memory-windows.md#22-per-process-attribution-under-concurrent-gpu-use)
for the empirical verification.
