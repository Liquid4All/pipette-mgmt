# Storage

## 1. Overview

This document defines the storage contract for `pipette-mgmt`.

The current backends are local filesystem and S3 (including S3-compatible
stores like MinIO and R2). The storage boundary is designed so that backends
can be added without changing the HTTP API or the logical data model.

The storage design must preserve these properties:

- one logical data model regardless of backend
- immutable submission payloads after write
- submission state represented by location, not by a mutable field in the JSON
- scored output treated as canonical (warehouse for aggregate metrics, eval
  sample results for per-sample eval outcomes)
- clear separation between storage domains with distinct access patterns
- backend-specific details contained behind store interfaces

## 2. Logical layout and invariants

The logical layout is the storage contract.

For filesystem-backed storage, this maps naturally to directories and files. For
other backends, these are logical namespaces and records rather than a literal
directory tree.

```text
data/
├── benchmarks/
│   └── {benchmark_id}.toml
├── clients/
│   └── {client_id}.json                 ← identity record (no tags field)
├── tags-index/                          ← tag membership, two mirrored trees
│   ├── by-client/
│   │   └── {client_id}/
│   │       └── {tag}                    ← empty marker (client → tags)
│   └── by-tag/
│       └── {tag}/
│           └── {client_id}              ← empty marker (tag → clients)
├── preauth/
│   └── {key_id}.json                    ← pre-auth key (stores sha256(secret))
├── signature-migration/
│   └── {client_id}.json                 ← written once, when a client first signs a v1 payload; its presence refuses that client the timestamp-only fallback
├── plans/
│   └── {plan_id}.json                   ← plan manifest (identity, lifecycle, progress); durable, outlives its jobs
├── cancelled_plans/
│   └── {plan_id}                        ← empty marker: cancel requested, teardown pending
├── model_params_mapping.toml
├── locks/
│   ├── mutate.lock                   # process-submissions / fix-* / requeue-eval
│   └── score-eval.lock               # serializes score-eval runs only
├── submissions/
│   ├── incoming/
│   │   └── {job_id}.json
│   ├── score-queue/                  # eval scoring pipeline (per-job JSON)
│   │   ├── to_do/                    #   awaiting the scoring-service call
│   │   │   └── {job_id}.json
│   │   └── to_finalize/              #   scored; { submission, score } awaiting warehouse write
│   │       └── {job_id}.json
│   ├── processed/
│   │   └── {job_id}.json.gz
│   └── unverified/
│       └── {client_id}/
│           └── {job_id}.json
├── todo/                                ← [todo_storage] backend
│   ├── tmp/                             ← planner writes job files here first
│   ├── avail/
│   │   └── {job_id}.{expires_at}.json  ← atomic rename from tmp/; expires_at is ISO 8601 or `never`
│   ├── eligible/
│   │   └── clients/
│   │       └── {client_id}/
│   │           └── {job_id}.{expires_at}   ← marker: client may claim this job; encodes job expiry
│   ├── leased/
│   │   └── {client_id}/
│   │       └── {job_id}.{lease_expiry}.json  ← renamed from avail/ on claim; partitioned by client
│   ├── denied/
│   │   └── {job_id}.{client_id}        ← empty marker: client failed this job
│   ├── pending-reindex/
│   │   └── {client_id}.{uuid}          ← one marker per reindex request (PATCH /clients/me); consumed by queue-maintenance
│   └── suspended/
│       └── {client_id}.json            ← client suspended for operator review
└── warehouse/
    ├── results/
    │   └── benchmark_id={benchmark_id}/
    │       └── client_id={client_id}/
    │           ├── day={YYYY-MM-DD}/      # new writes
    │           └── month={YYYY-MM}/       # legacy, frozen
    │               └── part-*.parquet
    └── eval_sample_results/
        └── {job_id}.parquet
```

The important contract is:

- benchmark definitions are keyed by `benchmark_id`
- client records are keyed by `client_id`
- tags are **not** stored on the client record. Each `(client, tag)` membership
  is an empty leaf marker in two mirrored trees under `tags-index/`, so both
  lookup directions are a single listing (the filenames are the data — nothing
  to (de)serialize): `tags-index/by-client/{client_id}/{tag}` (forward: a
  client's tags) and `tags-index/by-tag/{tag}/{client_id}` (reverse: a tag's
  clients). Tags are flat and `client_id` has no `/`, so every key is exactly
  two segments. The indexes are kept **out of** the `clients/` prefix so
  listing client records never enumerates tag markers. The **forward tree is
  authoritative** (`by-client/…`); the reverse (`by-tag/…`) is a derived
  accelerator. Mutations commit the forward marker first, so a crash between the
  two writes can only leave the reverse stale, never the truth. `delete_client`
  clears both trees. `reindex_tags` reconciles the reverse tree back to the
  forward truth (and drops markers for deleted clients — `clients/{id}.json` is
  the existence authority); it is idempotent
- submission records are keyed by `(state, job_id)`. `incoming` and
  `processed` are flat keyspaces over `job_id`; `unverified` is keyed
  by `(client_id, job_id)` so a client's held submissions can be
  promoted or deleted as a unit. For plan-attached submissions,
  `job_id` is the `job-{UUIDv7}` id assigned by the planner at job creation; the
  atomic write to `submissions/incoming/{job_id}.json` provides
  first-writer-wins deduplication — a duplicate submission for the
  same `job_id` is silently discarded. For ad-hoc submissions,
  `job_id` is freshly minted by the server (`job-{uuid}`).
- plan manifests are keyed by `plan_id`, a flat keyspace
  (`plans/{plan_id}.json`) in the `[storage]` backend — **not** in the ephemeral
  `todo/` queue, so a completed or cancelled plan stays queryable after its jobs
  have left the queue. The manifest's `job_ids` list is the only plan↔job record
  (job bodies carry no `plan_id`). Written first by ingestion
  (`creating` → `active` / `pending_clients`), then owned by `queue-maintenance`,
  which reconciles `status` and refreshes the optional `progress_snapshot`; the
  two writers never overlap, so the store needs no compare-and-swap. See
  [plan-ingestion.md §9](plan-ingestion.md)
- `cancelled_plans/{plan_id}` is an empty marker recording that an operator ran
  `plans cancel` — a *request* for teardown, not the teardown itself.
  `queue-maintenance` is the sole writer of both the manifest's `cancelled`
  status and the `todo/` deletes, so signaling out of band this way keeps a
  cancel from being lost to a concurrent status refresh. The marker is a
  **sibling** keyspace of `plans/`, not a key inside it, so listing manifests
  never has to filter markers out; it is deleted once the plan's jobs are all
  retired. A marker is written only for a plan that has a manifest, and writing
  one twice is a no-op. See [plan-ingestion.md §9](plan-ingestion.md)
- eval sample result records are keyed by `job_id`
- warehouse records are partitioned by `(benchmark_id, client_id, day)` for new
  writes; legacy `(benchmark_id, client_id, month)` partitions are frozen and
  read-only (reads union both)
