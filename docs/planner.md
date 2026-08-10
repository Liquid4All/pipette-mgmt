# Planner/Polling-Client

## Processes

Four actors are involved in the job queue. The two cron jobs are part of
`pipette-mgmt`; the **planner** is a *role* — authoring job files and promoting
them into `avail/` — filled either by an external service or by the Management
server's own plan-ingestion path (see [plan-ingestion.md](plan-ingestion.md)).

| Actor | Process | Replicas | Responsibilities |
|-------|---------|----------|-----------------|
| **Planner** | External service, or the Management server's plan-ingestion path | Any | Writes job files to `todo/tmp/` and atomically promotes them to `todo/avail/` |
| **Management server** | `pipette-mgmt serve` | One or more | HTTP API for clients; processes `POST /plans/claim`, `PUT /plans/{job_id}/heartbeat`, and `POST /benchmarks` |
| **Submission processor** | `pipette-mgmt process-submissions` (cron; `score` alias) | One at a time | Routes eval submissions, scores non-evals, finalizes scored evals, writes to the warehouse |
| **Eval scorer** | `pipette-mgmt score-eval` (cron) | One at a time | Drains `submissions/score-queue/to_do/`, calls the upstream scoring service, and stages eval scores for finalization |
| **Queue maintenance** | `pipette-mgmt queue-maintenance` (cron) | One at a time | Recycles expired leases, cleans up expired jobs, deletes stale `todo/tmp/` files, maintains the `eligible/` index (sole writer) |

**Multiple `serve` replicas:** Multiple `pipette-mgmt serve` instances can run
behind a load balancer and accept requests concurrently — `todo/` operations
use atomic rename and submission writes use unique `job_id` keys, so no
inter-replica coordination is required for correctness. This extends to plan
ingestion: a replica filling the planner role promotes jobs with the same atomic
`tmp/` → `avail/` rename, mints a unique `plan_id` / `job_id` set per submission,
and writes its manifest under a unique key — so a plan-ingesting replica needs no
more coordination than a claiming one, and is indistinguishable from an external
planner service (or from another replica ingesting a different plan
concurrently). `serve` replicas are read-only with respect to the `eligible/`
index; all writes to it are owned by `queue-maintenance`.

