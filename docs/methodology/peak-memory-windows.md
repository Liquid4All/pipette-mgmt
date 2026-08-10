# Peak Memory — Windows

Per-platform measurement details for the
[Peak Memory Usage methodology](peak-memory.md). Cross-platform invariants,
field definitions, and the cross-platform empirical reference live in the
parent document.

**Status:** ✅ implemented (Vulkan / HIP / SYCL / ARM64-CPU flavors all
share the same path).

Windows uses two parent-side counters and **no in-process injection**:

- `max_host_bytes` ← `PROCESS_MEMORY_COUNTERS.PeakWorkingSetSize` (PSAPI),
  read post-exit via a separately-held `PROCESS_QUERY_LIMITED_INFORMATION`
  handle.
- `max_gpu_bytes` ← `\GPU Process Memory(pid_<PID>_*)\Total Committed`
  (PDH), polled at 20 ms while the child runs and tracked as a peak.

Both work at any process integrity level — there's no equivalent of the
Vulkan loader's elevation guard in PDH or PSAPI. The implementation lives in
`pipette-llamacpp/src/execute/max_memory_usage/{windows,pdh_poller}.rs`.

## 1. Host total: `GetProcessMemoryInfo.PeakWorkingSetSize`

The kernel's lifetime-max working-set counter, equivalent to Task Manager's
"Peak Working Set" column. Read once after `wait_with_output` returns —
no polling needed because `PeakWorkingSetSize` is itself a lifetime maximum
the kernel maintains continuously. PSAPI counts pages physically resident
for this process: binary `.text`/`.data`, loaded DLLs, libc heap, stack,
and `Vulkan_Host`-class mirror pages (host-coherent GPU buffers the runtime
allocates from system memory).

