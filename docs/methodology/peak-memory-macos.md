# Peak Memory — macOS (Metal)

Per-platform measurement details for the
[Peak Memory Usage methodology](peak-memory.md). Cross-platform invariants,
field definitions, and the cross-platform empirical reference live in the
parent document.

**Status:** ✅ implemented.

macOS targets are Apple Silicon (M1–M5) only — **unified memory with no
host/GPU accounting split**. Per the
[unified-vs-split producer rule](peak-memory.md#12-unified-vs-split-memory-the-producer-rule),
the whole cost is reported as `max_host_bytes` and `max_gpu_bytes = null`;
the Metal allocator peak is captured as a **diagnostic**, not a reported
field. (This is a re-baseline: an earlier revision of this doc reported the
Metal `[MTLDevice currentAllocatedSize]` peak *as* `max_gpu_bytes`. It is now
`null`, because on unified memory the Metal allocation is a subset of
`phys_footprint`, not a second pool a device must separately fit.)

## 1. Host total: `phys_footprint`

Use the kernel's lifetime-max `phys_footprint` ledger entry, queried via
`proc_pid_rusage` with `RUSAGE_INFO_V4`:

```c
#include <libproc.h>
struct rusage_info_v4 ri;
proc_pid_rusage(pid, RUSAGE_INFO_V4, (rusage_info_t *)&ri);
uint64_t peak = ri.ri_lifetime_max_phys_footprint;
```

Two viable variants:

1. **Polling.** Sample `ri.ri_phys_footprint` every 20–50 ms while the child
   is alive and take the max. Robust; gives identical results to the lifetime
   max when polling is fast enough. Recommended.
2. **Lifetime max.** Read `ri.ri_lifetime_max_phys_footprint` at any point
   while the child is alive (one final query just before `waitpid` is enough).
   The kernel maintains the all-time maximum.

`phys_footprint` is what `/usr/bin/time -l` reports as "peak memory footprint"
and what Activity Monitor's "Memory" column reads. It is the kernel's
authoritative ledger of dirty anonymous pages + IOKit-mapped pages +
compressed pages charged to the process.

**Do not use `wait4`/`rusage.ru_maxrss` on macOS for this benchmark.** It
under-counts because it (a) excludes IOKit-charged GPU driver state and (b)
omits virtual reservations that were allocated but not yet faulted. On a
typical Metal run `ru_maxrss` reads 5–10% below `phys_footprint` and 5–7%
below the GPU buffer sum that the workload itself reports — i.e., it is
empirically wrong in the conservative direction and is not portable across
runs.

## 2. GPU diagnostic: `[MTLDevice currentAllocatedSize]` via DYLD shim

This counter is a **diagnostic**, not the reported `max_gpu_bytes` (which is
`null` on unified memory — see the re-baseline note above and
[§1.2 of the main doc](peak-memory.md#12-unified-vs-split-memory-the-producer-rule)).
It is captured to see *how much of the host footprint the Metal allocator
holds* and to cross-check against the runtime's announced buffers.

There is no per-process Metal memory query externally available on macOS that
is comparable to `nvidia-smi --query-compute-apps`. The clean path is to use
Apple's public Metal API from inside the inference process via a small
`DYLD_INSERT_LIBRARIES` shim that polls
[`MTLDevice.currentAllocatedSize`](https://developer.apple.com/documentation/metal/mtldevice/currentallocatedsize)
and writes the peak to a parent-supplied tempfile.

Reference shim (≈70 lines, builds with `clang -dynamiclib -framework Metal
-framework Foundation`):

```objc
#import <Metal/Metal.h>
#include <stdatomic.h>
#include <fcntl.h>

static _Atomic uint64_t peak_alloc = 0;
static _Atomic int done = 0;
static char outpath[4096];

static void write_snapshot(void) {
    if (outpath[0] == '\0') return;
    int fd = open(outpath, O_WRONLY | O_TRUNC | O_CREAT, 0600);
    if (fd < 0) return;
    char buf[256];
    int n = snprintf(buf, sizeof(buf),
        "metal_peak_allocated_bytes=%llu\n"
        "metal_unified=%d\n",
        (unsigned long long)atomic_load_explicit(&peak_alloc, memory_order_relaxed),
        /* unified flag */ 1);
    if (n > 0) (void)!write(fd, buf, (size_t)n);
    close(fd);
}

static void *poll(void *_) {
    NSArray<id<MTLDevice>> *devs = MTLCopyAllDevices();
    while (!atomic_load_explicit(&done, memory_order_relaxed)) {
        uint64_t total = 0;
        for (id<MTLDevice> d in devs) total += d.currentAllocatedSize;
        uint64_t prev = atomic_load_explicit(&peak_alloc, memory_order_relaxed);
        if (total > prev) {
            atomic_store_explicit(&peak_alloc, total, memory_order_relaxed);
            write_snapshot();   // <-- truncate-write on every grow
        }
        usleep(20000);
    }
    return NULL;
}

static void on_exit(void) {
    atomic_store_explicit(&done, 1, memory_order_relaxed);
    write_snapshot();
}

__attribute__((constructor))
static void init(void) {
    const char *env = getenv("PIPETTE_MEMPROBE_OUT");
    if (!env || !*env) return;          // dormant if not requested
    strncpy(outpath, env, sizeof(outpath) - 1);
    atexit(on_exit);
    pthread_t t; pthread_create(&t, NULL, poll, NULL);
}
```

Spawn the child with both env vars set:

```sh
DYLD_INSERT_LIBRARIES=/path/to/peakmtl.dylib \
PIPETTE_MEMPROBE_OUT=/tmp/run-XXXX/peak \
DYLD_FORCE_FLAT_NAMESPACE=0 \
  ./inference-binary <args>
```

After the child exits, read the tempfile and parse the `key=value` lines.
The truncate-write-on-grow contract guarantees the file always contains
the latest peak even under abnormal exits — CPython + MLX in particular
calls `_exit()`, which **skips both `atexit` and
`__attribute__((destructor))`**, so the on-grow writes are how we
recover the high watermark in that case.

The parent never reads the child's stderr to recover the peak — the
runtime's own stderr (llama.cpp init logs, llama-bench progress) passes
through unfiltered.

**Reference implementation:** the production shim is at
`pipette-clients/crates/pipette-memprobe/peakmtl/peakmtl.m`. Parent-side
extraction, env wiring, and parsing live in
`pipette-clients/crates/pipette-memprobe/src/probes/metal.rs`
(`MetalProbe` implementing the `Probe` trait).

### 2.1. Why this gives the right number

Empirically (Qwen3-0.6B Q4_K_M, llama.cpp b8797, M3 Max, `--mmap 0`):

| n_prompt | Σ MTL0 buffers from llama.cpp stderr (`%.2f MiB`) | `[MTLDevice currentAllocatedSize]` peak | Δ |
|---|---|---|---|
| 256 | 550.03 MiB (≈ 576,802,591 B) | 578,109,440 B (551.33 MiB) | **+1.30 MiB** |
| 2048 | 895.40 MiB (≈ 938,894,950 B) | 940,261,376 B (896.70 MiB) | **+1.30 MiB** |
| 8192 | 1567.40 MiB (≈ 1,643,538,022 B) | 1,644,904,448 B (1568.70 MiB) | **+1.30 MiB** |

The shim is the byte-exact figure (it sums real `currentAllocatedSize`
counters); the announced-sum column is only as precise as llama.cpp's
`%.2f MiB` print (≈ ±0.015 MiB per term), so the Δ is reliable to
±0.05 MiB.

The Metal API consistently reads about **1.30 MiB above** the sum of
GGML-named buffers (model + KV + compute), and the gap is constant across
prompt sizes despite KV growing 32×. That fixed overhead is real Metal
driver state — the default command queue, internal heaps, residency-set
objects, automatic resources for the device's first use — that is not
attributed to any named GGML buffer. The constant-with-respect-to-KV
behavior confirms the attribution.

### 2.2. Reporting `max_host_bytes`; `max_gpu_bytes = null`

```
max_host_bytes    = phys_footprint peak               (from the host poller)
max_gpu_bytes     = null                              (unified memory, no separate GPU pool)
max_npu_bytes     = null
# diagnostic, logged but not reported as a field:
metal_peak_allocated_bytes = … (from the shim)
```

On Apple UMA, Metal shared-mode `MTLBuffer` pages live in the unified DRAM
pool and *do* show up in `phys_footprint`, so the Metal allocation is a
**subset** of `max_host_bytes`, not a separate pool. Reporting the shim's
peak as `max_gpu_bytes` would label a slice of the host footprint as if it
were a second capacity dimension a device must independently fit, so per
the [unified-vs-split rule](peak-memory.md#12-unified-vs-split-memory-the-producer-rule)
it is `null` and the shim's `metal_peak_allocated_bytes` is kept only as a
logged diagnostic. `max_host_bytes` is the single device-fit number: "the
largest this process ever was according to the OS." The Metal diagnostic
remains useful for seeing how much of that footprint the Metal allocator
holds and for the cross-check against the runtime's announced buffers.

### 2.3. Verified at the OS level

vmmap/footprint output on the same run, with all numbers reconciling to
within rounding:

```
phys_footprint                                     = 1778 MB
  untagged (VM_ALLOCATE) dirty                     = 1435 MB  ← buffer-dirty
  MALLOC_LARGE + MALLOC_SMALL + metadata           =  189 MB  ← heap
  Owned physical footprint (unmapped, graphics)    =  144 MB  ← Metal driver
  IOAccelerator + small misc                       =   10 MB
```

Σ MTL buffers announced by llama.cpp (1567 MB) − dirty buffer pages
(1435 MB) = 132 MB of **virtual reservations that never became dirty**
(model alignment slop and worst-case compute scratch the scheduler reserves
but does not touch). Both `phys_footprint` and the Metal API ignore these
virtual-only pages — `phys_footprint` because it is a dirty/IOKit ledger,
the Metal API because `currentAllocatedSize` counts allocations, not page
faults. The two numbers therefore agree on the same memory model.

### 2.4. Polling cadence and failure-detection signal

The shim polls `[MTLDevice currentAllocatedSize]` at 20 ms intervals. This
is sufficient for inference workloads because llama.cpp's allocation
pattern is dominated by **persistent** buffers — model weights are
allocated at load time, KV cache at first-context creation, compute
scratch at `sched_reserve` — and all three live until process exit. The
peak is steady-state, not transient: any sample taken after the graph is
reserved captures the true peak. For a `-p 256 -n 100 -r 1` run that
takes hundreds of milliseconds end-to-end, 20 ms gives 15+ samples,
every one of them after peak.

**The empirical confirmation that polling is not the limiting factor**
is the +1.30 MiB delta in the table above being **constant** across
prompt sizes 256 / 2048 / 8192, even though KV cache grows 32× across
those points. If the cadence were missing transient peaks, the delta
would be variable and prompt-size-correlated; instead it's a fixed
driver-state offset.

**Failure-detection signal for operators**: if the shim reports a value
**below** `Σ MTL0 buffer size` from llama.cpp's verbose output (the
`load_tensors:`, `llama_kv_cache:`, `sched_reserve:` lines), polling has
missed a peak. The expected direction is shim ≈ Σ + ~1 MiB driver
overhead. Anything else means either:

1. The workload has a transient peak shorter than the poll interval
   (uncommon for steady-state inference; investigate the workload, not
   the shim, before tightening the cadence).
2. The model architecture doesn't emit the standard buffer-size lines
   (LFM2 in current llama.cpp doesn't), so the cross-check isn't
   available — fall back to comparing across multiple prompt sizes and
   verifying the delta is constant.

If a real workload ever trips signal #1, three escalations in order:

- Tighten `usleep(20000)` → `usleep(5000)` in `peakmtl/peakmtl.m`. 4×
  sample density, no behavior risk; on-grow snapshot writes only fire
  on actual peak growth so the I/O cost stays trivial.
- Add a synthetic warmup pass to ensure the worst-case allocation has
  been reached before the timed window — `llama-bench` already does
  one, so this only matters for custom runners.
- As a last resort, replace polling with allocator interposition
  (Objective-C method swizzling on `MTLDevice newBufferWithLength:` and
  `MTLBuffer dealloc`). ~200 lines, gives byte-perfect peaks regardless
  of cadence, but fragile across macOS major versions and outside
  Apple's supported API surface. We don't ship this today because the
  polling check above hasn't tripped.

## 3. Caveats

- **`DYLD_INSERT_LIBRARIES` is ignored** on binaries with Hardened Runtime +
  Library Validation, on `setuid` binaries, and on SIP-protected system
  binaries. The shim's constructor never runs in that case, the tempfile
  stays empty, and the parent surfaces an actionable error
  ("DYLD_INSERT_LIBRARIES was likely blocked by Hardened Runtime / Library
  Validation; see 'Caveats' below for diagnosis"). The reported result is
  unaffected — `max_host_bytes` comes from the host poller and
  `max_gpu_bytes` is `null` regardless — only the GPU-allocator diagnostic
  is lost. Verify on each runtime version bump that the tempfile is
  non-empty after a successful run if you rely on the diagnostic.
- **Multi-GPU systems** (Mac Pro towers with eGPU or AMD discretes) have
  more than one `MTLDevice`. The shim sums `currentAllocatedSize` across all
  devices for the diagnostic; single-GPU Apple Silicon writes
  `metal_devices=1` in the snapshot. (A discrete-GPU Mac would be a *split*
  device under the producer rule; the M1–M5 unified targets this doc covers
  are not.)
- **The shim runs in every child of the inference process** that inherits
  `DYLD_INSERT_LIBRARIES`. `llama-bench` has no children today, but if a
  future runtime forks workers, scope the env var to the right process or
  filter by parent PID.

## 4. Reference numbers

Same machine and config as the table above:

```
max_host_bytes =   1,135,084,912  (1082.5 MiB)  ← phys_footprint peak, the reported field
max_gpu_bytes  =   null                          ← unified memory, no separate GPU pool
max_npu_bytes  =   null
host_method    =   phys_footprint
gpu_method     =   null

# diagnostic only (not a reported field):
metal_peak_allocated_bytes = 940,261,376  (896.7 MiB)  ← Metal allocator high-water mark
```

`max_host_bytes` is the full `phys_footprint` peak — the figure a device
must actually fit. The Metal allocator high-water mark (896.7 MiB) is a
subset of it, logged as a diagnostic, not subtracted from it and not
reported as `max_gpu_bytes`.

## 5. Sidecar OS-counter observations

No sidecar — the in-process Metal probe is the canonical signal. macOS does
not expose a per-PID GPU memory counter that's comparable to PDH (Windows) or
DRM fdinfo (Linux), so there's nothing to surface alongside.

## 6. Cross-runtime consistency

The reported field, `max_host_bytes` (`phys_footprint`), is the same
physical quantity for both Apple clients (pipette-llamacpp and pipette-mlx)
and is directly comparable across them. The Metal allocator **diagnostic**
is likewise read the same way (Metal-driver-level total allocation via
`[MTLDevice currentAllocatedSize]`), so the diagnostic series are comparable
too — but neither is reported as `max_gpu_bytes`, which is `null` for both.

Empirical Δ between the shim diagnostic and each client's own internal
accounting on the same workload (Qwen3-0.6B Q4_K_M, n_prompt=2048, M3 Max):

| Client | In-runtime counter | Shim peak | Δ (shim − counter) |
|---|---|---|---|
| pipette-llamacpp | Σ MTL0 buffers from llama.cpp stderr (1 announced sum) | 940,261,376 B | **+1.30 MiB** |
| pipette-mlx | `mx.get_peak_memory()` = 1,459,462,300 B | 1,462,288,384 B | **+2.69 MiB** |

The shim consistently reads a small fixed amount above each runtime's own
allocator counter — it captures Metal driver state (default command
queue, internal heaps, residency-set objects) that the runtime's
allocator doesn't attribute to its own buffers. The Δ is larger on MLX
than on llama.cpp because Python + mlx-lm + tokenizer + HTTP machinery
loads more Metal infrastructure than a bare `llama-bench`. The
constant-with-respect-to-prompt-size property still holds within each
client.