`pipette-mgmt process-submissions`, `pipette-mgmt score-eval`, and
`pipette-mgmt queue-maintenance` should each run **once per schedule interval**,
not once per `serve` replica. A single cron host or a Kubernetes CronJob is the
normal pattern. See [operations.md §3](operations.md#3-cron-setup).

## Jobs Created by Planner

"Jobs" created by the planner are JSON files that contain all of the information needed to execute a specific benchmark configuration, including:

- Which clients are eligible to run the job (`clients` array, `requires` capability flags, optional `any_of` clause groups, or a combination)
- A job ID (`job-{UUIDv7}`, assigned by the planner at job creation)
- An optional expiry timestamp (`expires_at`)
- A run specification (`spec`) naming one benchmark (`spec.benchmark`), one
  model configuration, one runtime configuration, and the flags for that cell

The lease time window is not part of a job file: it is server configuration,
reported to the client as `time_window` in the claim response.

Jobs are stored across several directories under `todo/`:

```
todo/
  tmp/              ← planner writes here first; partial writes are safe
  avail/            ← atomic rename from tmp/ when complete; full job content
    {job_id}.{expires_at}.json
  eligible/
    clients/
      {client_id}/
        {job_id}.{expires_at}   ← marker: this client may claim this job;
                                  encodes the job's expiry so claim can rank
                                  and claim without re-scanning avail/
  leased/           ← active lease files; encodes client and expiry in filename
  denied/           ← marker files recording which clients are ineligible per job
  pending-reindex/
    {client_id}.{uuid}  ← one key per reindex request (PATCH /clients/me,
                          registration); consumed by queue-maintenance
  pending-reindex-jobs/
    {job_id}        ← job was leased during a client reindex; re-matched
                      against all clients when it returns to avail/
  liveness/
    {client_id}     ← stamped with the poll time by a claim that finds no work;
                      lets queue-maintenance tell an idle-but-available client
                      from an offline one when estimating plan progress
```

Files in `avail/` are named `{job_id}.{expires_at}.json`, where `expires_at`
is an ISO 8601 basic-format timestamp (`20240908T000000Z`) or the literal string
`never` for jobs with no expiry.
`job_id` is `job-{UUIDv7}` and contains no dots (`.` is excluded from
the job-id charset — see `JobId::try_new`); `expires_at` / `never` contains no
dots; so the single `.` is an unambiguous delimiter. Encoding the expiry
in the filename lets `queue-maintenance` detect expired jobs by listing and
parsing filenames, with no body reads required.

Whoever fills the planner role constructs the correct filename before the
`tmp/` → `avail/` rename, and may promote a file only once it has authored the
**complete** body — the atomic rename is what makes a partially written job
unobservable. An external planner service satisfies this by writing `tmp/` then
renaming; the Management server's plan-ingestion path
([plan-ingestion.md](plan-ingestion.md)) satisfies it the same way, since it
authors each job body in full before promoting it. The Management server's
*claim* path never promotes into `avail/` — it only renames `avail/` →
`leased/` — though it may delete files from `avail/` that fail validation (see
below).

Because `job_id` is `job-{UUIDv7}` — a constant prefix over a time-ordered
UUID — `avail/` keys sort in arrival order.
`queue-maintenance` uses this to maintain a key-based cursor for the new-job
eligible index pass: each run processes only keys after the last processed one,
so it skips re-fetching and re-evaluating jobs already in the index.

On S3 Express One Zone (the required `todo/` backend, [storage.md §9](storage.md#todo-requires-s3-express-one-zone))
the cursor does **not** shrink the list operation itself: Express `ListObjectsV2`
has no server-side `start-after`, so the run lists the full `avail/` prefix and
filters to keys past the cursor client-side. The saving is the skipped body GET
and match evaluation per already-indexed job, not the listing. On `local_fs` the
listing is a cheap directory read regardless.

`queue-maintenance` is the sole writer of the `eligible/` tree. `serve`
replicas are read-only with respect to `eligible/` — this eliminates
concurrent-writer races with no inter-replica coordination required.

`queue-maintenance` updates the index incrementally on each run using two
signals:

- **New jobs** (cursor-based): `queue-maintenance` tracks a key cursor into
  `avail/` (the last processed `{job_id}.{expires_at}.json` key). Because
  `job_id` is `job-{UUIDv7}`, keys sort in arrival order; each run processes only
  keys past the cursor (see the Express listing caveat above). For each new
  job it creates `eligible/clients/{client_id}/{job_id}.{expires_at}` markers
  for all eligible clients (the `expires_at` copied from the `avail/` filename) —
  straightforward enumeration for explicit `clients` arrays, capability
  containment against every client's effective capability set for `requires` jobs.
- **Updated client profiles** (pending-reindex flags): a device-profile or
  capability change writes `todo/pending-reindex/{client_id}.{uuid}` markers — a
  distinct key per request, never overwritten. `PATCH /clients/me` writes
  two: one before it relinquishes the client's leases (raising the gate for
  that whole window) and one after the new record is durable. On each run
  `queue-maintenance` lists `pending-reindex/`, re-evaluates each flagged
  client against all current `avail/` jobs, updates
  `eligible/clients/{client_id}/` accordingly, and deletes **exactly the
  flag keys it captured before the rebuild**.

  The distinct keys and the capture-then-delete discipline are one
  guarantee: a rebuild never consumes a reindex request newer than the
  client record it evaluated. Each rebuild reads the client's record fresh
  (not a run-start snapshot); a flag written while the rebuild runs has a
  key outside the capture and survives to re-trigger on the next run; and
  the post-persist flag write means even a rebuild racing the `PATCH`
  itself leaves a flag that postdates the durable record. Back-to-back
  profile changes are therefore safe without any serialization: whichever
  rebuild consumes a request has, by construction, evaluated a record at
  least that new. (Half-rebuilt markers are never observable — the gate
  stays up until the flag comes down, and the rebuild is a full recompute,
  so a deferred rebuild simply redoes the work.)

  A profile change also **relinquishes every lease the client holds**, before
  the new profile is persisted: a lease is granted against the profile at
  claim time, and a client must not continue a job it may no longer be
  eligible for. Each relinquished job returns to `avail/` for re-claiming —
  with no `denied/` marker, so a client that still matches under its new
  profile may claim the job fresh once reindexed. (A lease whose job already
  has a submission record is deleted rather than recycled, so a finished job
  is never made claimable again.) A client that wants to hand back a job
  gracefully without changing its profile reports a **retriable failure**
  instead; that path does write a `denied/` marker.

  While the flag is pending, the client has **no standing in the queue**.
  Its eligibility is unknown — its markers reflect the old profile (or, for
  a newly registered client, don't exist yet) — and any lease it held was
  relinquished by the profile change, so a client must not run, resume, or
  extend any work until it is re-evaluated. Every plan operation is
  therefore refused until `queue-maintenance` re-evaluates the client and
  clears the flag: **`claim` returns no work (204); `heartbeat`, `reclaim`
  (both its renewal and re-acquire paths), and plan-attached submissions
  return 404 — all without renaming a lease**, so nothing can resurrect a
  lease mid-relinquish.

  Only the server can compute whether a PATCH changed the profile, so the
  profile response's `reindex_pending` field is how the client learns its
  standing was voided (httpapi.md §2.4); on `true` it must discard local
  in-flight work — that work is forfeited — and poll `claim` until the gate
  lifts. A refused reclaim or submission is typically a restarted client
  that had in-flight work from before the change; a refused *heartbeat* is a
  protocol violation — it means the client was actively running work while
  its own profile update relinquished it. The cost is at most one cron
  interval of claim latency after a profile change or a fresh registration.

- **Deferred job reindexing** (pending-reindex-job flags): a client reindex
  rebuilds from `avail/`, so it cannot evaluate jobs that are leased (by any
  client) while it runs — their markers may be stale for anyone once the
  reindexed client's profile changed. Rather than reindex them eagerly —
  most leased jobs retire successfully, making their markers garbage anyway
  — the reindex pass flags each one into `todo/pending-reindex-jobs/`, and a
  settle pass re-matches every flagged job against **all** clients (writing
  matches, deleting non-matches) once the job is back in `avail/`, then
  clears the flag. A flagged job that turns out to have a submission record
  is terminal: its flag is cleared and its markers are left to the GC
  sweeps. A flag whose job is still leased simply waits for a later run.

A typical run with no new jobs and no pending-reindex flags lists two empty
prefixes — essentially free. The tradeoff is up to one cron interval of
latency before a new job or profile update is reflected in the eligible index;
this is acceptable given jobs run for many minutes.

All `eligible/` markers for a `job_id` are cleaned up when the job is
permanently removed (result submitted, expired, or planner-deleted).

When claimed, the file is renamed into `leased/`, partitioned by client, with
the lease expiry encoded in the leaf filename:

```
leased/{client_id}/{job_id}.{lease_expiry}.json
```

where `lease_expiry` is an ISO 8601 basic-format timestamp (e.g.
`20240901T154000Z`). The compact form is what filenames carry — the extended
form's `:` separators are not usable in a key.

Partitioning by `client_id` lets `heartbeat` and `reclaim` — which know their
own client — list a single `leased/{client_id}/` prefix instead of the whole
tree (see the claim algorithm below). It also simplifies parsing: `client_id`
is its own path segment, so the leaf `{job_id}.{lease_expiry}.json` — where
`job_id` contains no dots (its charset excludes `.`) and `lease_expiry`
is an ISO 8601 timestamp (no dots) — splits unambiguously on its single
`.`, and a `client_id` containing underscores or dots is not a hazard.

Denial markers are empty files named:

```
denied/{job_id}.{client_id}
```

Creating a marker file is atomic on both local filesystem and S3 Express One
Zone, so no read-modify-write on the job file is required.

> **S3 requirement:** The `tmp/` → `avail/` and `avail/` → `leased/` renames
> require the [S3 Express One Zone](https://aws.amazon.com/s3/storage-classes/express-one-zone/)
> storage class, which provides an atomic `RenameObject` API. The `todo/` tree
> is **not supported on standard S3** — copy + delete is not atomic, and two
> concurrent claimers can both copy a job before either deletes it from `avail/`,
> resulting in the same job being assigned to two clients simultaneously.
>
> The reason we chose this design over one that works on standard S3 is
> simplicity: atomic rename is a single clean primitive, and the whole claim
> and heartbeat path is built around it. Supporting standard S3 would require
> changing the `leased/` filename convention (moving `client_id` and
> `lease_expiry` into the file body) and replacing every rename with
> conditional writes (`If-None-Match: *` on claim, `If-Match: <etag>` on
> heartbeat) — more implementation surface area and more subtle failure modes
> to reason about. The cost difference between storage classes is not a
> meaningful factor for this workload; the `todo/` tree holds small files and
> sees modest request volume.
>
> If Express One Zone is not acceptable — for example, because it is
> single-AZ (lower durability than standard S3), not available in the
> required region, or adds unwanted operational complexity to a deployment
> already on standard S3 — the conditional-write redesign described above is
> the path forward. It is a straightforward change, just not the default.

The contents of a job file look like this:

```json
{
  "job_id": "job-550e8400-e29b-41d4-a716-446655440000",
  "clients": ["one", "two", "three"],
  "requires": ["os:macos", "chip:applem3pro", "ram_bytes:36000000000"],
  "any_of": [["device:macbookpro16", "device:macbookpro14"]],
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

A job file has two structurally separate halves, and the split is the contract:

- **The envelope** — `job_id`, `expires_at`, `clients`, `requires`, `any_of`.
  Scheduling state. The server mints `job_id`, resolves `expires_at`, and reads
  the eligibility fields to decide who may claim the job. None of it reaches the
  device.
- **`spec`** — the run specification, authored entirely by `pipette-plan`: what
  to benchmark, on which model, under which runtime, with which flags. The
  server stores and forwards it **verbatim**, and reads almost nothing from it
  (`spec.benchmark`, below). This is what lets the spec schema evolve in
  `pipette-clients` without a `pipette-mgmt` release — see
  [plan-ingestion.md](plan-ingestion.md) §1 and §7.

`spec` corresponds one-to-one with `pipette-plan-types`' `ClientRunSpec`, which
is the type the client deserializes it into. Its three required fields are
`benchmark`, `model`, and `runtime`; the three flag groups (`model_flags`,
`runtime_flags`, `benchmark_flags`) may be omitted when unset. Each flag group
carries its own `(benchmark_type, runtime_type, model_type)` discriminants, so a
cell whose flags contradict its model or runtime is refused by the client on
arrival rather than after the benchmark body has been fetched. **The flag groups
reject unknown keys**, so a field the client's revision does not define fails the
whole claim terminally; `spec` itself, `model`, and `runtime` all tolerate
unrecognized keys and are the forward-compatible places to add one.

The server interprets exactly one field of `spec`: **`spec.benchmark`**, which it
resolves against its benchmark catalog. It needs that to attribute a synthetic
failure record when it declares a job failed with no client run (see
"Consequences of Failure"), and to fill the claim response's `benchmark_id`.

It touches `spec.model` and `spec.runtime` in two ways that stop short of
interpreting them: ingestion checks they are *present* — a presence check, not a
schema check — so a truncated spec is refused up front instead of leased out to
fail on a device; and a synthetic failure record carries each one canonicalized
into an opaque `model_descriptor` / `runtime_descriptor` string, with any
`auth_token` dropped. Nothing else in `spec` is read.

The lease increment is a server configuration value (uniform across jobs); the
server reports it to the client as `time_window` (ISO 8601 duration, e.g.
`"PT5M"`) in the claim response. On a normal claim this is the full increment
just granted; on the idempotent claim path (see the existing lease check below)
it is instead the *remaining* life of the lease the client already holds, so the
client schedules its next heartbeat before that lease lapses rather than after.
`time_window` is never stored in a job file; a value found in one is ignored and
overwritten in the response.

`expires_at` is an optional ISO 8601 **basic-format** timestamp
(`20240908T000000Z`, matching the `avail/` filename encoding) after which the
job will no longer be assigned; if absent, the job never auto-expires. The body
value is authoritative on the recycle path — when a lapsed lease returns to
`avail/`, the filename is rebuilt from it — so a body carrying the extended form
(`2024-09-08T00:00:00Z`) fails that rebuild, and an absent value recycles to
`never`. Plans ingested through `plans ingest` / `POST /plans` are exempt from
both hazards: ingestion resolves the expiry once (defaulting to 30 days out when
the handoff omits it) and stamps it back in basic format, so body and filename
always agree — see [plan-ingestion.md](plan-ingestion.md) §8.

`any_of` is an optional list of clause groups — each an array of flags — that
defaults to `[]`; when present, a client matching through the capability path
must share at least one flag with **every** group (see matching rules below),
while an explicitly listed client is eligible regardless. `any_of` only narrows
the capability path and never widens eligibility.

At least one of `clients` (non-empty) or `requires` (non-empty) must be present;
`any_of` alone does not make a job claimable, since it only narrows. A job file
with neither `clients` nor `requires` is invalid; the planner must not write such
a file, and the Management server will delete it from `avail/` if it encounters
one.

There will naturally be some repetition between job files, but that is
acceptable.

### Client Matching Rules

A job's requirement is a set of **capability flags** — short strings such as
`os:ios` or `runtime:llama_cpp` — arranged as a conjunction of clauses: a flat
`requires` set (all-of) plus zero or more `any_of` groups (each at-least-one-of).
A client is eligible when its **effective capability set** contains every flag in
`requires` **and** shares at least one flag with every `any_of` group. The full
model is specified in [plan-ingestion.md](plan-ingestion.md) §Capability
matching; this section covers what the matcher reads out of a job file.

A client's effective capability set is computed by the server as:

```
effective_capabilities(client) = normalize(device_* profile) ∪ reported capabilities
```

The server **normalizes** each populated `device_*` field (reported via
`PATCH /clients/me`, see [httpapi.md](httpapi.md)) into a reserved-namespace
flag, and unions it with the free-form `capabilities` the client reports
directly. String values are slugified — lower-cased with whitespace removed — so
`device_os_name: "iOS"` becomes `os:ios` and `device_name: "iPhone 17 Pro"`
becomes `device:iphone17pro`; byte counts are matched exactly by their decimal
value (`device_ram_bytes: 17179869184` → `ram_bytes:17179869184`). Every flag —
including the `device:` family flags `any_of` groups name — is client-attested:
approval gates *who* is trusted, not what they claim, so matching is a
scheduling constraint, not hardware authentication.

| Namespace | Source `device_*` field |
|---|---|
| `os:` | `device_os_name` |
| `os_version:` | `device_os_version` |
| `device:` | `device_name` |
| `chip:` | `device_chip_model` |
| `form_factor:` | `device_form_factor` |
| `ram_bytes:` (exact) | `device_ram_bytes` |
| `gpu:` | `device_gpu_model` |
| `gpu_vram_bytes:` (exact) | `device_gpu_vram_bytes` |
| `npu:` | `device_npu_model` |
| `npu_vram_bytes:` (exact) | `device_npu_vram_bytes` |
| `runtime:` | reported by the client |
| *(free form)* | reported by the client |

Conceptually, the eligible set for a job is:

```
eligible = (clients array)
         ∪ {clients whose effective_capabilities ⊇ requires
            and intersect every any_of group}
```

In practice this set is materialized as `eligible/` markers by
`queue-maintenance` rather than recomputed on every poll (see above). A client
with no device profile and no reported capabilities has an empty effective set
and matches no `requires`, but can still be explicitly listed in `clients`.

Matching is **set containment** over the clauses: a client is eligible when its
effective capability set includes every flag in `requires`
(`effective_capabilities ⊇ requires`) and shares at least one flag with every
`any_of` group. Each flag is compared as a whole, opaque string, so
`runtime:llama_cpp:b9999` and `runtime:llama_cpp` are two distinct, independent
flags. Granularity is therefore explicit on both sides: a client advertises
**every level it supports** (a build-pinned client reports both
`runtime:llama_cpp` and `runtime:llama_cpp:b9999`, so it matches a job requiring
either), and `requires` is authored at the granularity clients report.

Fail-closed semantics protect against author error: a `requires` that is
**empty** matches no client (requiring zero capabilities would otherwise make
everyone eligible), an **empty `any_of` group** matches no client (an
at-least-one-of over nothing is unsatisfiable), and a **malformed** clause — not
a JSON array, or an array with a non-string element — is treated as
unsatisfiable. `any_of` only ever adds conjuncts, so it can shrink the eligible
set but never grow it, and a job whose base `requires` / `clients` is empty stays
unclaimable regardless of any `any_of`. A bad requirement can never *widen*
eligibility; the job simply matches fewer clients (only its explicit `clients`,
if any) until corrected.

## The Client/Management Interaction

The client, when not currently occupied, polls for work by calling
`POST /plans/claim`. The caller is identified by the `X-Client-Id` auth header
(see [authentication.md](authentication.md)). Before selecting a job, the Management server checks two preconditions:

1. **Suspension check:** if a `todo/suspended/{client_id}.json` marker exists in
   `[todo_storage]`, the server returns `204 No Content` immediately.
   Suspended clients cannot claim jobs until an operator clears the flag
   (see below).
2. **Existing lease check:** the server scans all of `todo/leased/` once. Each
   entry's key is `{client_id}/{job_id}.{lease_expiry}.json`, so the client and
   expiry are identifiable by parsing keys in memory — no file reads required.
   Because the expiry is in the key itself, the handler can distinguish a live
   lease from one that expired before the cron ran, with no risk of acting on
   stale state. The live entries belonging to *this* client determine what
   happens next:

   - **Exactly one live lease** → the client is asking for work while already
     holding a job. The innocent explanation is a claim whose response was lost
     in transit: the server leased the job and returned it, the reply never
     arrived, and the client — which never learned the `job_id` and so cannot
     `reclaim` — retried. `claim` is therefore **idempotent** on this path: the
     server reads that one lease's job body and returns it exactly as a fresh
     claim would, with `time_window` set to the lease's *remaining* life rather
     than a new increment. It does not select a second job, and it does not
     renew the lease. Leaving the expiry untouched preserves the recycle safety
     valve: a client that is in fact crash-looping keeps getting the same job
     back but never extends it, so the lease lapses on schedule and the job
     returns to `avail/` for a healthy client. Because the response echoes the
     same `job_id`, a client that *did* receive the original reply de-duplicates
     on it and runs the job only once. If the recovered lease had little life
     left and lapses before the client can heartbeat, the client is still not
     stranded: it now knows the `job_id`, so its heartbeat `404` leads it to
     `reclaim` the job by name — the same recovery used after any lease timeout.
   - **More than one live lease** → the protocol grants a client at most one
     lease at a time, so this is a genuine anomaly: a fast-rebooting client that
     accumulated leases across crashes. The server creates
     `todo/suspended/{client_id}.json` (recording the timestamp and one
     conflicting `job_id` as a triage breadcrumb; an operator can list
     `leased/{client_id}/` for the full set) and returns `204 No Content`,
     halting the accumulation. Suspended clients cannot claim until an operator
     clears the marker.

   The same scan collects every *other* client's live-lease `job_id`s (the
   `taken` set) so selection can skip jobs already claimed.

   This whole-tree scan is unavoidable here because of the `taken` set, but it
   is an O(active leases) scan and active leases are bounded by the number of
   simultaneously running benchmarks — a small number at any realistic fleet
   size, returned in a single S3 response. `leased/` is partitioned by client
   (`leased/{client_id}/...`) not for *this* path — a prefix list returns the
   same keys whether or not they are nested — but for `heartbeat`/`reclaim`,
   which know their own client and list only `leased/{client_id}/`.

If neither precondition fires, the server selects an eligible job by listing
`eligible/clients/{client_id}/` to get candidate jobs — each marker filename
encodes both the `job_id` and the job's `expires_at`. For each candidate the
server skips any with a `denied/{job_id}.{client_id}` marker (this client already
reported a retriable failure for it). It also skips any candidate whose
`expires_at` is already in the past: an expired job is never handed out, even if
`queue-maintenance` has not yet swept it from `avail/`. It does **not** pre-check
that a non-expired job is still in `avail/`: the atomic claim rename below is the
liveness test — if the job has been leased, completed, or removed, the rename
finds no source and the server moves to the next candidate.

**Selection order.** The `eligible/` listing itself is unordered (it reflects
the storage backend's native key order, not a priority). The server imposes the
selection order: candidates are tried **soonest-expiring first**, using the
`expires_at` encoded in each eligible marker filename, with jobs that never
expire (`never`) tried last. A single planner run typically stamps every job it
creates with the same expiry, so the soonest-expiring set is usually large rather
than a single job; **within an equal-expiry tier the order is randomised** so
that concurrent idle clients do not all stampede the same `job_id`. The server
walks this order and claims the first candidate whose atomic rename succeeds; if
a claim loses the rename race (or the marker's job is already gone), it falls
through to the next candidate in the tier, then to the next tier.

The server returns the lease envelope — `job_id`, `benchmark_id`, `time_window`,
and `expires_at` when the job has one — wrapped around the job's `spec`, which is
forwarded unmodified. The stored envelope's eligibility fields (`clients`,
`requires`, `any_of`) are not included: they are inputs to the selection that just
happened. Clients must tolerate unrecognized fields. If no applicable job is available, the server
returns `204 No Content` and stamps `todo/liveness/{client_id}` with the current
time — a blind, last-writer-wins write — so `queue-maintenance` can distinguish an
idle-but-available client from an offline one when estimating plan progress
([plan-ingestion.md §8](plan-ingestion.md)). A busy client needs no stamp: its
`leased/{client_id}/` entry is already proof of life. Clients should wait
approximately 5 minutes before retrying, plus a random jitter of 0–60 seconds to
avoid synchronized polling bursts. Clients may use a different interval if
explicitly configured.

When a job is given to a client, the Management server atomically renames the
file from `todo/avail/{job_id}.{expires_at}.json` to
`todo/leased/{client_id}/{job_id}.{lease_expiry}.json`, where `lease_expiry`
is the current time plus the server's configured lease increment (reported to
the client as `time_window` in the claim response). Because the rename is
atomic, at most
one claimer wins. If the rename fails (the source no longer exists — another
claimer won the race), the server moves on to the next candidate in the
eligible list. If all candidates are exhausted, the server returns
`204 No Content`.

The client is expected to send heartbeat requests via
`PUT /plans/{job_id}/heartbeat` at an interval of half the `time_window`
reported in its claim response (e.g. every 2.5 minutes for a 5-minute lease). On
each heartbeat, the Management server renames the file in `todo/leased/` to
extend the lease expiry by another lease increment (the same configured value). Two responses indicate the client no longer holds a
valid lease and should abort:

- **`404`** — no lease file exists for this `job_id`. The lease expired and
  the cron recycled the job back to `avail/`. If the client continued running
  the benchmark throughout (e.g. due to a network outage), it should try
  `POST /plans/{job_id}/reclaim` before giving up — if the job is still
  unclaimed the reclaim will succeed and the client can continue without
  restarting. If the reclaim also fails, the client should abort and re-poll
  via `POST /plans/claim`.
- **`409`** — a lease exists but belongs to a different client. The calling
  client has been superseded and should abort immediately; any result it
  submits will be silently discarded.

Expired leases are recycled by a periodic cron job (see
[operations.md](operations.md)): any `leased/` file whose `lease_expiry`
timestamp is in the past is renamed back to `avail/{job_id}.{expires_at}.json`
and becomes eligible for reassignment. `queue-maintenance` reads the job body
once to recover `expires_at` before constructing the rename target — this is
one body read per recycled lease, not per run. The claim path does not need to
perform this scan.

If a client's lease expired while it kept running (e.g. a network outage that
suppressed heartbeats), `POST /plans/{job_id}/reclaim` lets it re-acquire the
same job rather than discard the work in progress. The server first checks
whether the calling client *still* holds a lease on the job: if so — for example
the lease expired but `queue-maintenance` has not yet recycled it back to
`avail/` — the server renews that lease and returns `200`, which also pre-empts
a race with the recycler. Otherwise it re-acquires the job from `avail/` using
the same atomic rename `claim` uses, applying the same eligibility and `denied/`
checks. A job held by a *different* client yields `409`; a job that is gone from
`avail/` — completed, recycled-and-reassigned, or past its `expires_at` — yields
`404`. Because reclaim targets one named `job_id`, it returns `404` (not `204`)
when the job cannot be obtained, and a suspended client receives `403` rather
than the `204` `claim` would return.

### tmp/ Cleanup

Partial writes left behind by a crashed planner accumulate in `tmp/`. These are
cleaned up by the `queue-maintenance` cron job — see
[operations.md §3.1](operations.md#31-job-queue-maintenance) for the schedule
and threshold.

## Results

The client will attempt to run the benchmark and report the outcome via
`POST /benchmarks`, including the `job_id`. See [httpapi.md](httpapi.md) for
the full submission schema.

Submissions are written to `submissions/incoming/{job_id}.json`. An **ad-hoc**
submission — one that omits `job_id`, the common path today — has a fresh
`job_id` minted by the server and is **not** subject to the claim binding
below. The binding applies only when a submission **carries** a `job_id` (a
plan-attached run echoing its claim): because the storage key is not
partitioned by client, the server verifies the claim before writing, accepting
the result only from the client that currently holds that job's lease and
rejecting a `job_id` the caller does not hold with `404` (no active claim) or
`409` (held by another client) — see [httpapi.md
§2.7.3](httpapi.md#273-errors). This handles the zombie scenario: if a client's
lease expired and the job was reassigned, the original client's late submission
is **rejected** rather than overwriting the new owner's record (the late client
should `reclaim` first; see [Heartbeat timeout
](#heartbeat-timeout-dead-device)). The write itself is atomic (O_CREAT|O_EXCL
on local filesystem; conditional PUT on S3), giving first-writer-wins as a
backstop against a client double-submitting a job it still holds.

Upon receiving a **terminal** submission for a `job_id` — a success, or a
failure the client marks **non-retriable** (inherent to the
benchmark/model/runtime) — the Management server deletes the job's claimable
state from `todo/leased/` and `todo/avail/` so it is never handed out again.
The client then proceeds to poll for a new job. Orphaned `eligible/` and
`denied/` markers for the completed job are garbage-collected by
`queue-maintenance` within two runs (a marker is dropped only once the job is
seen removed by two consecutive sweeps, so a job caught mid-transition by one
run's listings never loses live markers).

A failure the client marks **retriable** (specific to that device) is the
exception: the job is *not* torn down — it stays in `avail/` for other eligible
clients. See [Consequences of Failure](#consequences-of-failure) below.

If the benchmark harness is *not* alive (device frozen, shutdown, or harness
crashed or hung), heartbeat requests will stop. Once `time_window` elapses
without a heartbeat (i.e. two sequential missed heartbeats), the job's lease
expires and it becomes eligible to be handed to the next matching client.

### Consequences of Failure

#### Client-reported failure

A failure submission carries a `retriable` flag
(see [httpapi.md §2.7.2](httpapi.md)) that tells the server whether the failure
is inherent to the job or specific to the reporting client.

*Non-retriable (`retriable: false`) — inherent failure.* The benchmark, model,
or runtime cannot run successfully on any client (e.g. the runtime errored out,
the model is malformed). Re-running elsewhere is pointless, so the submission is
the job's terminal result. It enters the normal failure pipeline — the scorer
skips warehouse and eval-sample-results writes and transitions it to
`submissions/processed/{job_id}.json.gz` — and the job is torn down: deleted
from `avail/` and `leased/`. Orphaned `eligible/` and `denied/` markers are
garbage-collected by `queue-maintenance` within two runs.

*Retriable (`retriable: true`) — client-specific failure.* Something is wrong
with *this* device (out of disk, thermal throttling, transient local fault), not
the job. The Management server writes a `denied/{job_id}.{client_id}` marker (an
empty existence marker) for the reporting client, removes that client's
`leased/` entry, and **keeps the job in `avail/`** so other eligible clients can
claim it. Whether any eligible client remains is checked by `queue-maintenance`
on each run; the check differs depending on how the job specifies eligibility:

- *`clients`-only jobs* (no `requires` flags): the eligible set is fully
  enumerable. If every client in the `clients` array has a denial marker, the
  job can never succeed, so the `queue-maintenance` all-denied pass converts
  it to a terminal failure: it writes a synthetic failure record to
  `submissions/processed/{job_id}.json.gz` using the reserved client ID
  `"system"`, a `failure_reason` of
  `"All eligible clients reported failure"`, and model/runtime fields copied
  verbatim from the job file. Like any failure submission the record lands
  directly in `processed/` — the scorer has nothing to compute for a failure,
  so `incoming/` would never drain it. The job is then torn down exactly as in
  the non-retriable case (deleted from `avail/`; `eligible/` and `denied/`
  markers GC'd by `queue-maintenance` within two runs). Until that run — at
  most one cron interval after the final denial — the job sits unclaimable
  but harmless: every listed client has a `denied/` marker, and `claim`
  skips denied candidates.
- *Jobs with `requires` flags:* the eligible set is open-ended — a new client
  could register at any time whose capabilities satisfy `requires`. These jobs are
  not failed when the last current denial is recorded; the unclaimed timeout
  (below) handles the case where no matching client appears.

#### Heartbeat timeout (dead device)

The lease expires and the job is returned to `avail/`. A timed-out client that
kept running — for instance because the network went down while the device
finished the benchmark — can still recover its work, but **must re-acquire the
job before submitting**: a result submission is accepted only from the client
that currently holds the lease (see [Results](#results)). On reconnect the
client `POST /plans/{job_id}/reclaim`s; if the job is still in `avail/` the
reclaim succeeds and it submits as normal, and if the job has since been
completed or re-claimed by another client the reclaim fails (`404`/`409`) and
the result is dropped. The Management server therefore does not mark the job
failed on timeout.

When a lease expires, the job simply returns to `avail/` and the client
remains eligible — a timeout is treated as "device unavailable" rather than
"job failed", since the device may have been running the benchmark the whole
time and could still return a result.

The **existing lease check** above governs what happens when a client polls
for work while it still holds a live lease. A client that reboots and re-polls
before its lease expires gets that same job handed back (the idempotent
single-lease path), so a lost claim response or a single-job restart recovers
transparently instead of stranding the client. A crash-looping client is not
suspended for this alone; it simply keeps reacquiring the one job without
renewing it, so the lease still lapses on schedule and the job recycles to a
healthy client — and a job that no client can ever complete is bounded by
`expires_at`, not by suspension. Suspension is reserved for the pathological
case: a client that has *accumulated more than one* live lease, which the
protocol never produces in normal operation. The `todo/suspended/{client_id}.json`
marker is visible to operators (via `pipette-mgmt clients list-suspended`) and
can be cleared with `pipette-mgmt clients unsuspend` once the underlying issue
is resolved.

The check intentionally does **not** cover the slow-reboot case: if a device
is unavailable for longer than `time_window` — whether due to a crash, a
hardware fix, planned maintenance, or a long network outage — the cron has
already recycled the lease back to `avail/` before the client returns. There
is no active lease to detect, and no suspension fires. This is the correct
behavior: from the system's perspective the device was simply absent and is
now ready to work again, which is indistinguishable from a legitimately
recovered device returning to service. No operator action is required.

The edge case is a device that is the sole eligible client for a job and
repeatedly times out without ever completing it. The primary mitigation is
`expires_at`: planners should set an expiry on all jobs so that a
repeatedly-failing job is eventually abandoned rather than spinning forever.

#### Expiration

If a job's `expires_at` filename component is in the past, it is
treated as abandoned. `queue-maintenance` writes a synthetic failure record to
`submissions/processed/{job_id}.json.gz` using client ID `"system"`, a
`failure_reason` of `"Job expired at {expires_at} before any client completed
it"`, and model/runtime fields from the job file. The job file is deleted from
`avail/`, and the same run's GC sweeps then collect its `denied/` and
`eligible/` markers. A job that already has a submission record is not
expired — terminal teardown is best-effort and can leave the `avail/` entry
behind, so `queue-maintenance` deletes the leftover entry and keeps the
existing record. The
record is a terminal result from the moment it is written — like the
all-clients-denied case above it lands directly in `processed/`, never passing
through the scorer. Jobs without `expires_at` (filename component
`never`) never auto-expire; the planner can cancel such a job at any time by
deleting the file from `avail/` directly.

`expires_at` is enforced only when work is *handed out*, not while it is in
progress. `claim` and `reclaim` skip a job whose `expires_at` is in the past,
treating it as gone — `claim` simply finds no eligible candidate (`204`), and
`reclaim` returns `404`. But `heartbeat`, and a `reclaim` that finds the calling
client still holding the lease, never fail on account of `expires_at`: a
benchmark already running is allowed to finish. A leased job that crosses its
deadline mid-run is expired only after its lease lapses, is recycled to `avail/`,
and is then found there past `expires_at` — so the deadline bounds how long a job
waits to be assigned, not how long a client that holds it may run.
