# CLI Reference

## Binary

The single binary is called `pipette-mgmt`.

## Global flags

| Flag | Env fallback | Default | Description |
|------|-------------|---------|-------------|
| `--config <path>` | `PIPETTE_MGMT_CONFIG` | `/etc/pipette-mgmt/config.toml` if it exists, otherwise `~/.config/pipette-mgmt/config.toml` | Path to TOML configuration file |

## Configuration file

`pipette-mgmt` reads a TOML file for all settings. Example:

```toml
# Required — base URL of the edge-evals scoring server.
evals_server_url = "http://evals:8000"

# Optional — address the HTTP server listens on.
# Default: "0.0.0.0:3000"
listen_addr = "0.0.0.0:3000"

# Optional — days back to scan for job metrics.
# Default: 14
warehouse_read_days = 14

# Optional — rows per Parquet part file.
# Default: 1000
warehouse_max_rows_per_part = 1000

# Optional — zstd compression level for Parquet writes. Valid range: 1..=22.
# Default: 3
parquet_zstd_level = 3

# Optional — incoming submissions or queued evals listed per processing chunk.
# Default: 50
score_chunk_size = 50

# Optional — model parameter mapping path/key.
# model_params_mapping_path = "/etc/pipette-mgmt/model_params_mapping.toml"

# Optional — reject every keyless registration.
# Default: false
# require_preauth_key = false

# Optional — also accept signatures covering only the timestamp, so clients can
# migrate to the v1 signed payload without a flag day. Set false once the
# "accepted timestamp-only signature" warnings stop.
# Default: true
# accept_legacy_signatures = true

# Storage backend. Default: local_fs with data_dir = "./data"
[storage]
backend = "local_fs"
data_dir = "/var/lib/pipette-mgmt"

# To use S3 instead:
# [storage]
# backend = "s3"
# bucket = "my-bucket"
# prefix = "v1/"
# region = "us-east-1"
# endpoint = "https://s3.custom.example.com"  # for MinIO, R2, etc.
# max_concurrent_requests = 32

# Required: storage for client auth data (keys, identities).
[auth_storage]
backend = "local_fs"
data_dir = "/var/lib/pipette-mgmt"

# To use S3 instead:
# [auth_storage]
# backend = "s3"
# bucket = "my-auth-bucket"
# region = "us-east-1"
# max_concurrent_requests = 32

# Optional: job-queue storage. Defaults to [storage]. For S3 planner
# deployments, use a separate S3 Express One Zone bucket; see storage.md §9.
# [todo_storage]
# backend = "s3"
# bucket = "my-todo-bucket"   # must be an Express One Zone bucket
# region = "us-east-1"
# max_concurrent_requests = 32

# Optional — hold pending-client submissions instead of rejecting them.
# [unverified_submissions]
# enabled = false

# Optional — approve a client at registration when its contact_email matches.
# NOT a security control: email is self-reported and unverified.
# [auto_approve]
# emails = ["alice@example.com"]
# domains = ["example.org"]
```

Credentials for S3 are read from the environment (not the config file):
`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_REGION`,
`AWS_DEFAULT_REGION`, or IAM instance/task roles on EC2/ECS/Lambda.

