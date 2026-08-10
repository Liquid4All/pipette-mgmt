# Peak Memory — iOS (Metal / MLX)

Per-platform measurement details for the
[Peak Memory Usage methodology](peak-memory.md). Cross-platform invariants,
field definitions, and the cross-platform empirical reference live in the
parent document.

**Status:** ✅ implemented (pipette-llamacpp and pipette-mlx on-device).

iOS is **unified memory with no host/GPU accounting split** (Apple Silicon:
Metal allocations are billed to the process footprint, with no driver
carve-out the host counter cannot see). Per the
[unified-vs-split producer rule](peak-memory.md#12-unified-vs-split-memory-the-producer-rule),
the entire cost is reported as `max_host_bytes` and `max_gpu_bytes = null`;
the GPU-allocator high-water mark is captured as a **diagnostic**, not a
reported field.

## 1. What the number means

Unlike the desktop CLI paths, the iOS benchmark runs **in-process** inside
the Pipette app — there is no child process to wrap. `max_host_bytes` is a
per-`(model, runtime, quant)` measure: *how much it takes to run that one
model on that one runtime*, recorded alongside `model_name`, `model_quant`,
`runtime_name`, and `runtime_version`.

The reported `max_host_bytes` is the **absolute** peak process
`phys_footprint` over the run — model + runtime + harness together. **No
baseline is subtracted, and no separate baseline field is recorded.** "How
much it takes to run the model" includes the process the model runs in, and
the app's own footprint before any model/runtime work (the harness floor) is
negligible next to a real run — see §2.2. The absolute peak is also the only
figure iOS jetsam actually acts on, so it is the figure a device must fit.

## 2. Host total: `phys_footprint`

```
max_host_bytes    = phys_footprint peak               (from the in-process sampler)
max_gpu_bytes     = null                              (unified memory, no separate GPU pool)
max_npu_bytes     = null
host_method       = phys_footprint
gpu_method        = null
```

iOS is unified — there is no separate GPU pool, so the host footprint is the
whole device-fit number and nothing is added to it. The `host_method` /
`gpu_method` lines are conceptual: iOS has no `extras.json` sidecar channel,
so it does not actually emit these diagnostic fields; they are shown here only
to describe how the reported values map onto the shared model.

- `max_host_bytes` = peak process **physical footprint**
  (`task_vm_info.phys_footprint`), the whole-process resident + compressed
  memory iOS jetsam kills on. This is the same class of counter the other
  host-reporting platforms use (macOS `phys_footprint`, Linux `VmHWM`,
  Android `Max RSS`, Windows `PeakWorkingSetSize`), so iOS host numbers are
  comparable across runtimes and platforms.
- `max_gpu_bytes` = `null`. There is no second pool to fit into; the host
  footprint already subsumes the Metal/MLX allocations.

An in-process sampler polls the current `task_vm_info.phys_footprint` on a
background thread every **20 ms** across the bracket of one model's load,
prefill, and single decode step, keeping the running max (plus a sample at each
end). It uses the **current** footprint, not the kernel lifetime-max
(`proc_pid_rusage`'s `ri_lifetime_max_phys_footprint` /
`task_vm_info.ledger_phys_footprint_peak`): that value is monotonic over the
process lifetime, so in a long-lived in-process app it would carry peaks from
earlier cells. (The macOS poller *can* use the lifetime-max — safe there, since
each cell is a fresh process.) A `max_memory` run's peak is a sustained plateau
(weights stay resident), not a sub-20 ms transient, so polling lands on it.
Because the current footprint also carries over between in-process cells, the
sampler opens only *after* the process is settled to a clean floor — see §2.3.

### 2.1. The GPU-allocator peak is a diagnostic, not a field

The runtime's own GPU-allocator high-water mark — Metal
`[MTLDevice currentAllocatedSize]` for llama.cpp, MLX `peakMemory` for MLX —
is a useful breakdown of *how much of the footprint the GPU runtime holds*,
but on unified memory it is a subset of the host footprint, not a separate
capacity dimension, so it is **not** reported as `max_gpu_bytes`. It remains
visible in captured runtime output (llama.cpp's own `MTL0 … buffer size`
stderr lines; MLX's peak-memory log) for auditing.

An earlier revision reported that allocator counter *as* the single host
figure. On-device cross-checks showed why that under-counts. The Metal
counter tracks only GPU-side allocations: it matches llama.cpp's announced
`MTL0` buffer sum to ~1 MiB, but omits two slices that are inside
`phys_footprint` — llama's own **CPU-side** buffers (~210 MiB on an 8B model)
and the **non-buffer Metal driver / pipeline-state / residency-set state** the
Metal stack holds on the runtime's behalf (not attributed to any named
buffer). Net, the Metal-only figure runs roughly **7% below `phys_footprint`
on the 8B model and ~17% below on a 230M model** — a larger fraction on the
small model, where fixed runtime overhead weighs more against a small weight
set. And it collapses entirely under **no GPU offload**, where the Metal
allocator holds nothing at all while the process still resides the full model.

The **size** of the process-over-buffers gap is model-specific. Two on-device
points, each reconciled line-for-line against llama.cpp's own buffer log:

- **LFM2.5-8B-A1B (MoE), Q4_K_M:** reported **5473 MiB** vs a buffer total of
  **~5425 MiB** (weights 5114 + KV/RS 24 + compute 286 + output/cache 2) →
  **~48 MiB** of driver/residency + harness overhead.
- **A dense 8B:** ~190 MiB over the buffer total.

Both omitted slices are runtime memory, not app/harness overhead — the harness
floor is ~8 MB (see §2.2). The footprint is the figure a device must actually
fit, so `phys_footprint` is the reported `max_host_bytes`.

### 2.2. The harness floor is left in deliberately

Measured on device, the Pipette app's footprint *before* any model or runtime
work — the harness floor — is ~8 MB (7.7–8.5 MB across runs). We do **not**
subtract it and we do **not** record it as a separate baseline field. It is
left in because it is negligible and because "how much it takes to run the
model" legitimately includes the process the model runs in:

- ~0.1% of a multi-GB run (a large model dwarfs it).
- ~2% of a 230M-param run (the smallest case, where fixed overhead is largest
  relative to the model).

Note this ~8 MB app floor is a different quantity from the runtime-side
slices in §2.1 (llama's CPU-side buffers and the Metal driver / pipeline /
residency state) that the Metal-only counter misses: those are held by the
*runtime*, not the harness. All are already inside the reported
`phys_footprint`; none is subtracted.

### 2.3. Cross-cell isolation (settle-to-floor)

The iOS app runs **every benchmark cell in one long-lived process** (the
desktop paths fork a fresh child per cell and are immune to this).
`phys_footprint` is process-wide, and freed memory is not returned to the OS
promptly — in particular MLX parks dropped weight buffers in its **Metal
buffer cache**, and dirty pages linger until the OS reclaims them. So without
care, a small model measured right after a large one inherits the large one's
un-reclaimed footprint as its "peak" (the fresh-load *bracket* alone does not
reset the process — the pages are still resident when the bracket opens).

Before opening the sampling bracket, `max_memory` therefore settles the
process to a clean floor (`ProcessMemory.settleToFloor`):

1. **Drain caches** — `MLX.GPU.clearCache()`, called regardless of the current
   runtime, so a prior MLX cell's buffer cache can't inflate a following llama
   measurement.
2. **Poll to a plateau** — read `phys_footprint` until it stops falling (three
   consecutive ~50 ms samples within 1 MiB) or a 4 s timeout elapses.

The reported figure is **not** reduced by this floor — the absolute peak is
still what jetsam counts. The floor is *logged alongside* the peak
(`enter=… floor=… peak=…`) so a run whose footprint never fell back to the
harness level — contamination the platform couldn't reclaim in time — is
detectable after the fact.

Verified on device (iPhone 17 Pro, one process, MLX): running the 8B then the
230M model back-to-back, the 230M cell **entered at 5756 MB** (the 8B's
un-reclaimed pages) but the gate scrubbed it and it **reported 462 MB**,
matching the 457 MB the same model reports from a clean process — versus
~5.7 GB (≈12× too high) without the gate.

## 3. Workload

The app loads the model **fresh**, tokenizes a prompt of the requested
prefill length, prefills it, and runs one greedy ignore-end-of-generation
decode step. The single decode step exercises the same first-decode
allocations the `--n-gen 1` flag covers on the CLI paths (see
[main doc §2.4](peak-memory.md#24-workload-phases-captured)).

The fresh load is mandatory: unlike the latency and throughput benchmarks,
which can attach to an already-loaded model, max-memory must observe the
model-load allocations, so the reuse-an-open-model entry point rejects
`max_memory_usage` and the caller uses the fresh-load path
(`LlamaBenchmark.maxMemory` / `MLXBenchmark.maxMemory`), which brackets the
entire load + drive with the sampler. One observation is reported, matching
the single-peak rule for `max_memory_usage`.

## 4. Caveats

- This assumes the model is loaded **without mmap** (the llama path sets
  `use_mmap = false`). With mmap, clean file-backed GGUF pages can be evicted
  without cost and may not all count toward `phys_footprint`, which would
  understate the peak. The counter assumes copied-resident weights.
- The in-process path does not emit the `llama-bench` JSON the CLI paths do,
  but the runtime's stderr (llama.cpp's `model buffer size` / `KV` /
  `compute buffer` lines) is captured and can be audited against the reported
  figure.
- The sampler uses the *polled* `phys_footprint`, deliberately not the kernel
  lifetime-max, which is sticky across cells in a long-lived in-process app.
  The *current* footprint also carries over between cells (freed pages aren't
  reclaimed immediately), so per-cell isolation comes from the settle-to-floor
  gate (§2.3), not from the fresh-load bracket alone.

## 5. Sidecar OS-counter observations

No sidecar. iOS exposes no per-PID GPU memory counter comparable to PDH
(Windows) or DRM fdinfo (Linux), and on unified memory there is no separate
GPU pool to surface alongside the host figure. The GPU-allocator diagnostic
(§2.1) is the only supplementary signal, and it lives in captured runtime
logs, not in `os_counter_observations`.

## 6. Code references

- `ios/Pipette/Pipette/Runtimes/Llama/LlamaBenchmark.swift` (`maxMemory`) —
  llama.cpp fresh-load + prefill + 1-decode bracket.
- `ios/Pipette/Pipette/Runtimes/MLX/MLXBenchmark.swift` (`maxMemory`) — MLX
  fresh-load + prefill + 1-decode bracket.
- `ios/Pipette/Pipette/Runtimes/ProcessMemory.swift` — the in-process
  `phys_footprint` sampler and the `settleToFloor` cross-cell gate (§2.3).
- `ios/Pipette/Pipette/Helpers/HeadlessRunner.swift` — the `memseq` diagnostic
  (`headlessrun memseq models=<big>,<small>`) that reproduces/verifies the gate
  by running `max_memory` across several models in one process.