GPU-driver-managed memory in the BIOS-reserved carve-out (UMA) or VRAM
(discrete) is **not** in PWSS. Host and GPU are separate pools on every
Windows flavor — the two peaks are reported independently and the
consumer-side "will this fit on a device with N bytes?" question requires
summing them on discrete; on UMA the `Shared Usage` portion of
`max_gpu_bytes` overlaps with `max_host_bytes`, so summing slightly
over-counts
(see [§1.1 of the main doc](peak-memory.md#11-how-to-interpret-the-fields)).

Reading after `wait_with_output` requires a separately-held process handle
because `std::process::Child` closes its own handle on reap; an extra
`OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` taken before the wait
keeps the kernel `EPROCESS` object alive past `wait4` so the lifetime-max
counters survive the post-exit query.

## 2. GPU total: PDH `\GPU Process Memory(pid_<PID>_*)\Total Committed`

WDDM tracks per-process GPU memory through a stable Win32 performance
counter set, `\GPU Process Memory(...)`. The same data Task Manager's "GPU
Memory" column displays. Polled at 20 ms from the parent process via
`PdhOpenQueryW` + `PdhAddEnglishCounterW` + `PdhCollectQueryData` +
`PdhGetFormattedCounterArrayW`. The instance wildcard `pid_<PID>_*` resolves
on every sample, so a child that hasn't allocated GPU memory yet (the first
~100 ms after spawn) gets no data; subsequent samples pick up the instance
once WDDM registers it.

PDH exposes five relevant counters per `(PID, physical-adapter)` pair:

| Counter | What it represents |
|---|---|
| `Dedicated Usage` | Memory the OS classifies as GPU-only: VRAM on discrete; BIOS-reserved carve-out on UMA. |
| `Shared Usage` | System RAM the GPU has access to. On UMA this is the host-coherent / `Vulkan_Host`-class portion. On discrete it's staging buffers. |
| `Local Usage` | Memory from the adapter's local segment (≡ Dedicated on this driver). |
| `Non Local Usage` | Memory from non-local segments (≡ system RAM on discrete; 0 on UMA). |
| **`Total Committed`** | **Per-tick joint sum** of Dedicated + Shared — what we report. |

**Why `Total Committed` and not `Dedicated + Shared` summed independently:**
Dedicated and Shared peak at *different moments* in a typical inference
run. Taking each counter's lifetime max and summing them double-counts the
non-coincident portions. `Total Committed` tracks the instantaneous joint
sum and reports its peak — the correct reading of "what was the highest
GPU memory pressure at any single moment."

Empirically on Strix Halo (Radeon 890M iGPU, AMDVLK driver 32.0.23002.1006,
LFM2-350M-Q4_K_M, `--n-prompt 256 --n-gen 1 -r 1`):

| Counter (peak alone) | Value |
|---|---:|
| Dedicated Usage | 301.17 MiB |
| Shared Usage | 86.54 MiB |
| `Dedicated + Shared` (peaks summed independently) | 387.71 MiB ← **over-reports** |
| Local Usage | 375.96 MiB ← ≡ Total Committed on this driver |
| Non Local Usage | 0 MiB |
| **Total Committed** | **375.96 MiB ← reported as `max_gpu_bytes`** |

The 11.75 MiB gap between the independent-peak sum (387.71) and Total
Committed (375.96) is the temporal-misalignment cost of the naive
sum-of-maxes — Total Committed is the honest joint peak.

### 2.1. Relationship to the runtime's announced allocations

`Total Committed` includes **WDDM driver-state attribution** beyond what
the GPU API (Vulkan / D3D12 / etc.) exposes: command-buffer pools,
descriptor heaps, paging-table entries, residency-set scratch, shader
bytecode caches. On Strix Halo this overhead grows modestly with context
length / allocation count — not a strict constant:

| Ctx | `max_gpu_bytes` (Total Committed) | Σ Vulkan allocations announced by llama.cpp | Δ (PDH − announced) |
|---:|---:|---:|---:|
| 256 | 375.95 MiB | 342.75 MiB | +33.2 MiB |
| 1024 | 458.27 MiB | 423.24 MiB | +35.0 MiB |
| 2048 | 481.68 MiB | 437.24 MiB | +44.4 MiB |

The driver-state Δ is in the **33–44 MiB range** across n_prompt =
256→2048: roughly the same order of magnitude across context lengths,
but not bit-stable. It grows mildly with the number / size of
allocations because the driver allocates additional command buffers,
descriptor pools, and per-allocation tracking metadata.

The runtime's announced sum (parsed from captured `llama-bench --verbose`
stderr) gives the allocator-level view; PDH gives the OS-attribution view.
The latter is the correct answer to "will this fit on a card with N MiB of
GPU memory" because the driver state has to live somewhere on the device.

### 2.2. Per-process attribution under concurrent GPU use

PDH attributes GPU memory **strictly per-process** on WDDM, with no
cross-process page sharing for Vulkan compute allocations. Verified
empirically on Strix Halo + AMDVLK with `pipette-clients/tools/pdh-multiproc-exp/`,
which spawns N concurrent `llama-bench` instances and compares Σ-per-PID
peaks against the adapter's `\GPU Adapter Memory(*)\Total Committed`
delta from baseline:

| Configuration | Σ per-PID peaks | Adapter Δ (peak − baseline) | Ratio |
|---|---:|---:|---:|
| 1 × `--n-prompt 256` | 375.96 MiB | 375.98 MiB | 1.000 |
| 2 × `--n-prompt 256` (concurrent) | 751.91 MiB (each = 375.96 MiB) | 751.91 MiB | 1.000 |

Two concurrent processes each consume their full per-process peak — the
33–44 MiB driver-state overhead is **per-process and additive**, not
amortized across processes. The byte-for-byte match between the
per-PID sum and the adapter delta confirms WDDM does no cross-process
de-duplication for these allocations (no shared shader-cache pages, no
shared command-buffer pools).

**Caveat — at higher GPU memory pressure:**