| Key | Required | Default | Used by | Description |
|-----|----------|---------|---------|-------------|
| `evals_server_url` | **yes** | — | `serve`, `score-eval` | Base URL of the edge-evals scoring server |
| `listen_addr` | no | `0.0.0.0:3000` | `serve` | Address the HTTP server listens on |
| `catalog_ttl_secs` | no | `180` | `serve` | Seconds to cache the benchmark catalog before re-reading from disk |
| `warehouse_read_days` | no | `14` | `serve` | Hard cap on how many days back a job-metrics lookup scans; a job scored longer ago is reported as `processed` with `metrics: null` |
| `warehouse_max_rows_per_part` | no | `1000` | `process-submissions` / `score` | Rows per Parquet part file; writes append to the tail part and roll at this size |
| `http_timeout_secs` | no | `600` | `serve`, `process-submissions` / `score`, `score-eval` | HTTP client request timeout in seconds |
| `mutate_lock_ttl_secs` | no | `1800` | `process-submissions` / `score`, `score-eval`, `fix-*` | `s3` storage lease duration; must be non-zero (see [Concurrency](#concurrency)) |
| `parquet_zstd_level` | no | `3` | `process-submissions` / `score`, `fix-*` | zstd compression level for Parquet writes; valid range `1..=22` |
| `score_chunk_size` | no | `50` | `process-submissions` / `score`, `score-eval` | Number of submissions or queued evals listed per scoring iteration; each command drains its backlog in chunks |
| `model_params_mapping_path` | no | storage-root `model_params_mapping.toml` | `serve`, `process-submissions` / `score`, `fix-model-param` | Path/key for the model parameter mapping catalog. For `local_fs`, relative paths resolve against the process cwd; for `s3`, the value is used as an object key without prepending `storage.prefix` |
| `storage.backend` | no | `local_fs` | all | Storage backend: `local_fs` or `s3` |
| `storage.data_dir` | no | `./data` | `local_fs` | Root directory for all persistent data |
| `storage.bucket` | when `s3` | — | `s3` | S3 bucket name |
| `storage.prefix` | no | `""` | `s3` | Key prefix for all S3 objects |
| `storage.region` | no | — | `s3` | AWS region (also reads `AWS_REGION` env) |
| `storage.endpoint` | no | — | `s3` | Custom S3-compatible endpoint (MinIO, R2) |
| `storage.max_concurrent_requests` | no | `32` | `s3` | Cap on concurrent S3 requests issued by fan-out operations |
| `auth_storage.backend` | **yes** | — | all | Auth storage backend: `local_fs` or `s3` |
| `auth_storage.data_dir` | no | `./data` | `local_fs` | Root directory for client auth data |
| `auth_storage.bucket` | when `s3` | — | `s3` | S3 bucket for client auth data |
| `auth_storage.prefix` | no | `""` | `s3` | Key prefix for auth S3 objects |
| `auth_storage.region` | no | — | `s3` | AWS region for auth bucket |
| `auth_storage.endpoint` | no | — | `s3` | Custom S3-compatible endpoint for auth bucket |
| `auth_storage.max_concurrent_requests` | no | `32` | `s3` | Cap on concurrent S3 requests issued by auth-store fan-out operations |
| `todo_storage.backend` | no | inherits `[storage]` | `queue-maintenance`, `serve` | Storage backend for the `todo/` job queue: `local_fs` or `s3`. Defaults to the `[storage]` backend if not set. For S3 deployments using the planner, this **must** point to an S3 Express One Zone bucket (see [storage.md §9](storage.md#todo-requires-s3-express-one-zone)) |
| `todo_storage.data_dir` | no | inherits `[storage]` | `local_fs` | Root directory for `todo/` data (defaults to `[storage].data_dir`) |
| `todo_storage.bucket` | when `s3` | — | `s3` | S3 Express One Zone bucket for the job queue |
| `todo_storage.prefix` | no | `""` | `s3` | Key prefix for job queue S3 objects |
| `todo_storage.region` | no | — | `s3` | AWS region for the job queue bucket |
| `todo_storage.endpoint` | no | — | `s3` | Custom S3-compatible endpoint for the job queue bucket |
| `todo_storage.max_concurrent_requests` | no | `32` | `s3` | Cap on concurrent S3 requests issued by todo-store fan-out operations |
| `plan_lease_duration_secs` | no | `300` | `serve` | Lease duration in seconds granted by `POST /plans/claim` and extended by each heartbeat; must be non-zero. Surfaced to clients as `time_window` (ISO 8601) in the claim response — clients heartbeat at half this interval |
| `todo_tmp_max_age_secs` | no | `86400` | `queue-maintenance` | Age in seconds past which a partial job file under `todo/tmp/` is deleted; must be non-zero. On S3 a lifecycle rule on the `todo/tmp/` prefix can replace the pass (see [operations.md §3.1](operations.md#31-job-queue-maintenance)) |
| `unverified_submissions.enabled` | no | `false` | `serve` | Hold a pending (unapproved) client's `POST /benchmarks` submissions under `submissions/unverified/{client_id}/` instead of rejecting them. When `false`, a pending client's submission is rejected with `403` |
| `auto_approve.emails` | no | `[]` | `serve` | Full email addresses that auto-approve a client at registration, matched case-insensitively against `contact_email`. Empty = off |
| `auto_approve.domains` | no | `[]` | `serve` | Email domains (the part after `@`) that auto-approve a client at registration, matched case-insensitively. Empty = off |
| `require_preauth_key` | no | `false` | `serve` | Reject keyless registrations with `403`; valid `preauth_key` registrations still auto-approve |
| `accept_legacy_signatures` | no | `true` | `serve` | Accept a signature covering only `X-Timestamp` when the `v1` signed payload fails to verify, logging each acceptance at `warn` with the client id. Such a signature is replayable against any authenticated endpoint within the 5-minute window, and is the one request kind that gets no replay protection. Offered only to clients that have never presented a `v1` signature: the first one a client sends withdraws its fallback permanently, so this setting governs the clients still to migrate rather than all of them — see [authentication.md §2.3](authentication.md#23-timestamp-only-signatures) |

## Subcommands

### `pipette-mgmt serve`

Starts the HTTP server. Reads `evals_server_url` and `listen_addr` from config.
Loads benchmark definitions from `{data_dir}/benchmarks/*.toml` at startup.

### `pipette-mgmt process-submissions` (alias `score`)

The fast pass. Processes all pending submissions and exits — routing eval
submissions to the score-queue, scoring non-eval submissions, and finalizing
evals the slow pass has already scored. It never calls the scoring service,
so it stays quick and holds the storage mutate lock only briefly. Intended to
run frequently as a cron job.

Loads benchmark definitions from `{data_dir}/benchmarks/*.toml` at startup.
Prints a summary line (e.g. `Done. 5 submission(s): 3 scored, 2 failed.`) to
stdout and exits with a non-zero code if any jobs failed.

`score` is a visible alias, so an existing `pipette-mgmt score` cron keeps
working as this pass. New scripts should use `process-submissions`.

Scoring copies submitted `model_name` and `model_quant` values into warehouse
rows for every runtime, including `mlx-lm`. It does not reconstruct MLX repo
ids or derive quantization from the model name.

### `pipette-mgmt score-eval`

The slow pass, and the actual scorer for eval benchmarks. Drains the
score-queue, makes the scoring-service call for each eval job, and stages the
result for the next `process-submissions` run to finalize. Without it, eval
jobs route into the queue but are never scored.

It takes its own `score-eval` advisory lock rather than the shared `mutate`
lock, so a run never blocks `process-submissions`, the `fix-*` commands, or
`requeue-eval`. The lock also makes overlapping invocations safe: a second
one exits immediately rather than double-scoring a job.

Runs on its own schedule — see [operations.md](operations.md) for the cron
entries and the split between the two passes.

### `pipette-mgmt queue-maintenance`

Reconciles all stale state in the `todo/` job queue and exits. Intended to run
as a cron job, every 1–5 minutes — the interval is a latency knob, not a
correctness requirement; it bounds how long a dead device's job stays
unclaimable past its lease expiry and how long a new job waits to enter the
eligible index (see [operations.md
§3.1](operations.md#31-job-queue-maintenance)). Each run, in order: recycles
expired leases back to `avail/`, converts jobs past their `expires_at` into
synthetic `"system"` failure records (written directly to
`submissions/processed/`), updates the `eligible/` index (new jobs and
pending-reindex flags), garbage-collects orphaned `eligible/` and `denied/`
markers, and deletes `todo/tmp/` files older than `todo_tmp_max_age_secs`.

Loads benchmark definitions from `{data_dir}/benchmarks/*.toml` at startup
(the expiry pass resolves each job's `benchmark_type` from the catalog) and
validates that the `todo/` backend supports atomic renames before mutating
anything. Takes no mutate lock — it writes only to `todo/` and
`submissions/processed/`, disjoint from the paths `process-submissions` /
`score` and the `fix-*` commands serialize on. A failed lease-recycle or
job-expiry item is logged, skipped, and retried on the next run; a failure in
any later pass (indexing, marker GC, `tmp/` cleanup) aborts the run, which is
safe to rerun. Either way the command exits non-zero so persistent problems
surface through cron monitoring.

### `pipette-mgmt fix-model-param` (temporary)

> **Temporary maintenance command.** Added to ease the rollout of the
> `model_params_total_millions` / `_active_millions` columns. Once
> historical warehouse rows are aligned with the current
> `model_params_mapping.toml`, this command can be removed.

Walks every Parquet file under `warehouse/results/` and rewrites the
`model_params_total_millions` and `model_params_active_millions`
columns from the current `model_params_mapping.toml` catalog. Use this after editing
the catalog (extending it, correcting a value, splitting MoE
total/active) so existing warehouse rows stay aligned with the
catalog without re-scoring affected jobs.

Behavior:

- Rows are resolved against the catalog the same way the scorer
  resolves them: by `model_name` first, then — when `model_name` is
  absent or unrecognized — by a substring match against the opaque
  `model_descriptor`. Both columns are set to the resolved entry's
  `(total, active)`. Rows already aligned are not rewritten.
- For rows that resolve through neither path, the row is left
  untouched and counted as unknown. Each distinct unknown identity is
  logged once per run.
- Files where every row is already aligned are not rewritten — mtime
  is preserved.
- Rewrites use `parquet_zstd_level` from the config (the same
  compression as the rest of the warehouse pipeline). To run with a
  different compression for a one-off, point at a config that sets
  the level you want.

Use `--dry-run` to count rows that would be updated without rewriting
any Parquet files:

```bash
pipette-mgmt --config config.toml fix-model-param --dry-run
```

Use `--model` to restrict the rewrite to specific models — every other
row is left untouched and not counted. The flag is repeatable (`--model
A --model B`) and accepts a comma-separated list (`--model A,B`); both
accumulate. Names are normalized the same way as catalog lookups, so a
single `--model LiquidAI/LFM2.5-230M` also covers the `-GGUF` repo and
any quant / distribution variant. A row matches on either identity: its
normalized `model_name`, or — for rows that carry no `model_name` — the
catalog key its `model_descriptor` resolved to. This is the safe way to
apply a single catalog edit (e.g. a newly added model) without rewriting
unrelated partitions:

```bash
pipette-mgmt --config config.toml fix-model-param \
  --model LiquidAI/LFM2.5-230M,LiquidAI/LFM2.5-230M-GGUF
```

After adding or correcting model sizes in `model_params_mapping.toml`,
re-run the live fix to align historical warehouse rows with the new
catalog values:

```bash
pipette-mgmt --config config.toml fix-model-param
```

Works on both `local_fs` and `s3` storage backends. Exits non-zero if
the catalog is empty (no `model_params_mapping.toml` or zero entries) — there's
nothing to "fix from" in that case.

Concurrency with `process-submissions` / `score` and the other `fix-*` commands
is enforced by the [storage mutate lock](#concurrency) — a second mutating
command fails fast rather than corrupting the warehouse. Individual file
rewrites are atomic on their own (tmp + rename on local-fs, single PUT on S3) —
a crash mid-run leaves data consistent.

On stdout you'll see a one-line summary like:

```
fix-model-param: updated 37 rows; 8 rows reference models not in the catalog and were left unchanged
```

With `RUST_LOG=info` the same totals are also emitted as a
`tracing::info!` event.

### `pipette-mgmt fix-canonical`

Walks every Parquet file under `warehouse/results/` and re-canonicalizes
the warehouse's five opaque JSON columns, recomputing each one's
`_sha256` content id from the canonical form:

| Column | Hash column |
| --- | --- |
| `model_descriptor` | `model_descriptor_sha256` |
| `runtime_descriptor` | `runtime_descriptor_sha256` |
| `benchmark_flags` | `benchmark_flags_sha256` |
| `model_flags` | `model_flags_sha256` |
| `runtime_flags` | `runtime_flags_sha256` |

Use it after a change to the canonicalization rules, or to backfill a
hash column that did not exist when a row was written. Rows stored
before `model_flags` / `runtime_flags` were normalized hold whatever the
client sent, so one logical configuration can sit in several grouping
buckets — this brings them onto the current rules. See
[storage.md § model_flags / runtime_flags](storage.md#model_flags--runtime_flags).

Behavior:

- Each column is rewritten to the output of the same
  `canonical_json` functions the ingest path uses: object keys sorted
  at every level, insignificant whitespace stripped.
- A value that does not parse as JSON is stored trimmed and otherwise
  unchanged, never dropped — `model_flags` / `runtime_flags` accept a
  plain string (`--n-gpu-layers 999`) by contract.
- On the three flag columns, a top-level empty object collapses to
  NULL so "nothing reported" has one spelling; a nested empty object
  is preserved.
- Each `_sha256` is recomputed from the canonical value whenever its
  column is in scope, which is what backfills a hash column that was
  NULL because it postdates the row.
- Rows already canonical are not rewritten; files where no row changes
  are not touched (mtime preserved).
- Rewrites use `parquet_zstd_level` from the config, like the rest of
  the warehouse pipeline.

Use `--dry-run` to count what would change without rewriting anything:

```bash
pipette-mgmt --config config.toml fix-canonical --dry-run
```

Use `--column` to restrict the rewrite to specific columns — every
other column is left as stored and not counted. The flag is repeatable
(`--column A --column B`) and accepts a comma-separated list
(`--column A,B`); both accumulate. An unknown column name is an error
rather than a silent no-op, so a typo can't report "nothing to do" on a
real backlog:

```bash
pipette-mgmt --config config.toml fix-canonical \
  --column model_flags,runtime_flags
```

Works on both `local_fs` and `s3` storage backends.

Concurrency with `process-submissions` / `score` and the other `fix-*` commands
is enforced by the [storage mutate lock](#concurrency) — a second mutating
command fails fast rather than corrupting the warehouse. Individual file
rewrites are atomic on their own (tmp + rename on local-fs, single PUT on S3) —
a crash mid-run leaves data consistent, and the command is idempotent, so a
partial run is fixed by running it again.

On stdout you'll see a one-line summary like:

```
fix-canonical: updated 412 rows (model_descriptor 0, runtime_descriptor 0, benchmark_flags 12, model_flags 400, runtime_flags 412)
```

The per-column breakdown says which rollout the backlog came from. With
`RUST_LOG=info` the same totals are also emitted as a `tracing::info!`
event.

### `pipette-mgmt requeue-eval`

Re-stages already-scored submissions for an eval benchmark back into
`submissions/incoming/` as fresh submissions, so the next scoring passes
(`process-submissions`, `score-eval`, then `process-submissions` again)
score them again. Use this after a scorer fix changes how an eval is
graded and the historical verdicts in the warehouse need to be
recomputed.

The named benchmark is looked up in the configured catalog; the command
errors unless it exists and is an `eval`. Jobs are then identified from
the **warehouse metrics**, not the submission bodies: one pass over the
warehouse (the same walk the `fix-*` commands use) collects the jobs
whose rows carry that `benchmark_id`. Each is rebuilt through the typed
model — the submit handler's own `into_submission` + `to_value` path,
with `benchmark_type` resolved from the catalog — and written into
`incoming/`: canonical shape, stray keys dropped, the original
`client_id` preserved, but a **fresh `job_id`** and **`submitted_at =
now`**. The original `job_id`'s processed archive and warehouse rows
are left untouched (copy, not move).

Re-scoring appends new warehouse rows and writes a new
`processed/{new_job_id}` archive; old verdicts remain readable by `job_id` and
`submitted_at`. Re-running over the whole benchmark creates another copy, so
use `--submitted-before` to exclude freshly re-staged submissions.

Required flag:

| Flag | Description |
|------|-------------|
| `--benchmark-id <id>` | Benchmark id to re-score, e.g. `eval_ifbench_2026.06.1` — a catalog key, not the bare eval id. Must resolve to an eval. Re-run per dataset to cover an eval's other benchmarks. |

Optional flags (the filters AND together):

| Flag | Description |
|------|-------------|
| `--submitted-after <ts>` | Only re-stage jobs submitted at or after `<ts>`. Accepts RFC3339 (`2026-06-01T00:00:00Z`) or a bare `YYYY-MM-DD` (midnight UTC). |
| `--submitted-before <ts>` | Only re-stage jobs submitted at or before `<ts>` (same formats). Set just before the migration started to exclude already-re-staged copies and avoid doubling on re-runs. |
| `--score-runtime-version <v>` | Only re-stage jobs whose recorded `score_runtime_version` matches exactly. For evals this is the client's on-device runtime version, not a scoring-service version. |
| `--dry-run` | Count the jobs that would be re-staged without writing anything |

```bash
# Preview, then re-stage this benchmark's pre-migration jobs, then re-score.
# Pin --submitted-before so repeat runs don't re-stage the fresh copies.
pipette-mgmt --config config.toml requeue-eval --benchmark-id eval_ifbench_2026.06.1 --submitted-before 2026-06-01T00:00:00Z --dry-run
pipette-mgmt --config config.toml requeue-eval --benchmark-id eval_ifbench_2026.06.1 --submitted-before 2026-06-01T00:00:00Z
pipette-mgmt --config config.toml process-submissions
pipette-mgmt --config config.toml score-eval
pipette-mgmt --config config.toml process-submissions
```

Individual jobs are skipped (logged at `warn`) rather than failing the
run when their processed body is gone, their body does not parse, or it
is a failure submission. The run prints a summary of how many were
re-staged versus skipped (and why). Works on both `local_fs` and `s3`
backends. A live run holds the storage mutate lock; `--dry-run` is
read-only and skips it.

### `pipette-mgmt clients list [--tag <tag>]...`

Lists all registered clients and their current status (see [authentication.md §4](authentication.md#4-access-matrix)).
Pass `--tag <tag>` to filter to clients carrying that tag; the filter is served
from the reverse tag index (`tags-index/by-tag/{tag}/`), not a full scan. The flag is
repeatable and ANDs — a client must carry every `--tag` given. To see a single
client's tags, use `clients tag list <client_id>`.

The `Migrated` column shows when each client first signed a `v1` payload, or `—`
for one that has only ever used the timestamp-only fallback. Once every client
shows a date, `accept_legacy_signatures` can be cleared
([authentication.md §2.3](authentication.md#23-timestamp-only-signatures)).

```
pipette-mgmt clients list --tag team-mobile --tag us-east
```

### `pipette-mgmt clients tag add|remove|list`

Manage a client's flat tags (see
[authentication.md §6](authentication.md#6-client-tags)). Tags are assigned
manually here on the mgmt side; clients never set their own.

```
# add one or more tags (idempotent — already-present tags are skipped)
pipette-mgmt clients tag add <client_id> team-mobile us-east

# remove one or more tags (no-op for tags the client does not have)
pipette-mgmt clients tag remove <client_id> us-east

# list a client's tags (sorted)
pipette-mgmt clients tag list <client_id>
```

Tag format: a flat token of `[a-z0-9_-]` (no `/`), trimmed and lowercased on
input, bounded at 64 chars.

### `pipette-mgmt clients approve <client_id>`

Approves a pending client, granting access to browse and submit benchmarks.

### `pipette-mgmt clients reject <client_id>`

Deletes a pending client's registration. This is only valid for clients whose
status is still `pending`.

### `pipette-mgmt clients delete <client_id>`

Deletes a client identity regardless of whether it is `pending` or `approved`.
Also cleans up the client's job-queue state on a best-effort basis: its
`todo/suspended/{client_id}.json` marker, all `eligible/clients/{client_id}/`
markers, and all `pending-reindex/` flags. A failure to remove any of these is
logged but does not fail the command — the identity is already gone, and
`queue-maintenance`'s orphan reconciliation collects every leftover within at
most two runs (it sweeps markers for any client absent from the auth roster,
so convergence does not depend on this purge having run or succeeded).

Idempotent and safe to re-run: every step tolerates an already-absent target,
so if a prior run partially failed (record removed but some queue-state markers
orphaned, or vice versa), running it again finishes the cleanup instead of
erroring. Because an absent record is not treated as an error, a mistyped id is
indistinguishable from an already-deleted one; the command reports whether an
identity record was actually found rather than failing.

It does not remove historical submissions, processed jobs, or warehouse data —
those records reference the `client_id` but are not owned by the client record.

### `pipette-mgmt clients list-suspended`

Lists all currently suspended clients with their `suspended_at` timestamp and
the `conflicting_job_id` that triggered the suspension. Useful for periodic
operator checks or as the target of an alerting script.

### `pipette-mgmt clients unsuspend <client_id>`

Clears a client's suspension flag, allowing it to claim jobs again. A client
is automatically suspended when it calls `POST /plans/claim` while already
holding an unexpired lease — a signal of unexpected reboot or crash loop (see
[planner.md](planner.md)). Unsuspending does not approve or otherwise modify
the client record; it only removes the `todo/suspended/{client_id}.json` marker.
Idempotent: clearing a client that is not currently suspended is a successful
no-op.

### `pipette-mgmt clients update <client_id> [flags]`

Updates mutable details on an existing client. The `client_id`, `public_key`,
`status`, and `registered_at` fields are not changed by this command — use
`approve`, `reject`, or `delete` to change client lifecycle state.

At least one of the following flags must be provided:

| Flag | Updates |
|------|---------|
| `--organization <name>` | The organization name |
| `--details <text>` | The free-form `client_details` field |
| `--email <email>` | The contact email |

If a flag is provided with a value that already matches the stored client, that
field is treated as unchanged. The command prints the before/after value for
each field that actually changed.

```bash
pipette-mgmt clients update ev1_abc123 --organization "Acme Inc." --email ops@acme.example
```

### `pipette-mgmt unverified`

Operator commands for the held-submission archive
(`submissions/unverified/{client_id}/`; see
[storage.md §4.1](storage.md#41-unverified-submissions)). None of these
interact with the warehouse, the eval sample results store, or the storage
mutate lock — the unverified tree is disjoint from all of those, so they
are safe to run while `serve` and `process-submissions` / `score` are active.
Each accepts `--dry-run` to report what would change without writing or
deleting.

#### `unverified promote --client-id <id>`

Re-stages a client's held submissions into the normal pipeline so the
scorer picks them up: `success` bodies move to `incoming/`, `failure`
bodies to `processed/`. Each object is removed from the unverified tree
only after its re-stage write succeeds. Run this after approving a client
whose earlier submissions were held.

```bash
pipette-mgmt --config config.toml unverified promote --client-id ev1_abc123
```

#### `unverified delete --client-id <id>`

Deletes every held submission for one client. Run this after rejecting a
client whose held submissions should be discarded.

```bash
pipette-mgmt --config config.toml unverified delete --client-id ev1_abc123
```

#### `unverified prune --older-than <duration>`

Deletes held objects across all clients whose backend modification time
(S3 `LastModified` / filesystem `mtime`) is older than the given age, to
bound the size of the archive.

| Flag | Description |
|------|-------------|
| `--older-than <duration>` | Required. Age threshold (e.g. `7d`, `24h`, `30m`). Objects modified more recently are kept |

```bash
pipette-mgmt --config config.toml unverified prune --older-than 7d
```

### `pipette-mgmt unlock`

Inspects the storage mutate lock (see [Concurrency](#concurrency)), and on the
`s3` backend clears a stale lease. Use it only to recover from a
`process-submissions` / `score`, `fix-*`, or `requeue-eval` command that
crashed.

```bash
pipette-mgmt --config config.toml unlock

# s3 only: clear the lease even if it is still active (a command may be running).
pipette-mgmt --config config.toml unlock --force
```

On `local_fs` the lock is an `flock(2)` that the kernel releases when the
holding process exits or dies, so there is never a stale lock to clear —
`unlock` only reports whether a command currently holds it. On `s3`, `unlock`
prints the holder, host, pid, and lease window; it deletes the lease if it has
already expired, and `--force` deletes it even while still active. With no lock
held it exits 0.

### `pipette-mgmt reindex`

Rebuild the reverse tag index (`tags-index/by-tag/`) by reconciling it against
the authoritative forward tree (`tags-index/by-client/`): create missing reverse
markers, drop orphan reverse entries, and prune markers for since-deleted
clients. Idempotent and convergent — safe to run any time; a no-op when the
trees already agree. See [authentication.md §6](authentication.md#6-client-tags).

The `todo/eligible/` index is rebuilt separately by
[`queue-maintenance`](#pipette-mgmt-queue-maintenance), which owns the job queue.

```bash
pipette-mgmt --config config.toml reindex
```

### `pipette-mgmt preauth create|list|revoke|prune`

Mint and manage pre-auth registration keys (see
[authentication.md §3.2](authentication.md#32-pre-auth-keys)). `create` prints
the token **once** — the secret is never stored or shown again.

```bash
# single-use key (expires in 90 days by default); a valid key always approves
pipette-mgmt --config config.toml preauth create

# multi-use, custom expiry, seeding tags + org onto each client
pipette-mgmt --config config.toml preauth create \
  --multi-use --expires-in 30d --tag team-mobile --org acme --note "field pilot"

# multi-use, never expiring
pipette-mgmt --config config.toml preauth create --multi-use --no-expiry

pipette-mgmt --config config.toml preauth list           # metadata only, no secret
pipette-mgmt --config config.toml preauth revoke <key_id>  # deletes the key

# delete expired keys and the markers left by spent ones
pipette-mgmt --config config.toml preauth prune --dry-run  # preview
pipette-mgmt --config config.toml preauth prune
```

Keys are single-use unless `--multi-use` is given, and expire after 90 days
unless `--expires-in <dur>` sets a different window or `--no-expiry` makes them
permanent. A key record is write-once — the only mutation is deletion: a spent
single-use key deletes itself on consume, `revoke` deletes on demand, and
`prune` deletes expired keys.

Spending a single-use key leaves a small `preauth/{key_id}.spent` marker, which
is what makes the spend exactly-once even against simultaneous registrations
(see [authentication.md §3.2](authentication.md#32-pre-auth-keys)). `prune` also
clears markers whose key record is already gone; it keeps any whose record still
exists, because there the marker is the only thing holding that key spent.
Markers never appear in `preauth list`.

Set `require_preauth_key = true` in the config to reject every keyless
registration with `403`.

### `pipette-mgmt plans ingest|list|status|cancel`

Ingest and administer plans. A **plan** is a set of jobs expanded by
`pipette-plan` into a directory of job-body files, ingested here as a unit and
tracked by a manifest. See [plan-ingestion.md](plan-ingestion.md) for the
handoff contract (§7), the ingestion flow (§8), and the plan lifecycle (§9).

```bash
# ingest a directory of job files as one plan
pipette-mgmt --config config.toml plans ingest ./out --plan-name afm-smoke-2026.07

pipette-mgmt --config config.toml plans list
pipette-mgmt --config config.toml plans list --status pending_clients

pipette-mgmt --config config.toml plans status plan-018fce2a-…
pipette-mgmt --config config.toml plans status --plan-name afm-smoke-2026.07

pipette-mgmt --config config.toml plans cancel plan-018fce2a-…
pipette-mgmt --config config.toml plans cancel --plan-name afm-smoke-2026.07
```

Runs on the management server's host with direct storage access, like the other
subcommands — no HTTP and no authentication involved. Loads benchmark
definitions from `{data_dir}/benchmarks/*.toml` (§6.2 resolves each job's
`benchmark_id` against the catalog) and validates that the `todo/` backend
supports atomic renames before staging anything. Takes **no mutate lock**: it
writes only `plans/` and `todo/`, disjoint from the paths `score` and the
`fix-*` commands serialize on.

**`ingest <dir>`** treats every `*.json` file in the directory as one job body.
Other files are ignored, subdirectories are not descended into, and the
directory is never modified — so a generator's README or log can sit alongside
the jobs, and the directory can be re-ingested or archived afterwards. Files are
processed in file-name order, which fixes the order ids are minted in.

The whole set is validated before anything is written, and **any rejection
rejects the ingest as a unit** (§6.2) — a malformed file, a job carrying a
`job_id`/`plan_id`, a job with no eligibility, more than one flag from a
reserved namespace, or an unknown `benchmark_id`. The command exits non-zero
naming the offending file and its reason, with nothing staged:

```console
$ pipette-mgmt --config config.toml plans ingest ./out
Error: rejected job "two-os.json"

Caused by:
    more than one `os:` flag in `requires` (reserved namespaces allow at most one)
```

On success it prints the **ingest report** as JSON on stdout — the `plan_id`,
the file→job-id map, and any warnings (§8). This is the record of which input
file became which job, so it is worth saving; it is also the exact body
`POST /plans` returns. A *warning* does not fail the ingest: a plan whose jobs
match no registered, approved client is accepted and waits, which is the honest
outcome for work queued ahead of the fleet.

Two things the handoff deliberately does not protect against (§7): a truncated
directory ingests as a smaller valid plan — check `job_count` against what you
expected — and **ingesting the same directory twice creates a second plan with
duplicate jobs**, since each ingest mints fresh ids. Both are accepted
operator-error windows.

**`status`** is a single manifest read. `progress_snapshot` is computed and
cached by `queue-maintenance`, never recomputed here, so a plan whose first
maintenance pass has not run yet reports the snapshot as not yet computed rather
than as zeroes. The `Warnings at ingestion` block is frozen at ingest time and
can legitimately disagree with the live starvation list once clients register
afterwards.

**`cancel`** records a cancellation request and returns; it does **not** delete
any jobs. It writes a marker (`cancelled_plans/{plan_id}`) for
`queue-maintenance` to consume, retiring the plan's jobs and latching the
manifest to `cancelled` — keeping that pass the single writer of both, so a
cancel can never be lost to a concurrent status refresh (§9).

> **Teardown is not yet in effect.** `queue-maintenance` does not consume the
> marker yet, so today `plans cancel` records intent and nothing more: the
> plan's jobs stay in `avail/`, remain claimable, and a client already running
> one runs to completion. Cancelling is currently useful as a record of the
> decision — not as a way to stop work. The consuming pass is tracked
> separately.

Once that pass ships, two consequences are worth expecting:

- A client already running one of the plan's jobs keeps going for up to one
  cron interval, then gets `404` on its next heartbeat and stops.
- The plan's `status` still reads `active` / `pending_clients` until that pass
  runs. `plans status` reports the pending cancellation meanwhile, and `plans
  list` flags it as `(cancel pending)`, so the delay is visible rather than
  looking like the cancel was dropped. **This part works today** — the marker
  is written and both views surface it.

Cancelling is refused for a plan with no manifest and for a `complete` plan
(every job already terminal, so there is nothing to stop); a plan already
`cancelled` reports a no-op. Re-cancelling before the pass runs is harmless.

A plan may be named by its `plan_id` or, as a convenience, by `--plan-name`.
The id is preferred: a name is a human label and **not an identity**, so
`--plan-name` scans every manifest and fails if the name matches no plan or
more than one, listing the candidates rather than guessing.

## Concurrency

`process-submissions` / `score` and the `fix-*` commands each do
read-modify-write on the warehouse Parquet partitions or submission bodies.
Running two of them at once interleaves one command's read with another's write
and silently drops rows.

To prevent that, every mutating run holds an advisory **storage mutate lock**
at `<storage-root>/locks/mutate.lock` for its whole duration. A second mutating
command finds the lock held and fails fast with a message naming the current
holder, rather than corrupting data. The mutate lock covers
`process-submissions` (`score` alias), `fix-model-param`, `fix-canonical`,
and `requeue-eval`. `--dry-run` invocations are read-only and skip the lock.
`serve` is **not** covered.

`score-eval` uses a separate `locks/score-eval.lock`: it prevents overlapping
eval scorer runs without blocking mutate-locked warehouse writers. `unlock`
clears only the mutate lock.

The mechanism is backend-specific, because each backend has a different correct
primitive:

- **`local_fs`** — an exclusive `flock(2)` on the lock file. The kernel
  releases it when the holding process exits or dies (including a crash or
  `kill -9`), so a stale lock is impossible and there is no takeover step.
- **`s3`** — a lease object. Its body records an `expires_at` set
  `mutate_lock_ttl_secs` (default 1800) into the future. A command that crashes
  without releasing leaves the object behind; the next mutating command run
  past `expires_at` treats the lease as stale and takes it over with a
  compare-and-swap, so two processes racing the same stale lease cannot both
  win. Set `mutate_lock_ttl_secs` comfortably above the longest expected
  `process-submissions` / `score`, `score-eval`, or `fix-*` run. The same TTL
  is used for the shared mutate lock and the separate `score-eval` lock; a run
  that outlives its lease can have the lock taken over mid-write. To clear a
  stale mutate lease, use [`pipette-mgmt unlock`](#pipette-mgmt-unlock).
