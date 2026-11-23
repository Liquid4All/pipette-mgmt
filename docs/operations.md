# Operations

## 1. Configuration

All settings live in one TOML file. The doc comments on `Config` in
[`crates/pipette-core/src/config.rs`](../crates/pipette-core/src/config.rs) are
the key reference, [`examples/config.toml`](../examples/config.toml) the
annotated example, and [cli.md](cli.md#configuration-file) says how the file is
found and where credentials come from. A missing `evals_server_url` fails the
load.

Every environment needs a `[database]` section, because the records and the job
queue are in PostgreSQL. A config without `[database]` fails to load. A bare
`[database]` section is valid: the libpq `PG*` environment variables supply the
connection.

`[todo_storage]`, `todo_attempt_timeout_secs` and `todo_operation_timeout_secs`
are retired keys. A config that has one of them fails to load. Delete each one
from every deployed config.

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
- **An eval corpus is the scores service's package.** The scores service packs
  every `vlm_eval` and `asr_eval` corpus into `scores-packages/tars/` of
  `[eval_packages_storage]` (default `[storage]`), and `GET /datasets` streams
  it from there. `build` registers such a pin without downloading it: the
  package must have the pinned size and a manifest naming the pin, and the
  evals server must still serve the pinned samples. A pin whose package the
  scores service has not written fails every build until it is there. mgmt
  only reads `scores-packages/tars/`; on S3 its role also needs `s3:ListBucket`
  on that prefix, so an absent package reads as absent, not as access denied.
- The two hold separate locks, so an 80 MB build never delays the minute probe,
  and a second run of either exits rather than overlapping.
- **A crashed build keeps its lock until the lease expires.** On `s3` the
  `build` lease is the run's worst case: every pinned artifact at
  `dataset_build_timeout_secs`, plus a minute, where an eval corpus counts only
  while it is unregistered. So a build killed mid-run (out of memory, an
  evicted task) blocks the next ones for up to that long. A registered eval
  corpus costs nothing. To clear it sooner, first make sure no `build` is still
  running, then delete `<storage-root>/locks/dataset-cache-build.lock` (the
  lease object; its body names the holder, host, pid and `expires_at`).
  `unlock` clears only the mutate lock. On `local_fs` the lock is a `flock(2)`
  that dies with its process, so there is nothing to clear.
- **A build checks its corpus against its entry.** An asr-eval corpus must
  hold its entry's `parameter_sample_count` samples, as its package's manifest
  counts them, or the build fails and `GET /datasets` refuses the entry, rather
  than every device downloading a corpus it then refuses. A corpus already
  registered is held to the same count, so an entry restated with another count
  fails the next build too.
- **Only git corpora need build scratch**, tens of megabytes each. After an
  evals server restart, a set's first probe may read `unavailable` until the
  server has its digests again.
- Both shell out to `git`: the host or image needs it on `PATH`, and outbound
  HTTPS to each upstream.
- Both call `evals_server_url` for an eval corpus, for `/samples/digest`
  alone: `probe` every minute, `build` once per registration.

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
  fields `run_id`, `command`, `entries_created`, `members_added`,
  `entries_hidden`, and `unclassified_assets`.
- `ERROR` `catalog import run failed`: the run recorded an error. The line has
  the same fields, plus `error`.

Alert when no `catalog import run finished` line with `command=import` appears
in 36 hours. Set the alarm to fire on missing data, because a stopped schedule
writes no line. Enable the alarm after you enable the daily import schedule.

A run that stops before it records its end writes no line. Its row stays
`running`, and the query below reports it as `interrupted` after 6 hours.

When the alert fires, run this query against PostgreSQL. The
`latest_import_outcome` column uses the same rule as the `outcome` field of the
[run history](api/descriptor-catalog/list_catalog_import_runs.md):

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
         ELSE 'failed'
       END AS latest_import_outcome,
       latest.error_summary
FROM (
  SELECT MAX(finished_at) AS last_successful_import_at
  FROM catalog_import_runs
  WHERE command = 'import' AND result = 'success'
) AS success
LEFT JOIN LATERAL (
  SELECT id, started_at, result, error_summary
  FROM catalog_import_runs
  WHERE command = 'import'
  ORDER BY started_at DESC, id DESC
  LIMIT 1
) AS latest ON true;
```

The alert detects a stopped daily import schedule, even if the catalog still
lists active entries. A successful `refresh` does not reset it.

**Unclassified assets.** An asset that the flavor table does not know becomes
no member, but the run still succeeds. The importer logs each asset at `INFO`
with the message `catalog asset is not classified` and the fields
`release_tag` and `asset`. The run history lists the assets of each run. If a
build variant is necessary:

1. Find the asset names in the
   [run history](api/descriptor-catalog/list_catalog_import_runs.md).
2. Add the flavor to the flavor table.
3. Run `catalog refresh` for each affected entry that is not active. The next
   import adds the member to the active entries.

For an upstream or storage error, inspect the run's `error_summary` and the
`pipette-mgmt-catalog` logs.

### 3.1. Job queue maintenance

`queue-maintenance` reconciles everything stale in the job queue; what each
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

Overlapping runs *can* be destructive. The plan-index GC deletes a content key,
a membership marker or a pending-source marker only after two consecutive runs
find it orphaned. Each run records its first sightings in the
`plan_index_gc_candidates` table for the next run. Each run clears that table
as it starts, so a run that fails partway leaves no stale sightings. It only
delays GC by one interval. The rule is safe only because the gap between two
sightings is a full cron interval. In that time, an ingest that was in
progress writes the records that make the key live. Two overlapping runs can
count two sightings seconds apart and delete a live key. A missing membership
marker lets teardown retire a job that another plan still needs.
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
the job's rows from `queue_lost_leases`. A `serve` outage longer than the lease adds one loss to every
leased job, so before long planned `serve` downtime pause this cron or set the
limit to `0`, which turns the count off.

**Stranded ingests.** A manifest still `creating` past
`plan_creating_max_age_secs` (default 6 hours) marks a crashed ingest, and its
jobs are torn down. Set it above the longest expected ingest
([plan-ingestion.md §8](plan-ingestion.md#8-ingestion-flow-atomic-ish)).

**Campaign group policies.** Pass 11, the last, applies the
[policies](api/campaign-groups.md#when-a-policy-acts) that are on. It reads
their results from pipette-duckdb, so while a policy is on the host needs a
`[duckdb]` section
([storage.md](storage.md#what-pipette-mgmt-reads-from-pipette-duckdb)); without
one every run fails with a `WARN` that names it. The pass starts no member
once the run is more than 60 s past its start, the cron's cadence, so a run
holds back the next run's lease recycling by at most one member's read and
revision. A revision of a plan with thousands of jobs can still take minutes.
A run takes the members by when the pass last revised each or failed to read
its rows, longest ago first, so a member it just revised, or whose rows
pipette-duckdb did not answer, goes behind the others. After a revision, the
pass defers the member's next re-runs and marks for one to 30 minutes, unless
the plan's earliest deadline comes first. The deadline check allows one 60 s
tick after the interval, so it assumes that queue maintenance runs every minute.
With a longer cadence, or a tick that `flock` drops, a deferred filing can
reach the ingest after the deadline, and the ingest refuses it. A
large campaign with many jobs that wait for each busy device gets the longest
interval ([when a policy acts](api/campaign-groups.md#when-a-policy-acts)).
For each deferred revision, the pass logs `queue-maintenance: a campaign group
filing waits for its interval`. The log includes `interval_secs`, `remaining`,
`running`, and `next_filing_at`. When the pass reports a member:

- **A revision that did not advance** (`filings_not_advanced`): the campaign
  does not run the plan the pass filed. The pass cancels that plan, in the same
  run or the next, and logs a `WARN`. Only a revision whose campaign write has
  an unknown outcome waits for you. Each run then fails with the `WARN`
  `queue-maintenance: a campaign group filing whose head write is unknown
  still runs; cancel it, or confirm the campaign runs it`, which names the
  campaign and the plan (`filed`). Read the campaign with `campaigns show`. If
  it runs the plan, the write landed, and the next run goes on from it.
  Otherwise cancel the plan with `plans cancel`, but not right after the
  failed write, which can still land
  ([§3.5](#35-recover-from-a-failed-campaign-filing), step 1).
- **A revision whose replaced plan still runs**: the `WARN`
  `queue-maintenance: a campaign group filing advanced, and the plan it
  replaced still runs` fails one run. The campaign runs the new plan
  (`filed`), and the error names the plan that was not cancelled after "but
  plan". No later run cancels it, and while it runs, stopping the campaign's
  plan leaves the jobs both plans hold running. Cancel that replaced plan with
  `plans cancel <replaced plan_id>`, never the plan the campaign runs.
- **A campaign that waits for devices** (`members_waiting_for_devices`):
  devices have not claimed every job the policies' last revision of it queued,
  so nothing of it is read. The `INFO` `queue-maintenance: the jobs a campaign
  group filing queued wait for devices, so its campaign is not read` names the
  campaign, the revision's plan (`filed`) and how many of its jobs wait
  (`waiting`), and the policy preview shows the same. `plans status <plan_id>
  --explain` shows why a job waits. The hold has no time limit, so a job no
  device can take holds the campaign until its deadline: mark it failed with
  the campaign's re-run route (`fail`), whose revision drops the job.
- **A member or group that does not read** (`members_unreadable`,
  `members_skipped`, `groups_skipped`): each is a failed item, and the next run
  reads it again. When pipette-duckdb fails or times out on a member's rows,
  the `WARN` `pipette-duckdb did not answer the campaign's rows; it reads
  unreadable` names the campaign, the run reads no more rows, and that member
  goes behind the others. A store read that fails logs its error with the
  member or group.
- **A member state that does not decode**: the `WARN` `queue-maintenance:
  unreadable campaign group member state, skipped` names the campaign and its
  row (`key`), and every run fails until the state reads. Take the campaign out
  of the group and add it back, on the group's page or with `DELETE
  …/members/{campaign_id}` and `POST …/members`: the new row keeps no state,
  and the next run reads the campaign's plan as new. While a policy of the
  group other than `model_params`, `smaller_failed` and `vault_resource` is on,
  adding it back takes `read_plans`,
  `cancel_plans` and `read_clients` (the `_any_` grants for another owner's
  campaign), so have someone who holds them do both steps.
- **A plan that cannot be filed again** (`refused_heads`): the plan's document
  is gone, no longer matches the plan, or the server refuses it, such as for a
  deadline that has passed. The run that first meets it fails. Later runs of
  the same release log the `WARN` `queue-maintenance: the campaign group
  policies cannot file into this plan again; it is read again once it or the
  policies change, or after a new release` with the reason, and do not fail.
  The first run of a new release tries the plan once more, and fails again if
  the plan is still refused. To have the policies act on the campaign again,
  file a revision with a document the server takes, such as one with a later
  `expires_at`.
- **A job that waits for rows** (`jobs_waiting_for_rows`): an answered job
  with no rows in pipette-duckdb yet, whose submission body gives no re-run.
  Each run reads its body and its rows again. The cron jobs above score results
  every 5 minutes. The rows become available after pipette-duckdb publishes
  them. An eval's job never waits here: the
  policies judge no eval ([policies](api/campaign-groups.md#policies)). The
  Re-run candidates have a second body read before filing. If the body changed
  or disappeared, the job waits for the next run. The
  `WARN` `queue-maintenance: a flagged submission body or its vault names did
  not read; the job waits for its rows` names a job whose body gives a re-run
  that the run did not file, because the body or vault read failed.
- **Result rows that lack model params** (`models_missing_params`), while the
  group's `model_params` policy is on: the `INFO` `queue-maintenance: result
  rows lack model params` names the campaign, the `key` an entry for the model
  would take, and the `rows` and `jobs` that lack it. It is no failed item. Add
  the entry ([model params](api/model-params.md)); the pages read the table, so
  they show the counts at once.
- **Re-run results** (`reruns_recorded`, `reruns_judged`): a revision the
  campaign moves onto records each re-run it queued, one row per policy that
  re-runs it, and the run that reads a re-run's rows records its outcome, or
  `dropped` for one with neither rows nor a record that the campaign's plan no
  longer holds ([re-run results](api/campaign-groups.md#re-run-results)). Each
  run reads each member's re-runs that wait, one database read, and of those
  its plan no longer holds, whether each has a record, one storage read each,
  and their rows, one pipette-duckdb read. A write that fails is a
  failed item. After the `WARN` `queue-maintenance: failed to record the
  re-runs a campaign group filing queued`, which names the campaign and the
  plan (`filed`), run `db backfill-rerun-results` for the group
  ([§3.6](#36-record-the-re-runs-filed-before-migration-0037)). After the
  `WARN` `queue-maintenance: failed to record how a campaign group member's
  re-runs answered`, the run files nothing for the member and keeps none of its
  state, so the next run reads those re-runs again and records their outcome.

**Clock synchronization.** The expiry pass judges deadlines by the maintenance
host's clock, while `serve`'s `claim`/`reclaim` gates use their own. A
maintenance clock *ahead* of a serve host's opens a window where `serve` still
hands a job out while `queue-maintenance` expires it, writing a synthetic
failure for a job a client is running. It also shortens the stranded-ingest
deadline, which compares the filing host's `created_at`. Keep all hosts
NTP-synced; a lagging maintenance clock is harmless, so single-host or
shared-clock deployments are inherently safe.

**Log lines.** Alarms and dashboards can match these `INFO` lines, each
carrying its figures as fields:

- `queue-maintenance: run finished`, once per completed run: the run's and each
  phase's duration, the stored data it read (plans, probes, content keys, client
  records), the CPU time, the slowest plan, and `failed_items` for a run that
  skipped items and then exits non-zero. A run that stops on an error logs no
  summary. `deadlines_restamped` counts the waiting jobs the run moved to the
  earliest horizon of their plans
  ([plan-ingestion.md §7](plan-ingestion.md#the-duplicate-job-merge)).

  `plans_full` counts the full manifests that the run reads. The run reads open
  plans, their predecessors, and terminal plans that a cancel marker or a queued
  job's membership marker names. An indexed job lookup also lets teardown
  find cancelled plans for queued jobs without markers. The run reads no plan
  summaries, so `plans_summary` is zero. `peak_rss_kib` reports the process's
  peak resident memory in kibibytes (KiB). Each phase line also has this field.

  `bodies_read` counts full job bodies that the run fetches from the
  SQL queue. The count includes these reads:
  - expiration
  - lease retirement
  - corrupt bodies, which each run reads again until the row is repaired

  Cache hits and reads of matcher projections do not count.
- `queue-maintenance: policy pass finished`, once per run with a `[database]`,
  also after queue passes that stopped on an error: the `groups` with a policy
  on and the `groups_skipped`, their `members`, the `members_read` and the
  `members_not_reached` within the 60 s (members of groups with no policy on
  left to clear included), the `members_unreadable` and `members_skipped`, the
  `members_cleared` of groups with no policy on, the
  `revisions` with their `rerun_jobs` and `marked_jobs`,
  the `reruns_recorded` for them and the `reruns_judged`, whose outcome the run
  recorded, `jobs_waiting_for_rows`, the `body_reads` of submission bodies for
  answered jobs with no rows (including the second reads of re-run candidates),
  the `body_reruns` that those bodies re-ran in
  revisions that advanced, `members_waiting_for_devices`, the
  `filings_deferred` that wait for the interval since the member's last
  revision, `filings_not_advanced`, the `cancel_markers_written` on them,
  `filings_stopped` by someone stopping the plan they revise, `refused_heads`,
  `other_owner_members`, `jobs_left_for_good`, `models_missing_params`, the
  keys the result rows it read lack a model params entry under,
  `failed_items`, the pass's `duration_ms`, and `run_duration_ms` from the
  run's start, the queue passes included. A pass that cannot read the policies
  logs no line. Alert when `run_duration_ms` passes 5 minutes: such a run holds
  back the next one's lease recycling.
- `queue-maintenance: a campaign group filing re-runs a job on the rows of its
  submission body`, once per job that a revision that advanced re-runs before
  pipette-duckdb has its rows: the `group_id`, the `campaign_id`, the `job_id`
  and the revision's plan (`filed`).
- `queue-maintenance: result rows lack model params`, once per key and member
  campaign while the group's `model_params` policy is on: the `campaign_id`,
  the `key` an entry would take, and the `rows` and `jobs` read this run that
  lack it.
- `queue-maintenance: S3 and CPU work of a phase`, before the summary, once per
  phase (`queue_passes`, `reconcile_plans`, `plan_index_gc`): HTTP attempts of
  both S3 clients by outcome (`throttled` counts S3's `SlowDown`), the peak in
  flight, the time waited, and the bodies decoded.
- `queue-maintenance: refreshed a plan's progress snapshot`, per plan whose jobs
  the pass probed or whose status changed, and
  `queue-maintenance: plan status changed` with `from` and `to`. A plan that
  fails gets the `WARN` `queue-maintenance: failed to reconcile a plan`.

S3 request attempts that reach the HTTP connector without an answer log
`S3 request attempt got no response`, with the client, method, path, host,
wait time and error. The SDK's per-attempt timeout can end an attempt before
that connector emits a log, so the request log does not count every slow
attempt. The `object_store` client waits 5 s to connect and 30 s for a
request. To fail a stalled maintenance read sooner, set
`queue_maintenance_read_timeout_secs`.

Each filing emits `plan job promotion latency` with its plan ID, completed
promotion count, failure count, p50, p95, p99 and maximum call duration in
milliseconds. The sample includes completed promotions before a filing error;
in-flight promotions dropped on that error have no duration.

### 3.2. Eligible index maintenance

The eligible index is maintained by `queue-maintenance` alone; `serve`
replicas only read it ([queue.md](queue.md#the-eligible-index)). No operator
action is needed after `clients approve`, a permission change or a profile
update: the next run reflects it. The console counts each worker's backlog from
the index at each request ([queue.md](queue.md#device-backlog)).

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

### 3.5. Recover from a failed campaign filing

A filing writes its plan and jobs before it writes the campaign. If it fails
while staging, the plan can remain `creating` with promoted jobs in the job
queue.
Queue maintenance leaves those jobs out of the eligible index while no final
plan also holds them ([queue.md](queue.md#committed-filings)). A job already
leased to a device can continue running. **Refile before you cancel or wait
for teardown** if you want to preserve those jobs and results.

1. **Find the plan and campaign.** Get their IDs from the error or the server
   log's `request failed` line (`promoting job <job_id> for <plan_id>`). A
   campaign-write error with an unknown outcome names both IDs. Read the
   campaign with `pipette-mgmt campaigns show <campaign_id>`. If its current
   plan or history includes `<plan_id>`, the filing completed. Stop here. A
   negative read immediately after a timed-out campaign write does not prove
   that the write will never land. Do not cancel or refile that plan until its
   write outcome is reconciled. The remaining steps cover a staging failure
   or a confirmed orphan.
2. **Read the plan and record its work.** Run `pipette-mgmt plans status
   <plan_id> --explain --leases` and note its status, available and leased job
   counts, and the clients under **Running now**. Save recorded results with
   `pipette-mgmt plans results <plan_id> --json`. Check how much time remains
   before `plan_creating_max_age_secs` (default 6 hours) tears down a plan
   stuck in `creating`.
3. **Refile the same document before that deadline.** Use the same owner and
   filing operation (`campaigns create` for a failed creation), not `--dry-run`.
   Keep duplicate merging
   enabled. A live job with the same content key joins the new plan; a
   completed cell can reuse its result for the same owner. A job already gone
   from both queue stages must be minted again. Do not cancel the old plan
   before the new filing succeeds: teardown would retire its exclusive jobs
   and end their leases.
4. **Verify the new filing.** Its final response must report success (`201`
   over HTTP). Compare `merged_job_count` with the old plan's available plus
   leased jobs from step 2. The count includes finished runs reused, so it
   should be at least that live count when those jobs remain in the queue.
   Read the new campaign and plan to confirm they exist. If the filing fails,
   retry with the same document before the old plan's teardown deadline.
5. **Let the old plan finish cleanup.** A `creating` plan past its age limit
   is torn down by queue maintenance. It releases jobs that the new plan now
   holds and retires only jobs with no other member. No manual cancel is
   needed. If the orphan was already final, cancel it only after the new
   filing succeeds, then verify that its shared jobs remain with the new plan.

A failed retry can leave extra membership markers even if it wrote no new
manifest. Check the new campaign and plan, not only its HTTP status. Keep the
original plan uncancelled while you retry; a later maintenance pass can then
release its joined jobs to the successful filing.

### 3.6. Record the re-runs filed before migration 0037

From the release that adds migration `0037`, the policy pass records each
re-run it files ([§3.1](#31-job-queue-maintenance)). The re-runs it filed
before have no row until `db backfill-rerun-results` records them
([cli.md](cli.md#pipette-mgmt-db-backfill-rerun-results-group_id)). Run the
command once, after the deploy, not before: until then the older binary still
files re-runs and records none.

1. Run `db migrate` before the deploy, as for every migration
   ([§5](#5-docker-deployment)).
2. Deploy the release.
3. With the deployed release and its config, name each group whose policies
   filed re-runs (`GET /api/v1/campaign-groups` lists the groups):

   ```
   pipette-mgmt db backfill-rerun-results <group_id>...
   ```

   The command reads pipette-duckdb, so its config needs a `[duckdb]` section;
   without one it exits naming it. It prints the filings it read and the rows it
   recorded. A run that fails can run again: a second run records only the
   rows still missing.

### 3.7. Stop the writes to `plan_jobs`

The table `plan_job_spans` records plan membership
([storage.md §4.7](storage.md#47-the-plan-job-spans)). This release stops the
writes to `plan_jobs` and removes the span backfill command. The release has no
migration. The `plan_jobs` table stays in the schema until a later release
drops it.

Before the deploy:

- Confirm that the `pipette-mgmt` service reads spans in production and that
  no read fails.
- Confirm that the pipette-duckdb reader reads `plan_job_rows` in production.
  Revoke `SELECT ON public.plan_jobs` from the pipette-duckdb reader role, then
  run one pipette-duckdb refresh. The refresh must succeed and publish the
  plans. If it fails, the reader still reads `plan_jobs`: grant the privilege
  again and do not deploy.
- Run `db backfill-plan-job-spans --dry-run` with the deployed release. The
  command must exit 0 and report no lineage that differs. If the command fails
  or does not run, do not deploy. This run is the last valid comparison with
  `plan_jobs`.

Deploy the release. Wait until no task of the previous build runs. During a
rolling deploy, old tasks can still write `plan_jobs`. Confirm that a new
campaign revision reads back correctly. Then run this query for it:

```sql
SELECT count(*) FROM plan_jobs WHERE plan_id = '<new-plan-id>';
```

The count must be 0. After the next refresh of pipette-duckdb, confirm that it
publishes the new plan.

After this deploy, do not run the span backfill command from an older release.
The `plan_jobs` rows do not follow the plans, so the command would replace the
correct spans with stale data. After this deploy, the spans have no second
record to compare with. The `put_plan` tests are the only check of the span
writes.

To roll back, deploy the release that added migration `0043`. That release
reads spans and writes both tables. No database step is necessary. The
rollback release writes the `plan_jobs` rows of each plan that it writes. The
other `plan_jobs` rows stay stale, and nothing reads them.

- Do not roll back to a build that reads `plan_jobs`.
- Do not run the span backfill command after a rollback.
- Do not move the schema below migration `0042`. Its down script drops
  `plan_job_spans`, which is the only record of plan membership.

The down script of migration `0043` drops the `plan_job_rows` view and its
grant. Each time `0043` runs again, grant `SELECT ON public.plan_job_rows`
again to the role pipette-duckdb reads the database with.

The release of [§3.8](#38-drop-plan_jobs) drops `plan_jobs`.

### 3.8. Drop `plan_jobs`

This release adds migration `0044`, which drops the table `plan_jobs`. The
table `plan_job_spans` records plan membership
([storage.md §4.7](storage.md#47-the-plan-job-spans)).

Before the migration:

- Confirm that each active task uses the release of [§3.7](#37-stop-the-writes-to-plan_jobs)
  or a later release. A task of an older build writes `plan_jobs`, and that write
  fails after the drop.
- Confirm that a rollback to a build before the release of §3.7 is not
  necessary.

To deploy the release:

1. Run `db migrate`. The active tasks do not use the table.
2. Deploy the release.
3. Record the sizes in PIP-900. Compare them with the 1,291 MB and 2,471,703
   rows of `plan_jobs` on 2026-11-16:

   ```sql
   SELECT pg_size_pretty(pg_database_size(current_database())) AS database,
          pg_size_pretty(pg_total_relation_size('plan_job_spans')) AS spans,
          (SELECT count(*) FROM plan_job_spans) AS span_rows;
   ```

To roll back:

- To the release of §3.7, deploy it. Its schema check accepts a newer schema,
  and it does not use `plan_jobs`.
- To the release that added migration `0043`, first run `db migrate --to 43`
  with this release. The command creates `plan_jobs` again and fills it from
  `plan_job_rows`. The fill can take minutes. Deploy only after the command
  ends. The rules of §3.7 for that release then apply.
- Do not roll back to a build that reads `plan_jobs`.

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

A deployment keeps its records (plans, campaigns, identities, vault records)
and the job queue on PostgreSQL through `[database]`
([design/sql-record-store.md](design/sql-record-store.md)); run `db migrate`
before a binary that needs a newer schema
([cli.md](cli.md#pipette-mgmt-db-migrate---to-version---dry-run-and-db-check)).
The examples below show `[database]` and the object-store sections, which keep
submissions, the warehouse, blobs and the dataset cache. Each example reads the
database connection from the libpq `PG*` environment variables.

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

[database]
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

# The records and the job queue; the libpq PG* environment variables supply
# the connection.
[database]

# The eval corpora the scores service packs, which mgmt only reads (§3.0).
[eval_packages_storage]
backend = "s3"
bucket = "my-eval-datasets"
region = "us-east-1"

# Vault blobs and staged uploads, apart from [storage]. Copy vault-resources/
# here before adding this section, or the existing blobs read as missing.
[vault_storage]
backend = "s3"
bucket = "my-pipette-vault"
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
EC2 IMDSv2). Use **separate buckets** for `[storage]` and `[auth_storage]` so
benchmark/warehouse data and private client identities can have different IAM
policies. The memberships, device ownerships and campaign and vault owners that
decide access ([storage.md §3.2](storage.md#32-organizations)) live in
PostgreSQL, and the database credentials are that authority.

`[storage]` and `[auth_storage]` are probed for conditional `PUT`, which the
storage mutate lock is built on. A backend that answers `200` to a write it should have refused **refuses to
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

[database]
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

[database]
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
durable pending client record behind it. `POST /api/v1/users/register` leaves
no such residue: it takes an invite and nothing else.

Each process admits at most 20 device registrations without a pre-auth key per
minute and answers the rest `429`, logging
`refused: too many registrations without a pre-auth key`. A registration with a
pre-auth key, and a device retrying a registration already on record, are not
counted. The count is per
process, so a deployment of several instances admits that many per instance.
Of those 20, one client address gets 1, an IPv6 one counted by its `/64`, since
a device registers once; clients behind one NAT share it. The
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
internet-facing deployment should set `require_client_preauth_key = true`,
which closes that path.

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

Nothing asks for an account and waits: there is **no self-registration for
users**, since an address is self-reported here as much as on a client, and an
operator account is the half of the vocabulary that authors work. An account is
minted by an operator (`users create`), an administrator
([`POST /api/v1/users`](api/users/create_user.md)), or a Clerk sign-in, where
Clerk has verified the address ([§6.3](#63-console-sign-in-through-clerk)).
`POST /api/v1/users/register` only binds a key to the account an invite names
([`POST /api/v1/users/register`](api/users/register_user_key.md)).

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
  organization must exist and not be disabled. Without a rule, an address at a
  domain `allowed_domains` lists gets an account on its first sign-in too, in a
  new organization of its own, named as the sign-in asks or `<email>'s personal org`, which it administers
  (`org_admin`). With `allowed_domains` unset, an address with no rule signs in
  only to an account an operator created for it, so sign-up stays closed to
  addresses nobody listed.

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
