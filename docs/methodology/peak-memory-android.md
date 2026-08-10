# Peak Memory — Android

Per-platform measurement details for the
[Peak Memory Usage methodology](peak-memory.md). Cross-platform invariants,
field definitions, and the cross-platform empirical reference live in the
parent document.

**Status:** ✅ host implemented; GPU intentionally `null`.

## 1. Host total

Same as Linux — `wait4 ru_maxrss × 1024`. Bionic's `ru_maxrss` follows the
Linux convention of KiB. The Android client wraps the `llama-bench`
invocation in `toybox time -v` and parses the `Max RSS (KiB)` line from its
summary.

The wrapped child runs in its own process group (via `pre_exec` →
`setpgid(0,0)`) so the parent's deadline killer can SIGKILL the whole
group (toybox + llama-bench child) when a deadline fires; killing only
toybox would orphan the wrapped llama-bench process.

### 1.1. The toybox wrapper doesn't add overhead

Verified by polling `/proc/<pid>/status:VmHWM` from an external observer
on a bare (non-toybox) `llama-bench` run against a Samsung S24 Ultra
build: the externally-polled peak agrees with `toybox time -v`'s
`Max RSS` to ≤ 0.02% (both read the same kernel `mm->hiwater_rss`
watermark). The toybox wrapper `fork()`s, the child `execve()`s into
`llama-bench`, and `wait4` returns rusage for the *child* only, not the
toybox parent — so the wrapper contributes nothing to the reported
number.

## 2. GPU: vendor-fragmented, deliberately not measured

`max_gpu_bytes` is `null` for the Android arm64-v8a CPU build, which is
what the upstream llama.cpp Android target uses today (CPU-only inference
on the device's big.LITTLE cluster). When a future Android target moves
to a GPU backend, the probe path depends on vendor:

| GPU | Candidate path | Notes |
|---|---|---|
| Mali (ARM) | Vendor-specific `mali_kbase` ioctls; no upstream DRM fdinfo | Fragmented across vendors / Android versions. |
| Adreno (Qualcomm) | KGSL legacy or MSM DRM (newer kernels) | If MSM DRM: same fdinfo path as Linux AMD/Intel. |
| PowerVR (Imagination) | Vendor SDK | Rare in current devices. |

`max_gpu_bytes = null` is likely to remain the right answer for Mali on most
Android devices for the foreseeable future — the ioctl interfaces are not
stable and not all OEMs ship the Android-side telemetry.

## 3. Empirical reference (Samsung S25 Ultra, Snapdragon 8 Elite)

llama.cpp `bench-tools-20260415-38cc8e3fd` android-arm64-v8a, LFM2-350M-Q4_K_M,
`--n-prompt N --n-gen 1 -r 1 --mmap 0`:

| Ctx | `max_host_bytes` | Σ announced runtime allocations | Note |
|---:|---:|---:|---|
| 256 | 348.83 MiB | 338.24 MiB | +10.59 MiB binary/libs/heap overhead |
| 1024 | 367.95 MiB | 413.24 MiB | runtime reserves more than is faulted |
| 2048 | 430.34 MiB | 425.24 MiB | +5.10 MiB overhead |

The n=1024 row is the "virtual reservations that never become dirty" case
documented in [§2.5 of the main doc](peak-memory.md#25-runtime-allocator-accounting-vs-process-level-rss)
— `sched_reserve` sizes the compute scratch for worst-case (132 MiB on
this run), but at n=1024 only a fraction of those pages get page-faulted
in. `wait4 ru_maxrss` reflects resident-set high water, not the runtime's
announced sum. At n=2048 the workload touches the full reservation and
the two figures align.

## 4. Sidecar OS-counter observations

The same DRM fdinfo poller that powers the Linux sidecar (see
[peak-memory-linux.md](peak-memory-linux.md)) is available on Android in
principle, but no current target lights it up:

- The CPU-only arm64-v8a build doesn't open DRM fds — there's nothing
  for the poller to read.
- Mali (the dominant Android GPU vendor) doesn't expose stable DRM fdinfo;
  the `mali_kbase` ioctl interface would need a vendor-specific reader
  rather than the shared Linux one.

When the Android target eventually grows GPU support, the OS-counter path
will be co-designed with that target's vendor in mind.
