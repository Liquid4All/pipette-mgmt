# Design: the job queue on PostgreSQL

**Status: the migration and the job methods of `SqlTodoStore` are implemented.
No caller uses `SqlTodoStore`.** The epic
is PIP-822. This design replaces the keyspace layout in
[storage.md §2](../storage.md#2-logical-layout-and-invariants) and keeps the
protocol in [queue.md](../queue.md). The other records moved to PostgreSQL in
[sql-record-store.md](sql-record-store.md).

## The problem

The job queue, `todo/`, is the one record keyspace still on the object store.
The claim of a job is an atomic rename, and S3 Express does a rename well. The
rename is not the problem. The work around it is:

- Each `queue-maintenance` run lists all of `leased/` three times, all of
  `avail/` once, all of `denied/` twice and all of `eligible/clients/` once. It
  also reads every `avail/` body to run the matcher again.
- Each claim lists all of `leased/` to find the leases of the other clients.
  Each claim, heartbeat, reclaim, submission and `GET /clients/me` lists all of
  `pending-reindex/`.
- Each filter by plan (`/api/v1/plans/{id}/jobs`, `plans status --leases`,
  teardown, the plan counts) lists all of `avail/` and `leased/`. It then
  intersects the keys in memory with `manifest.job_ids`. Nothing in `todo/`
  names a plan.
- A directory bucket returns a listing in pages of 1000 keys and has no
  server-side `start-after`. So each scan is many round trips, one after the
  other.
- The object store has no transactions. A transition that touches more than
  one key needs a manual crash-safe order. Examples:
  - the `tmp/` staging
  - the garbage collection (GC) in `.gc-candidates`, which deletes a key only
    after two sightings
  - the restore of a job on a late joiner in teardown
  - the reindex flags, each with a fresh UUID
  - the deadline in the `avail/` key, so that a recycle can rebuild the exact
    key

The production queue on 2026-10-31:

| Keyspace | Count |
|---|---|
| `avail/`, the available jobs | 9,016 (8.4 MB of bodies) |
| `leased/`, the leases | 93 |
| `eligible/clients/`, the eligibility markers | 86,727 |

## The decision

The queue moves into the `[database]` PostgreSQL that holds the plans, the job
sources and the job-plan membership. One row per job carries its lease. Each
transition is one statement. Each marker keyspace is a table. Transactions and
cascading deletes then remove most of the coordination work.

Seven decisions shape the work:

| # | Decision | Choice |
|---|---|---|
| D1 | The local_fs and S3 queue backends | Retire both after the cutover. The queue then requires `[database]`, and the queue tests run on PostgreSQL. |
| D2 | The cutover | A short pause with a mirror copy, not a live composite store. The copy runs warm and then cold. The `serve` outage is two to three minutes, and the cost is about zero lost leases. |
| D3 | Client suspension | Retire it. The claim transaction serializes the claims of each client and recycles the client's own lapsed leases first. So a client cannot hold two live leases. |
| D4 | Claim isolation | `READ COMMITTED` with `FOR UPDATE SKIP LOCKED`. This is the one documented exception to the crate's `REPEATABLE READ` rule. |
| D5 | The job body | A `text` column on the queue row, a copy of `job_sources.body`. The store stays self-contained, and the copy from S3 is verbatim. |
| D6 | Matching in SQL | No. Eligibility stays a table that the Rust matcher writes. So the scheduler and the fleet reports cannot disagree. |
| D7 | Local development | PostgreSQL, not SQLite. The SQL crate is specific to PostgreSQL throughout. A script starts the CI image locally. |

## Tables

All tables are in `[database]`, in one migration, `job_queue`, with an up and a
down script. The migrations are in
[`crates/pipette-store-sql/migrations/`](../../crates/pipette-store-sql/migrations/).
Ids are `text`, as in every other table. Instants are `timestamptz`. The body
stays `text`, so the store can report a corrupt body rather than refuse it.

```sql
-- One row per queued job. `leased_by IS NULL` means available.
CREATE TABLE queue_jobs (
  job_id        text PRIMARY KEY,
  body          text NOT NULL,                 -- the job JSON that ingestion promoted
  expires_at    timestamptz,                   -- the deadline; NULL = never
  enqueued_at   timestamptz NOT NULL DEFAULT now(),
  leased_by     text,                          -- the client that holds the lease
  lease_expiry  timestamptz,
  revision      bigint NOT NULL,               -- CAS: every write of the row bumps it
  CHECK ((leased_by IS NULL) = (lease_expiry IS NULL))
);
CREATE INDEX queue_jobs_avail  ON queue_jobs (expires_at) WHERE leased_by IS NULL;
CREATE INDEX queue_jobs_leased ON queue_jobs (leased_by, lease_expiry) WHERE leased_by IS NOT NULL;

-- Staged bodies between write_tmp and promote_avail.
CREATE TABLE queue_tmp (job_id text PRIMARY KEY, body text NOT NULL, written_at timestamptz NOT NULL DEFAULT now());

-- The eligibility index that queue-maintenance writes and claim reads. The
-- expires_at column stays until the simplification stage drops it.
CREATE TABLE queue_eligibility (
  client_id text NOT NULL, job_id text NOT NULL, expires_at timestamptz,
  PRIMARY KEY (client_id, job_id)
);
CREATE INDEX queue_eligibility_job ON queue_eligibility (job_id);

-- One row per (job, client) retriable failure. Every field is nullable. A row
-- with every field NULL reads as DenialRecord::default.
CREATE TABLE queue_denials (
  job_id text NOT NULL, client_id text NOT NULL,
  denied_at timestamptz, failure_reason text, client_version text, runtime_version text,
  PRIMARY KEY (job_id, client_id)
);

-- One row per lease that ended without a result.
CREATE TABLE queue_lost_leases (
  job_id text NOT NULL, client_id text NOT NULL, lease_expiry timestamptz NOT NULL,
  PRIMARY KEY (job_id, client_id, lease_expiry)
);

-- One row per reindex request. The pass deletes only the rows it captured.
CREATE TABLE queue_reindex_requests (
  request_id text PRIMARY KEY, client_id text NOT NULL, requested_at timestamptz NOT NULL
);
CREATE INDEX queue_reindex_requests_client ON queue_reindex_requests (client_id);

CREATE TABLE queue_reindex_jobs (job_id text PRIMARY KEY);

CREATE TABLE queue_suspensions (
  client_id text PRIMARY KEY, suspended_at timestamptz NOT NULL, conflicting_job_id text NOT NULL
);

-- The last signed request of each device. SQL sets last_seen_at with now(),
-- so the replicas agree, as the S3 server clock does.
CREATE TABLE client_liveness (client_id text PRIMARY KEY, last_seen_at timestamptz NOT NULL);

-- Transitional: the five run-state blobs (.eligible-indexed, .eligible-workers,
-- .gc-candidates, .backlog, .backlog-campaigns) as text, keyed by name.
CREATE TABLE queue_maintenance_state (name text PRIMARY KEY, body text NOT NULL);
```

The simplification stages add these later:

```sql
ALTER TABLE queue_jobs ADD COLUMN needs_reindex boolean NOT NULL DEFAULT true;  -- replaces .eligible-indexed and queue_reindex_jobs
CREATE TABLE queue_workers (client_id text PRIMARY KEY);                         -- replaces .eligible-workers
ALTER TABLE queue_eligibility DROP COLUMN expires_at;                            -- a listing joins queue_jobs for the live deadline
ALTER TABLE queue_eligibility ADD FOREIGN KEY (job_id) REFERENCES queue_jobs ON DELETE CASCADE;
ALTER TABLE queue_denials     ADD FOREIGN KEY (job_id) REFERENCES queue_jobs ON DELETE CASCADE;
ALTER TABLE queue_lost_leases ADD FOREIGN KEY (job_id) REFERENCES queue_jobs ON DELETE CASCADE;
CREATE TABLE plan_index_gc_candidates (key text PRIMARY KEY, first_seen_at timestamptz NOT NULL); -- the `plans:` half of .gc-candidates
CREATE INDEX job_plans_plan ON job_plans (plan_id);                              -- plan to queued jobs
```

No foreign key goes from `job_plans` to `queue_jobs`. A membership marker
outlives the queue row by design, and `gc_plan_index` collects it later.

## What each keyspace becomes

| Object store | End state |
|---|---|
| `tmp/{job}.json`, then a rename to `avail/{job}/{exp}.json` | A `queue_tmp` row, then `WITH moved AS (DELETE FROM queue_tmp … RETURNING body) INSERT INTO queue_jobs … ON CONFLICT DO NOTHING` |
| `avail/{job}/{exp}.json` | A `queue_jobs` row with `leased_by IS NULL`. `expires_at` is a column. |
| `leased/{client}/{job}.{exp}.{lease_exp}.json` | The same row with `leased_by` and `lease_expiry` set |
| The claim rename | `UPDATE … SET leased_by, lease_expiry WHERE job_id AND leased_by IS NULL RETURNING body`. The `WHERE` also matches the same client and lease expiry, so a retried claim answers `Claimed`, as S3 does. |
| The heartbeat rename | `UPDATE … SET lease_expiry WHERE job_id AND leased_by = $client`. On zero rows, `SELECT leased_by` to answer `NotFound` or `WrongClient`. |
| The recycle rename | `UPDATE … SET leased_by = NULL, lease_expiry = NULL WHERE job_id AND leased_by AND lease_expiry`. On zero rows, `SELECT` that the job is available at the same `expires_at`, to answer `Recycled` after a lost recycle race, as S3 does. |
| The re-date rename | `UPDATE … SET expires_at WHERE job_id AND leased_by IS NULL AND expires_at = $from` |
| The terminal delete of `avail/` and `leased/` | `DELETE FROM queue_jobs WHERE job_id`. The cascade removes the markers. |
| `eligible/clients/{c}/{job}.{exp}` | `queue_eligibility` |
| `denied/{job}.{client}` | `queue_denials` |
| `lost/{job}/{client}.{lease_exp}` | `queue_lost_leases` |
| `pending-reindex/{client}.{uuid}` | `queue_reindex_requests`. `has_pending_reindex` is an `EXISTS`, not a listing. |
| `pending-reindex-jobs/{job}` | `queue_reindex_jobs`, then `queue_jobs.needs_reindex` |
| `suspended/{client}.json` | `queue_suspensions`, then retired (D3) |
| The `liveness/{client}` modification time | `client_liveness.last_seen_at = now()` |
| `.eligible-indexed`, `.eligible-workers` | `needs_reindex`, `queue_workers` |
| `.gc-candidates` | The cascade, for queue keys. `plan_index_gc_candidates`, for the `plans:` keys. |
| `.backlog`, `.backlog-campaigns` | A live `GROUP BY` over eligibility, jobs and denials, joined to `job_plans` and `plans` for the campaigns |
| The plan filter (`manifest.job_ids` against a listing) | `WHERE job_id = ANY($ids)`, or `JOIN job_plans USING (job_id) WHERE plan_id = $1` |

## Invariants the SQL store keeps

- **Second precision.** `Config::lease_expiry_from(now)` gives a sub-second
  instant, and the S3 key truncates it to the second. Every SQL statement binds
  a timestamp through one `to_second()` at the store boundary. So a claim and
  then a recycle with the same raw value find the same row.
  `revert_claim_if_void` and `put_back_out_of_vault_reach` depend on this.
- **Availability filters.** `get_avail*`, `delete_avail*`, `avail_key_for_job`
  and `redate_avail` filter on `leased_by IS NULL`. So the merge's
  `avail_state` never reads a leased job as queued, and `retire_job` never
  deletes a lease.
- **Byte-identical keys.** The store builds `{job_id}/{expires_at}.json` and
  `{client}/{job}.{expires_at}.{lease_expiry}.json` exactly as
  `pipette_core::todo_filename` does (`never`, the basic format, `.json`). So
  the `.eligible-indexed` and `.gc-candidates` strings that an S3 run wrote read
  correctly in a SQL run. `list_avail` builds the keys and sorts them in
  Rust. The Rust sort gives the S3 byte order. In that order, `-` sorts
  before the `/` that ends a job id.
- **The server clock for liveness.** Liveness uses `now()` in SQL, not a
  timestamp that the application binds.

## Operations in the end state

Every write is one transaction through the crate's `retry`, at
`REPEATABLE READ`, with one exception.

**Claim for a client** is one transaction at `READ COMMITTED` (D4). A new
`SqlDatabase::begin_read_committed` opens it:

1. `pg_advisory_xact_lock(hashtext($client_id))` serializes the claims of one
   client.
2. Recycle the client's own lapsed leases:
   `UPDATE … SET leased_by = NULL WHERE leased_by = $c AND lease_expiry < now()`.
   Insert their `queue_lost_leases` rows. This keeps the rule that a device that
   comes back after `time_window` claims again.
3. If one live lease remains, hand it back. This is the idempotent claim. It
   counts as a lost lease, and past the limit the claim is withheld, as the S3
   store does.
4. Else select the tier that expires soonest, then pick at `random()` within
   it. The candidates are the client's eligibility rows, joined to the jobs
   that are not leased and not expired, minus the client's denials, with
   `FOR UPDATE SKIP LOCKED LIMIT 1`. Lease the job.

The vault reach check stays outside the transaction. It reverts with a recycle
and a denial, as the S3 store does. After steps 1 to 3 a client cannot hold two
live leases, so the suspension mechanism has nothing to detect.

Why not `REPEATABLE READ` with a retry: N idle devices poll one tier of M jobs
with the same deadline. They conflict as N²/M, and the 8 retries can run out.
Why not a partial unique index on `leased_by`: a client can claim a new job
while its lapsed lease waits for a recycle. The index cannot express
`lease_expiry > now()`.

**Heartbeat and reclaim** are one `UPDATE … WHERE job_id AND leased_by = $client`.

**A terminal submission** is `DELETE FROM queue_jobs WHERE job_id`. The cascade
removes the markers.

**Teardown of a plan's job** is one transaction. It deletes the `job_plans`
marker and counts the remaining members. If none remain, it runs
`DELETE FROM queue_jobs WHERE job_id AND revision = $read`. The merge joiner
must also write the job row in its own transaction after it inserts its marker
(`UPDATE queue_jobs SET revision = revision + 1 WHERE job_id`). Then the two
transactions conflict under `REPEATABLE READ`, and one retries. Without that
write both commit, and the joiner holds a marker on a deleted job. Teardown
then has no need to restore a job for a late joiner.

**Eligibility** stays a materialized table that the Rust matcher
`job_matches_client` writes. `queue-maintenance` indexes the jobs
`WHERE needs_reindex`. A recycle or a re-date sets the flag. A later option:
ingestion inserts the eligibility rows in the same transaction as the job. This
removes the claim lag of one interval.

**The backlog** is a query that the console runs. No snapshot files exist.

## The cutover

The queue is in use, so the cutover is a short pause with a mirror copy (D2).

`pipette-mgmt queue import [--dry-run]` makes the SQL queue equal to the object
store. It reads the object-store queue from `[todo_storage]`. So it runs with
the config from before the switch to `backend = "database"`.

- It copies every `todo/` keyspace, `leased/` included, through the two
  `TodoStore` implementations
  ([`crates/pipette-store/src/lib.rs`](../../crates/pipette-store/src/lib.rs)).
- It follows the `CopyMode::Mirror` pattern of `db export`
  ([`crates/pipette-pipeline/src/record_copy.rs`](../../crates/pipette-pipeline/src/record_copy.rs)).
  It writes the rows that differ and deletes the rows that the object store
  does not hold.
- It is idempotent, so it runs twice. The **warm** pass runs with `serve` up,
  for the bulk. The **cold** pass runs with `serve` stopped, for the delta.
- It refuses to run while `avail/` holds a flat key.
- When a job holds two entries after a recycle race, it keeps one and logs the
  other. A lease wins over an `avail/` entry. Between two leases, the later
  lease expiry wins. Between two `avail/` entries, the later deadline wins.
- It reports one row per keyspace with the counts Read, Written, Deleted and
  Unchanged, and the elapsed time.

`queue export` is the same copy in the other direction. It is the rollback.

The warm pass takes under a minute at the production sizes above. The full
`eligible/` listing took 21 seconds from a laptop. The 9,109 body reads take
seconds inside the region. The rows go in one at a time through the store, 16
at once. The cold pass copies only the few claims and heartbeats since the warm
pass. A timed dry run against production is the gate before anyone schedules
the pause.

The runbook assumes a deployment that runs `serve` as 2 replicas. Each replica
reads its config from a parameter store when it starts.

1. Deploy the binary with the config unchanged. Run `db migrate`.
2. Run `queue import --dry-run`, then `queue import` (warm), while `serve`
   runs. Run the command as a one-off task of the deployment.
3. Disable the `queue-maintenance` schedule.
4. Scale `serve` to 0 replicas. Wait for the drain (20 s for requests, 10 s
   for the audit log).
5. Run `queue import` (cold, the delta). The config parameter still names the
   object-store queue, which the command reads.
6. Update the config parameter: `[todo_storage] backend = "database"`.
7. Scale `serve` back to 2 replicas. The replicas read the new config and pass
   `db check`.
8. Wait one heartbeat interval (half of `plan_lease_duration_secs`, 150 s at
   the default), so that every live device renews its lease. Then enable the
   schedule again.

The `serve` outage is steps 4 to 7: the drain, the delta copy, the parameter
update and one replica start. That is two to three minutes. The replica start
takes most of it, not the copy. The cost at the defaults:

- Connection errors for that window. The documented client policy retries
  them.
- About zero lost leases. A heartbeat renews a lapsed lease that has no recycle
  yet, and no recycle runs before step 8.
- Zero suspensions, because no mix of replicas exists.
- A `last_seen_at` that is stale by the length of the window.

Until step 8, the rollback is: stop `serve`, revert the config parameter, run
`queue export`, and start `serve`. `queue export` reads the object-store queue
from the reverted parameter.

## The alternative not taken: a live composite store

A `MigratingTodoStore { sql, s3 }` would avoid the pause. It reads through both
backends, sends new jobs to SQL, and moves S3 jobs over in a `queue-maintenance`
pass. An adversarial review found what it needs:

- A **two-step flip**. During a rolling restart, an old S3-only replica and a
  composite replica disagree about where a lease is. A healthy device that
  claims on each replica gets suspended. Step A runs the composite everywhere
  with new jobs still on S3. Step B sends new jobs to SQL.
- Every `delete_*` and partition clear must hit **both** stores. A
  `pending-reindex/` flag from before the cutover that nobody deletes keeps a
  client on `204` forever.
- The reads that count (`list_lost_leases`, `list_eligible_for_client`,
  `list_leased_for_client`, `list_liveness`) must take the **union of both
  stores, without duplicates**. A fallback on a miss gives wrong counts.
- The migrate pass has three constraints:
  - It must rename to `migrating/{job}/{expires_at}.json`, because the key
    carries the deadline.
  - It must check `submissions.has_record` before it inserts a leftover again
    after a crash.
  - It must run before the `.gc-candidates` read at the top of `run`.
- A recycled S3 lease must land in S3 `avail/` with one rename. The next run
  migrates it.

That is a second `TodoStore` of about 60 methods that route to both backends.
The team deletes it after a drain that takes days. The alternative is a pause of
two to three minutes that costs the fleet about nothing. The pause won.

## Stages

Each PR has one child ticket. The sequence table on PIP-822 maps each stage to
its ticket and its blockers.

1. **Design note** in the repo. **Test seams**: a shared `TodoStore` contract
   suite, the queue fixtures through the trait, and a local PostgreSQL script.
2. **`SqlTodoStore` on the existing trait**, so no caller changes. First the
   jobs, the leases and tmp, then the markers and the run state. **Config**:
   `[todo_storage] backend = "database"`.
3. **Cutover tooling**: `queue import` and `queue export`, the runbook, and the
   deployment config.
4. **Production cutover.**
5. **Simplification**, one PR each:
   - targeted reads
   - the claim transaction
   - the retirement of suspension
   - incremental eligibility
   - transactional teardown and the cascade
   - the live backlog
   - the retirement of the object-store backends and the S3 Express bucket

## Open

- Ingestion could insert the eligibility rows in the same transaction as the
  job. That removes the claim lag of one interval. It needs the fleet snapshot
  that ingestion already reads for the plan status.
- The testkit could start an embedded PostgreSQL when `TEST_DATABASE_URL` is
  unset. Then `cargo nextest run` needs no setup.
- The production `plan_lease_duration_secs` sets the wait in step 8 of the
  runbook. Read it from the config before the cutover.
