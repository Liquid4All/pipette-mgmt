# Design: records on PostgreSQL, the queue and the results on S3

The records that mgmt keeps (identities, organizations, tags, plans, campaigns,
vault records) live on PostgreSQL, not on the object store. Production uses a
managed PostgreSQL instance, and development and tests use any PostgreSQL.

The job queue, the submissions, the warehouse that pipette-duckdb queries, and
every blob stay in the object store. The store traits do not change, so nothing
above `pipette-store-init` knows which backend a record came from.
[storage.md §2](../storage.md#2-logical-layout-and-invariants) describes the
keyspaces that this design replaces. `Config::database` in
[`crates/pipette-core/src/config.rs`](../../crates/pipette-core/src/config.rs)
describes the `[database]` section; [cli.md](../cli.md) covers `db migrate`.

## The problem

The object store does the work of a database, and most of the storage contract
is the cost of that work. The contract has these costs:

- **No transactions.** A record and the index that points to it are two
  objects. Every multi-key write therefore needs a manual crash-safe order.
  Examples are write-before-delete steps, `plan-sources-pending/` markers, and
  two-phase garbage collection (GC) candidates. Other examples are indexes that
  can drift, so a repair rebuilds them (`tags-index/by-tag/`, the `-index`
  locks).
- **Locks and swaps are object semantics.** Every uniqueness rule depends on
  exclusive create and on the etag compare-and-swap (CAS). A backend must
  therefore prove at startup that it supports a conditional HTTP PUT request
  (PUT) ([storage.md §8](../storage.md#8-architectural-decisions)).
- **No queries.** Every filter reads every object. Examples are a status filter
  on plans, the question "which devices does this org own", and the question
  "which plans asked for this job". The `-index` keyspaces answer the few
  questions that matter without a scan. Each index is one more thing to keep
  consistent.
- **A listing costs one HTTP GET request for each record.** The command
  `queue-maintenance` reads every plan manifest each minute.

None of that is true of the queue or the results. The claim of the queue is an
atomic rename that S3 Express does well. The warehouse is append-only Parquet
that DuckDB scans in place. Those are object-store workloads, and they stay in
the object store.

## The split

| Stays on object storage | Moves to `[database]` |
|---|---|
| `todo/`, the job queue (`[todo_storage]`) | clients, their permissions, pre-auth keys, signature migrations |
| `submissions/`: incoming, score queue, processed archive, runtime output, unverified | users, email claims, user keys, permissions, pre-auth keys, awaiting-sign-in |
| `warehouse/results/` and `warehouse/eval_sample_results/` (Parquet, read by pipette-duckdb) | `public-key-index/` |
| `vault-resources/` and `vault-staging/` (blobs) | orgs, slug claims, memberships, ownerships, tags |
| `dataset-cache/` | `org-benchmarks/` |
| `benchmarks/` (deployed with the server) | plans, cancel markers, plan sources, content keys, job-plan markers, job sources, job-plan links |
| `locks/` (the mutate and score-eval leases) | campaigns, vault records, vault imports |
| | an index of the outcome of each submission |

Submissions stay because they are bodies and not records. The completions of an
eval submission reach megabytes in size. The state of a submission is the queue
that the scorer processes. The database keeps an index of the outcome of each
submission. `queue-maintenance` reads one row from the index and not the body.
The locks stay because they serialize the writes to
the warehouse and to submissions, and those writes stay. No record write takes a
lock.

**One section moves the whole set.** No per-store switch exists. The campaigns
must be in the same database as the plans that they name. The client store must
be in the same database as the key index that both principal kinds claim. The
catalog, vault and submission stores keep their bodies and blobs on the object
store and their records on SQL; a flow writes the object before the row that
names it.

## Schema choices

A plan, an account's type, a campaign's labels, a membership and an ownership
are columns, so that SQL can filter and join on them. Every other record keeps
the body that the object store holds, as `text` and not `jsonb`, so the store
contract can still report a corrupt record rather than refuse to store it.

The note and the revision change of a plan are columns next to its body. A
rewrite of the manifest does not change them. Only the SQL store keeps them.
Without `[database]`, a plan has neither.

The two `-index` keyspaces are tables whose primary key is the claimed value,
so a constraint enforces the uniqueness. Listings that page keep the object
stores' order, `{id}.json` in byte order, because the shared contract requires
it. That is also why the database must sort text in byte order
(`LC_COLLATE 'C'`), and why the deployment stack creates no database: the operator
creates `pipette_mgmt` with that collation by hand.

Migrations only add, so an older binary runs on a newer schema after a
rollback.

## Concurrency

The stores take no locks. A statement in SQL gives each guarantee that a store
gave on the object store:

| Guarantee | Object store | SQL |
|---|---|---|
| exclusive create (a claim, a spend, a write-once source) | `PutMode::Create` | `INSERT … ON CONFLICT DO NOTHING`. Zero rows means "already there" |
| CAS (content keys, a self-healed client) | etag / version | `… WHERE revision = $read`. Zero rows means a lost swap |
| read-modify-write (permissions) | read, CAS, retry | one transaction, the write conditioned on the read |
| idempotent delete | `DELETE` of a missing key | `DELETE`. Zero rows is acceptable |

Every write runs at `REPEATABLE READ`. A transaction that loses a race to a
transaction that committed first fails with the PostgreSQL error code (SQLSTATE)
`40001`. The function `retry` then runs the loser again from its first
statement. The loser sees the row of the winner. A racing claim then reads as
taken, and a lost swap reads as lost.

Nothing outside the database happens inside a retried transaction. The race test
of each store requires the outcome that the object store gives.

Flows that span both sides keep their order and their convergence. Plan
ingestion writes the source, the link and the membership marker before the job
reaches `todo/`. `clients delete` deletes the identity before it purges the
queue. A record write is one transaction (a client and its permission set go in
one transaction). No transaction spans the database and the bucket.

## Readers outside mgmt

pipette-duckdb joins the records to the results. With `[database]` set, the
record keyspaces stop changing in the bucket, so a mirror of them goes stale.
The decision is that duckdb attaches the database read-only
(`PIPETTE_RECORDS_SOURCE=database`, its `docs/configuration.md`), as a role that
reads only the record tables, rather than mgmt exporting a copy: no mirror, so
no outdated copy, and memberships, ownerships and plan membership join as
columns without a JSON parse.

## Open

- **Whole-manifest writes.** `put_plan` takes the whole manifest, so each
  status refresh reads and compares every `plan_jobs` row of the plan. It also
  rewrites the `progress_snapshot` in the body. A store method that writes only
  the status and the snapshot would remove that work.
- **`[auth_storage]` is required.** `serve` probes it for conditional PUT,
  although no record lives there if `[database]` is set.
- **Listings sort on an expression** (`{id}.json`, the object-store order that
  the shared contract requires), so each page sorts the keyspace again. This
  cost is acceptable at thousands of rows. For more rows, a stored sort-key
  column under a unique constraint would remove the sort.
