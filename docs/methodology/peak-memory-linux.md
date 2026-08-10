# Peak Memory — Linux

Per-platform measurement details for the
[Peak Memory Usage methodology](peak-memory.md). Cross-platform invariants,
field definitions, and the cross-platform empirical reference live in the
parent document.

**Status:** ⌛ host implemented; in-process GPU probes proposed.

## 1. Host total: `wait4 ru_maxrss × 1024`

Equivalent to `/proc/<pid>/status:VmHWM`. Linux's `ru_maxrss` is in KiB
per the Linux/POSIX convention; multiply by 1024 to get bytes.

`phys_footprint`-equivalent counters that include GPU-driver kernel memory
do not exist on Linux, so the host counter and any in-process GPU probe
report disjoint bytes — host and GPU are separate pools and no subtraction
is applied. `max_host_bytes + max_gpu_bytes` is the total memory pressure.

## 2. GPU: in-process probes (not yet implemented)

Each runtime API needs its own probe:

| Runtime | Candidate probe | Notes |
|---|---|---|
| Vulkan | `VK_LAYER_*` library wrapping `vkAllocateMemory` / `vkBindBufferMemory` | Stable layer ABI; ~300 LoC. Covers llama.cpp Vulkan, ggml Vulkan backend. |
| CUDA | `LD_PRELOAD` interpose of `cuMemAlloc` / `cudaMalloc` / `cudaMallocAsync` | Fragile across CUDA toolkit versions; covers PyTorch CUDA, llama.cpp CUDA. |
| HIP | `LD_PRELOAD` interpose of `hipMalloc` family | Same shape as CUDA, separate lib. |
| SYCL / Level Zero | `ZE_LOADER_*` layer or `LD_PRELOAD` of `zeMemAlloc*` | Less mature surface. |

Until the relevant probe lands for a given runtime, `max_gpu_bytes` is
`null` and the diagnostic OS counters below carry the closest available
signal.

When a future Linux Vulkan path lands it will either reuse the Windows
PDH-style "OS attribution as primary" pattern (preferred, no injection;
see [peak-memory-windows.md](peak-memory-windows.md)) or fall back to an
in-process Vulkan layer — the decision will follow empirical comparison
on representative hardware.

## 3. Sidecar OS-counter observations

When `max_gpu_bytes` is `null` because no in-process probe is available,
the parent still spawns a per-platform OS-counter poller alongside the
child and records its readings in `MemoryReport::os_counter_observations`.
These flow into `extras.json` under `os_counter_observations`, with one
record per source that produced data.

These observations are **diagnostic**: cross-validation of the in-process
probe (when one runs alongside on a development box), an operator-facing
hint for "what is the GPU actually using even though we report null," and
a path to retrofit historical runs once a real probe lands.

### 3.1. DRM `fdinfo` poller (every 20 ms)

| Source label | Path / format |
|---|---|
| `drm_fdinfo_vram` | sum of `drm-memory-vram` across all DRM fds in `/proc/<pid>/fdinfo/<fd>` |
| `drm_fdinfo_gtt` | sum of `drm-memory-gtt` (system memory the GPU has access to) |

`drm-memory-cpu` is intentionally excluded — CPU-only memory the GPU never
touches.

### 3.2. NVIDIA poller (every ~500 ms)

When `/proc/driver/nvidia` exists, an additional poller fires (subprocess
fan-out is expensive; cheap counters stay at 20 ms):

| Source label | Path / format |
|---|---|
| `nvidia_smi` | `nvidia-smi --query-compute-apps=pid,used_memory --format=csv,noheader,nounits`, filtered to child PID, MiB → bytes |

CUDA workloads do **not** open DRM file descriptors (CUDA bypasses DRM and
opens `/dev/nvidia*` directly), so DRM fdinfo will report nothing for a
CUDA-only child even on a system with the open NVIDIA kernel module
present. This is by design — `nvidia_smi` is the appropriate signal for
that case.

### 3.3. Known caveats motivating sidecar (not primary) status

- `nvidia-smi --query-compute-apps` reports a snapshot at query time, not
  the runtime's high-water mark. Even with frequent polling, the value
  lags the GPU allocator's true peak, and the overhead numbers mix
  runtime and driver state in ways that aren't comparable across drivers /
  GPU vendors.
- DRM `drm-memory-gtt` mixes GPU-mapped system memory with Vulkan staging
  buffers under one number; `drm-memory-vram` doesn't separate cleanly
  when a process holds memory on multiple adapters.
