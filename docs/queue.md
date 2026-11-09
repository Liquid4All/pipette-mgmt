# Job queue

This document specifies the `todo/` queue, where a plan's jobs wait to be run,
and the claim, heartbeat and failure protocol that hands them to devices.

The key layout of `todo/` is [storage.md §2](storage.md#2-logical-layout-and-invariants).
The wire contract of each device route is [httpapi.md §2](httpapi.md#2-endpoints),
and how a device should drive it is [client-integration.md](client-integration.md).
Which client may run which job is [plan-ingestion.md §5](plan-ingestion.md#5-capability-matching).

## Processes

Five actors touch the queue. All of them are `pipette-mgmt`.

| Actor | Process | Replicas | Responsibilities |
|-------|---------|----------|-----------------|
| **Plan ingestion** | `pipette-mgmt serve` (the campaign routes), or the `campaigns` commands | Any | Writes job bodies to `todo/tmp/` and promotes them to `todo/avail/` ([plan-ingestion.md §8](plan-ingestion.md#8-ingestion-flow-atomic-ish)) |
| **Management server** | `pipette-mgmt serve` | One or more | The device routes: claim, heartbeat, reclaim, and result submission |
| **Submission processor** | `pipette-mgmt process-submissions` (cron; `score` alias) | One at a time | Routes eval submissions, scores non-evals, finalizes scored evals, writes to the warehouse |
| **Eval scorer** | `pipette-mgmt score-eval` (cron) | One at a time | Drains `submissions/score-queue/to_do/`, calls the upstream scoring service, and stages eval scores for finalization |
| **Queue maintenance** | `pipette-mgmt queue-maintenance` (cron) | One at a time | Recycles expired leases, retires jobs, deletes stale `todo/tmp/` files, maintains the `eligible/` index (sole writer), and repairs pending submission outcomes |

Plan ingestion is the only writer that promotes a new job into `avail/`. The
one other promote is teardown's restore of a job it just deleted
([plan-ingestion.md §9](plan-ingestion.md#cancellation-and-teardown)).

**Multiple `serve` replicas** need no coordination. Every `todo/` transition is
an atomic rename, submission keys are unique `job_id`s, and each ingest mints
its own `plan_id` and `job_id`s. `serve` replicas never write `eligible/`.

`process-submissions`, `score-eval` and `queue-maintenance` each run **once per
schedule interval**, not once per `serve` replica. See
[operations.md §3](operations.md#3-cron-setup).

Every queue transition rests on an atomic rename, so `todo/` requires S3
Express One Zone ([storage.md §8](storage.md#todo-requires-s3-express-one-zone)).

## Jobs

A job is a JSON body: a server-owned envelope (identity, eligibility, expiry)
around the `spec` that the device runs. [plan-ingestion.md §1](plan-ingestion.md#1-division-of-responsibility)
lists what the server reads of each field, and
[httpapi.md §2.9.2](httpapi.md#292-response-200-ok) what a device receives.

- The job id is server-minted at ingestion. The one requirement is the job-id
  charset (`JobId::try_new`), which excludes `.` and `/`, so the id splits
  cleanly out of every key. `job-{UUIDv7}` is the convention: listings and logs
  then read in arrival order.
- The lease length is server configuration, not part of a job. The claim
  response reports it as `time_window`.
- At least one of `clients` or `requires` must be non-empty. `any_of` alone
  only narrows, so it makes no job claimable.

**The `avail/` key carries the deadline the server acts on.** `expires_at` is an
ISO 8601 basic-format timestamp (`20240908T000000Z`) or `never`. The lease key
keeps it while the job is out, the expiry pass retires the job by it, and the
claim response reports it. The body's copy drives nothing; a claim logs a
warning when the two disagree. Ingestion stamps both from one value, so a plan
it files cannot disagree.

**A promoted body is immutable.** A writer promotes only a complete body, and
the atomic `tmp/` → `avail/` rename hides any partial write. The claim path
never promotes into `avail/` and never deletes from it: it only renames
`avail/` → `leased/`. `queue-maintenance` is the only writer that changes an
`avail/` key after the promote: it renames the `expires_at` component, inside
the job's directory, and only while that deadline is already lapsed
([Expiration](#expiration)).

### The eligible index

`queue-maintenance` materializes the matching rules
([plan-ingestion.md §5](plan-ingestion.md#5-capability-matching)) as
eligibility markers, one per client and job. Only `queue-maintenance` writes
eligibility markers. `claim` and `reclaim` trust each marker without another
match check. On the
database queue a marker is a `queue_eligibility` row, and a read takes the
deadline from the job.

- The index covers the **workers** only: approved clients that hold
  `perform_work`, the two checks `claim` makes.
- Each job carries a reindex flag, `queue_jobs.needs_reindex`. These events set
  the flag:
  - the job enters the queue
  - a recycle returns the job to the queue
  - a re-date changes the job's deadline
  - the store records a client denial
  - a client reindex runs while the job is out on lease
- Each run reads the bodies of the flagged available jobs only. It matches each
  job against every worker, replaces the job's markers, and clears the flag, in
  one transaction per job. If a claim or re-date occurs between the read and the
  write, the job keeps its flag for the next run.
- The first run indexes a promoted job after at least one of its plans leaves
  `creating`. Until then, the job keeps its flag. The next run indexes a job
  with no plan marker.
- The run compares the workers with the set that the last run indexed
  (`queue_workers`). It deletes the markers of a worker that left, and matches a
  worker that joined against every available job. So a change in the workers
  (an approval, a grant, a revocation) reaches the index within one cron
  interval, whichever code path made it.
- A job out on lease keeps its flag until it returns to the queue. The run
  defers the match check until the job returns. Most leased jobs finish, so
  their markers need no update.
- The run deletes a job's markers when the job permanently leaves the queue.
- The index needs the database queue. On a local_fs or S3 queue,
  `queue-maintenance` and `plans status --explain` stop with an error.

The cost is up to one cron interval between a change and the index. Jobs run for
many minutes, so that latency is acceptable.

A run reads each flagged available body once. Retirement, escalation for a
closed roster, and the index pass share that body. Failed retirement or
escalation keeps the flag for a retry. A denial write sets the flag and
advances the job revision. A denial that arrives during the index pass retains
its retry.

Retirement does not check an indexed job again. A build that makes the
retirement rules stricter must ship a migration that flags every available
job. `flag-reindex` flags clients, not jobs, so it does not cause this check.

Plan starvation follows the eligible index. An available job is starved if
no eligible row belongs to a client without a denial for that job. The run
reads only starved bodies to group their requirements before it chooses the
plan status. If a job leaves availability before that read, the run excludes
it from starvation. The run treats that job as matched for the plan status.

The run uses the cached body and the worker snapshot for a job that the index
withholds or that changes during the index pass. The run excludes denied
clients. After a client changes, the starvation check can follow the index
and not the client snapshot for one run.

A client rebuild reads each distinct set of stored matcher clauses. It
matches each set once and inserts eligibility rows for those jobs in a batch.
It reads no job bodies. The run still lists every available job.

### The reindex gate

A device-profile or capability change (`PATCH /clients/me`, or registration)
voids the client's standing in the queue until `queue-maintenance` re-evaluates
it.

- The change **relinquishes every lease the client holds**, before the new
  profile is persisted. A lease was granted against the old profile, and a
  client must not continue a job it may no longer match. Each job returns to
  `avail/` with no `denied/` marker, so the client can claim it again once
  re-indexed. A lease whose job already has a submission record is deleted
  instead, so a finished job never becomes claimable again.
- While the gate is up, every plan operation is refused **without renaming a
  lease**: `claim` answers `204`; `heartbeat`, `reclaim` (both paths) and
  plan-attached submissions answer `404`. The client learns of the gate from
  `reindex_pending` ([httpapi.md §2.4](httpapi.md#24-patch-clientsme)).
- Each request writes its own `pending-reindex/{client_id}.{uuid}` flag, and
  `PATCH` writes two: one before it relinquishes the leases, one after the new
  record is durable. A rebuild reads the record fresh and deletes exactly the
  flags it captured before it started. So a rebuild never consumes a request
  newer than the record it evaluated, and back-to-back changes need no
  serialization.

A client that wants to hand back a job without changing its profile reports a
retriable failure instead, which does write a `denied/` marker. A refused
heartbeat is a protocol violation: the client ran work while its own profile
update relinquished it. The gate costs at most one cron interval.

## Claiming a job

`POST /plans/claim` ([httpapi.md §2.9](httpapi.md#29-post-plansclaim)) checks
two things before it selects a job.

1. **Lapsed leases.** The claim settles each lapsed lease of the client as
   `queue-maintenance` does ([Heartbeat timeout](#heartbeat-timeout-dead-device)).
   The server deletes a lease if its job has a record. Any other lease counts as lost,
   and its job goes back to `avail/`, or retires once its losses reach the
   limit. A heartbeat on a settled lease finds no lease,
   so it cannot give the client a second live lease.
2. **An existing lease.** Claim is **idempotent**. A client that holds a live
   lease gets that same job back, with `time_window` set to the
   lease's *remaining* life. The lease is not renewed. The innocent cause is a
   claim whose response was lost: the client never learned the `job_id`, so it
   cannot `reclaim`. Leaving the expiry alone keeps a crash-looping client from
   extending the job: the lease lapses on schedule and a healthy client gets it.
   The server checks vault access before it counts a hand-back as lost. A vault
   refusal recycles the lease without another loss. Each permitted hand-back
   counts the lease as lost, once per lease, and a job out of
   losses is withheld with `204` ([Heartbeat timeout](#heartbeat-timeout-dead-device)).

   Each claim lists all of `leased/` once to build the set of jobs other clients
   hold; that is bounded by the number of benchmarks running at once.

A device that crashes or disconnects on every run needs no operator action.
Each idempotent hand-back counts one lost lease against the job. The server
withholds the job, then retires it at `plan_lost_lease_limit`
([Heartbeat timeout](#heartbeat-timeout-dead-device)).
On the database queue, the claim cannot grant a second live lease: one
transaction handles the claims of a device one at a time. On the local_fs and
S3 queues, two overlapping claims can each grant a lease. There, the guarantee
depends on the client. It must not have two `claim` or `reclaim` requests in
flight ([client-integration.md §4](client-integration.md#4-the-work-loop)).

Otherwise the server selects among the client's `eligible/` markers. It skips a
job the client denied (`denied/{job_id}.{client_id}`) and a job past its
`expires_at`, even one `queue-maintenance` has not swept yet. Queue maintenance withholds markers for jobs held only by creating or missing
plans ([Committed filings](#committed-filings)). It
does not check that a job is still in `avail/`: the claim rename is the liveness test, and a
rename that finds no source moves on to the next candidate. A grant, or a lease
already held, whose job names a vault resource the client may not fetch goes
back to `avail/`, denied to the client, and the claim moves on; a failed check
puts it back without the denial. Heartbeat and reclaim refuse such a lease with
`403` and put it back the same way, denied
([Vault](api/vault.md#who-may-do-what)).

**Selection order.** Soonest-expiring first, `never` last, by the deadline in
each marker. One ingest stamps many jobs with the same deadline, so **within a
tier the order is random**, which keeps idle clients from stampeding one
`job_id`.

**One lease per client.** A client holds at most one live lease. On the
database queue, loss accounting and new grants use a transaction. It takes a
lock for the client, settles the client's lapsed leases, then returns the live
lease or leases one job. Recycling and the lost-lease row commit with the grant.
A job with a pending record repair or enough losses to retire stays leased
while the server resolves its record outside the transaction. The server then
retries the claim. The
deadline comes from the job row, and the transaction skips the jobs that other
claims lock. The server processes two concurrent claims from one client sequentially. The
second returns the job that the first leased. A reclaim takes the same lock, and leases
no job while the client holds a live lease on another. The `local_fs` and S3
queues run the same steps one call at a time. Two concurrent claims can give a client two leases on these backends. The next
claim returns the lease with the later expiry.

The claim renames `avail/{job_id}/{expires_at}.json` to
`leased/{client_id}/{job_id}.{expires_at}.{lease_expiry}.json`, where
`lease_expiry` is now plus the lease length. The rename is atomic, so at most
one claimer wins. When every candidate fails, the answer is `204`. The idle
cadence after a `204` is the client's ([client-integration.md §5](client-integration.md#idle-polling-cadence)).

### Committed filings

Ingestion promotes a job into `avail/` before its plan leaves `creating`.
`queue-maintenance` reads the plan manifests and job memberships before it
builds the eligible index. The run excludes a job if all its memberships name
plans with missing manifests or `creating` status. The job keeps its reindex
flag. The first run after a plan
leaves `creating` evaluates the job. The run immediately indexes a job that an
active plan also holds. The next run indexes a job without membership markers.

The run also removes any eligible markers it finds for a withheld job. A lease
already held by a device remains valid until the usual lease or teardown path
handles it.

### Lease naming

The lease key ([storage.md §2](storage.md#2-logical-layout-and-invariants))
names both deadlines, because they answer different questions. `lease_expiry`
is when this client's hold lapses. `expires_at` is the job's own deadline,
copied unchanged from the `avail/` key. Carrying it makes a recycle one rename
that rebuilds the exact `avail/` key with no body read, and it lets
`queue-maintenance` keep a leased job's `avail/` key in its index set, so a
recycled lease is not re-indexed.

The `client_id` partition lets `heartbeat` and `reclaim`, which know their
client, list one prefix. The job-id charset excludes `.` and both timestamps are
dot-free, so the leaf splits unambiguously.

### Heartbeat, recycle and reclaim

The client heartbeats at half the `time_window`. Each heartbeat renames the
lease onto a new `lease_expiry`, one lease length from now. A lease's total
life has no cap, by design: a running benchmark may finish. Against a device
that renews a job without reporting, an operator revokes its `perform_work` or
unapproves it; either refuses its heartbeats, so the lease lapses.

`queue-maintenance` renames a lapsed lease back to `avail/` under exactly the
key it was claimed from, and the job becomes claimable again.

`reclaim` renews the lease when the caller still holds it, which also beats the
recycler to it. Otherwise it re-acquires the job from `avail/` with the claim's
rename and the claim's eligibility and `denied/` checks.
[httpapi.md §2.10](httpapi.md#210-put-plansjob_idheartbeat) and
[§2.11](httpapi.md#211-post-plansjob_idreclaim) give what each answer means.

### Device liveness

Each verified, signed device request stamps the empty marker
`todo/liveness/{client_id}`; its modification time is the device's
`last_seen_at` ([Devices: What a device row carries](api/devices.md#what-a-device-row-carries)
states what a reader gets).

- Every device route stamps it, even one that refuses the request.
- On S3 the storage server's clock sets the time, so all replicas agree.
- A failed stamp is logged and never fails the request.
- Age never deletes the marker. `clients delete` and the `queue-maintenance`
  orphan sweep do.

### Device backlog

A device's backlog is the number of queued jobs it could claim
([Devices: What a device row carries](api/devices.md#what-a-device-row-carries)
states the fields and their staleness). Each `queue-maintenance` run writes
the counts to `todo/.backlog`, and the device listings read that one object.
An `eligible/` marker counts when the claim would accept it: its job is in
`avail/` under the deadline the marker names, the deadline has not passed, and
the device has not denied the job.

Each run also writes the backlog of each device by campaign to
`todo/.backlog-campaigns`. Only the device page reads it
([`GET /api/v1/clients/{client_id}`](api/devices/get_client.md)).
[Devices: What a device row carries](api/devices.md#what-a-device-row-carries)
states which campaign holds a job and what the page counts.

### tmp/ cleanup

An ingest that crashes between `write_tmp` and the promote leaves a file in
`tmp/`. `queue-maintenance` deletes it; see
[operations.md §3.1](operations.md#31-job-queue-maintenance).

## Results

The client reports the outcome through `POST /benchmarks`
([httpapi.md §2.7](httpapi.md#27-post-benchmarks)), echoing the `job_id`.

**A plan-attached submission is bound to the lease.** The storage key
`submissions/incoming/{job_id}.json` is not partitioned by client, so the server
accepts a result only from the client that holds the job's lease. A client whose
lease lapsed and was reassigned is refused, so its late result never overwrites
the new owner's; it must `reclaim` first. Under its own lease a client's
resubmission supersedes its earlier body. Only the server's synthetic failures
write the key exclusively, so they never replace a body the client wrote. An
ad-hoc submission (no `job_id`) gets a fresh server-minted id and no binding.

**A terminal submission** (a success, or a non-retriable failure) deletes the
job from `leased/` and `avail/`, so it is never handed out again. Its orphaned
`eligible/` and `denied/` markers go within two `queue-maintenance` runs: a
marker is deleted only once two consecutive sweeps see its job gone, so a job
caught mid-transition never loses live markers.

A **retriable** failure keeps the job queued for other clients
([Consequences of Failure](#consequences-of-failure)).

A device that freezes or crashes stops heartbeating, and its lease lapses one
`time_window` after the last heartbeat.

### Consequences of Failure

#### Client-reported failure

A failure submission carries `retriable`
([httpapi.md §2.7.2](httpapi.md#272-failure-variant)).

*Non-retriable.* The job cannot succeed anywhere. The submission is the job's
terminal result, and the job is torn down as above.

*Retriable.* Something is wrong with this device, not the job. The server
writes a `denied/{job_id}.{client_id}` marker that records the
`failure_reason` (its last 4 KiB), the `client_version` and `runtime_version`
(256 bytes each), and the time. It logs a `WARN` with the full reason, removes
the client's lease, and **keeps the job in `avail/`** for other clients.

`queue-maintenance` then asks whether any eligible client remains:

- **A `clients`-only job** (no `requires`) has an enumerable roster: `clients`
  minus `exclude_clients`. When every client in it has denied the job,
  `queue-maintenance` retires the job with a synthetic `"system"` failure, a
  `failure_source` of `all_clients_denied`, and a summary of the denials as its
  reason. Until that run the job is unclaimable but harmless. A flag exclusion
  (`excludes`, `not_all_of`) does not close the roster, because the client can
  change its profile back.
- **A job with `requires`** stays open: a matching client can register at any
  time. Its deadline bounds it.

A job that no *possible* client can match is retired the same way, with
`failure_source` `unmatchable`
([plan-ingestion.md §5](plan-ingestion.md#bodies-that-can-never-match)).
[plan-ingestion.md §9](plan-ingestion.md#synthetic-failure-records) describes
every synthetic record.

#### The all-denied failure reason

The reason groups the roster by the reason each client gave, largest group
first, with a count and up to three client ids per group. Reasons that differ
only in timestamps, hex addresses or long numbers are one group. An empty
marker reads `no reason recorded`, and an unreadable one `reason unreadable`.

```text
All eligible clients reported failure (10 clients, 2 distinct reasons):
- 8 clients (ev1_a, ev1_b, ev1_c, +5 more): llama-server failed to load model: out of memory
- 2 clients (ev1_x, ev1_y): llama-server crashed mid-completion: exit signal: 11 (SIGSEGV)
```

The per-denial `WARN` keeps each reason in full.

#### Heartbeat timeout (dead device)

A lapsed lease returns the job to `avail/`, and the client stays eligible. One
timeout means "device unavailable", not "job failed": the device may have run
the benchmark the whole time. To submit, it must `reclaim` first; when the job
was completed or re-claimed meanwhile, the reclaim fails and the result is
dropped.

Each **lost lease** still counts against the job. `queue-maintenance` writes a
`todo/lost/{job_id}/{client_id}.{lease_expiry}` marker, and when a job's markers
reach `plan_lost_lease_limit` (default 3) it retires the job instead of
recycling it, with a synthetic failure of `failure_source` `lease_lost` that
names the clients. This bounds a job that crashes or disconnects every device
that runs it. The count belongs to the job, so it bars no client. The bound
holds whether the job's eligibility comes from `clients` or `requires`, so a job
only one device can run survives `plan_lost_lease_limit - 1` flakes on that
device. A `serve`
outage longer than a lease adds one loss to every leased job. A limit of `0`
turns the count off.

The idempotent claim feeds the same count: a client that re-polls inside its
lease gets the job back, each hand-back counts once per lease, and once the
job's losses reach the limit `claim` answers `204` and lets the lease lapse.

A device away for longer than `time_window` (a slow reboot, maintenance, a long
outage) finds its lease already recycled, or the next claim recycles it. The queue treats the device as available again.

#### Expiration

A job whose `avail/` key has lapsed is settled against the plans that hold it
([plan-ingestion.md §7](plan-ingestion.md#the-duplicate-job-merge)).

- **A member plan still holds a later horizon:** the job is **re-dated**, renamed
  within `avail/` onto the latest such horizon, and stays claimable. The plans
  whose horizons passed count the job `failed`.
- **No member horizon remains**, or the job has no member at all: the job is
  abandoned. `queue-maintenance` writes a synthetic `"system"` failure with
  `failure_source` `expired`, deletes the entry, and the same run's sweeps take
  its markers. A job that already has a submission record is not expired; a
  leftover `avail/` entry is deleted and the record kept.

A `never` job never expires. Stop it with `plans cancel`
([plan-ingestion.md §9](plan-ingestion.md#cancellation-and-teardown)). Delete
its `avail/` entry by hand only when no plan holds it any more
([plan-ingestion.md §7](plan-ingestion.md#the-duplicate-job-merge)).

**The deadline bounds assignment, not running.** `claim` and `reclaim` treat a
lapsed job as gone (`204` and `404`). `heartbeat`, and a `reclaim` that finds
the caller still holding the lease, never fail on it, so a running benchmark
finishes. A leased job that crosses its deadline is expired only after its lease
lapses and it returns to `avail/`.