- `locks/mutate.lock` is the advisory lock that serializes the
  read-modify-write batch commands (`process-submissions`, `fix-*`,
  `requeue-eval`); `locks/score-eval.lock` is a separate advisory lock that
  serializes `score-eval` runs only (so two long scoring runs can't overlap)
  without contending with the mutate lock. Both are transient coordination
  state, not data records — see [cli.md §Concurrency](cli.md#concurrency)
- `todo/` contains the job queue managed by the planner and
  `pipette-mgmt`. `avail/` holds unclaimed jobs named
  `{job_id}.{expires_at}.json` where `job_id` is `job-{UUIDv7}` (enabling
  key-ordered listing) and `expires_at` is an ISO 8601 timestamp or
  `never`; `leased/` holds active jobs with the client ID and lease
  expiry encoded in the filename; `denied/` holds empty marker files
  recording per-client failures; `eligible/clients/` holds pre-computed
  eligibility markers used by the claim path, maintained solely by
  `queue-maintenance`; `pending-reindex/` holds one marker per reindex
  request — a distinct `{client_id}.{uuid}` key, so a profile change
  arriving during an in-flight rebuild isn't lost — recording a client
  whose profile has been updated and not yet re-evaluated against the
  eligible index, written by `serve` on `PATCH /clients/me`, consumed
  and deleted by `queue-maintenance`. See [planner.md](planner.md) for
  the full lifecycle.
- `todo/suspended/{client_id}.json` flags a client that claimed a new job
  while holding an unexpired lease on a previous one — a signal of
  unexpected reboot or crash loop. Suspended clients receive `204 No
  Content` on `POST /plans/claim` until an operator clears the flag with
  `pipette-mgmt clients unsuspend`. The file records `suspended_at` and
  the `conflicting_job_id` for operator triage. Lives in `[todo_storage]`
  alongside the rest of the queue state — same access pattern (tiny,
  frequently polled, transient).

These invariants define the model:

- benchmark definitions are loaded from storage at startup
- client records are JSON documents keyed by `client_id`
- submission JSON payloads are immutable once written
- submission processing state is represented by logical location:
  - `submissions/incoming/...`
  - `submissions/score-queue/to_do/...` (eval submission routed by the fast
    `process-submissions` pass, awaiting the slow `score-eval` pass's call)
  - `submissions/score-queue/to_finalize/...` (`{ submission, score }` written
    by `score-eval`, awaiting the fast pass's warehouse write)
  - `submissions/processed/...` (terminal)
  - `submissions/unverified/...` (held; never enters the scorer until
    an operator promotes it)
- processed submissions contain the same JSON payload that was accepted at
  submission time
- warehouse data is the durable scored-results store for aggregate metrics
- eval sample results are the durable scored-results store for per-sample
  eval outcomes

The phrase "files are never modified" means:

- submission payload content is never edited after it is written
- state changes are represented by moving between logical locations
- warehouse data is the source of truth for scored metrics, not for the
  original submission payload

The current filesystem implementation uses the exact paths above.

## 3. Metadata contract

Metadata covers benchmark definitions and client records.

Benchmark logical key:

`benchmarks/{benchmark_id}.toml`

Each benchmark definition is a single TOML document. The `benchmark_id` is the
filename without `.toml` in the filesystem implementation.

Examples:

```toml
# benchmarks/prefill_throughput_256.toml
benchmark_type = "prefill_throughput"
parameter_prefill_tokens = 256
```

```toml
# benchmarks/eval_ifstruct_release_v1_0.toml
benchmark_type = "eval"
parameter_eval_id = "ifstruct"
parameter_dataset_name = "release_v1_0"
parameter_max_tokens = 8192
```

Servers scan benchmark definitions at startup and fail fast if any definition is
invalid. See [benchmarks.md](benchmarks.md) for field definitions.

Client logical key:

`clients/{client_id}.json`

Example:

```json
{
  "client_id": "ev1_a3f8...",
  "public_key": "hex-encoded-ed25519-public-key",
  "organization": "LiquidAI",
  "client_details": "Boston Jetson Orin",
  "contact_email": "lab@example.com",
  "status": "approved",
  "registered_at": "2026-03-08T10:00:00Z",
  "device_name": "MacBook Pro 14-inch (2023)",
  "device_form_factor": "laptop",
  "device_os_name": "macOS",
  "device_os_version": "15.3",
  "device_chip_model": "Apple M3 Pro",
  "device_ram_bytes": 36000000000,
  "device_gpu_model": null,
  "device_gpu_vram_bytes": null,
  "device_npu_model": null,
  "device_npu_vram_bytes": null,
  "capabilities": ["runtime:llama_cpp"]
}
```

| Field | Type | Description |
|-------|------|-------------|
| `client_id` | string | Derived from public key (see [authentication.md §1](authentication.md#1-identity-model)) |
| `public_key` | string | Hex-encoded Ed25519 public key |
| `organization` | string | Organization operating this client. Defaults to `"unknown"` for legacy client records that predate the field. |
| `client_details` | string | Freeform description |
| `contact_email` | string | Contact email for admin approval |
| `status` | string | `pending` or `approved` |
| `registered_at` | string | ISO 8601 registration time |
| `device_name` | string or null | Device model / marketing name; null if not set |
| `device_form_factor` | string or null | One of `phone`, `tablet`, `laptop`, `desktop`, `server`, `embedded`; null if not set |
| `device_os_name` | string or null | OS family; null if not set |
| `device_os_version` | string or null | OS version string; null if not set |
| `device_chip_model` | string or null | Chip / SoC model; null if not set |
| `device_ram_bytes` | int or null | System RAM in bytes; null if not set |
| `device_gpu_model` | string or null | GPU model; null if not set |
| `device_gpu_vram_bytes` | int or null | GPU VRAM in bytes; null if not set |
| `device_npu_model` | string or null | NPU model; null if not set |
| `device_npu_vram_bytes` | int or null | NPU VRAM in bytes; null if not set |
| `capabilities` | string[] | Free-form capability flags the client reports directly (e.g. `runtime:llama_cpp`); omitted from the record when empty (legacy records carry no key). Unioned with the flags the server derives from `device_*` to form the effective capability set used for job matching — see [planner.md](planner.md#client-matching-rules). |

The server resolves clients by `client_id` from the authenticated request
context.

A client's **tags** are not part of this record — they are stored as leaf
markers under `tags-index/by-client/{client_id}/{tag}` (forward) and
`tags-index/by-tag/{tag}/{client_id}` (reverse); see the invariants above and
[authentication.md §6](authentication.md#6-client-tags).

Its **signature migration** is not part of this record either. The marker at
`signature-migration/{client_id}.json` holds the time that client first signed a
`v1` payload:

```json
{ "first_seen": "2026-03-08T10:04:29Z" }
```

Its presence refuses that client the timestamp-only fallback
([authentication.md §2.3](authentication.md#23-timestamp-only-signatures)). It
sits outside the client record for two reasons. The migration ends — once every
client has a marker the fallback is switched off and this tree is deleted, where
a record field would outlive what it describes. And writing it never touches the
client record, so a request cannot clobber a concurrent status change: client
records are written whole, with no compare-and-swap.

On a filesystem backend the record is staged beside its destination and linked
into place, so a marker is only ever readable complete. A write interrupted
between the two steps leaves a `{client_id}.{uuid}.staged` file behind; it names
no client the listing selects, holds nothing the store reads, and is safe to
delete.

A marker that outlives its client is harmless. `client_id` derives from the
public key, so re-registering the same key inherits the marker — and inheriting
it *denies* the fallback, which is the strict direction. Cleanup is therefore
best-effort rather than an invariant.

**Pre-auth keys** live at `preauth/{key_id}.json`, one record per key. Each
stores `sha256(secret)` (never the secret), `usage` (`single_use` /
`multi_use`), `created_at`, optional `expires_at`, and the tags/organization to
seed onto a registering client. A valid key always approves the client (that's
its purpose), so there is no per-key approve flag. The record is **write-once**
— the only post-creation mutation is deletion: a single-use key is deleted as it
is spent, `revoke` deletes on demand, and `preauth prune` deletes expired keys.
A multi-use key is read-only on consume, so it never needs a rewrite.

Spending a single-use key first creates a sibling marker at
`preauth/{key_id}.spent`, carrying the spend timestamp, then deletes the record.
The create is exclusive (`If-None-Match: *` on S3, `O_EXCL` on a filesystem), so
it — not the delete — is what makes exactly one winner out of concurrent
registrations, across replicas as well as within one. A marker therefore
outlives the record it retired, and only a marker whose record is gone may be
removed: while a record survives, its marker is the one thing keeping that key
spent. `preauth prune` sweeps those record-less markers. Both `preauth list` and
the prune scan read only `*.json`, so markers are invisible to them. See
[authentication.md §3.2](authentication.md#32-pre-auth-keys).

An S3-compatible `[auth_storage]` backend therefore has to honor conditional
`PUT`. This is the same requirement the storage mutate lock (`src/storage_lock.rs`)
already places on `[storage]`, so a backend that serves one serves the other.

## 4. Submission contract

New submissions are written to a flat inbox:

`submissions/incoming/{job_id}.json`

Neither `incoming/` nor `processed/` is partitioned: both are flat
keyspaces over `job_id`. The scorer drains `incoming/` and reads
`benchmark_id` and `client_id` from each payload. The warehouse
remains the canonical analytics layer keyed by those columns.

The stored JSON is immutable after write.

When scoring succeeds:

- derived metrics are written to the warehouse
- for eval benchmarks, eval sample results are written
- the submission transitions from `incoming/{job_id}.json` to
  `processed/{job_id}.json.gz`

If scoring fails:

- the submission remains in `incoming/`
- it is eligible for retry

Job reads and job listings expose logical state:

- a submission in `incoming` is an unprocessed job
- a submission in `processed` is a processed job

The processed submission must preserve the original payload content.

`processed/` is an operational archive, not the durable scored-results store.
It may be retained, pruned, or deleted independently of `warehouse/results/`
and `warehouse/eval_sample_results/`.

Files in `processed/` are gzip-compressed (`{job_id}.json.gz`) — the same
JSON payload accepted at submission time, gzipped at level 6. Inspect with
`zcat`/`zless`/`gunzip -c`. Files in `incoming/` remain plain `.json` so
ingress is a single fsync.

Submission example, throughput:

```json
{
  "job_id": "job-550e8400-...",
  "benchmark_id": "prefill_throughput_256",
  "benchmark_type": "prefill_throughput",
  "client_id": "ev1_a3f8...",
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
  "submitted_at": "2026-03-10T12:01:00Z",
  "prefill_time_ms": 34.7,
  "prefill_time_ms_stddev": 1.2
}
```

Submission example, eval:

```json
{
  "job_id": "660f9500-...",
  "benchmark_id": "eval_ifstruct_release_v1_0",
  "benchmark_type": "eval",
  "client_id": "ev1_a3f8...",
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
  "submitted_at": "2026-03-10T12:05:00Z",
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

A `completions[]` entry may carry the optional `failed` (default
`false`) and `failed_reason` (default `null`) fields when the
client-side runtime crashed for that specific sample (see
[pipette-clients#103](https://github.com/Liquid4All/pipette-clients/pull/103)),
plus the optional `stop_reason` and `completion_tokens` fields
(default `null`) that record how generation ended for the sample (see
the [canonical enum](scoring-service.md#per-sample-stop_reason-canonical)).
All of these default on parse so submissions from pre-feature clients
keep working. They are mgmt-internal metadata — see
[scoring-service.md](scoring-service.md) for how the scorer call
strips them and the per-sample parquet re-injects them.

Submission example, VL throughput:

```json
{
  "job_id": "770a0600-...",
  "benchmark_id": "vl_throughput_384x384_32_64",
  "benchmark_type": "vl_throughput",
  "client_id": "ev1_a3f8...",
  "device_name": "Jetson Orin Nano 8GB",
  "device_form_factor": "embedded",
  "device_os_name": "Linux",
  "device_os_version": "Ubuntu 22.04",
  "device_chip_model": "NVIDIA Jetson Orin Nano",
  "device_ram_bytes": 8589934592,
  "model_name": "LiquidAI/LFM2.5-VL-450M-GGUF",
  "model_quant": "Q4_0",
  "model_params_total_millions": 450,
  "model_descriptor": "{\"mmproj\":\"mmproj-f16.gguf\",\"model\":\"LFM2.5-VL-450M-Q4_0.gguf\",\"org\":\"LiquidAI\",\"repo_name\":\"LFM2.5-VL-450M-GGUF\",\"source\":\"huggingface\",\"type\":\"gguf_vision\"}",
  "runtime_name": "github.com/ggml-org/llama.cpp",
  "runtime_version": "b8683",
  "runtime_descriptor": "{\"flavor\":\"macos-arm64\",\"repository_url\":\"github.com/ggml-org/llama.cpp\",\"repository_version\":\"b8683\",\"type\":\"llamacpp_cli_stock_tools\"}",
  "submitted_at": "2026-03-10T12:10:00Z",
  "prompt_tokens": 75,
  "prompt_ms": 352.3,
  "prompt_ms_stddev": 3.8,
  "predicted_ms": 32.7,
  "predicted_ms_stddev": 1.5
}
```

The server injects `client_id`, `submitted_at`, `job_id`, and `benchmark_type`
into the stored JSON. Hardware fields (`device_name`, `device_form_factor`,
`device_os_name`, `device_os_version`, `device_chip_model`, `device_ram_bytes`,
and optional `device_os_build`, `device_os_security_patch`, `device_gpu_model`,
`device_gpu_vram_bytes`, `device_npu_model`, `device_npu_vram_bytes`,
`device_battery_level`, `device_power_state`, `device_power_save_mode`) are sent
by the client. `device_os_build` is the precise OS build string, finer-grained
than `device_os_version` (e.g. iOS `22F76`, macOS `24F74`, Windows `26100.1234`,
Android `AP3A.240905.015.A2`, Linux full `uname -r`); `device_os_security_patch`
is the OS security-patch level where the platform exposes one (currently
Android-only, e.g. `2025-06-01`, null elsewhere). Both are optional and stored
as nullable warehouse columns — submissions from clients that predate them
persist as null.
Standard deviation is reported per timing field (e.g.
`prefill_time_ms_stddev`). The scorer propagates stddev to both direct and
derived metrics — see [benchmarks.md](benchmarks.md) for per-type fields and
propagation formulas.

### `model_descriptor` / `runtime_descriptor`

`model_descriptor` and `runtime_descriptor` are the client's full, lossless model and runtime
specifications. **They are JSON strings, not nested objects** — the field value
on the wire (and in the stored submission JSON above) is a string whose contents
are JSON, and the warehouse Parquet stores it as a single `Utf8` string column.
The server treats each as **opaque**: it never interprets the schema (partners
define their own runtimes and model formats), it only normalizes the string —
object keys sorted lexicographically at every level, insignificant whitespace
stripped — so that substring/pattern search over the column is stable regardless
of how the client ordered keys or spaced its payload. The scalar `model_name` /
`runtime_name` / `runtime_version` columns stay separate as the cheap
grouping/display keys.

On a **synthetic** failure — one the server writes when it retires a job with no
client run — the descriptors are derived instead from the job body's `spec.model`
and `spec.runtime`, canonicalized the same way, with any `auth_token` dropped
(omitted, not marked, since clients omit it too). The scalar grouping columns are
**null** on those rows: recovering them would require parsing the partner-defined
schemas the server deliberately does not interpret.

These rows are **not reliably joinable to client-submitted rows on descriptor
identity**, because the server canonicalizes the raw spec JSON while a client
round-trips it through a typed model — dropping any field its schema does not
describe. Equality holds only for specs carrying no fields beyond the client's
schema. See [plan-ingestion.md](plan-ingestion.md) §9 for the full statement.

Alongside each, the warehouse stores a `model_descriptor_sha256` /
`runtime_descriptor_sha256` / `benchmark_flags_sha256` / `model_flags_sha256` /
`runtime_flags_sha256` column — the hex sha256 of the canonical string,
computed mgmt-side at submission time. It is a stable content id (identical
descriptors hash identically regardless of client formatting) for cheap
grouping and joins; null when the descriptor is absent.

**Recommended shape.** Although the server accepts any JSON, first-party clients
should serialize their `pipette-plan-types` `Model` / `Runtime` enum variant — a
JSON object tagged by `type` plus that variant's fields — so the warehouse stays
queryable. The variants and their fields are the source of truth in that crate;
the canonical (stored) strings for each are below.

`model_descriptor` — every `Model` variant. Each artifact-bearing variant
flattens a `source`-tagged location object: `source: "huggingface"` (`org` +
`repo_name`, optional `revision` and per-file `sha256`), `source: "local"`
(on-disk `path`/`dir`), or `source: "url"` (direct `http(s)` download). The
model's *identity* fields only — per-cell generation flags (`enable_thinking`,
…) live on the plan cell, not the descriptor, and the gated-repo `auth_token` is
stripped before storage — so neither appears here:

```
# GgufText — single-file text GGUF (llama.cpp), HuggingFace source
{"org":"meta-llama","path":"Q4_K_M.gguf","repo_name":"llama-3.2-1b","source":"huggingface","type":"gguf_text"}

# GgufVision — backbone GGUF + separate projector, both in one repo
{"mmproj":"mmproj-f16.gguf","model":"LFM2.5-VL-450M-Q4_0.gguf","org":"LiquidAI","repo_name":"LFM2.5-VL-450M-GGUF","source":"huggingface","type":"gguf_vision"}

# Mlx — directory-style MLX bundle (optional "prefix" subdir when a repo bundles several)
{"org":"LiquidAI","repo_name":"LFM2.5-350M-MLX-4bit","source":"huggingface","type":"mlx"}

# Torch — directory-style PyTorch / Transformers weights
{"org":"Qwen","repo_name":"Qwen3-4B","source":"huggingface","type":"torch"}

# AppleFoundationText — OS-bundled model, no repo/file
{"type":"apple_foundation_text"}
```

`runtime_descriptor` — every `Runtime` variant. The llama.cpp variants flatten a
runtime source: a git build `{repository_url, repository_version}`
(`repository_url` defaults to `github.com/ggml-org/llama.cpp` when omitted;
`repository_version` is a non-empty string, so short commit hashes / 9-char
revisions round-trip fine) or a prebuilt `{url}` archive, plus a `flavor`. The
uv-provisioned variants carry a `requirements` tagged object, one of
`{"type":"catalog"}` / `{"type":"text","contents":"…"}` /
`{"type":"path","file":"…"}`; with `catalog`, the
`server_version`/`build`/`python_version` triple must resolve to a real entry in
`pipette-torch-oai`'s bundled catalog
(`<server>@<server_version>+<build>.py<python_version>`) — the values below are
real catalog entries:

```
# LlamacppCliStockTools — stock upstream llama.cpp CLI (pushed desktop binary)
{"flavor":"macos-arm64","repository_url":"github.com/ggml-org/llama.cpp","repository_version":"b8683","type":"llamacpp_cli_stock_tools"}

# LlamacppIosPipette — in-process llama.cpp inside the iOS pipette app
{"flavor":"ios-arm64","repository_url":"github.com/ggml-org/llama.cpp","repository_version":"b8683","type":"llamacpp_ios_pipette"}

# MlxIosPipette — in-process mlx-swift; version is the pinned Swift-package stack
{"flavor":"ios-arm64","packages":{"mlx_swift":{"repository_url":"github.com/ml-explore/mlx-swift","repository_version":"0.21.2"},"mlx_swift_lm":{"repository_url":"github.com/ml-explore/mlx-swift-examples","repository_version":"1.18.1"},"swift_transformers":{"repository_url":"github.com/huggingface/swift-transformers","repository_version":"0.1.17"}},"type":"mlx_ios_pipette"}

# MlxMacosPipette — desktop MLX (Python/uv): version + requirements + flavor
{"flavor":"macos-arm64","requirements":{"type":"catalog"},"type":"mlx_macos_pipette","version":"0.20.0"}

# DockerVllm
{"flavor":"nvidia_gpu","image_name":"vllm/vllm-openai","image_tag":"v0.25.0","type":"docker_vllm"}

# DockerSglang
{"flavor":"nvidia_gpu","image_name":"lmsysorg/sglang","image_tag":"v0.5.15-cu130","type":"docker_sglang"}

# UvVllm — server_version/build/python_version resolve to a real catalog entry
{"build":"cu129","python_version":"3.12","requirements":{"type":"catalog"},"server_version":"0.22.0","type":"uv_vllm"}

# UvSglang — likewise a real catalog entry
{"build":"cu121","python_version":"3.12","requirements":{"type":"catalog"},"server_version":"0.5.12.post1","type":"uv_sglang"}

# AppleFoundation — OS runtime, no coordinates
{"type":"apple_foundation"}
```

### `benchmark_flags`

The harness configuration a run executed under: how long it waited for the
device, whether it enforced the thermal criterion, its request timeouts, its
loop-detection settings. Stored as a canonical JSON string beside
`model_descriptor` / `runtime_descriptor`, normalized identically (object keys
sorted at every level, insignificant whitespace stripped), with a
`benchmark_flags_sha256` column computed mgmt-side at submission time. Opaque:
the server never interprets the schema, so a partner may carry its own
harness's settings under its own keys.

It is separate from `runtime_flags` because the two answer different questions.
`runtime_flags` is what the *runtime* was configured with — thread count, GPU
layers, context size — and every field of it changes the number being measured.
`benchmark_flags` is what the *harness around* the runtime did, which changes
how far the number can be trusted rather than what it is. The two are
[normalized the same way](#model_flags--runtime_flags); only the strictness of
the validation differs, because `benchmark_flags` must be a JSON object.

**Resolved, not authored.** A client submits the values it *ran with*, never the
values it was given. This is the whole point of the column and the one rule the
server cannot enforce, so it is stated here rather than left to each client:

- A client whose own configuration says nothing about thermal gating, run on a
  host where something waived the criterion, ran **ungated**. It submits
  `"skip_thermal": true`. Submitting `null` — the authored value — would
  record "no opinion" for a run that demonstrably had one, which is worse than
  omitting the field, because it reads as authoritative.
- A client that pins no readiness deadline gets its own per-platform default. It
  submits that default, not `null`.

Readiness is worth calling out because none of it originates here: the server
sends no readiness configuration in a claim and has no concept of a thermal
gate. That decision is made wholly client-side, so this column is the only
record of it that ever reaches the warehouse.

A field a client genuinely has no notion of is simply absent from the object.
Absent means "this harness has no such setting"; it never means "unset". A
client with nothing at all to report omits `benchmark_flags`; a **top-level**
empty object is accepted but stored as NULL, so "nothing reported" has one
spelling and one grouping bucket rather than two. A *nested* empty object is a
different claim — "there is a readiness block and it is empty" — and is stored
as sent.

**Why it matters.** Two submissions of the same cell can differ by several
percent purely because one waited for the device to cool and the other did not,
and nothing else in the row distinguishes them — the thermal telemetry shows a
hot device either way, leaving the reader to infer the gate state from the
numbers they are trying to interpret. Grouping on `benchmark_flags_sha256` makes
"compare only runs measured the same way" a join key instead of an inference.

**Recommended shape.** First-party clients serialize their
`pipette-plan-types` `BenchmarkFlags` for the cell, minus the
`runtime_type` / `model_type` / `benchmark_type` axis keys — those identify
*which* cell the flags belong to rather than what the harness did, and the
warehouse already carries that identity in `benchmark_type` and the
`model_descriptor` / `runtime_descriptor` columns. The readiness block is
resolved:

```
# A gated run that took the platform default deadline
{"readiness":{"max_wait_secs":300,"skip_thermal":false}}

# A run whose thermal criterion was waived, by the client's plan or its environment
{"http_timeout_seconds":1800,"readiness":{"max_wait_secs":300,"skip_thermal":true}}

# A cell that does not gate: no readiness block at all
{"doomloop":{"max_repeats":12,"window":40}}
```

The readiness fields are the same ones a plan authors, and resolution is what
makes them unambiguous: authored, each is optional and `null` means "no
opinion", which describes a request rather than a run. Resolved, both are set,
and `skip_thermal` is simply true or false. That is why the contract is about
*resolving* rather than about a separate vocabulary for the answer.

A missing `readiness` block means the cell does not gate — not that the client
declined to report. Some benchmark families (evals, peak-memory) never wait on
the device, and their flag shapes carry no readiness fields to fill.

### `model_flags` / `runtime_flags`

The model-generation and runtime-load configuration a run used:
`enable_thinking` and friends on one side, thread count / GPU layers / context
size on the other. Both are opaque — the server never interprets the schema —
and both are normalized before storage with a `model_flags_sha256` /
`runtime_flags_sha256` computed mgmt-side, exactly like the descriptors.

Normalization is **conditional on the value parsing as JSON**, and that is the
one way these two differ from every other canonicalized column. They are
documented to accept a plain string as well — `--n-gpu-layers 999` is a
perfectly good thing for a client to report — so:

- a value that parses as JSON is stored in canonical form (object keys sorted at
  every level, insignificant whitespace stripped) and hashed from that form;
- a value that does not parse is stored trimmed and otherwise verbatim, and
  hashed as-is. It is never rejected: the wire contract accepts both spellings,
  and a `400` here would break clients that have always sent a flag string.

A top-level empty object collapses to NULL, for the same reason it does on
[`benchmark_flags`](#benchmark_flags): "nothing reported" must have one spelling
and one grouping bucket. A nested empty object is a different claim and is
stored as sent.

**Why normalize at all.** These columns exist to answer "which runs were
configured the same way". Without canonicalization `{"threads":8,"gpu":99}` and
`{"gpu":99,"threads":8}` are two distinct grouping buckets describing one
configuration, and every `GROUP BY` over them silently under-counts. With it,
`runtime_flags_sha256` is a join key. The JSON spelling is the one worth
encouraging in new clients for exactly this reason — a plain string still
groups, but only against a byte-identical other plain string.

Rows written before these rules — non-canonical values, or a NULL hash column —
are brought up to date by
[`fix-canonical`](cli.md#pipette-mgmt-fix-canonical), which applies the same
functions the ingest path uses.

### `client_version`

Optional on both submission variants. The version of the client build that ran
the benchmark — the harness — stored verbatim in the `client_version` warehouse
column and NULL when a client does not report it.

Not a substitute for `runtime_version`, and not derivable from it. The runtime
version identifies the inference engine the client drove; this identifies the
code that decided how to drive it — how it warmed up, how many repetitions it
timed, how it gated on readiness. Those change the measured number while every
runtime and model coordinate on the row stays fixed, so a version bump in the
harness looks exactly like a device regression unless the row records which
harness produced it.

Opaque: never parsed, ordered, or compared, so semver, `git describe`, and
build numbers are all fine. The one rule is non-blank — an empty string would
be a second spelling of "not reported" and a second grouping bucket, so it is
a `400` at the boundary rather than a NULL.

Unverifiable, like [`benchmark_flags`](#benchmark_flags): the server has no
independent view of which build called it, and the value is whatever the client
says. It is a grouping key for reading the warehouse, not an authentication
signal — `client_id` is the identity that is actually checked.

### 4.1. Unverified submissions

When the server is configured with `[unverified_submissions] enabled = true`
(see [cli.md](cli.md)), a submission from a **pending** (validly-signed but
unapproved) client is held rather than rejected with `403`. The payload is
validated exactly like an approved submission and written to:

`submissions/unverified/{client_id}/{job_id}.json`

The keyspace is partitioned by `client_id`, then flat over `job_id` within
each client. The stored JSON is immutable after write and carries the
caller's real `client_id`, `job_id`, `submitted_at`, and `benchmark_type`
(injected the same way as an approved submission).

Unverified submissions are write-only from the request path's perspective.
They are not visible to the scorer, the warehouse, the eval sample results
store, the `fix-*` family, or `GET /jobs/{job_id}`. The `job_id` returned
in the `202` response is a receipt for operator triage, not a lookup key
— see [httpapi.md §2.12](httpapi.md#212-get-jobsjob_id).

The only consumers of this tree are the operator `unverified` subcommands
(see [cli.md](cli.md)), none of which take the storage mutate lock:

- `promote --client-id <id>` re-stages a client's held submissions into
  the normal pipeline once the client is approved: `success` bodies move
  to `incoming/` (for the scorer), `failure` bodies to `processed/`. Each
  object is deleted from the unverified tree only after its re-stage write
  succeeds.
- `delete --client-id <id>` discards a client's held submissions (e.g.
  after rejecting the client).
- `prune --older-than <age>` removes held objects across all clients by
  the storage backend's object modification time (S3 `LastModified` /
  filesystem `mtime`), not the payload's `submitted_at`, to bound archive
  size.

Listing cost scales with the size of the archive — operators should run
`prune` (or resolve clients) regularly.

## 5. Warehouse contract

Warehouse logical partition:

`warehouse/results/benchmark_id={benchmark_id}/client_id={client_id}/day={YYYY-MM-DD}/`

Warehouse data is the canonical scored-results store for aggregate metrics.

The storage contract is **per-day** Parquet partitioning by:

- `benchmark_id`
- `client_id`
- calendar **day** of `submitted_at`

Per-day partitions bound each scoring write: a `(benchmark_id, client_id, day)`
partition is read-modify-written on every `score` tick that lands a result in
it, so capping it to one day's rows (rather than a whole month's) keeps that
rewrite small.

**Legacy `month={YYYY-MM}` partitions.** Earlier writes used per-month
partitions. Those are **frozen** — the write path never creates or rewrites a
`month=` partition again; new results always go to `day=` partitions, even for a
day whose month still has a legacy partition. Reads union both schemes (see
below), so the two coexist with no migration. A `job_id`'s rows live in exactly
one partition (its `submitted_at` day, or a pre-cutover month).

Each partition contains one or more Parquet files. Example (day + leftover
legacy month side by side):

```text
warehouse/
  results/
    benchmark_id=prefill_throughput_256/
      client_id=ev1_a3f8.../
        month=2026-03/          # legacy, frozen
          part-0001.parquet
        day=2026-06-01/         # new
          part-0001.parquet
        day=2026-06-02/
          part-0001.parquet
```

Readers should treat the partition as the unit of access, not a single physical
file. The physical file layout inside the partition is an implementation detail
as long as the partition contents together represent the warehouse data for that
key.

`pipette-mgmt process-submissions` (alias: `score`) is the normal warehouse
writer. The one-off `fix-*` tools also rewrite warehouse files in place across
both `day=` and `month=` partitions; see `docs/cli.md`.

The scorer writes each `day=` partition **append-only**: a tick's metric rows
are appended to the tail `part-NNNN.parquet`, which rolls to a new part once it
reaches `warehouse_max_rows_per_part` (default 1000). Earlier parts are never
read or rewritten, so a write costs `O(max_rows_per_part + new rows)`, not
`O(partition)`.

There is **no write-time dedup**. The previous model rewrote the whole
partition and dropped a re-scored job's old rows; the append model does not. In
normal operation each job is scored once and appended once, so no duplicate
arises. The one exception is the at-least-once retry: if `score` crashes after
the warehouse write but before `mark_processed`, the job is re-scored next run
and its rows are **appended again**, leaving a duplicate row set for that
`job_id` until a future compaction pass folds it out. Consequently:

- `at most one scored metric set per job_id` is a **logical** property
  (physically there may be two copies), not an on-disk one;
- `read_job_metrics` resolves it at read time: among a job's rows it keeps only
  the **latest scoring run** — the rows carrying the maximum `scored_at` (all
  rows of one run share a `scored_at`) — and drops earlier copies *wholesale*,
  so a re-score that produced a different metric set leaves no stale rows.
  External/bulk readers over the raw Parquet should apply the same rule
  (`max(scored_at)` per `job_id`).

Warehouse rows are not the source of truth for job payloads. They are the
source of truth for scored metrics used by processed-job reads and analysis.

Most warehouse columns are copied verbatim from the submission, including
`model_name` and `model_quant` for every runtime. The exception is the pair
`model_params_total_millions` /
`model_params_active_millions`:

- if `model_name` is present in the server's `model_params_mapping.toml`
  catalog, the warehouse row carries the canonical catalog values even
  if the submission body had different numbers;
- otherwise (unknown model), the warehouse row uses the submission
  values verbatim — including `null` if the submission omitted them.

The submission body itself is never modified, so a
`processed/{job_id}.json.gz` may report e.g. `total = 9999` while the
warehouse parquet for the same job reports `total = 8340` — the
warehouse value is what analytics queries should use. See
[benchmarks.md](benchmarks.md) for the field definitions and the
"Model catalog" section below for the `model_params_mapping.toml` format.

### Model catalog

The optional `model_params_mapping.toml` at the storage backend root maps
normalized `model_name` values to parameter counts in millions. It is loaded by
`serve`, `process-submissions` (alias: `score`), and `fix-model-param`.

Two forms — bare integer for dense models (`total = active`), inline
table for MoE / selective-activation:

```toml
# data/model_params_mapping.toml (or s3://<bucket>/<prefix>/model_params_mapping.toml)
"LFM2-700M" = 742                                  # dense
"LFM2-8B-A1B" = { total = 8340, active = 1500 }    # MoE
"gemma-4-E4B-it" = { total = 7996, active = 4000 } # selective-activation
```

Keys are normalized — no `org/` prefix, no `:file.gguf` suffix, no
`-GGUF` suffix. The same normalization is applied to the submission's
`model_name` before lookup, so `LiquidAI/LFM2-700M-GGUF:Q4_0.gguf`
resolves to the `"LFM2-700M"` entry. When a submission has no `model_name`
(or an unrecognized one), the scorer falls back to a substring match of the
catalog keys against the opaque `model_descriptor` — longest match wins, an
ambiguous tie between equal-length keys resolves to nothing.
`fix-model-param` applies the same two-step resolution to warehouse rows, so
rows written from descriptor-only submissions stay repairable. Validation:
`total > 0`, `active > 0` (defaults to `total` when omitted), `active <= total`.

A missing `model_params_mapping.toml` is not an error — the server logs a warning at
startup and the scorer falls back to whatever values the client
supplied for `model_params_total_millions` and `model_params_active_millions`.

The path is configurable via the top-level `model_params_mapping_path`
config key. When unset (the default), the file is read from the storage
backend root as described above. When set, the value is used as-is: a
filesystem path for `local_fs` (relative paths resolve against the process
cwd, not `data_dir`) and an object key for `s3` (the storage `prefix` is
not prepended).

See [`examples/model_params_mapping.toml`](../examples/model_params_mapping.toml) for a starter
catalog covering the model families this project routinely benchmarks.

### Per-run metadata: `eval_metadata`

Some per-run information about an eval submission doesn't fit on the
metric axis — it's not a scored signal, it's context about the run
itself. Currently this includes the count of samples that ended in
`failure` — keyed solely on `stop_reason == failure` (see
[pipette-clients#103](https://github.com/Liquid4All/pipette-clients/pull/103)).

These values are written into a single nullable `eval_metadata`
column on every warehouse row of the submission, as a JSON-encoded
`{key: value}` object. Denormalized identically across all the rows
of a submission, like the device / model / runtime fields, so a
single row stands alone without a join. Currently emitted only when
there's something to record:

```json
{"samples_failed": 3}
```

Future per-run metadata can be added by introducing new keys in the
JSON without a parquet schema change. Nullable so non-eval rows and
parquet files written before this column existed (read back via
`union_by_name = true` in the downstream DuckDB layer) load cleanly
as `NULL`.

Consumers that want an "accuracy over samples that could actually be
evaluated" rate compute it themselves from the `samples_failed` key
in this blob — mgmt does not pre-compute alternative accuracy
variants. See [scoring-service.md](scoring-service.md) for how the
value is derived.

### Schema evolution

Schema evolution for nullable warehouse columns is forward-compatible. Older
Parquet files may omit a newly-added nullable column, and readers treat the
missing column as `null` rather than requiring file migration. However, schema
evolution is **not** backward-compatible: Parquet files containing columns that
have been removed or renamed in the current schema will be rejected. Such files
must be migrated or deleted.

The local filesystem implementation uses the same per-day partition model (with
frozen legacy month partitions). A partition may contain one or more Parquet
files.

## 6. Eval sample results contract

For eval benchmarks, the scorer writes per-sample results alongside the
warehouse metrics. Eval sample results store the prompt that was served, the
completion the model produced, and whether the completion was scored as
correct — one row per sample in the eval dataset.

Eval sample results logical key:

`warehouse/eval_sample_results/{job_id}.parquet`

Each file is a self-contained Parquet dataset written once per scored
eval job. The keyspace is flat: there is no `benchmark_id` /
`client_id` partitioning — `job_id` is globally unique, and both
readers (`pipette-mgmt` and `pipette-duckdb`) resolve a parquet path
directly from `job_id`.

Example:

```text
warehouse/
  eval_sample_results/
    job-660f9500-e29b-41d4-a716-446655440000.parquet
```

Only `pipette-mgmt process-submissions` (alias: `score`) writes eval sample
results.

The canonical invariant is:

- at most one eval sample results file per `job_id`

On retry, the scorer writes a new file and atomically replaces the existing
one — there is no in-place mutation. Both the warehouse write and the eval
sample results write must succeed before the submission transitions to
`processed`.

### Parquet schema

| Column | Type | Nullable | Description |
|--------|------|----------|-------------|
| `id` | string | no | Sample ID (matches `completions[].id` in the submission and `samples[].id` from the evals server) |
| `messages` | string | no | JSON-encoded prompt messages array |
| `completion` | string | no | Model-generated text from the submission |
| `is_correct` | boolean | no | Whether the evals server scored this sample as correct |
| `failed` | boolean | yes | `true` if the client-side runtime crashed mid-completion for this sample (see [pipette-clients#103](https://github.com/Liquid4All/pipette-clients/pull/103)). Nullable so pre-feature parquet files read back as `false`. |
| `failed_reason` | string | yes | Free-form, human-readable description of the failure when known. `null` for `failed = false` rows. |
| `stop_reason` | string | yes | Canonical stop reason: `eos` \| `truncated` \| `doom_loop` \| `failure` \| `unknown`. `null` = never labelled (distinct from `unknown`). The sole source of truth for failure. See the [canonical enum](scoring-service.md#per-sample-stop_reason-canonical). |
| `stop_reason_source` | string | yes | Provenance of `stop_reason`: `recorded` (captured at generation, or from the `failed` flag) \| `derived` (reconstructed later, e.g. a tokenizer backfill). `null` when `stop_reason` is `null`. |
| `stop_detail` | string | yes | Free-form observation behind `stop_reason` — the *why / raw signal*: crash detail for `failure`, the unclassified `stop_type` for `unknown`, the trigger for `doom_loop`; normally empty for a clean `eos` / `truncated`. Generalizes `failed_reason`. `null` when unreported. |
| `completion_tokens` | int64 | yes | Output token count for the sample, re-injected from the submission. Pairs with `stop_reason` to separate `eos` (< cap) from `truncated` (== cap). `null` when the client didn't report it. |

The `messages` column stores the prompt in chat-message format as a
JSON-encoded string. The value is the serialized form of the messages array
returned by the upstream evals server's
`GET /evals/{eval_id}/datasets/{dataset_name}/samples` endpoint:

```json
[{"role": "user", "content": "..."}]
```

The structure varies by eval type. MCQ evals have a single user message.
Tool-calling evals have a system message followed by a user message.

### Data assembly

The scorer assembles each row by joining three sources during scoring:

1. **Prompts** — fetched from the evals server via
   `GET /evals/{eval_id}/datasets/{dataset_name}/samples`. Each sample has an
   `id` and a `messages` array. This provides the `id` and `messages` columns.
2. **Completions** — from the submission payload's `completions` array. Each
   entry has an `id`, a `completion` string, and the optional `failed` /
   `failed_reason` and `stop_reason` / `stop_detail` / `completion_tokens`
   metadata. This provides the `completion`, `failed`, `failed_reason`,
   `stop_reason`, `stop_reason_source` (stamped `recorded`), `stop_detail`,
   and `completion_tokens` columns. The retiring `failed` / `failed_reason`
   are derived from `stop_reason == failure`, not copied from the client's
   flag.
3. **Scored samples** — from the evals server's scoring response
   (`POST /score`). The response includes a `scored_samples` array where each
   entry has an `id` and an `is_correct` boolean. This provides the
   `is_correct` column.

All three sources are joined on the sample `id`. Failed completions
are forwarded to the scorer with their empty `completion` and almost
always come back `is_correct: false` — see
[scoring-service.md](scoring-service.md) for the strip-and-re-inject
behavior. The scorer is required to return one `scored_samples`
entry per forwarded `completions` entry: any dropped row OR any
unknown id the scorer echoed back fails the job with
`scorer response mismatch: dropped {...}, unknown {...}` and the
submission stays in `incoming/`. Empty `completions` arrays also
short-circuit before the scorer call (`eval submission has no
completions to score`) so a malformed-or-empty payload doesn't
silently land as a `0.0` accuracy in the warehouse.

## 7. Storage interface boundary

The application should depend on separate store interfaces rather than direct
filesystem operations across the codebase.

The storage domains are:

- `CatalogStore`
  - load benchmark definitions
- `AuthStore` (backed by the required `[auth_storage]` config section)
  - read a client record
  - write or update a client record
  - list clients
- `SubmissionStore`
  - write incoming submissions
  - list incoming submissions for scoring
  - read a submission by key
  - transition a submission to processed
  - find a job by `(client_id, job_id)` across `incoming/` and `processed/`
  - hold an unverified submission under `unverified/{client_id}/`. The
    unverified tree is intentionally not surfaced through the scoring
    methods (`list_incoming` / `find_job` / `mark_processed`) so the
    scorer and `fix-*` commands cannot accidentally pick it up
  - operator-only: list / delete one client's held submissions, promote
    them back into the pipeline, and prune held objects older than a
    given age across all clients
- `WarehouseStore`
  - write canonical metric rows for a `day=` partition
    `(benchmark_id, client_id, day)`; legacy `month=` partitions are read-only
  - read metrics for a processed job (recent-window fast path + full-scan
    fallback)
- `EvalSampleResultStore`
  - write eval sample results for a job keyed by `job_id`
  - read eval sample results for a job keyed by `job_id`

The code implements these as Rust traits with matching names in `src/stores/mod.rs`.

Each store may eventually use a different backend implementation. The
application should not require one global storage backend for all domains.

## 8. Backend configuration

Three config sections control storage. The `[storage]` section selects the
backend for catalog, submissions, warehouse, and eval sample results. The
`[auth_storage]` section (required) selects the backend for client auth data.
The `[todo_storage]` section selects the backend for the job queue (`todo/`
tree); it defaults to `[storage]` if not set. See [cli.md](cli.md) for the
full config reference.

```toml
# Local filesystem (default) — [todo_storage] not required, shares [storage]
[storage]
backend = "local_fs"
data_dir = "./data"

[auth_storage]
backend = "local_fs"
data_dir = "./data"

# S3 / S3-compatible (MinIO, R2) — use separate buckets for each section.
# [todo_storage] must point to an S3 Express One Zone bucket (see §9).
[storage]
backend = "s3"
bucket = "my-data-bucket"

[auth_storage]
backend = "s3"
bucket = "my-auth-bucket"

[todo_storage]
backend = "s3"
bucket = "my-todo-bucket--use1-az4--x-s3"  # must be Express One Zone
```

All three sections implement the same store interfaces and satisfy the same
contracts defined in this document. Credentials for S3 are read from the
environment (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, IAM roles), not
from the config file.

The storage interface follows these rules:

- do not expose `PathBuf` values throughout the application
- do not bake directory walking into handlers or business logic
- treat state transitions as store operations, not direct `rename` calls in
  application code
- keep domain identifiers explicit in the interface
- keep scored-output writes deterministic and idempotent per job
- model warehouse updates as scorer-owned partition maintenance, not blind file
  append

This document specifies the storage contract that store implementations
should satisfy.

## 9. Architectural decisions

### Batched warehouse writes

The scorer collects all metric rows in memory, groups them by partition
`(benchmark_id, client_id, day)`, and writes one batch per partition.
This reduces S3 API calls from O(submissions) to O(partitions).

### Append-only part files

Each `write_partition_metrics` call **appends**: it reads only the tail
`part-NNNN.parquet` (≤ `warehouse_max_rows_per_part` rows), tops it up to that
cap, and rolls overflow into fresh parts. Earlier parts are never read or
rewritten, so the per-write cost is bounded by `max_rows_per_part` regardless of
how large the partition has grown — the point of the small default (1000). The
tradeoff is no write-time dedup (see §5): a crash-retry re-score appends a
duplicate row set rather than replacing it. A future compaction pass (merge a
cold day's parts, drop superseded rows) is the place to reclaim physical
single-copy-per-job; it is not part of the write path.

### todo/ requires S3 Express One Zone

The job queue (`todo/`) relies on atomic rename for two critical operations:
promoting a complete job file from `tmp/` to `avail/`, and claiming a job from
`avail/` to `leased/`. On a local filesystem both are single syscalls. On S3,
atomic rename is only available via the `RenameObject` API provided by
[S3 Express One Zone](https://aws.amazon.com/s3/storage-classes/express-one-zone/).

Standard S3 has no atomic rename. The copy + delete alternative breaks the
no-double-assignment guarantee: two concurrent claimers can both copy a job
from `avail/` before either deletes it, resulting in the same job being handed
to two clients simultaneously.

We chose this design because atomic rename is a simple, correct primitive and
the cost difference between storage classes is not meaningful for the `todo/`
tree (small files, modest request volume). The alternative — supporting
standard S3 by using a fixed `leased/{job_id}.json` key with conditional writes
(`If-None-Match: *` on claim, `If-Match: <etag>` on heartbeat) — is
implementable but adds conditional-write logic throughout the claim and
heartbeat paths.

If Express One Zone is not acceptable (it is single-AZ, has limited regional
availability, and adds a second storage class to manage), the conditional-write
redesign is the path forward. See [planner.md](planner.md) for a full
description of what that change entails.

For S3 deployments using the planner, `[todo_storage]` must therefore be
configured as a separate Express One Zone bucket. The rest of the data tree
(`submissions/`, `warehouse/`, `clients/`, etc.) has no such constraint and
can use any S3-compatible backend via `[storage]` and `[auth_storage]`. See
[§8](#8-backend-configuration) for the config shape and [cli.md](cli.md) for
the full key reference.

When `[todo_storage]` is `backend = "s3"`, a process that renames against
`todo/` validates the bucket at startup and refuses to start otherwise. It
probes `RenameObject` on a nonexistent key: an Express One Zone bucket reports
`NoSuchKey`, a general-purpose bucket reports `NotImplemented`. This tests the
exact capability the queue needs, catching a misconfigured regular-S3 bucket
before a non-atomic copy-then-delete can hand the same job to two clients. Any
other error (auth, network) propagates unchanged.

The rule is **validate iff you rename.** `serve` (claim, heartbeat) and
`queue-maintenance` (lease recycle) both rename, so both validate —
`queue-maintenance` independently, since on a cron-only host it may be the first
process to rename against the bucket. The `clients` admin commands only list and
delete markers, never rename, so they skip the probe. On `local_fs`, `rename(2)`
is always atomic and the probe is a no-op.

### mark_processed is not atomic on S3

S3 has no rename operation. `mark_processed` uses copy + delete. If the
process crashes between copy and delete, the submission exists in both
`incoming/` and `processed/`, so it is re-scored on the next run. The re-score
**appends** its rows again (append-only writes do not dedup); reads then return
only the latest scoring run (`max(scored_at)` per `job_id`), so the duplicate is
invisible through the API.

### Bounded read scans

`read_job_metrics` scans only partitions whose date overlaps the last
`warehouse_read_days` (default 14), ordered **all `day=` first (newest→oldest),
then legacy `month=` (newest→oldest)**. New data only ever lands in `day=`, so
checking days first finds recent jobs fast and consults the frozen month
partitions only as a fallback; it also makes the newer scheme win when a job
exists in both (a `day=` and a same-period `month=`). Because a `job_id`'s rows
live in one partition, the scan **stops at the first partition that matches**,
so a recent-job lookup touches ~one partition regardless of archive size.

The window is a **hard cap**, not just a fast path: a job scored longer ago than
`warehouse_read_days` is reported by `GET /jobs/{job_id}` as `processed` with
`metrics: null` — there is no whole-archive fallback scan. The job's rows remain
in the warehouse and stay available to bulk queries (DuckDB, Athena), which read
the tree directly and ignore the window.

### Credentials in environment, not config

S3 credentials (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`) are read
from environment variables or IAM instance roles, never from the config
file. This avoids secrets in version-controlled config, works with all
AWS credential sources (env vars, instance profiles, ECS task roles, SSO),
and follows the standard `object_store` / AWS SDK credential chain.

### Multiple collector instances

Multiple `pipette-mgmt serve` instances can safely accept submissions to the same
S3 bucket concurrently. Each submission gets a unique `job-{uuid}` `job_id`, so
write keys never collide. No coordination is required between collectors.
