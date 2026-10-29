# Operations

## 1. Configuration

All settings live in one TOML file. The doc comments on `Config` in
[`crates/pipette-core/src/config.rs`](../crates/pipette-core/src/config.rs) are
the key reference, [`examples/config.toml`](../examples/config.toml) the
annotated example, and [cli.md](cli.md#configuration-file) says how the file is
found and where credentials come from. A missing `evals_server_url` fails the
load.

`serve` holds no TLS and binds every interface by default. See
[§5.6](#56-network-exposure-and-tls) for the deployment shape that requires.

## 2. Running

```bash
# HTTP server
pipette-mgmt serve

# Fast submission pass
pipette-mgmt process-submissions

# Slow eval scorer
pipette-mgmt score-eval
```

### 2.1. Stopping the server

`serve` shuts down gracefully on `SIGTERM` (what a container runtime sends
before it kills the process) or `SIGINT` (`^C`). It stops accepting new
connections and lets the requests already in progress finish, so a rolling
restart does not cut a client off mid-submission. The log records
`shutdown requested` with the signal, then `server stopped` once the last
request completes.

The wait is capped at 20 seconds. If requests are still running then, the
server logs a
`connections still open at shutdown are abandoned when the process exits`
warning; they keep running while the server spends up to 10 more seconds
writing the audit events in flight, and the process then exits and abandons
them.
Set the runtime's grace period above 30 seconds so it never kills the server
before that.

### 2.2. Connection timeouts

`serve` speaks HTTP/1.1 only, as the server library bounds no HTTP/2
connection's idle time. It closes a connection that does not finish its request
headers within `read_timeout_secs` (default 75 seconds), and fails a request
whose body sends nothing for that long (`408`). The header timeout covers an
idle keep-alive connection too, so set `read_timeout_secs` above the idle
timeout of the load balancer in front (the ALB default is 60 seconds): the
balancer must close an idle connection first, or a request it sends on one the
server is closing fails with `502`. Raise both together when you raise the idle
timeout for long imports
([§5.7](#57-outbound-access-to-huggingface)).

A body that trickles in, or an answer the client never reads, has no time
bound. `serve` instead holds at most `max_connections` (default 512)
connections at once; a further one waits in the listen backlog until one
closes. Keep it below the process's open-file limit. The cap is shared by all
clients, so one host holding idle or slow connections can fill it: the listener
must only be reachable from the load balancer or proxy
([§5.6](#56-network-exposure-and-tls)), which completes request headers before it
opens a connection to the server.

## 3. Cron setup

Scoring runs as **two** crons. `process-submissions` is the fast pass and
`score-eval` the slow one, which eval benchmarks need
([cli.md](cli.md#pipette-mgmt-process-submissions-alias-score)). `score-eval`
holds its own lock, so an overlapping tick exits rather than double-scoring, and
never blocks the fast pass.

```
# /etc/cron.d/pipette-mgmt-score
*/5 * * * * app /usr/local/bin/pipette-mgmt --config /etc/pipette-mgmt/config.toml process-submissions 2>&1 | logger -t pipette-mgmt-process-submissions
# required for eval benchmarks
*/5 * * * * app /usr/local/bin/pipette-mgmt --config /etc/pipette-mgmt/config.toml score-eval          2>&1 | logger -t pipette-mgmt-score-eval
```

On S3 backends the cron environment needs AWS credentials (`AWS_*`, or an
EC2/ECS role).

Run `process-submissions`, `score-eval`, and `queue-maintenance` once per
schedule interval, regardless of server replica count. Use one cron host or one
Kubernetes CronJob. See [queue.md §Processes](queue.md#processes).

### 3.0. Dataset cache

Two recurring tasks keep `GET /datasets/{benchmark_id}` servable
([cli.md](cli.md#pipette-mgmt-dataset-cache-probebuildpins)). Schedule them
however the deployment schedules recurring work: a scheduled container task in
production, `cron` on a single host.

```
pipette-mgmt --config /etc/pipette-mgmt/config.toml dataset-cache probe   # every minute
pipette-mgmt --config /etc/pipette-mgmt/config.toml dataset-cache build   # every 15 minutes
```

- **`probe` must run every minute.** Its verdict is all the route reads, and a
  verdict older than `dataset_probe_max_age_secs` (default 180) refuses with
  `503`. A stopped probe therefore stops serving within three minutes rather
  than serving on a stale `ok`.
- **`build` only acts on a missing artifact**, so its cadence bounds how long a
  new catalog pin waits to become servable. It never rebuilds or deletes.
- The two hold separate locks, so an 80 MB build never delays the minute probe,
  and a second run of either exits rather than overlapping.
- **A crashed build keeps its lock until the lease expires.** On `s3` the
  `build` lease is the run's worst case: every pinned artifact at its deadline,
  plus a minute, where an asr-eval corpus counts only while it is unbuilt. So a
  build killed mid-run (out of memory, an evicted task) blocks the next ones
  for up to that long: hours while asr-eval corpora are unbuilt. A built
  corpus costs nothing. To clear it sooner, first make sure no `build` is still
  running, then delete `<storage-root>/locks/dataset-cache-build.lock` (the
  lease object; its body names the holder, host, pid and `expires_at`).
  `unlock` clears only the mutate lock. On `local_fs` the lock is a `flock(2)`
  that dies with its process, so there is nothing to clear.
- **A build checks its corpus against its entry.** An asr-eval corpus must
  hold its entry's `parameter_sample_count` samples, or the build fails and
  stores nothing, rather than every device downloading a corpus it then
  refuses. A corpus already built is held to the same count, so an entry
  restated with another count fails the next build too (an artifact recorded
  before it carried `samples` is checked against its stored manifest). A build past its deadline stops streaming and writing before the
  next one starts, and a `/samples` body that sends nothing for five minutes
  fails its build.
- **asr_eval corpora are large.** The largest is many gigabytes per
  prompt profile and the full catalog's corpora run to well over a hundred
  gigabytes, so size the `[storage]` volume or bucket for them, give the build
  host's `TMPDIR` room for twice the largest corpus, and expect the first build to take hours: each corpus
  streams its whole set from the evals server, one at a time, under
  `dataset_asr_build_timeout_secs` (default 7200). After an evals server
  restart, a set's first probe may read `unavailable` until the server has
  learnt its digests again.
- Both shell out to `git`: the host or image needs it on `PATH`, and outbound
  HTTPS to each upstream.
- Both call `evals_server_url` for a `vlm_eval` corpus: `probe` for
  `/samples/digest` alone, `build` for the whole `/samples` body (POPE's is
  1.1 GB), streamed to disk.

### 3.0.1. Descriptor catalog import

Apply the SQL migration before you run the importer. Set `GITHUB_TOKEN` in
the cron environment. The token needs read access to public GitHub releases.
Run one import each day at 03:00 UTC. Use `flock` to stop a second run from
starting before the first run ends:

```
# /etc/cron.d/pipette-mgmt-catalog
CRON_TZ=UTC
0 3 * * * app flock -n /run/lock/pipette-mgmt-catalog.lock /usr/local/bin/pipette-mgmt --config /etc/pipette-mgmt/config.toml catalog import 2>&1 | logger -t pipette-mgmt-catalog
```

A daily entry takes the newest complete build that is older than
`descriptor_catalog.llamacpp_min_build_age_secs`. The importer skips a younger
build. The entry never changes after the importer creates it. So a run that starts
less than the minimum age after 00:00 UTC can name the day after an older
build. Keep the cron at least the minimum age after 00:00 UTC. The default age
is 10800 seconds, which fits the 03:00 UTC cron. If you increase the age, move
the cron later by the same amount. Do not run a manual import before that time.

Each run applies automatic expiry to the imported entries
([cli.md](cli.md#pipette-mgmt-catalog)). An imported entry with an old release
starts hidden. To import older daily entries, run
`catalog import --since YYYY-MM-DD` with the first day to check. The importer also checks all stable releases.

**Alert for a stale catalog importer.** The importer writes one line to the
`pipette-mgmt-catalog` log at the end of each run:

- `INFO` `catalog import run finished`: the run succeeded. The line has the
  fields `run_id`, `command`, `entries_created`, `members_added`, and
  `entries_hidden`.
- `ERROR` `catalog import run failed`: the run recorded an error. The line has
  the same fields, plus `error` and `unclassified_assets`.

Alert when no `catalog import run finished` line with `command=import` appears
in 36 hours. Set the alarm to fire on missing data, because a stopped schedule
writes no line. Enable the alarm after you enable the daily import schedule.

A `catalog import run failed` line with `unclassified_assets` above zero and
`error` equal to `<n> unclassified assets` is an unclassified-assets report.
The importer finished its catalog work but found assets that it could not
classify. Any other `catalog import run failed` line is a failed import.
A run that stops before it records its end writes no line. Its row stays
`running`, and the query below reports it as `interrupted` after 6 hours.

When the alert fires, run this query against PostgreSQL. The
`latest_import_outcome` column tells an unclassified-assets report apart from a
failed import. It uses the same rule as the `outcome` field of the
[run history](api/descriptor-catalog/list_catalog_import_runs.md). Keep the
`' unclassified assets'` text equal to `unclassified_summary` in
`pipette-plan-types`:

```sql
SELECT success.last_successful_import_at,
       success.last_successful_import_at IS NULL
         OR success.last_successful_import_at < now() - interval '36 hours' AS stale,
       CASE
         WHEN latest.id IS NULL THEN 'never_run'
         WHEN latest.result = 'running'
           AND latest.started_at < now() - interval '6 hours' THEN 'interrupted'
         WHEN latest.result = 'running' THEN 'running'
         WHEN latest.result = 'success' THEN 'success'
         WHEN jsonb_array_length(latest.unclassified_assets) > 0
           AND latest.error_summary = jsonb_array_length(latest.unclassified_assets)::text || ' unclassified assets'
           THEN 'unclassified'
         ELSE 'failed'
       END AS latest_import_outcome,
       latest.error_summary
FROM (
  SELECT MAX(finished_at) AS last_successful_import_at
  FROM catalog_import_runs
  WHERE command = 'import' AND result = 'success'
) AS success
LEFT JOIN LATERAL (
  SELECT id, started_at, result, unclassified_assets, error_summary
  FROM catalog_import_runs
  WHERE command = 'import'
  ORDER BY started_at DESC, id DESC
  LIMIT 1
) AS latest ON true;
```

The alert detects a stopped daily import schedule, even if the catalog still
lists active entries. A successful `refresh` does not reset it. An import that
finishes with unclassified assets logs `catalog import run failed`, so it does
not reset the 36-hour limit. Handle it separately from a failed import:

1. Inspect the unclassified assets in the
   [run history](api/descriptor-catalog/list_catalog_import_runs.md).
2. Update the flavor table.
3. Run `catalog refresh` for the affected entries.

For an upstream or storage error, inspect the run's `error_summary` and the
`pipette-mgmt-catalog` logs.

### 3.1. Job queue maintenance

`queue-maintenance` reconciles everything stale in the `todo/` queue; what each
run does is [cli.md](cli.md#pipette-mgmt-queue-maintenance), and the protocol it
maintains [queue.md](queue.md).

Run every 1 to 5 minutes. The interval is a latency knob, not a correctness
requirement: every pass is idempotent, and the handlers enforce lease and expiry
rules themselves. But a crashed device's job becomes claimable again only at
lease expiry **plus** one interval, so if you shorten `plan_lease_duration_secs`
to speed up dead-device turnaround, tighten the cron to match. The eligible
index reflects new jobs, profile changes, approvals and permission changes
within one interval, and a new job is claimable only after the run that indexes
it. Every minute is the right default when claim latency matters:

```
# /etc/cron.d/pipette-mgmt-queue
* * * * * app flock -n /run/lock/pipette-mgmt-queue.lock /usr/local/bin/pipette-mgmt --config /etc/pipette-mgmt/config.toml queue-maintenance 2>&1 | logger -t pipette-mgmt-queue
```

`queue-maintenance` takes no lock of its own, so **serializing runs is a
correctness requirement, not just an optimization**: the `flock -n` above
makes a tick exit immediately while the previous run is still going, rather
than queue behind it. A skipped tick loses no work: each run reconciles
everything outstanding at that moment.

Overlapping runs *can* be destructive. The reconciliation sweep removes a
marker only after its storage key is seen orphaned in two consecutive runs, and
first sightings are persisted (`todo/.gc-candidates`) for the next run to read.
Each run consumes that file as it starts, so a run that fails partway leaves no
stale sightings behind; it merely delays GC by one interval. That rule is safe
only because the gap between sightings is a full cron interval: long enough
for an entity caught mid-transition (a job racing `leased/ → avail/`, a client
racing re-registration) to become visible. Two overlapping runs can both miss
the same in-transition entity and count as two "consecutive" sightings seconds
apart, deleting a live job's `eligible/`/`denied/` markers, which are never
rebuilt, and leaving it permanently unclaimable. Overlap also lets an older run
overwrite the `todo/.eligible-indexed` set a newer one wrote, causing wasteful
re-indexing.
If you deploy as a **Kubernetes CronJob**, set `concurrencyPolicy: Forbid` to
get the same serialization the `flock -n` provides. An EventBridge schedule
that starts an **ECS task** starts a new task at each scheduled time. Put a
launcher between the schedule and the task. The launcher checks the task
family for a running task. It starts a task only if no task runs.

**Lost leases.** A job whose lease lapses `plan_lost_lease_limit` times (default
3) is retired as a `lease_lost` failure, with a `WARN`
`queue-maintenance: retired a job whose lease was lost {n} times`. Alert on it:
the job is likely crashing or disconnecting the devices that run it. To give a
retired job another chance, `campaigns rerun --job` it; to lift a count, delete
`lost/{job_id}/`. A `serve` outage longer than the lease adds one loss to every
leased job, so before long planned `serve` downtime pause this cron or set the
limit to `0`, which turns the count off.

**Stranded ingests.** A manifest still `creating` past
`plan_creating_max_age_secs` (default 6 hours) marks a crashed ingest, and its
jobs are torn down. Set it above the longest expected ingest
([plan-ingestion.md §8](plan-ingestion.md#8-ingestion-flow-atomic-ish)).

**Clock synchronization.** The expiry pass judges deadlines by the maintenance
host's clock, while `serve`'s `claim`/`reclaim` gates use their own. A
maintenance clock *ahead* of a serve host's opens a window where `serve` still
hands a job out while `queue-maintenance` expires it, writing a synthetic
failure for a job a client is running. It also shortens the stranded-ingest
deadline, which compares the filing host's `created_at`. Keep all hosts
NTP-synced; a lagging maintenance clock is harmless, so single-host or
shared-clock deployments are inherently safe.

On S3, an S3 lifecycle rule expiring `todo/tmp/` objects after 1 day can replace
the `tmp/` age check.

**Log lines.** Alarms and dashboards can match these `INFO` lines, each
carrying its figures as fields:

- `queue-maintenance: run finished`, once per completed run: the run's and each
  phase's duration, the stored data it read (plans, probes, content keys, client
  records), the CPU time, the slowest plan, and `failed_items` for a run that
  skipped items and then exits non-zero. A run that stops on an error logs no
  summary.
- `queue-maintenance: S3 and CPU work of a phase`, before the summary, once per
  phase (`queue_passes`, `reconcile_plans`, `plan_index_gc`): HTTP attempts of
  both S3 clients by outcome (`throttled` counts S3's `SlowDown`), the peak in
  flight, the time waited, and the bodies decoded.
- `queue-maintenance: refreshed a plan's progress snapshot`, per plan whose jobs
  the pass probed or whose status changed, and
  `queue-maintenance: plan status changed` with `from` and `to`. A plan that
  fails gets the `WARN` `queue-maintenance: failed to reconcile a plan`.

Each attempt that gets no response logs a `WARN`,
`S3 request attempt got no response`, naming the client, method, path, bucket
host, the time waited and the error. The `object_store` client waits 5 s to
connect and 30 s for the request, and retries a request it can safely resend.
To fail a stalled request sooner, set `queue_maintenance_read_timeout_secs`.
The `aws-sdk-s3` client of the `todo/` queue waits 3.1 s to connect,
`todo_attempt_timeout_secs` (default 5 s) for each attempt, and
`todo_operation_timeout_secs` (default 20 s) for the whole request with its
retries.

### 3.2. Eligible index maintenance

The `todo/eligible/` index is maintained by `queue-maintenance` alone; `serve`
replicas only read it ([queue.md](queue.md#the-eligible-index)). No operator
action is needed after `clients approve`, a permission change or a profile
update: the next run reflects it. Each run also writes each worker's backlog to
`todo/.backlog`, and the backlog by campaign to `todo/.backlog-campaigns`. The
console shows both ([queue.md](queue.md#device-backlog)). A failed write logs a
warning. A run with nothing new touches only empty prefixes and a few small
objects.

### 3.3. Forcing an eligible index rebuild

`flag-reindex` forces a client's rebuild when the automatic path did not
converge. When to use it, and the work gate it imposes, are
[cli.md](cli.md#pipette-mgmt-flag-reindex).

### 3.4. Audit log retention

The audit log keeps two years of events, and only `audit prune` deletes them
([cli.md](cli.md#pipette-mgmt-audit-listprune)). Run `audit prune` daily from
one host:

```
# /etc/cron.d/pipette-mgmt-audit
30 3 * * * app /usr/local/bin/pipette-mgmt --config /etc/pipette-mgmt/config.toml audit prune 2>&1 | logger -t pipette-mgmt-audit-prune
```

If a run does not happen, the audit log keeps the old events until the next
run deletes them.

## 4. Reprocessing

**Whole warehouse.** To re-score everything: move files from
`submissions/processed/` back to `submissions/incoming/`, clear all
scorer-owned output directories (`warehouse/results/` and
`warehouse/eval_sample_results/`), then run cron. On S3, use `aws s3 mv`
to move objects between the `processed/` and `incoming/` prefixes, and
delete the relevant warehouse and eval sample result objects.

**One eval benchmark (e.g. after a scorer fix).** Use
[`pipette-mgmt requeue-eval`](cli.md#pipette-mgmt-requeue-eval), then run the
scoring passes. It copies, so scope a repeat run with `--submitted-before`.

**A campaign's evals.** To re-score the evals of the plan a campaign runs,
file a recalculation
([`POST /api/v1/campaigns/{id}/recalculate`](api/campaigns/recalculate_jobs.md),
or the app's **Recalculate** buttons). It copies each named job's record
the same way, under a new job of a revision the campaign moves onto, so the
fresh scores read on the campaign's pages. Copies from either path name the job
they copy in `rescored_from`. They keep the time of the device run in
`run_submitted_at`. The next `process-submissions` and
`score-eval` score the copies.

## 5. Docker deployment

The `liquidai/pipette-mgmt` image (multi-arch: `linux/amd64`, `linux/arm64`) ships
the `pipette-mgmt` binary. The default `CMD` is `serve`. Override with
`process-submissions`, `score-eval`, or `clients <action>` to run other
subcommands. The image runs as
non-root uid `10001` and reads its config from `/etc/pipette-mgmt/config.toml`
(override with `PIPETTE_MGMT_CONFIG`).

The only env vars consumed at runtime are `PIPETTE_MGMT_CONFIG`, `RUST_LOG`,
`PIPETTE_MGMT_LOG_FORMAT`, the `AWS_*` family (for an S3-compatible backend),
libpq's `PG*` family (for `[database]`), and `GITHUB_TOKEN` (for `catalog
import` and `catalog refresh`; see [§3.0.1](#301-descriptor-catalog-import)).
`HOSTNAME`, `SUDO_USER`, `USER` and `LOGNAME` only label audit records and
storage locks. Everything else comes from the TOML config. Set
`PIPETTE_MGMT_LOG_FORMAT=json` where a collector reads the container's log; see
[cli.md § Logging](cli.md#logging).

A deployment keeps its records (plans, campaigns, identities, vault records) on
PostgreSQL through `[database]`
([design/sql-record-store.md](design/sql-record-store.md)); run `db migrate`
before a binary that needs a newer schema
([cli.md](cli.md#pipette-mgmt-db-migrate---to-version---dry-run-and-db-check)).
The examples below show the object-store sections, which keep the queue,
submissions, the warehouse, blobs and the dataset cache.

### 5.1. Local filesystem (mounted volume)

`config.toml`:

```toml
evals_server_url = "http://evals:8080"
listen_addr = "0.0.0.0:3000"
authorities = ["mgmt.example"]  # the hosts clients reach serve by (§5.6)

[storage]
backend = "local_fs"
data_dir = "/data"

[auth_storage]
backend = "local_fs"
data_dir = "/data"
```

```bash
docker run -d --name pipette-mgmt \
  -p 3000:3000 \
  -v "$(pwd)/config.toml:/etc/pipette-mgmt/config.toml:ro" \
  -v "$(pwd)/data:/data" \
  liquidai/pipette-mgmt:<version>
```

The `/data` mount must be writable by uid `10001` (the image's `/data` is
already mode `1777`, so a fresh bind mount works as long as the host directory
itself permits writes from that uid, `chmod 1777 ./data` or
`chown 10001:10001 ./data` on the host).

### 5.2. AWS S3

`config.toml`:

```toml
evals_server_url = "http://evals:8080"
listen_addr = "0.0.0.0:3000"
authorities = ["mgmt.example"]  # the hosts clients reach serve by (§5.6)

[storage]
backend = "s3"
bucket = "my-pipette-data"
prefix = "v1/"
region = "us-east-1"

[auth_storage]
backend = "s3"
bucket = "my-pipette-auth"
region = "us-east-1"

# The todo/ queue needs atomic RenameObject, so a separate S3 Express One Zone
# bucket (docs/storage.md §8).
[todo_storage]
backend = "s3"
bucket = "my-pipette-todo--use1-az4--x-s3"  # Express One Zone bucket naming convention
region = "us-east-1"
```

```bash
docker run -d --name pipette-mgmt \
  -p 3000:3000 \
  -v "$(pwd)/config.toml:/etc/pipette-mgmt/config.toml:ro" \
  -e AWS_ACCESS_KEY_ID \
  -e AWS_SECRET_ACCESS_KEY \
  -e AWS_REGION=us-east-1 \
  liquidai/pipette-mgmt:<version>
```

On EC2/ECS/EKS, omit the access-key env vars and let the standard AWS
credential chain pick up the instance role / task role / IRSA token:
`object_store` resolves the same env vars and metadata endpoints as the AWS
SDK (`AWS_CONTAINER_CREDENTIALS_RELATIVE_URI`, `AWS_WEB_IDENTITY_TOKEN_FILE`,
EC2 IMDSv2). Use **separate buckets** for `[storage]`, `[auth_storage]`, and
`[todo_storage]` so benchmark/warehouse data, private client identities, and
the job queue can have different IAM policies. Without `[database]`, write
access to `[storage]` is an authority too: it holds the memberships, device
ownerships and campaign and vault owners that decide access
([storage.md §3.2](storage.md#32-organizations)), so grant bucket-wide writes on
it only to roles you would let write `[auth_storage]`. With `[database]`, those
live in PostgreSQL, and the database credentials are that authority. The `[todo_storage]`
bucket must be Express One Zone (see
[storage.md §8](storage.md#todo-requires-s3-express-one-zone)).

Three commands rename against `todo/`: `serve` (claim, heartbeat),
`queue-maintenance` (lease recycle), and the `campaigns` commands that file a
plan (promotion into `avail/`). Each validates the bucket at startup and **refuses to start** if the
rename does not work. The probe stages a temporary object under `todo/tmp/` and
proves that a rename moves it.
[storage.md §8](storage.md#todo-requires-s3-express-one-zone) lists the steps
and the failure verdicts. This catches a misconfigured regular-S3 bucket before
it can silently corrupt claims (a non-atomic copy would let two clients win the
same job). Probe keys carry the `.rename-probe-` prefix; the stale-`todo/tmp/`
sweep collects one that survives a failed delete. The `clients` admin commands
only list and delete markers, never rename, so they skip the probe.

`[storage]` and `[auth_storage]` are probed the same way for conditional `PUT`,
which the storage mutate lock is built on, and without `[database]` the
exactly-once pre-auth key spend too. A backend that answers `200` to a write it should have refused **refuses to
start** the process. The locked batch commands (`process-submissions`,
`score-eval`, `fix-*`, `requeue-eval`) probe `[storage]` for both a conflicting
`If-None-Match: *` create and an `If-Match` compare-and-swap against a superseded
version, since the lock needs the first to acquire and the second to take over a
crashed holder's lease. `serve` probes `[auth_storage]` for the create alone.
AWS S3 honors both; this is aimed at S3-compatible endpoints, which vary. Two
operational consequences: leave `AWS_CONDITIONAL_PUT` unset (setting it to
`disabled` fails the probe), and give the credentials `s3:DeleteObject` so each
probe can clean up the key it wrote under `probe/`.

On AWS (or any other backend confirmed to honor conditional `PUT`), the probe can
be switched off with `probe_backend_capabilities = false`, which skips its writes
and leaves nothing under `probe/`. Keep it on for an S3-compatible endpoint: it is
the only thing standing between a non-conformant one and a silently dropped row.
With it on against a **versioned** bucket, add a lifecycle rule on
`<prefix>/probe/` expiring noncurrent versions and expired delete markers.
`DeleteObject` there only writes a marker, so the probe's own objects accumulate,
a few per run of every locked command, unread by anything. See
[storage.md §8](storage.md#storage-and-auth_storage-require-conditional-put).

#### The vault prefix

`vault-resources/` under `[storage]` holds the private artifacts mgmt serves
([storage.md §3.4](storage.md#34-the-vault)). mgmt is the only writer: a client
uploads each file over `PUT /api/v1/vault/blobs/{principal_id}/{sha256}`, and mgmt
keeps it at `vault-resources/{principal_id}/blobs/{sha256}` only if its bytes
hash to that digest. Nothing else needs credentials for the prefix. These points
apply to an operator:

- **Bytes pass through the service both ways.** An upload streams through a
  request handler into a multipart upload of 32 MiB parts, as a download
  streams back out. Each upload buffers up to ~190 MiB of parts, so mgmt runs
  at most four at once (`VAULT_UPLOADS_MAX`) and answers `429` past that.
- **The bucket needs an incomplete-multipart-upload lifecycle rule** on
  `vault-resources/` (`AbortIncompleteMultipartUpload`, a day is plenty). A
  refused upload is aborted, but one cut off by a deploy or a crash is not, and
  its parts are billed and invisible to a listing until something aborts them.
  An import that a client disconnect stops also leaves its upload open, so the
  rule covers imports too.
- **The bucket needs a lifecycle rule on `vault-staging/`** with the same
  one-day `AbortIncompleteMultipartUpload`, and also an `Expiration` of one
  day. An import of a public model streams each archive to
  `vault-staging/{principal_id}/{uuid}`. Then mgmt copies the archive to
  `vault-resources/` and deletes the staging object. A deploy or a crash
  between these steps leaves the staging object, and the rule deletes it.
  Give the credentials `s3:DeleteObject` on `vault-staging/`. mgmt copies an
  archive over 5 GiB with a multipart `UploadPartCopy`. An S3-compatible
  backend must support a ranged `UploadPartCopy`.
- **Set `endpoint` and `region` in `[storage]`.** mgmt copies a staged archive
  over 5 GiB through an `aws-sdk-s3` client, and sends every other vault request
  through `object_store`. The two clients read the environment in different ways. For
  example, `object_store` reads `AWS_ENDPOINT` and the SDK does not. The
  `[storage]` keys set both clients to the same bucket.
- **mgmt deletes a file only when no record names it**: a resource delete
  removes the files no remaining record of its owner names, and
  `pipette-mgmt vault prune` removes older orphans. It learns a file went
  missing only when asked: `pipette-mgmt vault show` reports a missing or
  resized file, ingestion refuses a plan naming a record whose file is gone,
  and a device fetching one gets a `404` with a `vault_object_missing` event in
  the log. Uploading a missing file again restores every record naming it. A
  resized one must be removed from the bucket first, since an upload keeps
  whatever already sits at its digest.

### 5.3. Cloudflare R2

R2 is S3-compatible. Set `region = "auto"` and point `endpoint` at the
account-scoped R2 URL. Generate an R2 API token (Account → R2 → Manage R2 API
Tokens) and pass it as the AWS keys.

`config.toml`:

```toml
evals_server_url = "http://evals:8080"
listen_addr = "0.0.0.0:3000"
authorities = ["mgmt.example"]  # the hosts clients reach serve by (§5.6)

[storage]
backend = "s3"
bucket = "pipette-data"
prefix = "v1/"
region = "auto"
endpoint = "https://<account_id>.r2.cloudflarestorage.com"

[auth_storage]
backend = "s3"
bucket = "pipette-auth"
region = "auto"
endpoint = "https://<account_id>.r2.cloudflarestorage.com"
```

```bash
docker run -d --name pipette-mgmt \
  -p 3000:3000 \
  -v "$(pwd)/config.toml:/etc/pipette-mgmt/config.toml:ro" \
  -e AWS_ACCESS_KEY_ID="<r2_access_key_id>" \
  -e AWS_SECRET_ACCESS_KEY="<r2_secret_access_key>" \
  liquidai/pipette-mgmt:<version>
```

R2 ignores `region` content but the AWS sigv4 signer requires *some* value;
`"auto"` is the conventional choice.

### 5.4. MinIO (and other self-hosted S3)

Point `endpoint` at the MinIO URL. HTTP endpoints are accepted because the
S3 client enables `allow_http` whenever `endpoint` is set in the TOML.

`config.toml`:

```toml
evals_server_url = "http://evals:8080"
listen_addr = "0.0.0.0:3000"
authorities = ["mgmt.example"]  # the hosts clients reach serve by (§5.6)

[storage]
backend = "s3"
bucket = "pipette-data"
prefix = "v1/"
region = "us-east-1"
endpoint = "http://minio:9000"

[auth_storage]
backend = "s3"
bucket = "pipette-auth"
region = "us-east-1"
endpoint = "http://minio:9000"
```

```bash
docker run -d --name pipette-mgmt \
  --network minio_net \
  -p 3000:3000 \
  -v "$(pwd)/config.toml:/etc/pipette-mgmt/config.toml:ro" \
  -e AWS_ACCESS_KEY_ID="<minio_access_key>" \
  -e AWS_SECRET_ACCESS_KEY="<minio_secret_key>" \
  -e AWS_REGION=us-east-1 \
  liquidai/pipette-mgmt:<version>
```

Both buckets must exist before startup (`mc mb local/pipette-data`,
`mc mb local/pipette-auth`). MinIO doesn't enforce a real region but the
sigv4 signer needs one. Match whatever `MINIO_REGION` is configured with
(defaults to `us-east-1`).

### 5.5. Running other subcommands

Override the default `CMD` to run a cron pass or an admin command against the
same image and config. In a containerized deployment the §3 crons run this way,
as scheduled jobs:

```bash
docker run --rm \
  -v "$(pwd)/config.toml:/etc/pipette-mgmt/config.toml:ro" \
  -e AWS_ACCESS_KEY_ID -e AWS_SECRET_ACCESS_KEY -e AWS_REGION \
  liquidai/pipette-mgmt:<version> process-submissions
```

### 5.6. Network exposure and TLS

The server speaks plaintext HTTP; TLS terminates in the proxy placed in front of
it. `listen_addr` defaults to `0.0.0.0:3000`, so a container started from the
examples above is reachable on every interface it has.

**The required shape is a TLS-terminating proxy in front of the listen port,
with a network control that admits only that proxy.** Both halves are load
bearing. TLS on the public hop protects clients; the network control is what
keeps the plaintext hop between the proxy and this process from being reachable
directly, which would otherwise offer the same traffic unencrypted on a
different port.

What rides on it:

- `POST /clients/register` with `generate_key: true` returns a freshly generated
  **private key** in the response body: the one exchange in the API that
  carries a long-lived credential. (The supported clients generate their own
  keypair and send `public_key`, so this is a convenience path rather than the
  normal one, but it is reachable and unauthenticated.)
- Registration accepts a pre-auth key as a bearer secret, and every
  authenticated request carries its signature headers in the clear.

The server cannot see what sits in front of it, so it cannot check any of this.
What it does report is the moment a credential actually leaves it:

```
WARN client_id=ev1_a3f8... returned a server-generated private key in the registration response
```

Since the supported clients generate their own keypair, this names a caller
taking the one path where a long-lived secret transits.

How much weight a single line carries depends on who can reach the endpoint.
Under `require_client_preauth_key`, registration takes a minted secret, so each
occurrence is worth reading on its own. Under the default open policy, any
anonymous caller can produce these at request rate. Read a burst as probing
rather than as many separate exposures, and note that each one also leaves a
durable pending client record behind it. `POST /api/v1/users/register` leaves the same
kind of residue on the other keyspace when it is asked for an account: a pending
user record and the claim on the address it asked with, one per distinct
address. `require_user_preauth_key = true` closes that path the way
`require_client_preauth_key` closes this one.

Each process admits at most 20 registrations without a pre-auth key per
minute, devices and accounts together, and answers the rest `429`, logging
`refused: too many registrations without a pre-auth key`. A registration with a
pre-auth key, and a device retrying a registration already on record, are not
counted; an account request retried with the same keypair is. The count is per
process, so a deployment of several instances admits that many per instance.
Of those 20, one client address gets 1, an IPv6 one counted by its `/64`, since
a device or an account registers once; clients behind one NAT share it. The
address is the `X-Forwarded-For` entry `trusted_proxy_hops` from the right
(default `1`, the entry one load balancer appends); `0` ignores the header and
takes the peer. A header with fewer entries names no address, and only the
ceiling of 20 holds. This reads right only when exactly that many proxies
stand in front and each appends the address it received from: the reference
load balancer does, as its deployment template configures it, but nothing
here checks that. Count every appending proxy, or the limit falls on one of
them and refuses every keyless registration behind it. A proxy that does not
append lets a caller name its own address. The `429` carries `Retry-After`,
one second past the end of the minute. Enroll more than one device a minute
from one site with a multi-use pre-auth key.

The ceiling slows the residue rather than stopping it: a caller holding the
window full still leaves up to 28,800 pending records a day on each instance.
A flood from 20 addresses or more still fills the shared budget, and then
refuses every keyless registration, genuine ones included, for as long as it
lasts. An
internet-facing deployment should set `require_client_preauth_key = true` and
`require_user_preauth_key = true`, which close both paths.

A reference deployment on AWS runs an Application Load Balancer holding the
certificate, listening on 443 with 80 redirecting to it, forwarding to the
container's port over plaintext HTTP inside the VPC. The tasks sit in a public
subnet with public IPs, so the security group admitting the load balancer's
security group on that port is the **only** thing keeping the plaintext port off
the internet: widening that rule exposes it directly, with no second control
behind it.

The proxy must forward the client's `Host` unchanged, and `authorities` must
name each host clients reach this deployment by (`mgmt.example`, or
`mgmt.example:8443` off the default port). An RFC 9421 or device `v3` signature
over any other host is refused
([authentication.md §2.4](authentication.md#24-rfc-9421-signatures-user-accounts)),
and with the list empty `serve` warns at startup and refuses every one, the
console's key sign-in included.

### 5.7. Outbound access to HuggingFace

Ingestion imports a gated HuggingFace model into the vault when a model in a
plan document carries an `auth_token` ([plan-ingestion.md §6.2](plan-ingestion.md)). For
this import, mgmt makes outbound HTTPS requests to these hosts:

- `huggingface.co`, or the host of `hf_import_endpoint`, for the Hub API and
  each download request.
- `cdn-lfs.huggingface.co`, where a download of a file in Large File
  Storage (LFS) redirects.
- `cas-bridge.xethub.hf.co`, where a download from a repository on Xet storage
  redirects.

Allow all three in the egress rules of the task or the host. If mgmt cannot
reach them, an ingest that imports answers `502` and stages nothing. A plan
with no token makes no outbound request.

The import runs inside the request. A client disconnect stops the import and
files no plan. Thus:

- **A client that sends `Accept: application/x-ndjson` keeps the connection
  open.** mgmt then sends a line at intervals of 15 seconds or less
  ([Campaigns: Response `201 Created`](api/campaigns.md#response-201-created)). This interval
  is shorter than the ALB default idle timeout of 60 seconds.
- **For a client that does not send the header, set the idle timeout of the
  load balancer** above the longest import that you expect, and
  `read_timeout_secs` above that ([§2.2](#22-connection-timeouts)). Without the
  header, mgmt sends no bytes until the import completes. A retry downloads
  each directory model again, and each gguf file that the import did not store
  completely.
- **Keep the `AbortIncompleteMultipartUpload` rule on `vault-resources/`**
  ([§5.2, "The vault prefix"](#the-vault-prefix)). A stopped import leaves an
  open upload.

Each ingest that downloads a model holds one permit. Set the number with
`hf_import_max_concurrent` (default 2). Each import streams one file at a time,
and an S3 upload buffers about 190 MiB of parts. The import stores each file
under `vault_blob_limit_bytes`.

Each request to HuggingFace holds a request permit. Set the number with
`hf_max_concurrent_requests` (default 8). The number must be greater than
`hf_import_max_concurrent`. A request waits for a permit when every permit is
held. The process then logs `hf request waited for a permit`. The process logs
`hf request rate-limited by hub` when HuggingFace answers `429`. The ingest
then answers `429`.

An import logs `hf import started`, `hf import stored record` and
`hf import reused record`, with the owner and the HuggingFace reference. It
never logs the token. A request that asks for the stream logs
`ingest answers as an ndjson stream`. If the caller disconnects before the last
line, the request logs `ingest stream closed by the caller; the ingest
stopped`.

## 6. Client administration

Devices and accounts are administered with the `clients` and `users` commands
([cli.md](cli.md#commands)), or over HTTP by an account that holds the grant
([Device administration](api/devices.md),
[Users: Account administration](api/users.md#account-administration)).

### 6.1. Auto-approve at registration

`[auto_approve]` approves a client at registration when its `contact_email`
matches a rule, saving the manual `clients approve`. **It is not a security
control**: the address is self-reported, so anyone can register under it. See
[authentication.md §3.1](authentication.md#31-auto-approve-rules).

### 6.2. User administration

A machine can ask for an account at `POST /api/v1/users/register` with no key
([`POST /api/v1/users/register`](api/users/register_user_key.md)). The account waits
`pending` in `users list`, since nothing here sends mail, until `users approve`
admits it or `users delete` frees its address and keypair.

There is deliberately **no auto-approve for users**: an address is self-reported
here as much as on a client, and an operator account is the half of the
vocabulary that authors work. Set `require_user_preauth_key = true` to refuse
the keyless request outright, so every account is minted by an operator. The
one account created without an operator step is a Clerk sign-in's, where Clerk
has verified the address ([§6.3](#63-console-sign-in-through-clerk)).

### 6.3. Console sign-in through Clerk

With a `[clerk]` section, the console app's sign-in page (`/app/`) offers
**Clerk** beside the key sign-in, so people sign in with whatever the Clerk
instance offers, such as a Google account. Devices, scripts and every other
caller of the JSON API are unaffected: they still sign every request
([Users: Calling the API yourself](api/users.md#calling-the-api-yourself)).

On the Clerk side:

1. Create a JWT template with these claims. Its name and its `aud` are both
   the `jwt_template` set below, `pipette-mgmt` by default. The server refuses
   a token from another template, and one without a verified address:

   ```json
   {
     "aud": "pipette-mgmt",
     "email": "{{user.primary_email_address}}",
     "email_verified": "{{user.email_verified}}"
   }
   ```

2. Serve the console from an origin the instance serves: its domain, a
   subdomain of it, or a satellite domain. A reader with no Clerk session is sent
   to the instance's sign-in page, which has to be allowed to send them back to
   the page they left under `/app/`.

On this side:

```toml
[clerk]
publishable_key = "pk_live_…"
public_key = """
-----BEGIN PUBLIC KEY-----
…
-----END PUBLIC KEY-----
"""
authorized_parties = ["https://console.example.test"]
jwt_template = "pipette-mgmt"
allowed_domains = ["example.test"]

[[clerk.provision]]
domain = "example.test"
org = "acme"
```

- `publishable_key` and `public_key` (the **PEM public key**) are both on the
  Clerk dashboard's API keys page. Tokens are verified against the key offline,
  so the server holds no Clerk secret; update it if Clerk rotates the key.
- `authorized_parties` lists the origins the console is served from, as a
  browser sends them in `Origin`. The sign-in page offers Clerk only at these
  hosts; at any other name the server answers to, it offers keys alone.
- `jwt_template` names the template from step 1, `pipette-mgmt` when unset.
  The sign-in page asks Clerk for it, and a token's `aud` must be that name, so
  a token another app on a shared instance asks for signs nobody in here. A
  name Clerk has no template for fails every Clerk sign-in, so create the
  template first.
- `allowed_domains`, when set, limits Clerk sign-in to addresses at those
  domains, matched exactly: `example.test` admits neither `sub.example.test` nor
  `evilexample.test`. Any other address is refused `403` before its account is
  looked up, so an account an operator made for it still signs in only with its
  key. Set it whenever the Clerk instance is shared with apps that admit outside
  users, since Clerk then verifies addresses this deployment should not trust. A
  provision rule must name an allowed domain, or the server does not start.
  Without the key, any verified address may sign in.
- A `[[clerk.provision]]` rule gives an address at `domain` that no account
  holds an active account on its first sign-in: a member of `org`, holding the
  `org_member` group, as `users create --org-member` and `users set-org` would. The
  organization must exist and not be disabled. Without a rule, an address signs
  in only to an account an operator created for it. A keyless account request
  at the domain is refused, so nobody can claim an address ahead of its owner.
  A pending request from before the rule is such a claim: `users delete` it
  rather than approving it, and its owner's next sign-in creates the account.

A rule trusts Clerk with the domain: whoever Clerk verifies an address at it for
gets an account. **Offboard with `users disable`, not `delete`.** A disabled
account's sign-ins and keys are all refused, while a deleted one at a
provisioned domain is created afresh on the next sign-in.

When a Clerk sign-in fails, the log shows why. A `checking a Clerk token's
claims` line at `info` carries what the template rendered (`aud`, `azp`, `iss`,
`email_verified`, the address's domain and the rest), and a `refused a Clerk
sign-in` line at `warn` names the check that failed. Only the refusal appears
for a token whose signature does not verify, which usually means `public_key`
is not this instance's.
