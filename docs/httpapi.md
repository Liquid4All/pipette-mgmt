# HTTP API

Base URL: `http://localhost:3000` (configurable via `listen_addr` in config TOML)

## 1. Authentication

Most endpoints require authentication. The following are unauthenticated:
`GET /health`, `POST /clients/register`, `GET /benchmarks`, and
`GET /benchmarks/{benchmark_id}`. See [authentication.md](authentication.md)
for the identity model, required headers, and signing details.

## 1.1. Error responses

Every error response carries the same envelope, a single `error` string:

```json
{"error": "contact_email is not a valid email address"}
```

For a `4xx`, that string describes what the caller got wrong and is safe to
surface to a user. For a `5xx` it is a fixed string — `"internal server error"`
(`500`) or `"upstream request failed"` (`502`) — carrying no detail about the
failure. Diagnosing a `5xx` requires the server log, which records the error in
full; the per-endpoint error tables below list the conditions each status
covers.

The one place an error appears outside this envelope is the per-item `error`
field of a `POST /benchmarks/batch` result (see
[§2.8](#28-post-benchmarksbatch)). It holds the same string, under the same
rules.

## 1.2. Request size limits

A request body is read in full before it is parsed, so each route caps how much
it will read. A body past the cap is rejected with `413` and
`{"error": "request body is too large"}` — the body is never parsed, so a
request that is both oversized and malformed reports only its size.

| Routes | Limit |
|--------|-------|
| `POST /benchmarks`, `POST /benchmarks/batch` | 128 MB |
| Every other route | 64 KB |

The submission routes are the only ones whose body scales with the workload: a
submission carries one `completions` entry per eval sample, and a batch carries
up to 1000 submissions. Everything else carries a fixed set of identity and
device fields, for which 64 KB is far beyond any legitimate request.

## 2. Endpoints

### 2.1. `GET /health`

Health check. **Unauthenticated.**

**Response** `200 OK`

```json
{"status": "ok"}
```

---

### 2.2. `POST /clients/register`

Register a new client. **Unauthenticated.**

#### 2.2.1. Request body

| Field | Type | Required | Description | Example | Suggested source |
|-------|------|----------|-------------|---------|-----------------|
| `public_key` | string | no* | Hex-encoded Ed25519 public key | `"a3f8b2c1..."` | `ed25519` key generation library |
| `generate_key` | bool | no* | If `true`, the server generates a keypair | `true` | — |
| `organization` | string | yes | Organization operating this client | `"LiquidAI"` | Operator-configured |
| `client_details` | string | yes | Freeform description of the client | `"Boston Jetson Orin"` | Operator-configured |
| `contact_email` | string | yes | Contact email for admin approval | `"lab@example.com"` | Operator-configured |
| `preauth_key` | string | no | Pre-auth key (`preauth_{key_id}.{secret}`) from `preauth create`. Valid key → auto-approved (+ seeded tags/org); invalid → `401`/`403`, no client created. See [authentication.md §3.2](authentication.md#32-pre-auth-keys) | `"preauth_a1b2….c3d4…"` | `pipette-mgmt preauth create` |
| `device_name` | string | no | Device model / marketing name | `"MacBook Pro 14-inch (2023)"` | macOS: `sysctl hw.model`; Linux: `/sys/devices/virtual/dmi/id/product_name`; Windows: WMI `Win32_ComputerSystem.Model`; embedded: `/proc/device-tree/model` |
| `device_form_factor` | string | no | One of `phone`, `tablet`, `laptop`, `desktop`, `server`, `embedded` | `"laptop"` | Operator-configured (hardcoded per deployment) |
| `device_os_name` | string | no | OS family | `"macOS"` | macOS: `sw_vers -productName`; Linux: `NAME` in `/etc/os-release`; Windows: `platform.system()` |
| `device_os_version` | string | no | OS version string | `"15.3"` | macOS: `sw_vers -productVersion`; Linux: `VERSION_ID` in `/etc/os-release`; Windows: `platform.version()` |
| `device_chip_model` | string | no | Chip / SoC model | `"Apple M3 Pro"` | macOS (Apple Silicon): `sysctl hw.chip_model`; macOS (Intel): `sysctl machdep.cpu.brand_string`; Linux: `model name` in `/proc/cpuinfo`; Windows: WMI `Win32_Processor.Name` |
| `device_ram_bytes` | int | no | System RAM in bytes | `36000000000` | macOS: `sysctl hw.memsize`; Linux: `MemTotal` in `/proc/meminfo`; Windows: WMI `Win32_ComputerSystem.TotalPhysicalMemory` |
| `device_gpu_model` | string | no | Discrete GPU model; omit or `null` if none | `"NVIDIA RTX 4090"` | NVIDIA: `nvidia-smi --query-gpu=name`; macOS: Metal device API; Linux: `lspci` |
| `device_gpu_vram_bytes` | int | no | Discrete GPU VRAM in bytes | `24000000000` | NVIDIA: `nvidia-smi --query-gpu=memory.total`; macOS: Metal device API |
| `device_npu_model` | string | no | NPU / neural accelerator model; omit or `null` if none | `"Apple Neural Engine"` | Usually hardcoded from chip (Apple Silicon → `"Apple Neural Engine"`; Qualcomm → `"Hexagon DSP"`); vendor SDK otherwise |
| `device_npu_vram_bytes` | int | no | NPU VRAM in bytes; `null` when not separately addressable (e.g. unified memory) | `null` | Vendor SDK; `null` for most unified-memory architectures |
| `capabilities` | string[] | no | Free-form capability flags the client reports directly, e.g. installed runtimes (`"runtime:llama_cpp"`). Must not use a server-owned reserved namespace (see below) | `["runtime:llama_cpp"]` | Client runtime inventory |

\* Exactly one of `public_key` or `generate_key: true` must be provided.

Capability flags participate in job matching (see
[planner.md](planner.md#client-matching-rules)): the server unions them with the
flags it derives from the `device_*` profile to form the client's effective
capability set. Each flag must be in **canonical form** — lowercase with no
whitespace (`400` otherwise) — since matching is exact and the device-derived
flags are canonical. The device-derived namespaces (`os:`, `os_version:`,
`device:`, `chip:`, `form_factor:`, `ram_bytes:`, `gpu:`, `gpu_vram_bytes:`,
`npu:`, `npu_vram_bytes:`) are **reserved**: the server owns them, so a client
may not report them in `capabilities` (doing so is a `400`). Report installed
runtimes as `runtime:<name>` (optionally versioned, e.g. `runtime:llama_cpp:b9999`);
any other canonical string is a free-form flag. For guidance on *which* flags a
client should report — and how this set relates to the one sent to
[`PATCH /clients/me`](#24-patch-clientsme) — see
[client-integration.md §3](client-integration.md#3-choosing-a-capability-set).

#### 2.2.2. Response `201 Created`

Client-generated key:

```json
{
  "client_id": "ev1_a3f8...",
  "status": "pending"
}
```

Server-generated key:

```json
{
  "client_id": "ev1_a3f8...",
  "status": "pending",
  "private_key": "hex-encoded-ed25519-private-key"
}
```

The server-generated form is the only response in the API that returns a
long-lived credential, and the server holds no TLS itself — so this endpoint
depends on the deployment terminating TLS in front of it
([operations.md §5.6](operations.md#56-network-exposure-and-tls)). Generating
the keypair client-side and sending `public_key` keeps the private key off the
wire entirely, which is what the supported clients do.

`status` is normally `"pending"`. It is `"approved"` when the request carries a
valid `preauth_key` ([§3.2](authentication.md#32-pre-auth-keys)) or its
`contact_email` matches an `[auto_approve]` rule
([§3.1](authentication.md#31-auto-approve-rules)).

Registration is **idempotent on the public key**. A repeat request for an
already-registered public key returns `200 OK` with the existing `client_id` and
`status` — it creates nothing and consumes no `preauth_key`. This lets a client
that registered but failed to persist its identity locally retry safely with the
same keypair (and, if it used one, the now-spent `preauth_key`) and recover its
`client_id` without a fresh key.

#### 2.2.3. Errors

| Status | Condition |
|--------|-----------|
| 400 | Missing required fields or both/neither `public_key` and `generate_key` |
| 400 | Invalid `device_form_factor` (must be one of: `phone`, `tablet`, `laptop`, `desktop`, `server`, `embedded`) |
| 400 | `device_gpu_vram_bytes` present without `device_gpu_model` |
| 400 | `device_npu_vram_bytes` present without `device_npu_model` |
| 400 | `device_os_version` present without `device_os_name` |
| 400 | A `capabilities` flag is empty, not canonical (lowercase + no whitespace), or uses a reserved namespace |

(A repeat registration for an already-registered public key is **not** an error —
it returns `200 OK` idempotently; see [§2.2.2](#222-response-201-created).)

---

### 2.3. `GET /clients/me`

Get the authenticated client's profile.

#### 2.3.1. Response `200 OK`

```json
{
  "client_id": "ev1_a3f8...",
  "organization": "LiquidAI",
  "client_details": "Boston Jetson Orin",
  "contact_email": "lab@example.com",
  "status": "approved",
  "tags": ["team-mobile", "us-east"],
  "reindex_pending": false,
  "capabilities": ["runtime:llama_cpp"],
  "device_name": "MacBook Pro 14-inch (2023)",
  "device_form_factor": "laptop",
  "device_os_name": "macOS",
  "device_os_version": "15.3",
  "device_chip_model": "Apple M3 Pro",
  "device_ram_bytes": 36000000000,
  "device_gpu_model": null,
  "device_gpu_vram_bytes": null,
  "device_npu_model": null,
  "device_npu_vram_bytes": null
}
```

Device profile fields are `null` when the client has not yet registered a device
profile via `PATCH /clients/me`. `capabilities` is the set the client reported
directly (empty array when none); the server-derived `device_*` flags are **not**
echoed here — they are visible through the `device_*` fields themselves.

`tags` is read-only and always present (`[]` when the client is untagged). Tags
are assigned by an operator on the mgmt side (`pipette-mgmt clients tag …`); a
client cannot set them via `POST /clients/register` or `PATCH /clients/me`. See
[authentication.md §6](authentication.md#6-client-tags).

`reindex_pending` is `true` while the client's eligible-index re-evaluation is
pending (after a device-profile change or a fresh registration, until the next
`queue-maintenance` run). While `true`, the client has no standing in the
queue — see [§2.4](#24-patch-clientsme). A client may poll this field to watch
for the gate to lift instead of blind-polling `claim`.

#### 2.3.2. Errors

| Status | Condition |
|--------|-----------|
| 401 | Missing or invalid auth headers |

---

### 2.4. `PATCH /clients/me`

Update the authenticated client's profile. `client_details` and the `device_*`
profile fields are mutable; `organization` and `contact_email` are set at
registration and cannot be changed.

Clients may supply device profile fields at registration (`POST /clients/register`)
or update them later via this endpoint. Calling this endpoint on startup to
refresh the profile is recommended but not required. The device profile is used
by the plan assignment system to match jobs to eligible clients (see
[planner.md](planner.md#client-matching-rules)). Clients without a device
profile can still be explicitly listed in job `clients` arrays.

The eligible index is not updated inline — the handler writes the profile and
sets a reindex flag that `queue-maintenance` processes on its next run. The
updated profile is reflected in job offers within one cron interval (typically
one minute). See [planner.md §Eligible index maintenance](planner.md#jobs-created-by-planner)
for the index design and [operations.md §3.2](operations.md#32-eligible-index-maintenance)
for the cron setup.

A device-profile change **voids the client's standing in the queue**: jobs are
matched against the profile, so the server relinquishes every lease the client
holds (each job returns to `avail/` for re-claiming) and refuses all plan
operations until the reindex completes — `claim` returns `204`, and
`heartbeat`, `reclaim`, and plan-attached submissions return `404` (see
§2.7.3, §2.10, §2.11). A client must therefore update its profile **at
startup, before claiming or resuming any work, and do nothing else until the
update completes**.

Only the server can tell whether a PATCH actually *changed* the profile — a
client restarting after a crash resubmits its hardware description without
knowing what the server had stored. The response's `reindex_pending` field
([§2.3.1](#231-response-200-ok)) carries the verdict: when `true`, the
client's standing was voided and it must **discard any locally persisted
in-flight work** — job ids, unsubmitted results — and start fresh, polling
`claim` (or watching `reindex_pending` via `GET /clients/me`) until the gate
lifts, within one cron interval. Work performed under the old profile is
forfeited: a client must not run, resume, or submit work it may not qualify
for under its new profile. When `false`, nothing was voided and the client
may reclaim in-flight work or submit held results as usual.

#### 2.4.1. Request body

All fields are optional; only fields that are present with a value are updated. A
field that is absent **or** `null` leaves the stored value unchanged — device
fields cannot be individually cleared once set (the profile is fixed device
hardware metadata). To reset a profile, re-register the client.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `client_details` | string | no | Updated client description |
| `device_name` | string | no | Device model / marketing name |
| `device_form_factor` | string | no | One of `phone`, `tablet`, `laptop`, `desktop`, `server`, `embedded` |
| `device_os_name` | string | no | OS family (e.g. `"macOS"`, `"Linux"`) |
| `device_os_version` | string | no | OS version string |
| `device_chip_model` | string | no | Chip / SoC model (e.g. `"Apple M3 Pro"`) |
| `device_ram_bytes` | int | no | System RAM in bytes |
| `device_gpu_model` | string | no | GPU model |
| `device_gpu_vram_bytes` | int | no | GPU VRAM in bytes |
| `device_npu_model` | string | no | NPU model |
| `device_npu_vram_bytes` | int | no | NPU VRAM in bytes |
| `capabilities` | string[] | no | Replacement capability set (see note) |

Unlike the per-field `device_*` merge, `capabilities` is **set-granular**: when
present it *replaces* the stored set wholesale (report the full current set),
and when absent (or `null`) the stored set is left unchanged. Note that `[]` is a
*present* value and therefore **clears** the stored set; "leave unchanged" is an
absent key or `null`. Reserved-namespace and empty flags are rejected exactly as
at registration ([§2.2.1](#221-request-body)). A capabilities change voids the
client's queue standing the same way a device-profile change does
(`reindex_pending`). See
[client-integration.md §3](client-integration.md#3-choosing-a-capability-set) for
how a client should choose and maintain the set across registration and PATCH.

#### 2.4.2. Response `200 OK`

Returns the updated client profile (same shape as `GET /clients/me`). Check
`reindex_pending`: `true` means this PATCH (or an earlier, still-unsettled
change) voided the client's queue standing — discard local in-flight work.

#### 2.4.3. Errors

| Status | Condition |
|--------|-----------|
| 400 | Invalid `device_form_factor` (must be one of: `phone`, `tablet`, `laptop`, `desktop`, `server`, `embedded`) |
| 400 | `device_gpu_vram_bytes` present without `device_gpu_model` |
| 400 | `device_npu_vram_bytes` present without `device_npu_model` |
| 400 | `device_os_version` present without `device_os_name` |
| 400 | A `capabilities` flag is empty, not canonical (lowercase + no whitespace), or uses a reserved namespace |
| 401 | Missing or invalid auth headers |

---

### 2.5. `GET /benchmarks`

List the benchmark catalog. **Unauthenticated.** Each entry includes its
`benchmark_id`, `benchmark_type`, and `parameter_`-prefixed fields. Eval-type
benchmarks omit `samples`. See [benchmarks.md](benchmarks.md) for per-type
parameter details.

#### 2.5.1. Query parameters

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `type` | string | no | Filter by `benchmark_type` (e.g. `prefill_throughput`) |

#### 2.5.2. Response `200 OK`

| Field | Type | Description |
|-------|------|-------------|
| `benchmark_id` | string | Unique benchmark identifier |
| `benchmark_type` | string | Type of one of the configured benchmarks (see [`GET /benchmarks`](#25-get-benchmarks) for the live catalog) |
| `parameter_*` | varies | Benchmark parameters (type-specific, see [benchmarks.md](benchmarks.md)) |

---

### 2.6. `GET /benchmarks/{benchmark_id}`

Get benchmark details. **Unauthenticated.** For eval-type benchmarks, samples
are fetched from the evals server and included in the response. Parameters
come from the local catalog.

#### 2.6.1. Path parameters

| Parameter | Type | Description |
|-----------|------|-------------|
| `benchmark_id` | string | Benchmark identifier (e.g. `prefill_throughput_256`) |

#### 2.6.2. Response `200 OK`

Same fields as section 2.5, plus for eval benchmarks:

| Field | Type | Description |
|-------|------|-------------|
| `parameter_max_tokens` | int | Maximum generation length |
| `parameter_mcq_choices` | string[] or null | Valid answer choices (MCQ evals only) |
| `samples` | array | Eval samples from the upstream evals server; only the `samples` field is proxied into this response (see [benchmarks.md](benchmarks.md#25-eval-accuracy)) |

Throughput benchmark:

```json
{
  "benchmark_id": "prefill_throughput_256",
  "benchmark_type": "prefill_throughput",
  "parameter_prefill_tokens": 256
}
```

Eval benchmark:

```json
{
  "benchmark_id": "eval_gpqa_diamond_2026.06.1",
  "benchmark_type": "eval",
  "parameter_eval_id": "gpqa_diamond",
  "parameter_dataset_name": "2026.06.1",
  "parameter_max_tokens": 8192,
  "samples": [
    {
      "id": "a1b2c3d4e5f6",
      "messages": [{"role": "user", "content": "..."}]
    }
  ]
}
```

#### 2.6.3. Errors

| Status | Condition |
|--------|-----------|
| 404 | Benchmark not found |
| 502 | Upstream unreachable (eval benchmarks only) |

---

### 2.7. `POST /benchmarks`

Submit a benchmark result. The single submission endpoint for both
ad-hoc runs and plan-attached runs. Requires an approved client
(a pending client is `403`, or held — see below). Returns the assigned
`job_id`. See
[benchmarks.md](benchmarks.md) for per-type payload fields and
[storage.md](storage.md) for the on-disk layout.

Valid auth headers are always required. An *approved* client's
submissions flow through the normal pipeline. A *pending* client is
normally rejected with `403`; when the server is configured with
`[unverified_submissions] enabled = true`, its submissions are instead
**held** in a write-only archive partitioned by `client_id` (see
§2.7.4) rather than rejected.

The body is a tagged union discriminated by `message_type`:

- `"success"` *(default)* — the benchmark completed. Body carries the
  device / model / runtime context plus the type-specific result
  fields. This is the historical shape of `POST /benchmarks`.
- `"failure"` — the benchmark could not be executed. Body records only
  the model / runtime configuration and a human-readable
  `failure_reason`. No device or result fields are required.

`message_type` may be omitted on success bodies for backward
compatibility. When `job_id` is set the submission is tied to the
plan task obtained via [`POST /plans/claim`](#29-post-plansclaim); the
descriptors are serialized from that claim's `spec.model` / `spec.runtime`.

Unknown fields in a submission body are ignored, never rejected. This is
a compatibility guarantee: a retired field an older client may still send
(e.g. the legacy `plan_id`) must not turn its submission into a `400`, so
the submission parsers deliberately do not reject unknown keys.

Standard deviation is reported per timing field (e.g.
`prefill_time_ms_stddev`, `decode_time_ms_stddev`). The scorer propagates
stddev to both direct and derived metrics — see
[benchmarks.md](benchmarks.md) for per-type fields and propagation formulas.

#### 2.7.1. Success variant

Used when `message_type` is `"success"` or absent. Carries the
device / model / runtime context plus the type-specific result fields.

##### 2.7.1.1. Request body

Per-type result fields (`prefill_time_ms`, `decode_time_ms`,
`completions[]`, etc.) are listed in [benchmarks.md](benchmarks.md).

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `message_type` | string | no | `"success"` (default) — may be omitted on success bodies |
| `job_id` | string or null | no | Job identifier assigned by the planner; included when this submission is tied to a [`POST /plans/claim`](#29-post-plansclaim). Omit for ad-hoc submissions |
| `benchmark_id` | string | yes | Benchmark identifier (e.g. `"prefill_throughput_256"`) |
| `device_name` | string | yes | Device model / marketing name |
| `device_form_factor` | string | yes | One of `phone`, `tablet`, `laptop`, `desktop`, `server`, `embedded` |
| `device_os_name` | string | yes | OS family (e.g. `"Linux"`) |
| `device_os_version` | string | yes | OS version (e.g. `"Ubuntu 22.04"`) |
| `device_chip_model` | string | yes | Chip / SoC model |
| `device_ram_bytes` | int | yes | System RAM in bytes |
| `device_gpu_model` | string or null | no | GPU model; required if `device_gpu_vram_bytes` is set |
| `device_gpu_vram_bytes` | int or null | no | GPU VRAM in bytes; requires `device_gpu_model` |
| `device_npu_model` | string or null | no | NPU model; required if `device_npu_vram_bytes` is set |
| `device_npu_vram_bytes` | int or null | no | NPU VRAM in bytes; requires `device_npu_model` |
| `device_battery_level` | int or null | no | Battery charge (0–100) at run time; null if unavailable. Unknown fields are accepted and ignored by older servers |
| `device_power_state` | string or null | no | Run-environment power state: `charging`, `not_charging`, or `plugged_in_not_charging`. Null if unavailable |
| `device_power_save_mode` | bool or null | no | Whether OS low-power / battery-saver mode was active (can lower CPU clocks) |
| `device_android_cpuset` | string or null | no | Android only. The cpuset cgroup the benchmark process ran under (`/top-app`, `/foreground`, `/moderate`, …); surfaces OEM scheduling demotion |
| `device_android_cpu_affinity_list` | string or null | no | Android only. Allowed CPU list in Linux CPU-list syntax (e.g. `0-5`, `0-3,6-7`) |
| `device_android_cpu_affinity_excludes_top_tier` | bool or null | no | Android only. True when the highest-frequency core tier is absent from the allowed set (the demotion signal) |
| `model_name` | string or null | no | Human-facing model identity / grouping key (e.g. `"llama-3.2-1b"`). Optional — a submission may carry only `model_descriptor`, or both |
| `model_quant` | string or null | no | Convenience quantization label for the primary artifact (e.g. `"q4_0"`). Optional and lossy for multi-artifact models; authoritative per-piece quant lives in `model_descriptor` |
| `model_descriptor` | string (JSON) or null | no | Full, lossless model specification as a JSON **string**. Handles single- and multi-artifact models. Opaque to the server — schema never interpreted, only key-order/whitespace normalized. When present, must be valid JSON (`400` otherwise). Recommended shape: the serialized `pipette-plan-types` `Model` variant ([examples](storage.md#model_descriptor--runtime_descriptor)) |
| `model_flags` | string or null | no | Opaque configuration affecting model behavior. Typically a JSON string going forward (e.g. `{"enable_thinking":true}`) but a plain string is equally valid; never validated or interpreted. Key-order/whitespace normalized when it parses as JSON, stored trimmed-but-verbatim when it doesn't ([why](storage.md#model_flags--runtime_flags)) |
| `model_params_total_millions` | int or null | no | Total parameter count; positive `i32` |
| `model_params_active_millions` | int or null | no | Active parameter count; positive `i32`, must be ≤ `model_params_total_millions` |
| `runtime_name` | string or null | no | Optional, opaque runtime name label (e.g. `"llama.cpp"`); the authoritative identity lives in `runtime_descriptor` |
| `runtime_version` | string or null | no | Optional, opaque runtime version label (e.g. `"b5000"`), stored verbatim; never derived, backfilled, required, or interpreted by the server. Full runtime spec lives in `runtime_descriptor` |
| `runtime_descriptor` | string (JSON) or null | no | Full, lossless runtime specification as a JSON **string**, version baked in. Opaque to the server and normalized like `model_descriptor`. When present, must be valid JSON (`400` otherwise). Recommended shape: the serialized `pipette-plan-types` `Runtime` variant ([examples](storage.md#model_descriptor--runtime_descriptor)) |
| `runtime_flags` | string or null | no | Opaque configuration affecting the runtime itself. Typically a JSON string going forward but a plain string is equally valid; never validated or interpreted. Normalized exactly like `model_flags` ([why](storage.md#model_flags--runtime_flags)) |
| `benchmark_flags` | string (JSON) or null | no | The **resolved** harness configuration the run actually executed under — readiness gating, timeouts, loop detection — as a JSON **string**. Opaque to the server: schema never interpreted, only key-order/whitespace normalized like `model_descriptor`. When present, must be a valid JSON object (`400` otherwise). Resolved, not authored: a client that left a setting unset submits the value it ran with, never `null` ([why](storage.md#benchmark_flags)) |
| `runtime_cpu_variant` | string or null | no | Runtime-selected CPU kernel variant, interpreted per `runtime_name`; for llama.cpp/ggml the `ggml-cpu-<tag>` variant (e.g. `"armv8.2_1"`, `"apple_m2_m3"`). Null on single-static-backend builds |
| `client_version` | string or null | no | Version of the **client build** that produced the run — the harness, not the inference runtime it drove (`runtime_version`). Opaque to the server: stored verbatim, never parsed, ordered, or compared. Must be non-blank when present (`400` otherwise) |

Throughput submission:

```json
{
  "benchmark_id": "prefill_throughput_256",
  "device_name": "Jetson Orin Nano 8GB",
  "device_form_factor": "embedded",
  "device_os_name": "Linux",
  "device_os_version": "Ubuntu 22.04",
  "device_chip_model": "NVIDIA Jetson Orin Nano",
  "device_ram_bytes": 8589934592,
  "model_name": "llama-3.2-1b",
  "model_quant": "q4_0",
  "model_params_total_millions": 1000,
  "runtime_name": "llama.cpp",
  "runtime_version": "b5000",
  "prefill_time_ms": 34.7,
  "prefill_time_ms_stddev": 1.2
}
```

Eval submission:

```json
{
  "benchmark_id": "eval_ifstruct_release_v1_0",
  "device_name": "Jetson Orin Nano 8GB",
  "device_form_factor": "embedded",
  "device_os_name": "Linux",
  "device_os_version": "Ubuntu 22.04",
  "device_chip_model": "NVIDIA Jetson Orin Nano",
  "device_ram_bytes": 8589934592,
  "model_name": "llama-3.2-1b",
  "model_quant": "q4_0",
  "model_params_total_millions": 1000,
  "runtime_name": "llama.cpp",
  "runtime_version": "b5000",
  "completions": [
    {"id": "a1b2c3d4e5f6", "completion": "The answer is B"},
    {"id": "f6e5d4c3b2a1", "completion": "The answer is D"},
    {
      "id": "51cd2cdc9277",
      "completion": "",
      "failed": true,
      "failed_reason": "[2026-03-10T12:04:51Z] llama-server crashed mid-completion: exit signal: 11 (SIGSEGV)"
    }
  ]
}
```

`completions[]` may include optional `failed` (default `false`) and
`failed_reason` (default `null`) fields to flag samples where the
client-side runtime crashed mid-completion. See
[storage.md §4](storage.md#4-submission-contract) for the wire
shape and [scoring-service.md](scoring-service.md) for how mgmt
strips those fields before forwarding to the scoring service.

##### 2.7.1.2. Response `202 Accepted`

For ad-hoc submissions (no `job_id`), a fresh id is assigned (`job-{uuid}`):

```json
{"job_id": "job-550e8400-e29b-41d4-a716-446655440000"}
```

For plan-attached submissions, `job_id` echoes the value from the claim:

```json
{"job_id": "job-550e8400-e29b-41d4-a716-446655440000"}
```

If a submission for this `job_id` already exists (e.g. a zombie client
returning after its lease expired and the job was completed by another client),
the request is accepted with `202` and the result is silently discarded. The
client can treat this response as a normal success.

#### 2.7.2. Failure variant

Used when `message_type` is `"failure"`. Only plan-attached benchmarks
produce failure submissions — ad-hoc runs (no `job_id`) report only
on success — so `job_id` is required here.

##### 2.7.2.1. Request body

A failure body identifies, by `job_id`, which (benchmark, model, runtime)
tuple could not be executed and captures a human-readable reason.
No device, metric, or type-specific fields are accepted — anything
beyond the fields below is ignored by the server. The required
`retriable` flag tells the server whether the failure is specific to the
reporting client (others may still attempt the job) or inherent to the
job itself (a terminal result).

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `message_type` | string | yes | Must be `"failure"` |
| `job_id` | string | yes | Job identifier from the [`POST /plans/claim`](#29-post-plansclaim) response, echoed verbatim. The descriptors are serialized from the claim's `spec.model` / `spec.runtime` (with any local HuggingFace token stripped) |
| `failure_reason` | string | yes | Why the benchmark could not be run. Typically includes a timestamp and the runtime's own error output |
| `retriable` | bool | yes | Whether **another** client may still attempt this job. `true` — the failure is specific to this client (e.g. out of disk, thermal throttle, transient local fault): the server records a `denied/` marker for this client and keeps the job available to others. `false` — the failure is inherent to the benchmark/model/runtime and would recur anywhere: the server records it as the job's terminal result and tears the job down. See [planner.md](planner.md#consequences-of-failure) |
| `benchmark_id` | string | yes | Benchmark that was being attempted |
| `model_name` | string or null | no | Model identity / grouping key. Optional |
| `model_quant` | string or null | no | Convenience quantization label. Optional |
| `model_descriptor` | string (JSON) or null | no | Full, lossless model specification; opaque (schema never interpreted, only normalized). Must be valid JSON when present |
| `model_flags` | string or null | no | Opaque configuration affecting model behavior; a JSON string or a plain string, never validated. Normalized when it parses as JSON, stored trimmed-but-verbatim otherwise |
| `runtime_name` | string or null | no | Optional, opaque runtime name label |
| `runtime_version` | string or null | no | Optional, opaque runtime version label, stored verbatim; never derived or required by the server |
| `runtime_descriptor` | string (JSON) or null | no | Full, lossless runtime specification; opaque (schema never interpreted, only normalized). Must be valid JSON when present |
| `runtime_flags` | string or null | no | Opaque configuration affecting the runtime itself; a JSON string or a plain string, never validated. Normalized exactly like `model_flags` |
| `client_version` | string or null | no | Version of the client build reporting the failure; see the success table above. Worth sending here especially — a failure raises the question of which harness build produced it |

Example:

```json
{
  "message_type": "failure",
  "benchmark_id": "prefill_throughput_256",
  "failure_reason": "[2026-03-10T12:04:51Z] llama-server failed to load model: out of memory",
  "retriable": true,
  "model_name": "llama-3.2-1b",
  "model_quant": "q4_0",
  "model_flags": null,
  "runtime_name": "llama.cpp",
  "runtime_version": "b5000",
  "runtime_flags": null,
  "job_id": "job-550e8400-e29b-41d4-a716-446655440000"
}
```

A **non-retriable** failure (`retriable: false`) is the job's terminal
result: it skips the warehouse / eval-sample-results writes, and the scorer
recognises it by `message_type` and transitions it straight to
`processed/`. `GET /jobs/{job_id}` then reports `status: "failed"` and
surfaces `failure_reason` verbatim.

A **retriable** failure (`retriable: true`) is *not* recorded as a job
result. The server marks the reporting client as denied for the job
(`denied/{job_id}.{client_id}`) and leaves the job in `avail/` for other
eligible clients (see [planner.md](planner.md#consequences-of-failure)). A
later `GET /jobs/{job_id}` still reports the job as in progress / unclaimed,
not failed — unless every eligible client has since reported a retriable
failure, which converts it to a terminal `"All eligible clients reported
failure"`.

##### 2.7.2.2. Response `202 Accepted`

`job_id` equals the `job_id` from the claim:

```json
{"job_id": "job-550e8400-e29b-41d4-a716-446655440000"}
```

Same deduplication applies as for success submissions: if a result for this
`job_id` already exists, the failure is silently discarded.

#### 2.7.3. Errors

| Status | Condition |
|--------|-----------|
| 400 | Invalid `message_type` (must be `"success"` or `"failure"` when present) |
| 400 | `job_id` present but not a well-formed job id (contains characters outside `[A-Za-z0-9-]`) |
| 400 | Missing or invalid `benchmark_id` |
| 400 | Failure: missing `job_id`, `failure_reason`, or `retriable` |
| 400 | Failure: `retriable` present but not a boolean |
| 400 | Success: missing required field (`device_name`, `device_form_factor`, `device_os_name`, `device_os_version`, `device_chip_model`, `device_ram_bytes`, or type-specific fields) |
| 400 | Success: `model_params_total_millions` or `model_params_active_millions` present but not a positive `i32`, or `_active > _total` |
| 400 | Success: invalid `device_form_factor` (must be one of: `phone`, `tablet`, `laptop`, `desktop`, `server`, `embedded`) |
| 400 | Success: `device_gpu_vram_bytes` present without `device_gpu_model` |
| 400 | Success: `device_npu_vram_bytes` present without `device_npu_model` |
| 400 | Success: eval submission: `completions[].id` missing or not a string |
| 400 | Success: eval submission: duplicate `completions[].id` (each id must appear at most once) |
| 400 | Failure: `failure_reason` present on a success body, or `device_*` / metric fields present on a failure body |
| 401 | Missing or invalid auth headers |
| 403 | Client is pending (not approved) and `[unverified_submissions] enabled = false` on this server |
| 404 | Benchmark not found |
| 404 | `job_id` supplied but the client holds no active claim on it (the lease was recycled, the job completed or expired, or it was never claimed). The client should [`POST /plans/{job_id}/reclaim`](#211-post-plansjob_idreclaim) and re-submit if reclaim succeeds |
| 404 | `job_id` supplied while the client's profile re-evaluation is pending (`reindex_pending`) — a profile change ([§2.4](#24-patch-clientsme)) relinquishes the client's leases, voiding the claim; the result is forfeited and the claim is never renewed |
| 409 | `job_id` supplied but the job is leased to a **different** client (the caller was superseded and should abort) |

A submission that omits `job_id` (an ad-hoc run; the server mints one) is never
subject to the `404`/`409` claim-binding checks — there is no claim to bind.
Held submissions from a pending client (§2.7.4) also skip the check: they are
written to the client-partitioned `unverified/` tree, not the shared
`incoming/` and `processed/` keys, so there is nothing to hijack.

#### 2.7.4. Unverified (held) submissions

When the server is configured with `[unverified_submissions] enabled = true`,
a submission from a **pending** (unapproved but validly-signed) client is
*held* instead of rejected with `403`. The body is validated with exactly
the same rules as an approved submission (§2.7.1 / §2.7.2) and written to:

`submissions/unverified/{client_id}/{job_id}.json`

These held submissions are **never scored** while they sit there: the
scorer, the warehouse, and the `fix-*` family never read the unverified
tree. The `client_id`, `submitted_at`, and `benchmark_type` server fields
are injected as usual, and `job_id` is taken from the submission when the
client supplied one (a plan-attached run echoing its claim) or assigned by
the server otherwise — exactly as on the normal path. The stored
`client_id` is the caller's real id, so the archive is partitioned per
client.

An operator resolves a client's held submissions out-of-band once the
client is approved (or rejected):

- `pipette-mgmt unverified promote --client-id <id>` re-stages them into
  the normal pipeline (`success` → `incoming/`, `failure` → `processed/`).
- `pipette-mgmt unverified delete --client-id <id>` discards them.

See [cli.md](cli.md) and [storage.md §4.1](storage.md#41-unverified-submissions).

##### 2.7.4.1. Response `202 Accepted`

```json
{"job_id": "job-550e8400-e29b-41d4-a716-446655440000"}
```

The `job_id` is a receipt for operator triage. It is **not** resolvable
via [`GET /jobs/{job_id}`](#212-get-jobsjob_id) until the submission is
promoted — that endpoint resolves ids only once they reach the normal
pipeline (`submissions/incoming/`, `submissions/score-queue/`, or
`submissions/processed/`), never the held `unverified/` tree.

##### 2.7.4.2. Errors

Held submissions use the same `400` / `404` validation table as §2.7.2 —
holding changes only the write destination, not what counts as a valid
body.

---

### 2.8. `POST /benchmarks/batch`

Submit multiple benchmark results in a single request. Each element of
the `submissions` array is validated and written independently with the
same rules as [`POST /benchmarks`](#27-post-benchmarks).

Authentication and disposition follow [`POST /benchmarks`](#27-post-benchmarks):
the whole batch is processed under the single authenticated client. An
approved client's items flow through the normal pipeline; a pending
client's items are all **held** under
`submissions/unverified/{client_id}/{job_id}.json` (see §2.7.4) when
`[unverified_submissions] enabled = true`. There is no per-item mixing.

**Per-item failures are swallowed.** Unlike `POST /benchmarks`, this
endpoint returns `200 OK` even when some submissions fail validation or
fail to write. Each element of the returned `results` array reports
either a `job_id` (success) or an `error` string (failure) tagged with
its original `index`. Callers must inspect every item to know what
actually succeeded.

The whole request only fails (4xx) when the request envelope itself is
bad: missing/invalid auth headers, a pending client when
`[unverified_submissions] enabled = false`, missing `submissions`
array, empty array, or more than 1000 items.

#### 2.8.1. Request body

| Field | Type | Description |
|-------|------|-------------|
| `submissions` | array | 1–1000 submission objects, each with the same shape as the body of `POST /benchmarks` |

Example:

```json
{
  "submissions": [
    {
      "benchmark_id": "prefill_throughput_256",
      "device_name": "Jetson Orin Nano 8GB",
      "device_form_factor": "embedded",
      "device_os_name": "Linux",
      "device_os_version": "Ubuntu 22.04",
      "device_chip_model": "NVIDIA Jetson Orin Nano",
      "device_ram_bytes": 8589934592,
      "model_name": "llama-3.2-1b",
      "model_quant": "q4_0",
      "model_params_total_millions": 1000,
      "runtime_name": "llama.cpp",
      "runtime_version": "b5000",
      "prefill_time_ms": 34.7
    },
    {
      "benchmark_id": "decode_throughput_64",
      "device_name": "Jetson Orin Nano 8GB",
      "device_form_factor": "embedded",
      "device_os_name": "Linux",
      "device_os_version": "Ubuntu 22.04",
      "device_chip_model": "NVIDIA Jetson Orin Nano",
      "device_ram_bytes": 8589934592,
      "model_name": "llama-3.2-1b",
      "model_quant": "q4_0",
      "model_params_total_millions": 1000,
      "runtime_name": "llama.cpp",
      "runtime_version": "b5000",
      "decode_time_ms": 12.4
    }
  ]
}
```

#### 2.8.2. Response `200 OK`

| Field | Type | Description |
|-------|------|-------------|
| `results` | array | One entry per input submission, in the same order |
| `results[].index` | integer | Position of the submission in the request array |
| `results[].job_id` | string or absent | Assigned job id (`job-{uuid}`) when the submission was accepted |
| `results[].error` | string or absent | Human-readable reason when the submission was rejected |

Exactly one of `job_id` or `error` is present per entry.

Example with a mix of successes and failures:

```json
{
  "results": [
    {"index": 0, "job_id": "job-550e8400-e29b-41d4-a716-446655440000"},
    {"index": 1, "error": "missing field: model_name"},
    {"index": 2, "error": "benchmark not found"}
  ]
}
```

#### 2.8.3. Errors

These apply to the request envelope; per-item failures are reported
inside `results` (see above) rather than as a 4xx status.

| Status | Condition |
|--------|-----------|
| 400 | Missing `submissions` array |
| 400 | Empty `submissions` array |
| 400 | More than 1000 submissions in one request |
| 401 | Missing or invalid auth headers |
| 403 | Client is pending (not approved) and `[unverified_submissions] enabled = false` on this server |

---

### 2.9. `POST /plans/claim`

Claim the next available job for execution. Requires an approved client
(403 if pending). The server checks the client's eligibility index, selects
one available job, leases it to the calling client, and returns the lease
envelope and the job's run specification. See [planner.md](planner.md) for the
job lifecycle and storage layout.

The client must run the benchmark and report the outcome via
[`POST /benchmarks`](#27-post-benchmarks), echoing the `job_id`. Its
`model_descriptor` / `runtime_descriptor` are serialized from the same
`spec.model` / `spec.runtime` the claim carried, so they are the echo the server's
claim-binding check expects. Failures use the same endpoint with
`message_type: "failure"`. While running, the client must send periodic
heartbeats via [`PUT /plans/{job_id}/heartbeat`](#210-put-plansjob_idheartbeat)
at an interval of half the `time_window`.

#### 2.9.1. Request body

Empty (`{}` or no body).

#### 2.9.2. Response `200 OK`

A server-owned **envelope** wrapped around the **`spec`** the planner authored.
The envelope is the small set of fields the management server acts on — identity,
lease, expiry — and `spec` is the run specification, forwarded verbatim: the
server stores it without interpreting it. Clients must tolerate unrecognized
fields in both. Benchmark parameters and eval samples are not included — call
[`GET /benchmarks/{benchmark_id}`](#26-get-benchmarksbenchmark_id) if needed.

The job's eligibility fields (`clients`, `requires`, `any_of`) are **not**
returned. They are inputs to selection and already spent by the time a job is
handed out.

| Field | Type | Description |
|-------|------|-------------|
| `job_id` | string | Job identifier, assigned by the server at ingestion |
| `benchmark_id` | string | Benchmark identifier. Always equal to `spec.benchmark` — the server lifts it from there, so the two cannot disagree |
| `time_window` | string | Lease increment as an ISO 8601 duration (e.g. `"PT5M"`), **set by the server** from its configured lease duration. Each heartbeat extends the lease by this much; the client should heartbeat at half this interval. On the idempotent-claim path (§2.9.1) it is instead the *remaining* life of the lease the client already holds |
| `expires_at` | string, optional | ISO 8601 basic-format timestamp (`20240908T000000Z`) after which the job will no longer be assigned. **Absent** when the job never auto-expires |
| `spec` | object | The run specification — see below |

`spec` is `pipette-plan-types`' `ClientRunSpec`, authored by `pipette-plan` and
passed through unchanged:

| Field | Type | Description |
|-------|------|-------------|
| `benchmark` | string | Benchmark identifier; must equal the envelope's `benchmark_id` |
| `model` | object | Full, lossless model specification, internally tagged on `type` (`gguf_text`, `gguf_vision`, `mlx`, `torch`, `apple_foundation_text`) with the source coordinates flattened alongside |
| `runtime` | object | Full, lossless runtime specification, internally tagged on `type`, with version/build/flavor coordinates flattened alongside |
| `model_flags` | object, optional | Model-generation flags for this `(benchmark, model)`. Eval-only. Omitted when unset |
| `runtime_flags` | object, optional | Runtime load flags for this `(benchmark, runtime, model)`. Omitted when unset |
| `benchmark_flags` | object, optional | HTTP timeout, doom-loop, and readiness settings for this cell. Omitted when unset |

Each flag group carries its own `benchmark_type` / `runtime_type` / `model_type`
discriminants, which must agree with the `benchmark`, `model`, and `runtime` the
spec names. The flag groups **reject unrecognized keys**; `spec`, `model`, and
`runtime` tolerate them. See [planner.md](planner.md) for the full schema.

Example:

```json
{
  "job_id": "job-550e8400-e29b-41d4-a716-446655440000",
  "benchmark_id": "eval_ifbench_2026.06.1",
  "time_window": "PT10M",
  "expires_at": "20240908T000000Z",
  "spec": {
    "benchmark": "eval_ifbench_2026.06.1",
    "model": {
      "type": "gguf_text",
      "source": "huggingface",
      "org": "unsloth",
      "repo_name": "Qwen3.5-0.8B-GGUF",
      "path": "Qwen3.5-0.8B-Q4_0.gguf"
    },
    "runtime": {
      "type": "llamacpp_cli_stock_tools",
      "source": "github_release",
      "repository_url": "github.com/ggml-org/llama.cpp",
      "repository_version": "b9050",
      "flavor": "macos-arm64"
    },
    "model_flags": {
      "model_type": "gguf_text",
      "benchmark_type": "eval",
      "enable_thinking": true
    },
    "runtime_flags": {
      "runtime_type": "llamacpp_cli_stock_tools",
      "model_type": "gguf_text",
      "benchmark_type": "eval",
      "number_gpu_layers": 99,
      "threads": 8
    },
    "benchmark_flags": {
      "runtime_type": "llamacpp_cli_stock_tools",
      "model_type": "gguf_text",
      "benchmark_type": "eval",
      "http_timeout_seconds": 600,
      "doomloop": {
        "exact_repeat": {
          "window": 4096,
          "min_period": 32,
          "required": 3,
          "min_chars": 256
        }
      }
    }
  }
}
```

A claim whose `spec` the client cannot read — absent, unparseable, naming a
benchmark that disagrees with the envelope, or pairing an incompatible model and
runtime — is **mis-authored, not transient**. The client reports it as a
`retriable: false` failure rather than letting the lease lapse, since retrying it
anywhere would fail identically.

#### 2.9.3. Response `204 No Content`

Returned when no job is currently available for this client. Possible reasons:

- No jobs in the queue list this client as eligible.
- All eligible jobs are currently leased to other clients.
- All eligible jobs have passed their `expires_at`. An expired job is never
  handed out, even if `queue-maintenance` has not yet removed it from the queue
  (see [planner.md §Expiration](planner.md#expiration)).
- The client is suspended (it claimed a new job while holding an unexpired
  lease on a previous one — see [planner.md](planner.md) for details).
  The response is identical to the no-job case; clients do not need to
  distinguish the reason. An operator must clear the flag with
  `pipette-mgmt clients unsuspend` before the client can claim again.

The body is empty. The client should wait approximately 5 minutes before
retrying, plus a random jitter of 0–60 seconds to avoid synchronized polling
bursts from multiple clients. A different interval may be used if explicitly
configured.

#### 2.9.4. Errors

| Status | Condition |
|--------|-----------|
| 401 | Missing or invalid auth headers |
| 403 | Client is pending (not approved) |

---

### 2.10. `PUT /plans/{job_id}/heartbeat`

Renew the lease on an active job. The client must call this endpoint at an
interval of half the `time_window` (e.g. every 5 minutes for a 10-minute
window) to signal that it is still running the benchmark. Each successful
heartbeat extends the lease by another `time_window` from the current time.

Two responses indicate the lease is no longer valid and the client should
abort the benchmark:

- **`404 Not Found`** — no active lease exists for this `job_id`. The lease
  expired and the cron has recycled the job back to `avail/`. If the client
  has been running the benchmark throughout (e.g. due to a network outage), it
  should try [`POST /plans/{job_id}/reclaim`](#211-post-plansjob_idreclaim)
  before giving up — if the job is still unclaimed the reclaim will succeed and
  the client can continue without restarting. If the reclaim fails, the client
  should abort and re-poll via `POST /plans/claim`.

  `404` is also returned — without renewing the lease — while the client's
  profile re-evaluation is pending: a profile change
  ([§2.4](#24-patch-clientsme)) relinquishes every lease the client holds, so
  there is nothing it may legitimately renew. A client heartbeating in this
  state is violating the protocol (profile updates happen at startup, before
  any work); it should abort and re-poll.
- **`409 Conflict`** — the job is leased to a different client. The client is
  running a zombie benchmark — it has been superseded — and should abort
  immediately. Any result it submits will be silently discarded.

A heartbeat never fails because the job has passed its `expires_at`. The
deadline governs when a job may be *assigned*, not how long a client that already
holds it may run, so a benchmark already in progress is allowed to finish — such
a job is expired only after its lease lapses and it returns to the queue (see
[planner.md §Expiration](planner.md#expiration)).

#### 2.10.1. Path parameters

| Parameter | Type | Description |
|-----------|------|-------------|
| `job_id` | string | Job identifier assigned by the planner |

#### 2.10.2. Request body

Empty (`{}` or no body).

#### 2.10.3. Response `200 OK`

Empty body.

#### 2.10.4. Errors

| Status | Condition |
|--------|-----------|
| 400 | `job_id` in the path contains characters outside `[A-Za-z0-9-]` |
| 401 | Missing or invalid auth headers |
| 403 | Client is pending (not approved) |
| 404 | No active lease found for this `job_id` — lease was reaped; client should try `POST /plans/{job_id}/reclaim`, then abort and re-poll if that also fails |
| 404 | Client's profile re-evaluation is pending — its leases were relinquished by the profile change ([§2.4](#24-patch-clientsme)); the lease is never renewed. Heartbeating in this state is a protocol violation; abort and re-poll |
| 409 | Lease belongs to a different client — client is a zombie and should abort |

---

### 2.11. `POST /plans/{job_id}/reclaim`

Re-acquire the lease on a job the client was previously running. Intended for
recovery after a network outage: if a client's heartbeats could not be sent
while it continued running the benchmark and the lease was reaped in the
meantime, this endpoint lets the client reclaim the same job rather than
abandoning the work.

The server first checks whether the calling client still holds a lease on the
job. If it does — for example the lease expired but `queue-maintenance` has not
yet recycled it back to `avail/` — the server renews that lease and returns
`200`. Otherwise it re-acquires the job from `avail/` using the same atomic
rename and the same eligibility and `denied/` checks as
[`POST /plans/claim`](#29-post-plansclaim). Requires an approved client (403 if
pending); a suspended client receives `403`.

While the client's profile re-evaluation is pending (`reindex_pending`, see
[§2.4](#24-patch-clientsme)) the whole endpoint returns `404` without renewing
anything: the profile change relinquished the client's leases and its
eligibility is not yet known, so there is nothing it may resume or re-acquire.
Work from before the profile change is forfeited.

A job that has passed its `expires_at` is treated as gone and returns `404`,
even if `queue-maintenance` has not yet removed it — an expired job is
indistinguishable from one that no longer exists (see
[planner.md §Expiration](planner.md#expiration)). This applies only to the
re-acquire path; if the client still holds the lease, the job is in progress and
is allowed to finish — see [§2.10](#210-put-plansjob_idheartbeat).

#### 2.11.1. Path parameters

| Parameter | Type | Description |
|-----------|------|-------------|
| `job_id` | string | Job identifier the client wishes to reclaim |

#### 2.11.2. Request body

Empty (`{}` or no body).

#### 2.11.3. Response `200 OK`

Lease successfully re-acquired. The client may continue running the benchmark
and must resume sending heartbeats immediately. The response body is empty —
the client already has the job JSON from the original claim.

#### 2.11.4. Errors

| Status | Condition |
|--------|-----------|
| 400 | `job_id` in the path contains characters outside `[A-Za-z0-9-]` |
| 401 | Missing or invalid auth headers |
| 403 | Client is pending (not approved), or client is suspended |
| 404 | Job cannot be re-acquired and is not leased to anyone — it is past its `expires_at`, was completed, or was re-acquired and finished by another client; client should abort and re-poll |
| 404 | Client's profile re-evaluation is pending (`reindex_pending`) — its leases were relinquished by the profile change ([§2.4](#24-patch-clientsme)); prior work is forfeited. Client should abort and re-poll |
| 409 | Job is currently leased to a different client; client should abort |

---

### 2.12. `GET /jobs/{job_id}`

Get the status of a job. The server verifies the job belongs to the authenticated
client. If the job is processed, metrics are included in the response.

For plan-attached submissions, `job_id` is the same identifier returned by
[`POST /plans/claim`](#29-post-plansclaim), so no separate tracking is needed.

Held submissions (see §2.7.4) return a `job_id` in their `202` response
but are not retrievable through this endpoint until promoted. The id is
a receipt for operator triage, not a lookup key. This endpoint resolves
ids under `submissions/incoming/`, `submissions/processed/`, and the
`submissions/score-queue/` stages (reported as `status: "scoring"`).

#### 2.12.1. Path parameters

| Parameter | Type | Description |
|-----------|------|-------------|
| `job_id` | string | Job id (`job-{uuid}` for server-minted jobs) |

#### 2.12.2. Response `200 OK`

| Field | Type | Description |
|-------|------|-------------|
| `job_id` | string | Job id (`job-{uuid}` for server-minted jobs) |
| `benchmark_id` | string | Benchmark identifier |
| `benchmark_type` | string | Benchmark type |
| `status` | string | `incoming` (awaiting processing), `scoring` (an eval in the score-queue awaiting/finishing the scoring-service call), `processed` (scored), or `failed` (client-side failure record; not scored) |
| `submitted_at` | string | ISO 8601 submission time |
| `scored_at` | string or null | ISO 8601 scoring time (present only when `status: "processed"`) |
| `score_runtime_version` | string or null | Version of the evals server that scored this job (present only for eval benchmarks when `status: "processed"`; null otherwise) |
| `metrics` | array or null | Benchmark metrics (present only when `status: "processed"`) |
| `failure_reason` | string | Client-supplied failure reason (only when `status: "failed"`) |
| `model_name`, `model_quant`, `runtime_name`, `runtime_version` | string or null | Model / runtime identity echoed verbatim from the failure record (only when `status: "failed"`); each is null when the failure record omitted it |
| `client_version` | string or null | Version of the client build that reported the failure, echoed verbatim (only when `status: "failed"`); null when the failure record omitted it. A failure is never scored, so this response is the only place it is readable — successes carry it in the warehouse `client_version` column instead |
| `metrics[].metric` | string | Metric name (e.g. `prefill_throughput`). See [benchmarks.md](benchmarks.md) for per-type metric definitions |
| `metrics[].value` | number | Metric value |
| `metrics[].value_stddev` | number or null | Standard deviation, propagated from the submitted per-field stddev. Present on both direct and derived metrics when the input stddev is available. See [benchmarks.md](benchmarks.md) for propagation formulas. Null when the submission did not include stddev for the relevant timing field |
| `metrics[].unit` | string | Unit of measurement (e.g. `tokens/sec`) |

Incoming job (awaiting processing):

```json
{
  "job_id": "job-550e8400-...",
  "benchmark_id": "prefill_throughput_256",
  "benchmark_type": "prefill_throughput",
  "status": "incoming",
  "submitted_at": "2026-03-10T12:01:00Z",
  "scored_at": null,
  "score_runtime_version": null,
  "metrics": null
}
```

Processed job (with metrics from Parquet):

```json
{
  "job_id": "job-550e8400-...",
  "benchmark_id": "prefill_throughput_256",
  "benchmark_type": "prefill_throughput",
  "status": "processed",
  "submitted_at": "2026-03-10T12:01:00Z",
  "scored_at": "2026-03-10T12:02:30Z",
  "score_runtime_version": null,
  "metrics": [
    {"metric": "ttft", "value": 34.7, "value_stddev": 1.2, "unit": "ms"},
    {"metric": "prefill_throughput", "value": 7373.0, "value_stddev": 255.1, "unit": "tokens/sec"}
  ]
}
```

Processed eval job (with score runtime version):

```json
{
  "job_id": "661f3a00-...",
  "benchmark_id": "eval_ifstruct_release_v1_0",
  "benchmark_type": "eval",
  "status": "processed",
  "submitted_at": "2026-03-15T10:00:00Z",
  "scored_at": "2026-03-15T10:01:15Z",
  "score_runtime_version": "1.2.3",
  "metrics": [
    {"metric": "accuracy", "value": 0.6667, "value_stddev": null, "unit": "ratio"}
  ]
}
```

Failed job (client posted a `message_type: "failure"` body):

```json
{
  "job_id": "7c2a1d00-...",
  "benchmark_id": "prefill_throughput_256",
  "benchmark_type": "prefill_throughput",
  "status": "failed",
  "submitted_at": "2026-04-02T09:30:00Z",
  "failure_reason": "runtime crashed: OOM at decode step",
  "model_name": "mlx-community/Qwen3.5-4B-4bit",
  "model_quant": "4bit",
  "runtime_name": "mlx-lm",
  "runtime_version": "0.26.0",
  "client_version": "0.14.2"
}
```

`failed` is terminal — the body sits in `processed/` (the failure
record went straight there from the handler) and the scorer never
touches it. No `scored_at`, `score_runtime_version`, or `metrics`
fields are present.

#### 2.12.3. Errors

| Status | Condition |
|--------|-----------|
| 400 | `job_id` in the path contains characters outside `[A-Za-z0-9-]` |
| 401 | Missing or invalid auth headers |
| 404 | Job not found |

---

### 2.13. `GET /jobs/{job_id}/eval-sample-results`

Get the eval sample results for a processed eval job. Returns the prompt,
completion, and correctness for each sample in the dataset. Only available
for eval-type benchmarks that have been scored.

#### 2.13.1. Path parameters

| Parameter | Type | Description |
|-----------|------|-------------|
| `job_id` | string | Job id (`job-{uuid}` for server-minted jobs) |

#### 2.13.2. Response `200 OK`

```json
[
  {
    "id": "a1b2c3d4e5f6",
    "messages": [{"role": "user", "content": "..."}],
    "completion": "The answer is B",
    "is_correct": true,
    "failed": false,
    "failed_reason": null
  },
  {
    "id": "51cd2cdc9277",
    "messages": [{"role": "user", "content": "..."}],
    "completion": "",
    "is_correct": false,
    "failed": true,
    "failed_reason": "[2026-03-10T12:04:51Z] llama-server crashed mid-completion: exit signal: 11 (SIGSEGV)"
  }
]
```

| Field | Type | Description |
|-------|------|-------------|
| `id` | string | Sample ID |
| `messages` | array | Prompt messages as served by the evals server (`[{"role": "...", "content": "..."}]`). Stored as a JSON-encoded string in Parquet; the API deserializes it to an array in the response |
| `completion` | string | Model-generated text from the submission |
| `is_correct` | boolean | Whether the evals server scored this sample as correct. Failed samples (`failed: true`) almost always come back `false` because their completion was empty |
| `failed` | boolean | `true` if the client-side runtime crashed mid-completion for this sample (see [pipette-clients#103](https://github.com/Liquid4All/pipette-clients/pull/103)). Defaults to `false`. Pre-feature parquet files read back as `false` |
| `failed_reason` | string \| null | Free-form, human-readable description of the failure when known. `null` for `failed: false` rows |

#### 2.13.3. Errors

| Status | Condition |
|--------|-----------|
| 400 | `job_id` in the path contains characters outside `[A-Za-z0-9-]` |
| 401 | Missing or invalid auth headers |
| 404 | Job not found, job does not belong to client, job is not processed, or job is not an eval benchmark |