| Configuration | Σ per-PID peaks | Adapter Δ | Note |
|---|---:|---:|---|
| 3 × `--n-prompt 256` | 375.96 MiB (1 PID only) | 375.97 MiB | other 2 PIDs read 0 |
| 2 × `--n-prompt 2048` (concurrent) | 482 MiB (1 PID only) | 482 MiB | other PID reads 0 |

When the workload set exceeds the GPU's effective memory carve-out (Strix
Halo's default ~2 GiB UMA reservation), the OS appears to serialize the
processes: only one is active in PDH at a time. The per-process
attribution remains exact for whichever process is currently allocating;
the experiment can't distinguish "the other process never ran" from "the
other process completed before PDH registered its instance" without
additional instrumentation (exit codes, per-process timing). This caveat
matters only for benchmark hosts deliberately running concurrent GPU
workloads — single-bench operation is unaffected.

The reproducible harness lives at `pipette-clients/tools/pdh-multiproc-exp/`.

### 2.3. Why not an in-process Vulkan layer

An earlier implementation used a vendored Vulkan layer DLL
(`peakvk.dll`) injected into the child via `VK_LAYER_PATH` +
`VK_INSTANCE_LAYERS` env vars, hooking `vkAllocateMemory` /
`vkFreeMemory`. It was byte-exact to the Vulkan allocator (matching the
runtime's announced sum within ~3% of constant driver-state overhead) but
had three material drawbacks PDH avoids:

1. **Vulkan-only.** Each GPU API (Vulkan, HIP, SYCL, D3D12) would have
   needed its own interposer crate and DLL. PDH is API-agnostic — one
   path covers every Windows GPU runtime WDDM tracks. HIP and SYCL
   flavors get `max_gpu_bytes` populated with no extra code.
2. **Elevation-sensitive.** The Vulkan loader's
   `loader_running_with_secure_environment` filters `VK_LAYER_PATH` and
   `VK_INSTANCE_LAYERS` when the process token is at High integrity
   level — typical for Windows OpenSSH sessions of Administrators-group
   users, services running as admin, and binaries with
   `requireAdministrator` manifests. The workaround was a
   per-bench-run write into
   `HKLM\SOFTWARE\Khronos\Vulkan\ExplicitLayers` (system-wide
   visibility while the bench ran; stale-entry cleanup needed after
   crashes; required either elevation or a vacuum routine). PDH reads
   regardless of process integrity level.
3. **Build complexity.** Required MSVC `cl.exe` on every build host to
   compile the layer DLL via `cc-rs`, plus a `peakvk.cpp` source with
   manually-vendored Vulkan types. PDH is pure Win32 from Rust via
   `windows-sys`.

The accuracy difference is structural: the Vulkan probe under-reports
real GPU pressure by the size of driver state (~33–44 MiB on Strix
Halo); PDH over-reports the allocator-level view by the same amount.
Neither is "wrong" — they answer different questions. The
allocator-level number stays available to operators via the captured
stderr in `extras.json` (`load_tensors:`, `llama_kv_cache:`,
`sched_reserve:` lines).

## 3. Sidecar OS-counter observations

Windows has no sidecar — PDH `\GPU Process Memory(pid_<PID>_*)\Total
Committed` is the **primary** source for `max_gpu_bytes`. The earlier
"sidecar" framing of PDH was based on an assumption that the
authoritative number would come from an in-process Vulkan layer; the
elevation-guard / build-complexity / GPU-API-coverage trade-offs of that
approach landed us on PDH instead.

Operators who want allocator-level numbers (excluding WDDM driver-state
attribution) can parse them from the runtime's verbose stderr lines —
captured in `extras.json` under `outcome.stderr`. For llama.cpp Vulkan
those are `load_tensors: Vulkan0 model buffer size = …`,
`llama_kv_cache: Vulkan0 KV buffer size = …`,
`sched_reserve: Vulkan0 compute buffer size = …`, etc.
