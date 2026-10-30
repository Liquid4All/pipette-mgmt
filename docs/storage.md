# Storage

## 1. Overview

This document is the storage contract of `pipette-mgmt`: the object-store
layout, the shape of each stored record, and the rules that keep stored data
readable across releases. It is storage-only. Routes, request rules and
response fields are in [httpapi.md](httpapi.md).

The object-store backends are the local filesystem and S3 (including
S3-compatible stores such as MinIO and R2).

The storage design keeps these properties:

- one logical data model regardless of backend
- immutable submission payloads after write
- submission state represented by location, not by a mutable field in the JSON
- scored output treated as canonical (the warehouse for aggregate metrics, eval
  sample results for per-sample eval outcomes)
- separate storage domains for separate access patterns

### 1.1. Where records live

With a `[database]` section, which production runs, the records are PostgreSQL
tables: plans and their indexes, campaigns, identities (clients, users, keys,
permissions, pre-auth keys), organizations with their memberships, ownerships
and tags, private benchmarks, model params, and vault records. The schema is
`crates/pipette-store-sql/migrations/`, and
[design/sql-record-store.md](design/sql-record-store.md) is the design. The
record shapes and compatibility rules in §3 hold on either backend; a uniqueness
rule that an exclusive create enforces on the object store is a key constraint
on SQL.

The audit log is the table `audit_events`. The table `audit_event_orgs` holds
the organizations whose log lists each event. The table `audit_event_records`
holds the records that each event names, with the account or device that
acted.

The descriptor catalog uses `descriptor_entries` and `descriptor_members`.
Only `[database]` keeps these tables. `db import` and `db export` do not copy
the catalog.

Only `[database]` keeps the audit log. No object store keeps it, and
`db import` and `db export` do not copy it. The server and the `pipette-mgmt`
commands only append events and never edit them. `audit prune` deletes the
events that are older than the two-year retention.

The `local_fs` and S3 record stores are retiring (PIP-759) and kept for tests.
Their keyspaces are the second tree in §2. A new record feature lands on SQL
(and on `local_fs` where tests need it), never on the S3 record store.

The object store keeps, in every deployment:

- the job queue, `todo/`, on `[todo_storage]`
- submissions, under `submissions/`
- the warehouse, under `warehouse/`
- vault blobs, under `vault-resources/` and `vault-staging/`
- the dataset cache, under `dataset-cache/`
- the public benchmark catalog, `benchmarks/`, and the batch locks, `locks/`

## 2. Logical layout and invariants

The logical layout is the storage contract. On a filesystem backend it maps to
directories and files; on S3 each path is a key.

**Reading a prefix name.** A name ending `-index` is *derived*: it can be
rebuilt from the records it points at. Two of the indexes are also **locks**
(`user-email-index/` and `public-key-index/` are claimed by exclusive create),
so while one is missing, the uniqueness it enforces is not enforced. A name says
what a key *is*: `public-key-index/{digest}` holds a principal id, not a key.

**Why content digests include a version.** `content-keys/v1/` holds both v1 and
v2 digests; the directory name stays for compatibility. Each digest hashes its
algorithm's version token, because a stale content key could produce a **false
match**: a digest written under an old preimage says "this content already
exists", and ingestion merges two jobs the new rule keeps apart. A stale entry
in a lock can only produce a miss, a *false free*. A version segment does not
fix a false free: an empty `v2/` namespace passes every uniqueness check until
it is backfilled. A derivation change on a lock is handled by **rebuilding it
from the records**: the address is on `users/{user_id}.json`, and the keys are
on `clients/` and under `user-keys/`.

### Object store

```text
data/
├── benchmarks/
│   └── {benchmark_id}.toml              ← the public catalog, read at startup and cached
├── locks/
│   ├── mutate.lock                      ← process-submissions / fix-* / requeue-eval
│   └── score-eval.lock                  ← serializes score-eval runs only
├── submissions/
│   ├── incoming/
│   │   └── {job_id}.json
│   ├── score-queue/                     ← eval scoring pipeline (per-job JSON)
│   │   ├── to_do/                       ←   awaiting the scoring-service call
│   │   │   └── {job_id}.json
│   │   ├── to_finalize/                 ←   scored; { submission, score } awaiting the warehouse write
│   │   │   └── {job_id}.json
│   │   ├── failures/                    ←   { attempts, error } of a queued job's failed passes
│   │   │   └── {job_id}.json
│   │   └── parked/                      ←   a queued job that failed every pass; never retried
│   │       └── {job_id}.json
│   ├── processed/
│   │   └── {job_id}.json.gz
│   ├── runtime-output/                  ← what the runtime printed, apart from the body (§4.2)
│   │   └── {job_id}/
│   │       └── {client_id}.json.gz
│   └── unverified/
│       └── {client_id}/
│           └── {job_id}.json
├── todo/                                ← [todo_storage]
│   ├── .eligible-indexed                ← avail/ keys queue-maintenance has indexed
│   ├── .eligible-workers                ← client ids queue-maintenance indexed as workers
│   ├── .gc-candidates                   ← keys the last run saw orphaned
│   ├── .backlog                         ← claimable-job count of each device, and the time of the count
│   ├── .backlog-campaigns               ← the same count, by campaign
│   ├── tmp/                             ← job files are staged here first
│   ├── avail/
│   │   └── {job_id}/
│   │       └── {expires_at}.json        ← atomic rename from tmp/; expires_at is ISO 8601 or `never`
│   ├── eligible/
│   │   └── clients/
│   │       └── {client_id}/
│   │           └── {job_id}.{expires_at}   ← marker: client may claim this job
│   ├── leased/
│   │   └── {client_id}/
│   │       └── {job_id}.{expires_at}.{lease_expiry}.json  ← renamed from avail/ on claim
│   ├── denied/
│   │   └── {job_id}.{client_id}         ← marker: client failed this job; its reason as JSON, or empty
│   ├── pending-reindex/
│   │   └── {client_id}.{uuid}           ← one marker per client reindex request
│   ├── pending-reindex-jobs/
│   │   └── {job_id}                     ← empty marker: this job's eligible/ markers are owed a rewrite
│   ├── lost/
│   │   └── {job_id}/
│   │       └── {client_id}.{lease_expiry}  ← empty marker: one lease on this job ended without a result
│   ├── suspended/
│   │   └── {client_id}.json             ← client suspended for operator review
│   └── liveness/
│       └── {client_id}                  ← empty marker: its modification time is the client's last-seen time
├── warehouse/
│   ├── results/
│   │   └── benchmark_id={benchmark_id}/
│   │       └── client_id={client_id}/
│   │           ├── day={YYYY-MM-DD}/    ← new writes
│   │           └── month={YYYY-MM}/     ← legacy, frozen
│   │               └── part-*.parquet
│   └── eval_sample_results/
│       └── {job_id}.parquet
├── vault-resources/
│   └── {principal_id}/blobs/{sha256}    ← one blob per owner and digest (§3.4)
├── vault-staging/
│   └── {principal_id}/{uuid}            ← a blob while mgmt computes its digest; S3 only
└── dataset-cache/
    └── artifacts/                       ← repacked corpora (§5)
```

### Record keyspaces (without `[database]`)

On the retiring record stores (§1.1), the records are these keyspaces. With
`[database]`, each has a table named after it, underscored, holding the same
fields. The exceptions: `tags-index/` is one `client_tags` table, the `.spent`
markers are `*_spent` tables, and a plan's job ids are in `plan_jobs`.

```text
[auth_storage]
├── clients/
│   └── {client_id}.json                 ← client identity record (one device, one key)
├── signature-migration/
│   └── {client_id}.json                 ← written when a client first signs a v1 payload
├── client-permissions/
│   └── {client_id}.json                 ← the set that client holds
├── client-preauth/
│   ├── {key_id}.json                    ← client pre-auth key (stores sha256(secret))
│   └── {key_id}.spent                   ← the single-use spend lock, created exclusively
├── users/
│   └── {user_id}.json                   ← user account
├── user-email-index/
│   └── {email}                          ← body is the bare user_id. Claimed by exclusive create
├── user-keys/
│   └── {user_id}/
│       └── {key_id}.json                ← one registration per machine
├── public-key-index/
│   └── {digest}                         ← body is the bare principal id. Spans both kinds (§3.1.3)
├── user-permissions/
│   └── {user_id}.json                   ← the set that user holds
├── user-preauth/
│   ├── {key_id}.json                    ← user pre-auth key: the account it admits
│   └── {key_id}.spent                   ← same single-use lock as client-preauth/
├── user-awaiting-sign-in/
│   └── {user_id}                        ← empty marker: nobody has signed in as this account yet
└── user-first-clerk-sign-in/
    └── {user_id}.json                   ← the first Clerk sign-in and the keys held then. Exclusive create

[storage]
├── org-benchmarks/
│   └── {org_id}/
│       └── {benchmark_id}.toml          ← an organization's private benchmarks (§3)
├── orgs/
│   └── {org_id}.json                    ← organization record (§3.2)
├── org-slug-index/
│   └── {slug}                           ← body is the bare org_id. Claimed by exclusive create
├── user-orgs/
│   └── {user_id}.json                   ← { user_id, org_id }. No record for an account in none
├── client-owners/
│   └── {client_id}.json                 ← { client_id, owner }. No record for an unowned device
├── tags-index/
│   ├── by-client/
│   │   └── {client_id}/
│   │       └── {tag}                    ← empty marker (client → tags)
│   └── by-tag/
│       └── {tag}/
│           └── {client_id}              ← empty marker (tag → clients)
├── plans/
│   └── {plan_id}.json                   ← plan manifest; outlives its jobs
├── cancelled_plans/
│   └── {plan_id}                        ← empty marker: cancel requested, teardown pending
├── plan-sources/
│   └── {plan_id}.toml                   ← the plan document a plan was filed from. Write-once
├── plan-sources-pending/
│   └── {plan_id}                        ← empty marker: an ingest is writing this plan's source and manifest
├── campaigns/
│   └── {campaign_id}.json               ← name, owner, current_plan_id, labels, state (§3.3)
├── model-params/
│   └── {key}.json                       ← the model parameter table, one row per model (§3.5)
├── vault/
│   └── {resource_id}.json               ← vault record. Written once, never edited (§3.4)
├── vault-imports/
│   └── {principal_id}/{sha256}.json     ← the records one import wrote, or one source's archive record
├── content-keys/
│   └── v1/
│       └── {digest}                     ← JSON { job_id, plan_id, created_at }; claimed by exclusive create
├── job-plans/
│   └── {job_id}/
│       └── {plan_id}.{expires_at}       ← empty marker: this plan wants this job, until this deadline
├── job-sources/
│   └── {job_id}.json                    ← the body the job ran. Write-once
└── job-plan-links/
    └── {job_id}/
        └── {plan_id}                    ← empty link: this plan asked for this job. Write-once
```

### Object-store contract

- Benchmark definitions are keyed by `benchmark_id`.
- Submission records are keyed by `(state, job_id)`. `incoming` and `processed`
  are flat keyspaces over `job_id`. Every body enters at `incoming/`, success
  and failure alike, and reaches `processed/` through the scorer. `unverified`
  is keyed by `(client_id, job_id)`, so a client's held submissions can be
  promoted or deleted as a unit. A plan-attached submission's `job_id` is the
  `job-{UUIDv7}` the planner assigned; an ad-hoc submission's is minted by the
  server. A client submission overwrites `submissions/incoming/{job_id}.json`,
  so a resubmission under the same lease supersedes the earlier body. Only the
  server's synthetic failures write that key exclusively, leaving a body already
  there alone.
- Eval sample result records are keyed by `job_id`.
- Warehouse records are partitioned by `(benchmark_id, client_id, day)` for new
  writes. Legacy `(benchmark_id, client_id, month)` partitions are frozen and
  read-only; reads union both.
- `locks/mutate.lock` serializes the read-modify-write batch commands
  (`process-submissions`, `fix-*`, `requeue-eval`). `locks/score-eval.lock`
  serializes `score-eval` runs only, so two long scoring runs cannot overlap,
  without contending with the mutate lock. Both are transient coordination
  state; see [cli.md](cli.md#concurrency).
- `todo/` is the job queue. `avail/` holds unclaimed jobs, one directory per
  job: `{job_id}/{expires_at}.json`, where `job_id` is `job-{UUIDv7}` (so keys
  list in creation order). The job directory makes `avail/{job_id}/` a prefix a
  directory bucket accepts, so a read that holds the `job_id` alone costs one
  listing ([§8](#todo-requires-s3-express-one-zone)). `leased/` encodes the
  client and the lease expiry in the key. `denied/` holds one marker per client
  failure. `eligible/clients/` holds the eligibility markers the claim path
  reads, maintained solely by `queue-maintenance`. `pending-reindex/` holds one
  marker per reindex request, written by `serve` when a client's profile changes
  and consumed by `queue-maintenance`; the `uuid` keeps a request that arrives
  during a rebuild from collapsing into the one being served. See
  [queue.md](queue.md).
- `todo/pending-reindex-jobs/{job_id}` is the job-scoped counterpart: this job's
  `eligible/` markers are owed a rewrite for **every** client. A job leased
  while a client reindex ran, and a job the expiry pass re-dated, raise it. The
  key is the bare `job_id`, so raising the same debt twice is one marker. The
  settle pass re-matches the job once it is back in `avail/`, then deletes the
  marker.
- `todo/lost/{job_id}/{client_id}.{lease_expiry}` records one lease that ended
  without a result: it lapsed, or its holder claimed again while still holding
  it. The markers are the job's loss count, which `queue-maintenance` compares
  against `plan_lost_lease_limit`
  ([queue.md](queue.md#heartbeat-timeout-dead-device)). The key names the
  lease, so recording one loss twice writes one marker. The markers go with the
  job, through the orphan sweep.
- These files hold `queue-maintenance` run state, not queue records:
  - `todo/.eligible-indexed`
  - `todo/.eligible-workers`
  - `todo/.gc-candidates`
  - `todo/.backlog`
  - `todo/.backlog-campaigns`

  `queue-maintenance` rewrites each file whole. The files sit at the root so
  no marker listing returns one. Missing **and**
  corrupt read as empty, the safe direction for each: an empty
  `.eligible-indexed` re-indexes everything (indexing is idempotent), an empty
  `.eligible-workers` rebuilds every worker's markers once, and an empty
  `.gc-candidates` delays a delete by one run. The sweep deletes a key only on a
  second consecutive sighting, so a listing that raced a write cannot delete
  live state. `.gc-candidates` also carries the plan-index orphans of the record
  store (`content-keys/v1/`, `job-plans/`, `plan-sources-pending/`) under a
  `plans:` key prefix, so they cannot collide with a `todo/` key. `.backlog`
  holds each device's claimable-job count and the time of the count; the device
  listings read it ([queue.md](queue.md#device-backlog)). `.backlog-campaigns`
  holds the count of each device by campaign. Only the device page reads it.
  Each device page view reads the whole object, so the cost of a view grows
  with the number of devices times the campaigns that hold a job for each
  device. If that product becomes large, store one object for each device.
- `todo/suspended/{client_id}.json` flags a client that held more than one
  live lease, as checked by claim, heartbeat and reclaim: a sign of a crash
  loop or of racing claims. A suspended client gets no job, and its heartbeats
  and reclaims are refused, so its leases lapse, until an operator clears the
  flag (`clients unsuspend`). The record holds `suspended_at` and the
  `conflicting_job_id`.
- `todo/liveness/{client_id}` is an empty marker that each verified, signed
  device request stamps, at most once a minute per `serve` process. Its
  modification time is the device's last-seen time, so one listing of
  `todo/liveness/` reads every device's time and no body
  ([queue.md](queue.md#device-liveness)).

### Record contract

These rules hold for the record keyspaces above and for their tables.

- Client records are keyed by `client_id`, user accounts by `user_id`, in a
  namespace of their own. A user is the operator principal: it has no device
  profile, and a fleet pass over `clients/` never reaches one.
- An account's **address is unique**, and a **keypair belongs to one
  principal** of either kind. Both are exclusive creates
  (`user-email-index/{email}`, `public-key-index/{digest}`), so of two writers
  exactly one wins (§3.1).
- Tags are **not** on the client record. Each `(client, tag)` membership is an
  empty leaf marker in two mirrored trees, so both lookup directions are one
  listing and the filenames are the data: `tags-index/by-client/{client_id}/{tag}`
  (a client's tags) and `tags-index/by-tag/{tag}/{client_id}` (a tag's
  clients). A tag is flat and a `client_id` has no `/`, so every key is exactly
  two segments. The trees sit outside `clients/`, so listing client records
  never enumerates a marker. They are in `[storage]` rather than
  `[auth_storage]` because a marker holds a client id and a tag and nothing
  else: a reader of the data bucket can group the fleet without the auth bucket,
  which holds every address and key. The **forward tree is authoritative** and
  the reverse is a derived accelerator. A mutation commits the forward marker
  first, so a crash between the two writes can only leave the reverse stale.
  The operations are idempotent, so re-running a tag change converges, and
  `pipette-mgmt reindex` reconciles the reverse tree to the forward one and
  drops markers of deleted clients (`clients/{id}.json` is the existence
  authority). Rejecting or deleting a client clears both trees, best-effort once
  its record is gone, and a registration under the same id clears what that
  missed, since the id derives from the key. A user has no tags (§3.1.4).
- Plan manifests are keyed by `plan_id` and live with the records, **not** in
  the ephemeral `todo/` queue, so a completed or cancelled plan stays queryable
  after its jobs leave the queue. The manifest's `job_ids` is the only plan to
  job record (job bodies carry no `plan_id`). Ingestion writes it first
  (`creating`, then `active` or `pending_clients`); from then on
  `queue-maintenance` owns it, reconciling `status` and refreshing the optional
  `progress_snapshot`. The two writers never overlap, so the store needs no
  compare-and-swap. A write replaces the whole record, so a writer drops every
  field its build does not know: deploy every manifest writer at a build that
  knows each field. `settled_jobs` is exempt: it caches record outcomes, and a
  writer that drops it only makes the next run read those records again. A
  listing reports a manifest it cannot parse rather than skipping it, since
  skipping makes a plan read as absent. See
  [plan-ingestion.md §9](plan-ingestion.md#9-plan-lifecycle).
- `cancelled_plans/{plan_id}` records that an operator asked to cancel a plan:
  a *request* for teardown. `queue-maintenance` is the sole writer of the
  manifest's `cancelling` and `cancelled` status and of the `todo/` deletes, so
  signalling out of band keeps a cancel from being lost to a concurrent status
  refresh. The marker is a sibling of `plans/`, so a manifest listing never
  filters it out. It is deleted once the manifest reads `cancelling` or
  `cancelled`. A marker is written only for a plan that has a manifest, and
  writing one twice is a no-op.
- `plan-sources/{plan_id}.toml` is the plan document a plan was filed from,
  byte for byte. A document whose models carry an `auth_token` is stored with
  each imported model as its vault arm, under a header comment, and holds no
  token ([plan-ingestion.md §6.2](plan-ingestion.md#62-ingestion-time)). The
  manifest's `source_sha256` hashes the stored bytes; for a document stored with
  vault arms, `submitted_sha256` hashes the text the client sent, which is not
  stored. The source is write-once: a second put leaves the first unchanged, and
  an existing source under a new `plan_id` fails the ingest. Ingestion writes
  the source before the `creating` manifest, and nothing deletes a source a
  manifest names. A plan filed as job bodies has no source. The manifest's
  `plan_warnings` is written with the `creating` manifest and never changed; it
  is absent when empty, and a manifest from an older build reads as empty.
  The manifest's `benchmark_types` maps each benchmark id the plan's jobs name
  to the type the filer's catalog gave it. It is written with the `creating`
  manifest and never changed, and every later parse of the source types its
  cells by it. A manifest from an older build has none, and its plan reads with
  each id typed by its prefix, as it was filed.
- `plan-sources-pending/{plan_id}` is written before the source and deleted
  after the `creating` manifest, so a marker that remains names a source that
  may have no manifest. `queue-maintenance` collects a marker older than
  `plan_creating_max_age_secs` and seen on two runs: with no manifest it deletes
  the source and then the marker, with one it deletes only the marker.
- `content-keys/v1/{digest}` is the duplicate-job index. The digest is a sha256
  over the submitted job body, excluding server-assigned identity, the deadline
  and the merge control; eligibility clauses normalize to sorted sets. The
  record names the job that owns that content. Ingestion claims a digest with a
  **create-if-absent** write before a client can claim the job, so two
  concurrent ingests of one cell produce one job. Every later write is a
  **compare-and-swap on the version read**: an unconditional overwrite would
  destroy the record of a rival that claimed the digest in between, and orphan
  its live job. `queue-maintenance` deletes a record once no live job answers to
  it. A job with `merge_duplicates: false` still claims its record: the flag
  stops that job from *reading* the index, and a later plan still joins it. See
  [plan-ingestion.md](plan-ingestion.md#the-duplicate-job-merge).
- `job-plans/{job_id}/{plan_id}.{expires_at}` records that a plan wants a job,
  until a horizon: the last moment it still wants an answer. A plan may hold
  several markers on one job, and its horizon is the latest. A plan writes a
  marker for **every** job, so an absent marker means the plan released the job
  or its horizon lapsed (the manifest's `expired_jobs` tells them apart).
  Teardown retires a cancelled plan's job only when its last membership goes,
  and `queue-maintenance` reads the tree to find lapsed plans and to re-date a
  job whose own deadline passed. `plan_id` excludes `.` by charset and
  `expires_at` by encoding, so the leaf's single `.` splits unambiguously.
  `queue-maintenance` deletes a marker whose horizon lapsed, whose job left the
  queue, or whose plan's manifest is gone.
- `job-sources/{job_id}.json` is the body ingestion promotes into `todo/`, with
  `job_id` and `expires_at` stamped. A body does not change after promotion, so
  this is the body the job ran, and it outlives the job's time in `todo/`:
  - It is write-once. The first plan that asks for the job writes it.
  - A job minted before job sources existed has none until a plan joins it; the
    joiner writes its own stamped body, which has the queued body's content key
    but can differ in field order and in the fields the key leaves out.
  - The stored `expires_at` keeps the value from ingestion; the `avail/` key is
    the authority after the expiry pass re-dates a shared job.
  - It holds no `auth_token`. A source written before the vault import step can
    hold one, which stays until an operator deletes the source by hand, so a
    reader drops every token before it serves a job source.
  - A source that is not JSON fails every read of it until an operator repairs
    or deletes it.
  - Nothing deletes a job source or a link. Ingestion writes the source before
    the link, so every link names a stored source.
- `job-plan-links/{job_id}/{plan_id}` records that a plan asked for a job,
  whatever the plan's status and whether or not it still holds the job, so a
  listing of `job-plan-links/{job_id}/` names every such plan. It is write-once.
  The `job-plans/` markers cannot be this index: teardown and the expiry pass
  delete them. See [plan-ingestion.md §8](plan-ingestion.md#8-ingestion-flow-atomic-ish).

### Invariants

- benchmark definitions are loaded from storage at startup
- submission JSON payloads are immutable once written
- submission processing state is the logical location:
  - `submissions/incoming/...`
  - `submissions/score-queue/to_do/...` (an eval routed by the fast
    `process-submissions` pass, awaiting the slow `score-eval` call)
  - `submissions/score-queue/to_finalize/...` (`{ submission, score }` written
    by `score-eval`, awaiting the fast pass's warehouse write)
  - `submissions/score-queue/parked/...` (a submission that failed 12
    scoring or finalize passes; reads as `scoring`, never retried)
  - `submissions/processed/...` (terminal)
  - `submissions/unverified/...` (held; never scored until an operator promotes
    it)
- a processed submission holds the payload accepted at submission time, less
  the runtime output stored beside it
- the warehouse is the durable store for aggregate metrics, and eval sample
  results for per-sample outcomes; neither is the source of truth for the
  original payload

**Transient artifacts the tree does not list.** Every record above is written by
staging a complete body and then moving it into place, so a reader never sees a
half-written one. The staging names are `.tmp-{uuid}` (some with a further
`.tmp` or `.parquet` suffix) on the local-fs backend and for the auth store's
`*-index/` claims, and `{principal_id}.{uuid}.staged` for the auth store's
records. They are siblings of the record being written. None is a record: every
keyspace selects records by a rule a staging name fails, so a stranded one is
inert and safe to delete. Nothing sweeps them, unlike `todo/tmp/`, which has a
stale-file reaper ([operations.md](operations.md)).

## 3. Metadata contract

Metadata covers benchmark definitions and the records of §1.1.

Benchmark logical key:

`benchmarks/{benchmark_id}.toml`

Each benchmark definition is a single TOML document. The `benchmark_id` is the
filename without `.toml`.

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

An organization's private benchmarks are a second keyspace beside it
([benchmarks.md §1.1](benchmarks.md#11-private-benchmarks)), a record with
`[database]`:

`org-benchmarks/{org_id}/{benchmark_id}.toml`

```toml
# org-benchmarks/org_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e/eval_acme_internal_v3.toml
benchmark_type = "eval"
parameter_eval_id = "acme_internal"
parameter_dataset_name = "v3"
parameter_max_tokens = 4096
```

The document is exactly a public entry's: the path carries the id and the
organization, and the body carries neither. **A sibling prefix, not a
subdirectory of `benchmarks/`**, because the public loader lists its prefix
recursively on S3 and would otherwise serve every organization's entries to
everyone. Keyed by the minted id rather than the handle, so an entry cannot
follow a handle that is released and claimed by another organization.

The public catalog is read once at startup and cached for `catalog_ttl_secs`;
an organization's entries are read **on each read** that reaches them, so a
write is served on the next read and nothing has to be invalidated. An
organization with no entries has an empty catalog, which is not the
misconfiguration an empty `benchmarks/` is. An unreadable entry is logged and
skipped, yet it still holds its id: it refuses its organization's delete
(§3.2.5), and a delete finds it by key rather than by a parse. On the filesystem
an entry is replaced atomically, so a read never sees a half-written one.

The server's own passes (scoring, the dataset cache, requeueing, campaign
administration) read every organization's entries at once. An entry whose body
does not parse is skipped, and any other failed read fails the whole read.
**An id is kept under at most one organization.** The writer enforces it, since
the keyspace cannot; a pair planted behind its back resolves to neither entry
server-side.

Client logical key, a JSON object with these fields:

`clients/{client_id}.json`

| Field | Type | Description |
|-------|------|-------------|
| `client_id` | string | Derived from public key (see [authentication.md §1](authentication.md#1-identity-model)) |
| `public_key` | string | Hex-encoded Ed25519 public key |
| `organization` | string | Free-text label the device sent about itself, not ownership (§3.2.4). Defaults to `"unknown"` for a record that predates the field. |
| `client_details` | string | Freeform description |
| `contact_email` | string | Contact email for admin approval |
| `status` | string | `pending` or `approved` |
| `registered_at` | string | ISO 8601 registration time |
| `device_name` | string or null | Device model / marketing name; null if not set |
| `device_form_factor` | string or null | One of `phone`, `tablet`, `laptop`, `desktop`, `server`, `embedded`; null if not set |
| `device_os_name` | string or null | OS name; null if not set |
| `device_os_version` | string or null | OS version string; null if not set |
| `device_cpu_arch` | string or null | Raw CPU architecture; null if not set |
| `device_os_family` | string or null | Raw OS family; null if not set |
| `device_chip_model` | string or null | Chip / SoC model; null if not set |
| `device_ram_bytes` | int or null | System RAM in bytes; null if not set |
| `device_gpu_model` | string or null | GPU model; null if not set |
| `device_gpu_vram_bytes` | int or null | GPU VRAM in bytes; null if not set |
| `device_npu_model` | string or null | NPU model; null if not set |
| `device_npu_vram_bytes` | int or null | NPU VRAM in bytes; null if not set |
| `capabilities` | string[] | Free-form capability flags the client reports (e.g. `runtime:llama_cpp`); omitted when empty, and absent on legacy records. Unioned with the flags the server derives from `device_*` for job matching ([plan-ingestion.md §5](plan-ingestion.md#5-capability-matching)). |

A client's **tags** are not part of this record (§2, "Record contract").

Its **signature migration** is not part of this record either. The marker at
`signature-migration/{client_id}.json` holds the time that client first signed a
`v1` payload:

```json
{ "first_seen": "2026-03-08T10:04:29Z" }
```

Its presence refuses that client the timestamp-only fallback
([authentication.md §2.3](authentication.md#23-timestamp-only-signatures)). It
sits outside the client record for two reasons: the migration ends, and once it
does this keyspace is deleted, where a record field would outlive it; and
writing it never touches the client record, so it cannot clobber a concurrent
status change (client records are written whole, with no compare-and-swap).
Nothing deletes a marker. One that outlives its client is harmless:
re-registering the same key inherits it, which *denies* the fallback, the
strict direction.

Its **permissions** are not part of this record either.
`client-permissions/{client_id}.json` holds the set an operator granted to that
principal:

```json
{ "permissions": ["perform_work", "submit_results"] }
```

A user's set lives at `user-permissions/{user_id}.json` (§3.1.5). An **absent
object** and an object holding `[]` are two states, kept apart. An absent object
is a record nobody has granted, and it grants nothing; no default fills it in,
and an operator grants such a record by hand. `[]` is a set an operator cleared.
The two grant alike and are stored apart so an operator can see which records
still need granting, which is why the set is one object rather than a marker per
permission ([authentication.md §5.8](authentication.md#58-storage)).

`queue-maintenance` and plan ingestion read the client sets **whole**, once per
pass, and join them against the client roster in memory. An entry a backend
cannot read is left out of that join, so the client reads as holding nothing:
the direction that leaves a plan waiting rather than matched to a machine that
is refused.

The set sits outside the client record because that record has two writers of
opposite trust: a device rewrites its own record when its profile changes, and
an operator writes it through the CLI, both whole and without compare-and-swap.
A device that read its record just before a revocation and wrote just after
would restore the permission. A separate object closes the race, because the
device never writes it. A write replaces the set atomically, and the CLI's
read-modify-write on S3 is conditioned on the object's version, so an operator
whose write loses a race learns of it.

The server deletes the set with its client. A re-registration does not inherit
the old set: registration writes the set before the record, replacing whatever
is there.

**Pre-auth keys** admit a *client*, and only a client; a user is admitted from
`user-preauth/` (§3.1.5). One record per key, at
`client-preauth/{key_id}.json`. Each stores `sha256(secret)` (never the
secret), `usage` (`single_use` / `multi_use`), `created_at`, optional
`expires_at`, and the tags, organization label and owner (§3.2.4) to seed onto a
registering client. The owner is omitted when the key names none. `minted_by` is
the account that minted the key through the API, omitted for a key the host
minted. A valid key always approves the client, so there is no per-key approve
flag. The record is **write-once**: a single-use key is deleted as it is spent,
`revoke` deletes on demand, and `clients preauth prune` deletes expired keys. A
multi-use key is read-only on consume.

Spending a single-use key first creates a sibling marker,
`client-preauth/{key_id}.spent`, holding the spend time, then deletes the
record. The create is exclusive (`If-None-Match: *` on S3, `O_EXCL` on a
filesystem), so it, not the delete, makes exactly one winner of concurrent
registrations, across replicas too. A marker therefore outlives the record it
retired, and only a marker with no consumable record left may be removed.
`clients preauth prune` sweeps the record-less markers, and the markers of the
expired records it deletes. Listings read only `*.json`, so markers are
invisible to them. The marker is scoped to its prefix, which is why a key is
never *copied* between keyspaces: the same key under two prefixes could be spent
once under each. See [authentication.md §3.2](authentication.md#32-pre-auth-keys).

An S3 `[auth_storage]` therefore has to honor conditional `PUT`
([§8](#storage-and-auth_storage-require-conditional-put)).

### 3.1. The user principal

A **user** is the operator principal. It authors work where a client does it.
It gets its own records rather than fields on the client record, because nothing
about it is device-shaped and a fleet pass that lists `clients/` must not reach
one.

**Records are split by kind; attributes are shared only when the kinds share the
value.** A *record* (`users/`, `clients/`) is separated: the two hold different
shapes, and listing `clients/` is the fleet the matcher narrows. The keypair
claim is shared, because one keypair may belong to one principal across both
kinds, so a shared keyspace is the lock (§3.1.3). An attribute whose value
differs by kind is separated, even where the object has the same shape:
permissions (the vocabulary is partitioned,
[authentication.md §5.2](authentication.md#52-the-vocabulary)) and pre-auth
keys. A reader therefore always knows which kind it deserializes, and a delete
of one kind cannot reach the other's object.

#### 3.1.1. The account: `users/{user_id}.json`

```json
{
  "user_id": "user_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e",
  "contact_email": "operator@example.com",
  "account_type": "human",
  "status": "active",
  "created_at": "2026-09-04T12:00:00Z"
}
```

Five fields, none optional; a record written before `account_type` reads as
`human`. A campaign reader carries a sixth, `campaign_scope`, the ids of the
campaigns it reads ([Users](api/users.md#the-rules-that-make-delegation-safe));
every other account has none. On SQL the scope is the `campaign_scope` column
and not the body, as the type is `account_type`, so a rolled-back binary that
rewrites the body does not erase it; on the object stores it does. Either way
an older binary ignores the scope while it runs and serves a reader as a member,
so disable readers before rolling back
([Users](api/users.md#the-rules-that-make-delegation-safe)). Reverting migration
`0011` is refused while a reader exists. The id is a hyphenated **UUIDv7** under a `user_` prefix, so the
keyspace lists in creation order with no separate index.

Only `status`, `account_type` and `campaign_scope` change after creation, and
only by an administrator, never by the account itself. The id and `created_at` are
immutable, and the address is the index key, so changing it is a delete and a
create (§3.1.2). A write replaces the whole record with no compare-and-swap:
two changes that race leave whichever landed last, including the field the
other did not set, so a retype racing a disable can restore the old status.
Retype an account while nobody is changing its status. Every create is under a
freshly minted id, serialized against a second create for the same address by
the claim in §3.1.2.

[authentication.md §5.11](authentication.md#511-users) carries the
field-by-field rationale.

#### 3.1.2. Address → account: `user-email-index/{email}`

```text
user-email-index/operator@example.com   →  user_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e
```

**The address is the key; the account id is the body.** One get answers "who
holds this address?", and an exclusive create answers "is it taken?"
atomically: two creates for one person contend for *the same key* and exactly
one wins. Spelling it `user-email-index/{email}/{user_id}` would give two
racers two keys, and both would win. A lock has to be one key. The body is the
bare id, not JSON.

The address is used directly, not hashed, so `ls user-email-index/` is a
readable roster. That rests on two invariants:

- **Safe key**: `ContactEmail` refuses a quoted local part and a bare `/`.
- **One mailbox, one key**: the account's `contact_email` is case-folded when
  it is created, and it *is* the index key. Case is the only thing folded:
  `operator+ci@` and `operator@` are two accounts, which is how a service
  account gets an address distinct from its owner's.

**Open: a non-ASCII address has more than one Unicode spelling.** NFC and NFD
forms of one address are different keys, and APFS normalizes a filename to NFD
where Linux keeps the bytes, so one address written from two machines could land
on two keys. No address in use is non-ASCII. Closing it changes how the key is
derived, handled by rebuilding the index from `users/`, not by a `v2/` segment
(§2).

Deleting an account removes the key. A rename is a delete and a create, never a
blind overwrite of the record, or the old key keeps pointing at the account.
An operator's create and a machine's own request for an account both claim
here, and each releases its claim when the writes it was made for fail. A claim
stranded by a crash is cleared by an operator, never adopted automatically,
because it is indistinguishable from an attempt still in flight.

#### 3.1.3. Keys: `user-keys/`, and the claim that indexes them

```text
user-keys/{user_id}/{key_id}.json      ← the registration
  { "key_id": "userkey_9f2c…", "public_key": "a3f8…", "key_details": "work laptop",
    "added_at": "2026-09-04T12:00:00Z" }
  { …, "key_details": "console app, Chrome on macOS",
    "added_at": "2026-09-04T12:00:00Z", "expires_at": "2026-10-04T12:00:00Z" }

public-key-index/{sha256(raw public key bytes)}   ← the claim, and the reverse mapping
  ev1_… | user_…
```

A user holds many keys, one per machine. The registrations are stored under the
account; the reverse direction is `public-key-index/`, which has to exist anyway,
so there is no second tree.

A `key_id` is `userkey_` plus the hex `sha256` of the public key, a pure
function of the key: two registrations of one key described differently are one
key, and re-registering a revoked key restores the same id. A revocation is
not a permanent ban on a key: delete the account to retire an identity.
`key_details` describes the machine, so "revoke the laptop" is decidable from a
listing.
`expires_at`, on a key made to sign in one browser, is when it stops signing; a
machine's key has none, and the record stays after it passes, until revoked.

**Write order.** Registering claims `public-key-index/…` first, then writes the
registration. A crash between them leaves a claim with no registration, which
authenticates nothing. Revoking deletes the **registration first**, then the
claim. Authentication reads the registration, not the claim, so the opposite
order would leave a revoked key that still authenticates. Both orders leave the
same, fail-closed kind of leftover: a claim with no registration.

##### One keypair, one principal: `public-key-index/{digest}`

It is the lock ("is this keypair already somebody's?") *and* the reverse mapping
from a keypair to its owner. A client id is `ev1_` + `sha256(key)` and a key id
is `userkey_` + the same digest, so without one claim spanning both kinds, a
holder could register one key as a client and as a user, and choose per request
which principal to present, bypassing the permission partition
([authentication.md §5.2](authentication.md#52-the-vocabulary)).

A signed request names its own principal, so authentication reads the
registration directly (`user-keys/{user_id}/{key_id}.json`); a forged
`X-Client-Id` names a path that holds nothing. Only a key-only request, which
has no principal to address, reads the claim
([authentication.md §5.12](authentication.md#512-signing-in-from-a-browser)).
Both registration paths, client and user, claim it.

#### 3.1.4. Tags, not here

A user has no tags. Grouping operators would reuse the client tag index rather
than a second one: a client id and a user id share a charset and differ by
prefix, so one tree could hold both. Widening `tags-index/by-client/` to
`by-principal/` is a migration of live data (markers under the old segment are
invisible to a binary reading the new one), and it waits for something that
needs it.

#### 3.1.5. Permissions and pre-auth: each kind's own

Both are attributes, and both are separated by kind because **their bodies
differ by kind**: the path tells a reader which type it deserializes.

- **Permissions** are `user-permissions/{user_id}.json`, beside
  `client-permissions/`, with the same object-per-principal shape and the same
  absent-object answer: nothing. Only the client keyspace is listed; every user
  read is by principal id. A store from before the split held both kinds under
  one `permissions/` keyspace that nothing reads now, so a set left there
  grants nothing.
- **Pre-auth keys** are `user-preauth/{key_id}.json`, with the spent marker at
  `user-preauth/{key_id}.spent`. They share the design of `client-preauth/`:
  `sha256(secret)` and never the secret, single- or multi-use, an optional
  `expires_at`, and a `.spent` marker claimed by an exclusive create. The
  records differ: a client's key carries `default_tags`,
  `default_organization` and `default_permissions` for the record its
  registration creates; a user's carries `user_id`, the account it admits, and
  no permission template, since the account exists before its key and is
  granted directly. The token prefixes differ too (`client_preauth_` and
  `user_preauth_`).
- **The first sign-in** is `user-awaiting-sign-in/{user_id}`, an empty object an
  operator's create writes and the account's first key registration or Clerk
  sign-in deletes. Its presence is the one fact the key tree cannot give: that
  nobody has signed in as the account. An account created before the marker
  existed has none, so it reads as signed in. The key registration deletes the
  marker before it writes the key, so an account never holds a key while still
  marked.
- **The first Clerk sign-in** is `user-first-clerk-sign-in/{user_id}.json`,
  `{ "at", "prior_keys" }`: when the account first signed in through Clerk and
  the ids of the keys it held then, besides the one that sign-in registered.
  It is written once, by an exclusive create, after the sign-in's key, so of
  racing first sign-ins one records and a refused one leaves the next sign-in
  the first. Two racing first sign-ins can each list the other's new browser
  key, so the winner may record that key among the earlier ones. A pre-auth
  key's registration clears `user-awaiting-sign-in/` before it, which is why
  the two are apart
  ([Users: Signing in through Clerk](api/users.md#signing-in-through-clerk)).

#### 3.1.6. Deletion and disabling

**Prefer disabling.** `status: "disabled"` refuses every request from the
account and leaves the record in place, so the plans it filed stay attributed to
a principal that still resolves. Deletion is for an account created in error.

A delete reads the address and the key ids first, then deletes the account
record first, so the account stops authenticating in one write and every
remaining key is inert. It then removes the registrations, their
`public-key-index/` claims, the address claim, the permission set, the
membership (`user-orgs/`, §3.2.3), the first-sign-in marker and the first
Clerk sign-in record. A crash partway leaves state that grants nothing; a
leftover address claim must be cleared before the account is created again. The delete spans both principal
keyspaces and `[storage]`, so whatever performs it needs write access to all
three.

No reconciliation pass reaps a stranded user key or address claim: `reindex`
reads client records only, and the key index cannot be listed, so a leftover
claim stays until an operator removes it.

### 3.2. Organizations

A provisional shape, a holding position until a longer-term tenancy model
lands: the record and its handle claim (§3.2.1, §3.2.2), memberships (§3.2.3)
and device ownerships (§3.2.4).

An organization holds no permission, but these records are inputs to
authorization: a membership bounds what its account's bare grants reach, and an
ownership decides which organization's administrators reach a device and which
private benchmarks and vault resources it is served. What each decides is
[authentication.md §5.2](authentication.md#52-the-vocabulary) and
[topology.md §7](topology.md#7-decisions-per-object).

On the object-store record backends, **all four keyspaces live in `[storage]`**,
beside plans and campaigns, not in `[auth_storage]` beside the accounts and
devices they name. They hold ids and no address or key, so a reader of the data
bucket can join them to plans and results without the auth bucket. **So write
access to `[storage]` carries authority**: whatever writes the bucket directly
can move an account into an organization, adopt a device or take over a
campaign. Grant writes on the data bucket as narrowly as on the auth bucket.

**Two relations, each its own record keyed by the id it describes**, so each
fact has one place to be true:

| Relation | Where it lives | Values |
|---|---|---|
| user belongs to organization | `user-orgs/{user_id}.json` | an `org_id`, or no record |
| device belongs to organization | `client-owners/{client_id}.json` | `{type: "org"}`, `{type: "user"}`, or no record |

**There is no reverse index, deliberately.** "Who belongs to this organization"
and "what does it own" filter the two relation keyspaces. An index would be two
writes where there is one, a repair loop, and a class of bug where the answer is
confidently wrong. The cost falls on operator surfaces only, over people-sized
and fleet-sized reads.

#### 3.2.1. The record: `orgs/{org_id}.json`

```json
{
  "org_id": "org_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e",
  "slug": "acme",
  "display_name": "Acme Corp., Inc.",
  "status": "active",
  "created_at": "2026-09-04T12:00:00Z"
}
```

`org_id` is minted, never derived: an organization holds no key, and deriving it
from the name would make a rename a new organization. It is a UUIDv7 under an
`org_` prefix, so `orgs/` lists in creation order and the kind is readable off a
bare id.

`slug` is the index key: one path segment, stable. `display_name` may be
rewritten. **A rename touches the display name only**, so no stored reference
goes stale. An organization that needs a different handle is a new organization.

`status` is `active` or `disabled`. `disabled` marks a wound-down organization
without deleting the record its members came from. A disabled organization
takes no new member: a create or a move into it is refused, and Clerk
provisioning creates no account in it
([authentication.md §5.12](authentication.md#signing-in-through-clerk)). Members
keep what their membership reaches. There
is no `pending`: nothing self-registers an organization.

#### 3.2.2. Handle to organization: `org-slug-index/{slug}`

```
org-slug-index/acme    → "org_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e"
```

The same shape as `user-email-index/` (§3.1.2), for the same reasons. The handle
is folded and charset-checked (`[a-z0-9_-]`, at most 64 characters) on the way
in, so `" ACME "` and `acme` reach one key. A handle **may not begin `org_`**,
which is how an id is spelled, so the two namespaces stay disjoint.

The claim is written **before** the record and released **after** it. A crash
between the two strands the handle rather than freeing one an organization still
holds: recoverable by hand (`orgs release-claim <slug>`,
`orgs create --adopt-stranded-claim`).

#### 3.2.3. Membership: `user-orgs/{user_id}.json`

```json
{ "user_id": "user_0195c3e2-…", "org_id": "org_0195c3e2-…" }
```

**An account in no organization has no record**, a real state rather than a
migration artifact. Written by an administrator assigning or creating an
account, and by Clerk provisioning; never by an account naming an organization
for itself. An assignment resolves the organization first, so a membership names
a record that exists. Clearing it deletes the record, and so do the account's
delete (§3.1.6) and `orgs delete --detach` (§3.2.5).

#### 3.2.4. Ownership: `client-owners/{client_id}.json`

```json
{
  "client_id": "ev1_…",
  "owner": { "type": "org", "org_id": "org_0195c3e2-…" }
}
```

or, for a device somebody owns personally:

```json
{ "client_id": "ev1_…", "owner": { "type": "user", "user_id": "user_0195c3e2-…" } }
```

The tag is `type`. The same object is the `owner` on a campaign (§3.3) and on a
vault record (§3.4).

**Distinct from the client record's `organization`**, which is free text the
device sent about itself. An ownership points at a record. Merging them would
reinterpret every stored label as a membership nobody approved.

**Two owner kinds**, because a rack machine and somebody's laptop are different
things. The organization a *user-owned* device rolls up to is **derived** from
that owner's membership, not stored: moving an account moves its devices. A
device owned by an account in no organization rolls up to none, a state and not
an error.

A fresh self-registration has no record. A registration with a pre-auth key that
names an owner writes one before the client record; a registration without one
clears whatever is there, since a device deleted and registered again comes back
under the same id. Rejecting or deleting the device deletes its ownership,
best-effort.

#### 3.2.5. Deletion

`orgs delete` is **refused while anything still names the organization**: a
membership, a directly owned device, an entry under `org-benchmarks/{org_id}/`,
a campaign or vault record it owns, or an unexpired client pre-auth key whose
`default_owner` it is. A private benchmark counts by its key, so an unreadable
one counts too; left under a deleted id, it would still hold its benchmark id
and dataset pin server-wide. `users delete` is refused the same way while the
account owns a device, a campaign or a vault record. Both take `--detach` to
clear the memberships and ownerships first; the rest is cleared one record at a
time. The refusal exists because ids are minted: a stranded reference never
resolves again, silently.

A relation can outlive its record when a create or a teardown fails partway;
the refusals count it, and `--detach` clears it like any other.

The record is deleted before the handle is released, as in §3.1.6: the record
is the existence authority.

### 3.3. Campaigns

A **campaign** is the stable id for work whose plan keeps being revised. A plan
manifest is an immutable ingestion, so changing what a run covers files a new
plan under a new `plan_id`. A campaign is the identity those revisions belong
to: a minted id that never changes, and a `current_plan_id` naming the plan
being executed.

**A campaign mints its plans.** Nothing attaches an existing plan to a campaign.
A campaign is created with its first plan, and each revision ingests a new plan
recording the campaign and the plan it supersedes.

```text
Campaign ──current──> P2 ──previous──> P1
```

**One plan, one campaign.** `campaign_id` is written on the manifest when the
plan is minted and never moved. (Two campaigns' plans may still *share a job*
where they describe the same cell: the plan is exclusive, not the work.)

**The chain is the history; the campaign holds the head.** Walking
`current_plan_id` back through each manifest's `previous_plan_id` is what a
campaign has run. The chain points backwards because a plan is written once.

```json
{
  "campaign_id": "campaign_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e",
  "name": "Nightly LFM2 regression",
  "owner": { "type": "org", "org_id": "org_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e" },
  "current_plan_id": "plan-0195c400-1111-7000-8000-000000000002",
  "labels": { "study": "lfm2-regression", "device": "s26" },
  "created_at": "2026-09-04T12:00:00Z",
  "updated_at": "2026-09-18T09:30:00Z"
}
```

No index: the minted id is the handle. The record lives with the plans it names.

`current_plan_id` is **absent** only on a record written before every campaign
was created with a plan; it reads as a campaign running nothing. It is set only
by ingestion, so it always names a plan this campaign minted, though possibly
one an older build deleted.

`owner` is **required**: an unowned campaign is one no scoped caller could
administer. It is read as it stands and **not derived through the owner's
membership**, unlike a user-owned device: moving an account between
organizations must not hand its personal campaigns to a team. A plan has no
owner; its campaign does, so handing a campaign over rewrites this record alone.
Who may read and administer a campaign is
[Campaigns: Who may do what](api/campaigns.md#who-may-do-what).

`labels` are free `key: value` pairs for grouping and filtering, on the rules of
an AWS resource tag: at most 50, one value a key, a key of 1 to 128 characters
and a value of 0 to 256, each of letters, digits, spaces and `_ . : / = + - @`,
with no space at either end, compared as written. Absent on a record nobody
labelled. Nothing on the server reads a label.

`state` is `"archived"` on a campaign its administrator took out of use, which the
console's listings hide by default and which takes no new plan. Absent while the
campaign is active, so older records read as active. On SQL it is a
column beside `labels`, outside the body, so a rolled-back build that rewrites
the body keeps it; an object store keeps it in the body, where such a build
drops it.

**A pointer with an owner, not a container.** A campaign holds no jobs and gates
nothing but new plans while archived. `queue-maintenance` neither reads nor writes it, and deleting a
campaign leaves its plans on their own lifecycle.

**The two writes, and their order.** Filing a revision writes the plan manifest
first and advances the campaign second. A failure between them leaves a plan
that names the campaign and never took effect, which is the direction to fail
in; the repair is to file again. Creating a campaign keeps the same order. If
the campaign write fails, the create writes the plan's cancel marker. A failure
of both leaves a live plan naming a campaign that does not exist; the repair is
to cancel that plan.

#### 3.3.1. Ownership

`owner` takes the one owner shape (§3.2.4). The administration rules are in
[Campaigns: Who may do what](api/campaigns.md#who-may-do-what) and drawn in
[topology.md §7.3](topology.md#73-campaigns-and-vault-resources-one-ownership-rule-read-twice).

#### 3.3.2. Concurrency

A write replaces the whole record with **no compare-and-swap**: the operator
surface is its only writer, and no cron pass writes it. Two administrators
changing one campaign at once is last-writer-wins, and the record still holds
one name and one owner that were asked for.

A revision (a variant edit, a document revision, a re-run or a recalculation)
re-reads the record and advances it only if it still names the plan it revised.
That narrows the race between two revisions to the gap between the read and the
write; it does not close it.

A write that asks for the state the record is already in writes nothing and does
not move `updated_at`, so it is safe to retry.

### 3.4. The vault

A **vault resource** is a private artifact a plan can name: one uploaded file of
a customer's own checkpoint or an internal build. The record is an index entry
plus a gate over one blob. The design is
[design/vault-resources.md](design/vault-resources.md); the routes are
[Vault](api/vault.md).

**The blobs and the records are separate keyspaces, and the split is the
security boundary.** The blobs are on the object store; the records are records
(§1.1).

```text
vault-resources/
  {principal_id}/blobs/{sha256}
                        ← the bytes of one file, uploaded for that principal.
                          Written only whole and only if the bytes hash to
                          {sha256}.
vault-staging/
  {principal_id}/{uuid} ← S3 only: the bytes of one blob while mgmt computes
                          their digest. No record names one.
vault/
  {resource_id}.json    ← name, owner, object_key, sha256, bytes, artifact,
                          origin, created_at. Written once, never edited.
vault-imports/
  {principal_id}/{sha256}.json
                        ← resource_ids: the records one import of a public
                          model wrote, or the archive record of one
                          HuggingFace directory source. A later write
                          replaces the entry.
```

The `owner` is the one owner shape (§3.2.4). The `artifact` is the plan variant
the file was uploaded as, with every file of it as
`{resource_id, filename, sha256}`, so each record of one upload carries the same
artifact and knows its own file by its id. A record written before uploads went
through mgmt has no `artifact`.

The `origin` is the public source the file is a copy of. mgmt writes it only
when mgmt itself fetched the bytes; an uploaded file has none, because mgmt
cannot check where uploaded bytes came from. An origin names its source at one
full 40-character commit, and the record's `sha256` names the stored bytes. Two
kinds:

- `{"source": "huggingface", "org": …, "repo_name": …, "commit": …, "path": …}`
  is a HuggingFace repository. `path` is one file in it, absent if the record
  holds the whole repository packed into one archive.
- `{"source": "github_release", "owner": …, "repo": …, "commit": …, "asset": …}`
  is one release asset of a GitHub repository, built at that commit.

**A record's key is always its own owner's blob**
(`pipette_core::vault::blob_key`): the first segment under
`vault-resources/` is the owner's principal id. `VaultObjectKey` refuses a key
that is absolute, carries a `..` segment, or already begins with
`vault-resources/`, so no record names an object outside the prefix. The prefix
holds every principal's artifacts, so existence there is not entitlement.

**A blob is written only whole, and only as its digest says.** A reader never
sees a partial blob or one its key misnames. An upload a dying process leaves
open, and a staging object whose delete failed, are reclaimed by the bucket
lifecycle rule ([operations.md §5.2](operations.md#the-vault-prefix)). The
`sha256` on the record is therefore checked when the blob was written, and the
client still verifies what it downloaded, which catches a blob replaced behind
mgmt's back.

**A blob is deleted after the records.** A resource delete removes the blobs no
remaining record of that owner names, so a failure leaves a blob no record names,
never a record without its blob. One race does leave such a record: a record
written for the same file while the delete runs can outlive the blob, until the
file is uploaded again. A blob no record names stays until
`pipette-mgmt vault prune` takes it
([cli.md](cli.md#pipette-mgmt-vault-listshowdeleteprune)).

**An import entry maps an import to its records**, by a digest of the model's
origins and file list. A retry of the same import reuses the records and
downloads nothing again, but only if each record the entry names still exists.
A replaced entry's records stay in `vault/` as duplicates.

**A source entry maps one HuggingFace directory source to its archive record**,
by a digest of the repository, commit, prefix and file list. The archive of a
commit is the same each time. An import of a different model with that source
uses the blob of that record. That import downloads nothing for the source. Each
import that stores the archive replaces the entry, so the entry names the newest
record. If that record or its blob does not exist, the import downloads the
source again.

#### 3.4.1. Concurrency

**A record is immutable**: written once under a minted id and deleted when no
longer wanted. A plan carries the id in a URL, so a record that could be
repointed or handed over would make that URL mean something else later;
different bytes or a different owner are a new record. One writer per key and no
second write, so no compare-and-swap.

**Two uploads of one blob** write the same bytes under the same key; whichever
completes last stays, and it is the same file.

**An import that reuses a blob does not lock it.** An import can find a gguf
blob or an archive that the owner holds, and then write a new record that names
that blob. A delete of the last other record that names the blob can run between
those two steps. The delete then removes the blob, and the new record names a
blob that does not exist. The import checks the blob again before it writes the
record. That check makes the window short, but the window stays open.

### 3.5. Model params

A **model-params row** is one model's parameter counts in millions. The rows are
the model parameter table ([§5](#model-parameter-table)): the scorer reads them
for the counts a warehouse row stores, and a filing reads them for its
`uncatalogued_model` warning. A row is added, edited and deleted on its own, so
a model gets its counts without a release.

```text
model-params/
  {key}.json    ← total_millions, active_millions, note, updated_at, updated_by
```

On SQL, the `model_params` table with a column per field. The key is the
normalized model name (`acmelm-4.5b-text`) or a vault resource id, confined to
`[a-z0-9._-]`. Both counts are positive and
`active` is at most `total`, equal for a dense model. `note` is optional text of
at most 500 characters on where the counts come from; nothing reads it but a
person.

```json
{
  "key": "acmelm-4.5b-text",
  "total_millions": 4512,
  "active_millions": 4512,
  "note": "summed tensor counts of model.safetensors at 93f895af: 1,728,801,536",
  "updated_at": "2026-10-10T18:00:00.000000Z",
  "updated_by": "user_0195c3e2a4f37b8c"
}
```

**Seeded once.** On SQL, the migration that creates the table inserts the
entries of the `model_params_mapping.toml` file the rows replace, each under its
normalized key with a `null` `updated_by`. The other backends start empty;
[`models import`](cli.md#models-import-rows-from-a-file) adds rows from such a
file, and leaves a key that has a row alone.

**Read when scoring.** A scoring pass loads the rows once and stores each row's
counts on the warehouse rows it writes. An edit reaches the rows scored after
it; `fix-model-param` rewrites the ones stored before. A read of a plan's
results fills an empty count from the rows and rewrites nothing
([Model parameters](api/model-params.md)).

**Last writer wins.** A write replaces the whole row and a delete is idempotent.

## 4. Submission contract

New submissions are written to a flat inbox:

`submissions/incoming/{job_id}.json`

Neither `incoming/` nor `processed/` is partitioned: both are flat keyspaces over
`job_id`. The scorer drains `incoming/` and reads `benchmark_id` and `client_id`
from each payload.

Every body enters at `incoming/`, whatever its `message_type`. A
`message_type: "failure"` body carries no measurement, but it carries the
identity of the cell nothing could measure, so the scorer records one warehouse
row per metric that cell intended to produce ([§5](#failure-rows)). A synthetic
`"system"` failure that `queue-maintenance` writes takes the same path.

The stored JSON is immutable after write.

When scoring succeeds:

- derived metrics are written to the warehouse
- for eval benchmarks, eval sample results are written
- the submission moves from `incoming/{job_id}.json` to
  `processed/{job_id}.json.gz`

If scoring fails, the submission stays in `incoming/` and is retried on the
next run. Each run pages through `incoming/` and the `score-queue/` stages past
the last id it listed, so jobs that keep failing never hold back the ones after
them. The server refuses an eval body with no completions, or with a corpus
digest that is not its entry's, but one stored before those checks, or one
the scoring service refuses, or whose score never finalizes, never scores.
Each failed pass of a queued job, `score-eval`'s of a `to_do` job and
`process-submissions`' finalize of a `to_finalize` one, is counted in
`score-queue/failures/{job_id}.json` as `{ attempts, error }`, cleared when the
step succeeds. A pass that cannot connect to the service, or gets a `502` or
`503`, counts nothing and pauses; a timeout or `504` is counted, as it may be
the job's own size, and pauses. At 12 attempts, about an hour of the
five-minute crons, the submission moves to `score-queue/parked/` (from
`to_finalize/`, without its score), the log says `eval failed every scoring
pass; parked` at warn with the job, benchmark and client ids and the stage, and
no pass retries it. The job still reads as `scoring`. List `parked/` to find
them; to retry one, move its body back to `to_do/` and delete its `failures/`
entry.

The processed submission preserves the original payload, less the runtime
output, which is stored beside it ([§4.2](#42-runtime-output)).

`processed/` is an operational archive, not the durable scored-results store.
It may be retained, pruned or deleted independently of the warehouse.

Files in `processed/` are gzip-compressed (`{job_id}.json.gz`, level 6); read
them with `zcat`. Files in `incoming/` stay plain `.json`, so ingress is a
single fsync.

A stored submission, eval (the body fields are the device protocol's,
[httpapi.md §2.7](httpapi.md#27-post-benchmarks)):

```json
{
  "job_id": "job-660f9500-...",
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

A `completions[]` entry may carry the optional `failed` (default `false`),
`failed_reason`, `stop_reason` and `completion_tokens` (default `null`)
([canonical enum](scoring-service.md#per-sample-stop_reason-canonical)). All of
them default on parse, so a stored body from a client that predates them still
reads. See [scoring-service.md](scoring-service.md) for what the scorer call
forwards and strips.

The server injects `client_id`, `submitted_at`, `job_id` and `benchmark_type`
into the stored JSON. The `device_*` hardware fields are the client's.
`model_descriptor` and `runtime_descriptor` are stored as JSON strings whose
contents are JSON, as sent. Standard deviation is reported per timing field
(e.g. `prefill_time_ms_stddev`); see [benchmarks.md](benchmarks.md) for
per-type fields and propagation.

### 4.1. Unverified submissions

With `[unverified_submissions] enabled = true`
([config.rs](../crates/pipette-core/src/config.rs)), a submission
from a **pending** (validly signed but unapproved) client is held. The payload
is validated like an approved one and written to:

`submissions/unverified/{client_id}/{job_id}.json`

The keyspace is partitioned by `client_id`, then flat over `job_id`. The stored
JSON is immutable and carries the caller's real `client_id`, `job_id`,
`submitted_at` and `benchmark_type`, injected as for an approved submission.
The three
[`requested_*_flags`](#requested_model_flags--requested_runtime_flags--requested_benchmark_flags)
are null: this path binds no claim, so there is no lease to read the authored
`spec` from.

A held submission is invisible to the scorer, the warehouse, the eval sample
results and the `fix-*` family. The only consumers of this tree are the operator
`unverified` subcommands ([cli.md](cli.md)), none of which takes the mutate
lock:

- `promote --client-id <id>` re-stages a client's held submissions into
  `incoming/` once the client is approved, through the routing the live path
  takes. Each object is deleted from the unverified tree only after its
  re-stage write succeeds. A held object whose job already has a record, a
  job source or a `todo/` entry is refused and stays held: its `job_id` was
  never checked against a lease.
- `delete --client-id <id>` discards a client's held submissions.
- `prune --older-than <age>` removes held objects across all clients by the
  backend's modification time (S3 `LastModified`, filesystem `mtime`), not the
  payload's `submitted_at`.

Listing cost scales with the archive, so run `prune` (or resolve clients)
regularly.

### 4.2. Runtime output

What the runtime printed while it ran, as a client captured it: the subprocess
the client spawned, or the app itself when the runtime runs in-process.

`submissions/runtime-output/{job_id}/{client_id}.json.gz`

```json
{"runtime_stdout": "…", "runtime_stderr": "…"}
```

Each key is present when the client captured that stream, and `""` records that
the process printed nothing to it.

This is the one part of a submission not kept in its body. Several paths read
whole bodies and need none of the output, and two full streams would grow a
throughput body of a few kilobytes to 2 MiB. So the write that stores a body in
`incoming/` first moves both streams here, and a body in `incoming/` or
`processed/` carries neither.

- **One object per client:** a job can hear from several clients. A retriable
  failure denies the job to one client and returns it to the queue, and another
  may then finish it. Each keeps its own output.
- **Written first:** the object is written before the result it belongs to, the
  `incoming/` body or the `denied/` marker, so a result that came with output
  never exists without it. A crash between the two leaves an object with no
  result, which is harmless. A resubmission under the same lease replaces the
  object only if it carries output of its own.
- **Only when there is output:** a submission with neither stream writes no
  object.
- **Retriable failures keep theirs:** such a failure records no result, and its
  `denied/` marker keeps only the last 4 KiB of its reason. This object keeps the
  rest.
- **Held bodies keep it inline:** a body under `submissions/unverified/` is
  stored whole. `unverified promote` moves the output here, and `unverified
  delete` and `prune` drop it with the body.
- **Capped:** each stream keeps at most its last 1 MiB, cut at ingest behind a
  leading `…`, because a runtime writes its final error last.
- **No expiry:** nothing deletes these objects on a schedule.
- **Not in the warehouse:** every metric row repeats its job's fields, so output
  there would be copied once per metric and rewritten on every run.

Read it with `pipette-mgmt runtime-output <job_id>` ([cli.md](cli.md)).

### 4.3. The outcome index

With `[database]`, the table `submission_outcomes` holds each job's outcome
(`failed` for a `message_type: "failure"` body, `finished` for any other), so
`queue-maintenance` reads a row rather than a body of tens of megabytes. A write
deletes the row, writes the body, then writes the row, so a row never holds an
older body's outcome. A row outlives a record pruned from `processed/`. A job
with no row (a record from before the index, or a failed row write) is answered
from `submissions/`; a failed row write fails the submission write, so the
client resends it.

A `finished` row also holds `run_flags`. These are the flags that the record
proves the run held. The function `recorded_run_flags` gives them from the
normalized `device_*` fields and from `client_version`. A `failed` row holds no
flags. A row written before
migration `0007` holds none until `db backfill-job-cells` fills it
([cli.md](cli.md#pipette-mgmt-db-backfill-job-cells---dry-run)).

### 4.4. The job-cell index

With `[database]`, the table `job_cells` holds the cell digest of each job that
ingestion minted. The cell digest is the content digest without the eligibility
clauses and `required_flags`, so it names what a job measures, whoever runs it.
Ingestion writes the row beside the job source. The write is best effort. A
lookup does not find a job that has no row; `db backfill-job-cells` writes the
missing row. A row is write-once, and nothing deletes one.

The column `scope` names who may reuse the run of the job. Ingestion writes it
from the owner of the campaign that minted the job:

- `org:<org_id>` for an organization, or for a user in that organization.
- `user:<user_id>` for a user in no organization.

The organization of a user is read when the job is minted. A row written before
migration `0010`, or by `db backfill-job-cells`, has no scope.

A lookup by cell digest and scope joins `job_cells` to `submission_outcomes`
and `job_sources`. It gives each `finished` run that has `run_flags` and the
scope of the lookup, in
descending `job_id` order. A minted id holds a UUIDv7, so this order follows
the mint time to the millisecond. The order of other ids is their text order.
It leaves out a job that a plan reran (`plan_jobs.rerun_of`), and a row with
no scope. It gives at most the 32 newest runs of each cell. A cell gets a job
for each device that ran it, so this limit bounds the read. A cell whose only
answering run is older runs again.
Ingestion reads the lookup to satisfy a cell from an earlier run
([plan-ingestion.md §7](plan-ingestion.md#satisfying-a-cell-from-an-earlier-run)).
The record stores in the object store keep no index and find no run. A job minted
before migration `0007` has no row until `db backfill-job-cells` writes it.

### 4.5. The descriptor catalog

Migration `0013` adds `descriptor_entries` and `descriptor_members`. Each entry
has these fields:

- the name
- the kind
- the state
- the runtime type
- the release time, which does not change after creation
- the creation time
- the creator

Each member has these fields:

- the position
- the flavor, if the descriptor sets one
- the digest
- the descriptor
- the time of addition

The member positions give the order of precedence.

Migration `0016` gives entries and members a `scope`: `''` means global, and
an organization id means that organization's namespace. Existing entries and
members become global. Names, member positions, flavors and digests are unique
within an entry's scope; the same name can occur in several scopes. The member
foreign key includes both scope and name. The store indexes release time for
both one-scope and multi-scope listings. An org entry must be `hand-made`.

A rollback of `0016` deletes every organization entry and its members, then
restores the global keys. Back up organization catalog data before the rollback.

Migration `0017` lets an entry hold model members:

- The entry's `runtime_type` column becomes `descriptor_type`. It holds the
  runtime type of a runtime entry, or the model type of the first member of a
  model entry.
- The new entry column `descriptor_kind` holds `runtime` or `model`. Existing
  entries become `runtime`. A model entry must be `hand-made`.
- The new member column `descriptor_type` holds each member's runtime type or
  model type. The migration copies the type of each existing member from its
  entry. A filter on a model type reads this column, so it finds every format
  of a model entry.
- The new member columns `format` and `quantization` hold a model member's key.
  Both columns contain null for a runtime member and a value for a model member.
  The pair is unique within an entry.

A rollback of `0017` deletes every model entry and its members, then restores
the `runtime_type` column. Back up model catalog data before the rollback.

Migration `0015` adds `catalog_import_runs`. The importer writes a `running`
row before it contacts GitHub. It sets the result to `success` or `error` when
the run ends. The row keeps these values:

- The start time and the finish time.
- The command and the source.
- The target name, for a refresh.
- The counts of entries and members that the run added.
- The unknown asset names, with their tags.
- An error summary.

A `running` row with no
finish time shows a run that did not finish. `db import` and `db export` do not
copy these SQL-only rows.

The store ignores case when it compares names. It keeps the spelling for
display. An entry can gain members only at the end. It cannot lose or reorder
members. A tombstone reserves its name permanently. The store cannot move a
tombstoned entry to another state.

The store compares the digest of each member with the digest of its typed
runtime or model. A change to the serialized form of a runtime type or a model
type thus needs a migration of `descriptor_members`.

## 5. Warehouse contract

Warehouse logical partition:

`warehouse/results/benchmark_id={benchmark_id}/client_id={client_id}/day={YYYY-MM-DD}/`

The warehouse is the canonical scored-results store for aggregate metrics,
partitioned by `benchmark_id`, `client_id` and the calendar **day** of
`submitted_at`. A partition is rewritten on every scoring run that lands a
result in it, so capping it to one day keeps that rewrite small.

**Legacy `month={YYYY-MM}` partitions are frozen**: the write path never creates
or rewrites one. Reads union both schemes. A `job_id`'s rows live in exactly one
partition.

```text
warehouse/
  results/
    benchmark_id=prefill_throughput_256/
      client_id=ev1_a3f8.../
        month=2026-03/          # legacy, frozen
          part-0001.parquet
        day=2026-06-01/         # new
          part-0001.parquet
```

Treat the partition, not a file, as the unit of access. `pipette-mgmt
process-submissions` (alias `score`) is the normal writer; the one-off `fix-*`
tools rewrite files in place across `day=` and `month=` partitions
([cli.md](cli.md)).

The scorer writes each `day=` partition **append-only**: a run's rows are
appended to the tail `part-NNNN.parquet`, which rolls to a new part at
`warehouse_max_rows_per_part`. Earlier parts are never read or rewritten.

**There is no write-time dedup.** If `score` crashes after the warehouse write
and before `mark_processed`, the job is scored again and its rows are
**appended again**. So "one scored metric set per `job_id`" is a logical
property:

- A reader keeps, per `job_id`, only the rows of the **latest scoring run**: the
  maximum `scored_at` (all rows of one run share it), dropping earlier copies
  wholesale. External readers over the raw Parquet apply the same rule. A row
  written before the `job_id` column existed reads it NULL; recover it with
  `regexp_replace(result_id, '_[0-9]+$', '')` (`result_id` is `{job_id}_{i}`,
  and a `job_id` never contains `_`).
- A count needs no `scored_at` reasoning: `count(DISTINCT job_id)` collapses
  the copies.

Most columns are copied verbatim from the submission. Three groups are not:

- `model_params_total_millions` / `model_params_active_millions` carry the model
  parameter table's values when the model resolves ([Model parameter table](#model-parameter-table)),
  else the submission's, including `null`. The stored body is never modified,
  so `processed/` may disagree with the warehouse; the warehouse value is the
  one to use.
- The `requested_*_flags` columns and their `_sha256` are stamped by the server
  from the job's authored `spec`
  ([below](#requested_model_flags--requested_runtime_flags--requested_benchmark_flags)).
- Every `parameter_*` column, and `benchmark_params` with its `_sha256`, come
  from the catalog entry the row is scored against, on a failure row too.

### What pipette-mgmt reads from pipette-duckdb

The results reads ([`GET /api/v1/plans/{plan_id}/results.csv`](api/plans/get_plan_results_csv.md)
and the plan page's `results=true`) do not read the warehouse files: mgmt keeps
no index from a plan to its warehouse rows. pipette-duckdb publishes mgmt's
`plan_jobs` beside the warehouse in its `internal` database (`[duckdb] database`),
so one query joins them, and the server reads it with the `[duckdb]` credential
([cli.md](cli.md#configuration-file)). mgmt reads nothing else from
pipette-duckdb, so this is the whole contract. A missing table or column
answers `502`.

- **The query API:** `POST {url}/api/query?db={database}`, with the `[duckdb]`
  user and password as HTTP Basic, and a body of `{"sql": "…"}`. The answer is
  `{"columns": [...], "rows": [[...], ...]}`, each row in `columns` order. The
  user must be able to read `database`.
- **`plan_jobs`:** `plan_id`, `job_id` and `ordinal`, one row per job a plan
  lists, carried-over jobs included.
- **`warehouse`:** `submission_id`, joined to `plan_jobs.job_id`, and
  `result_id`, `scored_at`, `benchmark_id`, `benchmark_type`, `metric`, `value`,
  `value_stddev`, `unit`, `failed`, `failed_reason`, `model_descriptor`,
  `model_descriptor_sha256`, `runtime_descriptor`, `runtime_descriptor_sha256`,
  `model_vault_descriptor`, `model_flags`, `runtime_flags`, `benchmark_flags`,
  `requested_model_flags`, `requested_runtime_flags`,
  `requested_benchmark_flags`, `eval_metadata`, `client_extras`, `client_id`,
  `device_name`, `submitted_at`,
  `device_time_ms`, `failure_source`, `rescored_from`, `run_submitted_at`,
  `score_runtime_version`, `client_version`, `model_params_total_millions`,
  `model_params_active_millions`, and
  the `device_*` columns `device` lists
  ([result rows](api/plans/get_plan_results_csv.md#result-rows)). Every
  `parameter_*` and `observation_*` column is read too, into `parameters` and
  `observations`; none is required.

### Columns

Every metric row carries the common columns. Parameter and observation columns
are nullable and populated for the benchmark types that use them. The meanings
below say what a stored value is; which submitted values are refused is the
device protocol, [httpapi.md §2.7](httpapi.md#27-post-benchmarks).

#### Common columns

| Column | Type | Nullable |
|--------|------|----------|
| `result_id` | string | no |
| `job_id` | string | yes |
| `benchmark_id` | string | no |
| `benchmark_type` | string | no |
| `metric` | string | no |
| `client_id` | string | no |
| `device_name` | string | yes |
| `device_form_factor` | string | yes |
| `device_os_name` | string | yes |
| `device_os_version` | string | yes |
| `device_os_build` | string | yes |
| `device_os_security_patch` | string | yes |
| `device_cpu_arch` | string | yes |
| `device_os_family` | string | yes |
| `device_chip_model` | string | yes |
| `device_gpu_model` | string | yes |
| `device_gpu_vram_bytes` | int64 | yes |
| `device_npu_model` | string | yes |
| `device_npu_vram_bytes` | int64 | yes |
| `device_ram_bytes` | int64 | yes |
| `device_battery_level` | int32 | yes |
| `device_power_state` | string | yes |
| `device_power_save_mode` | bool | yes |
| `device_android_cpuset` | string | yes |
| `device_android_cpu_affinity_list` | string | yes |
| `device_android_cpu_affinity_excludes_top_tier` | bool | yes |
| `device_apple_thermal_state_before` / `_after` | list&lt;string&gt; | yes |
| `device_apple_soc_temp_c_before` / `_after` | list&lt;f32&gt; | yes |
| `device_android_thermal_status_before` / `_after` | list&lt;string&gt; | yes |
| `device_android_thermal_headroom_before` / `_after` | list&lt;f32&gt; | yes |
| `device_android_thermal_sensors_before` / `_after` | list&lt;struct&gt; | yes |
| `device_linux_thermal_zones_before` / `_after` | list&lt;struct&gt; | yes |
| `device_hardware_readings` | string | yes |
| `device_os_throttling_readings` | string | yes |
| `readiness_gate_waits` | string | yes |
| `device_time_ms` | int64 | yes |
| `model_name` | string | yes |
| `model_quant` | string | yes |
| `model_params_total_millions` | int32 | yes |
| `model_params_active_millions` | int32 | yes |
| `model_descriptor` | string | yes |
| `model_descriptor_sha256` | string | yes |
| `model_vault_descriptor` | string | yes |
| `model_flags` | string | yes |
| `model_flags_sha256` | string | yes |
| `runtime_name` | string | yes |
| `runtime_version` | string | yes |
| `runtime_descriptor` | string | yes |
| `runtime_descriptor_sha256` | string | yes |
| `runtime_flags` | string | yes |
| `runtime_flags_sha256` | string | yes |
| `runtime_cpu_variant` | string | yes |
| `runtime_speculative_enabled` | bool | yes |
| `benchmark_flags` | string | yes |
| `benchmark_flags_sha256` | string | yes |
| `requested_model_flags` | string | yes |
| `requested_model_flags_sha256` | string | yes |
| `requested_runtime_flags` | string | yes |
| `requested_runtime_flags_sha256` | string | yes |
| `requested_benchmark_flags` | string | yes |
| `requested_benchmark_flags_sha256` | string | yes |
| `benchmark_params` | string | yes |
| `benchmark_params_sha256` | string | yes |
| `client_version` | string | yes |
| `client_configuration` | string | yes |
| `client_extras` | string | yes |
| `value` | f32 | yes |
| `value_stddev` | f32 | yes |
| `unit` | string | no |
| `submitted_at` | timestamp_utc | no |
| `scored_at` | timestamp_utc | no |
| `score_runtime_version` | string | yes |
| `eval_metadata` | string | yes |
| `failed` | bool | yes |
| `failed_reason` | string | yes |
| `failure_source` | string | yes |
| `failure_retriable` | bool | yes |
| `rescored_from` | string | yes |
| `rerun_of` | string | yes |
| `run_submitted_at` | timestamp_utc | yes |

`value` and the device identity columns are nullable because a failure row
measures nothing ([Failure rows](#failure-rows)). `unit` stays required.

#### Per-benchmark parameter columns

| Column | Type | Used by |
|--------|------|---------|
| `parameter_prefill_tokens` | int32 | prefill_throughput, decode_throughput, end_to_end_latency, max_memory_usage |
| `parameter_decode_tokens` | int32 | decode_throughput, end_to_end_latency, vl_throughput, vl_max_memory (null when the entry sets none: one token), asr_throughput, asr_max_memory |
| `parameter_eval_id` | string | eval; vlm_eval's `parameter_lmms_task`; asr_eval's `parameter_asr_task` |
| `parameter_image_width` | int32 | vl_throughput, vl_max_memory |
| `parameter_image_height` | int32 | vl_throughput, vl_max_memory |
| `parameter_text_tokens` | int32 | vl_throughput, vl_max_memory, asr_throughput, asr_max_memory |
| `parameter_num_images` | int32 | vl_throughput, vl_max_memory |
| `parameter_audio_duration_ms` | int32 | asr_throughput, asr_max_memory |
| `parameter_audio_sample_rate_hz` | int32 | asr_throughput, asr_max_memory |
| `parameter_audio_channels` | int32 | asr_throughput, asr_max_memory |
| `parameter_draft_length` | int32 | vl_spec_decode |
| `parameter_dataset_subset` | string | vl_spec_decode |
| `parameter_batch_size` | int32 | prefill_throughput, decode_throughput, end_to_end_latency, vl_spec_decode |
| `parameter_temperature` | float64 | eval, vlm_eval, asr_eval, vl_spec_decode |
| `parameter_top_p` | float64 | eval, vlm_eval, asr_eval |
| `parameter_top_k` | int32 | eval, vlm_eval, asr_eval |
| `parameter_min_p` | float64 | eval, vlm_eval, asr_eval |
| `parameter_seed` | int64 | eval, vlm_eval, asr_eval |
| `parameter_presence_penalty` | float64 | vlm_eval |
| `parameter_repeat_penalty` | float64 | vlm_eval, asr_eval |
| `parameter_prompt_profile` | string | vlm_eval, asr_eval |
| `parameter_num_beams` | int32 | asr_eval |
| `parameter_beam_repetition_penalty` | float64 | asr_eval |
| `parameter_beam_length_penalty` | float64 | asr_eval |

Parameter columns are **never namespaced by benchmark type**: one concept gets
one column wherever it appears, so a `GROUP BY parameter_prefill_tokens` spans
four types.

- `parameter_dataset_subset` is the catalog's `dataset.subset`, NULL for an
  unfiltered entry ([benchmarks.md §2.8.5](benchmarks.md#285-derived-metrics)).
- `parameter_batch_size` is the catalog's value, `1` when the entry omits it.
  NULL on rows written before the column existed, all of which ran at 1.
- The seven sampling columns are the catalog's values, NULL wherever the entry
  pinned nothing and the client's default applied. NULL means "unpinned", never
  a value: `parameter_temperature = 0` asked for greedy decoding, and `IS NULL`
  took the eval-id policy. The shipped eval entries now pin the `0.6` their
  earlier rows ran at unpinned, so group on the benchmark id, not the column, to
  span that boundary. `float64`, so `WHERE parameter_temperature = 0.6` matches
  the literal exactly.
- On a `vlm_eval` row a NULL sampling column is no client default: the entry
  left it at the greedy, neutral setting
  ([benchmarks.md §2.9](benchmarks.md#29-vlm-eval-accuracy)). The two penalty
  columns are NULL on every older row.
- `parameter_prompt_profile` is the prompt profile a `vlm_eval` entry names,
  as written (`internvl`): the model family whose VLMEvalKit prompts its
  samples were worded in. NULL where the entry names none, so every model got
  the dataset's own prompt, and on every older row. An `asr_eval` row always
  holds its profile (e.g. `minicpm_o`): the family whose request each clip became.
- An `asr_eval` row's decoding columns are the decoding it **ran**, not only
  what the entry pins: each knob the entry leaves unset holds its prompt
  profile's card value (a `minicpm_o` row's `parameter_repeat_penalty` is
  `1.02`, a `qwen25_omni` row's `parameter_top_k` is `1`, or `0` when the
  entry pins a temperature above `0`; a `gemma4` row's `parameter_temperature`,
  `parameter_top_p` and `parameter_top_k` are `1.0`, `0.95` and `64`), else the
  off value (`top_p` `1.0`, `min_p` `0.0`, `temperature` `0.0`), as pipette-plan-types'
  `AsrEvalBenchmark::effective_decoding` resolves them
  ([benchmarks.md](benchmarks.md#212-asr-eval-error-rate)). A beam search's
  sampling columns are NULL, as it samples nothing, and its unpinned beam
  penalties are the card's (repetition `1.2`, length `1.0`); a sampled row's
  three beam columns are NULL. Only
  `parameter_seed` stays NULL when unpinned, which is unseeded. A row whose
  profile mgmt has no card for holds only what the entry pins.
  `benchmark_params` still holds the entry's pinned fields alone, so a card
  value is never part of a benchmark's identity.
- `benchmark_params` holds the same parameters in one value: the catalog entry's
  `parameter_*` fields without the prefix, including those no column holds (an
  eval's `max_tokens`, and its `max_context` where the entry sets a context
  bound), as canonical JSON with the `benchmark_type` tag inside. `benchmark_params_sha256`
  is its hex sha256 by plan-types' `descriptor_digest`, so a client reproduces
  it. A key is absent wherever its column reads NULL, except an eval's
  `temperature`, which holds the eval-id rule's value. The corpus is not a
  parameter, so two `vl_spec_decode` entries that differ only in corpus share a
  sha; group by `benchmark_id` for that. A submission's declared
  `benchmark_params` is checked against the entry and not stored.

#### Per-benchmark observation columns

Measured workload facts: not parameters, not metrics. All nullable.

| Column | Type | Used by |
|--------|------|---------|
| `observation_vl_throughput_prefill_tokens` | int32 | vl_throughput |
| `observation_vl_throughput_image_tokens` | int32 | vl_throughput |
| `observation_asr_throughput_prefill_tokens` | int32 | asr_throughput |
| `observation_asr_throughput_audio_tokens` | int32 | asr_throughput |
| `observation_asr_context_sizing` | string | asr_throughput, asr_max_memory |
| `observation_asr_audio_tokens_source` | string | asr_throughput (asr_max_memory only if a client sends it) |
| `observation_asr_beyond_model_audio_limit` | bool | asr_throughput, asr_max_memory |
| `observation_max_swap_bytes` | int64 | all but eval |
| `observation_max_host_bytes` | int64 | all but eval |
| `observation_vl_spec_decode_turns` | int32 | vl_spec_decode |
| `observation_dataset_digest` | string | any type that serves a corpus (today: vl_spec_decode, vlm_eval) |
| `observation_dataset_digest_matched` | bool | any type that serves a corpus |
| `observation_output_digest` | string | any type that traverses a corpus (today: vl_spec_decode) |

**Naming rule.** An observation is namespaced by benchmark type when the fact
exists only for that type (`observation_vl_throughput_*`), and left bare when it
belongs to a mechanism several types share (`observation_max_*_bytes`, the
`observation_dataset_*` pair, `observation_output_digest`), so one query spans
every type that has it.

`observation_asr_context_sizing` and `observation_asr_audio_tokens_source` are
typed out of the submission's `client_extras` (which keeps them too), required at
intake on the types listed, and NULL on failure rows and every other type:

- **Compare an ASR peak only where `observation_asr_context_sizing = 'exact'`.**
  The context was sized from the audio tokens the runtime counted for this clip,
  so the KV cache holds the clip's audio and no more. An `upper_bound` row's
  context holds KV for audio the clip does not have, and a `pinned` row's is the
  plan's size: neither peak is the clip's own
  ([benchmarks.md §2.11](benchmarks.md#211-asr-peak-memory)).
- **`observation_asr_audio_tokens_source`** says how
  `observation_asr_throughput_audio_tokens` was counted: `calibrated` is the
  runtime's own count, marker-free; `measured` is the prompt with the clip less
  the same prompt without it, so it includes the audio markers.

`observation_asr_beyond_model_audio_limit` is typed out of `client_extras` the
same way and required at intake on both ASR types (a boolean; NULL on failure
rows and every other type):

- **`true` marks a speed or memory scenario, not a measurement of the model's
  documented use.** The clip is longer than the model's own documentation says
  it takes (Gemma 4: 30 s of audio). The row's timings and peak are what the
  runtime did with the whole clip, but its transcription quality is not
  established there, and the reference may give no count past the limit
  (Gemma 4 E2B and E4B cut the clip at 30 s). Keep such
  rows apart from the rest, or label them, when comparing models
  ([benchmarks.md §2.10](benchmarks.md#clips-longer-than-30-s)).

`observation_max_host_bytes` and `observation_max_swap_bytes` are the run's peak
resident memory and peak swap, copied onto every row of the submission. Read
them by these rules:

- **Never sum them.** The resident peak and the swap peak need not fall at the
  same instant. A non-zero swap term says the resident peak beside it was
  suppressed by reclaim, and roughly by how much.
- **`0` is a reading, NULL is none.** `observation_max_swap_bytes = 0` means
  the platform sampled swap and the run stayed resident. NULL means nothing
  sampled it: an `eval` run, a client that does not sample memory, or a run
  whose last read missed the process.
- **The observation is not the `max_host_usage` metric.** The metric is the
  measured run's alone, and on llama.cpp under Android and Linux it is the
  swap-aware peak, so it can read above the observation; the observation can
  read above the metric where a cell warms up first, since it covers the
  warm-up's process. Read the metric to compare peak-memory runs, and the
  observations to see what any run held and whether it swapped
  ([benchmarks.md §2.4](benchmarks.md#24-peak-memory-usage)).
- **torch-oai rows from earlier client builds** observed a cgroup peak that
  counts the page cache, so they read higher and are not comparable with newer
  rows; `client_version` tells them apart.

### `model_descriptor` / `runtime_descriptor`

The client's full model and runtime specifications, each a JSON string stored as
a `Utf8` column. The server treats each as **opaque** (partners define their own
runtimes and model formats) and only canonicalizes it: object keys sorted at
every level, insignificant whitespace stripped, so a pattern search over the
column is stable however the client ordered or spaced it. The scalar
`model_name`, `runtime_name` and `runtime_version` stay separate as cheap
grouping and display keys. Three exceptions read inside a descriptor: the model
parameter table ([below](#model-parameter-table)), the vault alias and the vault
label (both below).

`model_descriptor_sha256`, `runtime_descriptor_sha256`,
`benchmark_flags_sha256`, `model_flags_sha256` and `runtime_flags_sha256` are
the hex sha256 of the canonical string, computed mgmt-side, null when the value
is absent: a stable content id for grouping and joins. A descriptor's id is
taken without a vault source's `custom_label`
([plan-ingestion.md §4](plan-ingestion.md#4-the-server-mode-plan-format-summary)),
so a labelled descriptor and the same one unlabelled share an id; group a vault
model's rows by the id, not the string.

The recommended shape is a `pipette-plan-types` `Model` or `Runtime` variant,
which is the source of truth for the fields; a stored descriptor holds the
model's identity fields only, without the per-cell generation flags or an
`auth_token`.

**A synthetic failure** (a job the server retires with no client run) derives
its descriptors from the job body's `spec.model` and `spec.runtime`,
canonicalized the same way, with every `auth_token` omitted. Its scalar grouping
columns are null. These rows are **not reliably joinable to client-submitted
rows on descriptor identity**: the server canonicalizes the raw spec while a
client round-trips it through a typed model, dropping fields its schema does not
describe ([plan-ingestion.md §9](plan-ingestion.md#9-plan-lifecycle)).

**A vault model is recorded under its public alias.** When every vault file the
reported descriptor names has a `huggingface` origin, and the files of one
source come from one repository at one commit, the scorer stores the
HuggingFace descriptor of the same model in `model_descriptor` (pinning the
origin's commit as `revision`, with no `sha256`) and keeps the reported
descriptor, canonicalized, in `model_vault_descriptor`. A vault run and a
HuggingFace run at the same commit therefore share `model_descriptor_sha256`.
On every other row `model_vault_descriptor` is null. The alias's coordinates
come from the vault records, which only mgmt writes
([design/vault-resources.md](design/vault-resources.md)).

**A vault source is recorded with its label.** A vault source that arrives
without a `custom_label` is stored with the `name` its records share, if each is
an upload of the source's `principal_id`, in both descriptors. A label that
arrives is stored as it is. Every source in `model_vault_descriptor` is
unlabelled. Rows stored before the scorer filled the label are given one by
[`fix-vault-labels`](cli.md#pipette-mgmt-fix-vault-labels).

**Both read only the records the job names.** A descriptor is the device's
text, so when it names a vault resource that the body of the job it ran (a
copy's `rescored_from`) does not, the scorer reads no record for that
submission: its rows take neither a label nor an alias, and keep the
descriptors as reported. A job with no stored source, such as a
`requeue-eval` copy, is judged by the first job up its `rescored_from` chain
that has one; a source that is not JSON, or a chain that ends or loops first,
names none. `fix-vault-labels` obeys the same rule per descriptor, starting
from the row's `rescored_from` or `job_id`.

### `benchmark_flags`

The harness configuration a run executed under: readiness gating, request
timeouts, loop detection. Canonical JSON beside the descriptors, with
`benchmark_flags_sha256`. Opaque, so a partner may carry its own harness's
settings under its own keys.

It is separate from `runtime_flags`: `runtime_flags` is what the *runtime* was
configured with, and every field of it changes the number measured.
`benchmark_flags` is what the *harness around* it did, which changes how far the
number can be trusted.

**Resolved, not authored.** A client submits the values it *ran with*, never
the values it was given. A client that ran ungated because something waived the
thermal criterion submits `"skip_thermal": true`, not `null`; a client that
pinned no readiness deadline submits its platform default. A claim's
`spec.benchmark_flags.readiness` may carry `max_wait_secs` and `skip_thermal`,
and a host may waive or default either, so this column is the only record of
what the run applied.

A field a client has no notion of is absent, never "unset". A client with
nothing to report omits the field; a **top-level** empty object is stored as
NULL, so "nothing reported" has one spelling and one grouping bucket. A
*nested* empty object is a different claim and is stored as sent. A missing
`readiness` block means the cell does not gate (evals and peak-memory never
wait on the device).

**Why it matters.** Two submissions of one cell can differ by several percent
because one waited for the device to cool. Grouping on `benchmark_flags_sha256`
makes "compare only runs measured the same way" a join key.

The recommended shape is the cell's `pipette-plan-types` `BenchmarkFlags`, minus
the `runtime_type` / `model_type` / `benchmark_type` axis keys, with the
readiness block resolved:

```
{"readiness":{"max_wait_secs":300,"skip_thermal":false}}
{"http_timeout_seconds":1800,"readiness":{"max_wait_secs":300,"skip_thermal":true}}
{"doomloop":{"max_repeats":12,"window":40}}
```

### `model_flags` / `runtime_flags`

The model-generation and runtime-load configuration a run used:
`enable_thinking` and friends on one side, thread count, GPU layers and context
size on the other. Opaque, canonicalized, and hashed into `model_flags_sha256` /
`runtime_flags_sha256`, like the descriptors, with one difference:
canonicalization is **conditional on the value parsing as JSON**, because a
plain string (`--n-gpu-layers 999`) is a valid report too.

- A value that parses as JSON is stored canonical and hashed from that form.
- A value that does not parse is stored trimmed and otherwise verbatim, and
  hashed as-is.

A top-level empty object is stored as NULL, as for
[`benchmark_flags`](#benchmark_flags). Canonical JSON makes
`runtime_flags_sha256` a join key; a plain string groups only with a
byte-identical string. Rows written before these rules are brought up to date by
[`fix-canonical`](cli.md#pipette-mgmt-fix-canonical).

### `requested_model_flags` / `requested_runtime_flags` / `requested_benchmark_flags`

The flag groups the job **asked for**, beside the groups the run **executed
with**. Each holds one authored group from the job's `spec`, canonical as for
[`model_flags`](#model_flags--runtime_flags), with a `requested_*_flags_sha256`.
A group authored as a plain string takes the plain-string path, so both sides
compare byte for byte. Every `auth_token` is dropped at every depth.

**Server-stamped only**, from the lease the server issued, at the moment it
accepts the result; a client cannot supply them. That is what lets a reader
trust the column. score-eval reads `enable_thinking` back from
`requested_model_flags` and forwards it to the
[scoring service](scoring-service.md#post-score), not the client's report.

**NULL means "no request recorded", never "an empty request".** Four cases:

- an ad-hoc submission answers no job;
- a held submission binds no claim (§4.1), and a later promotion does not fill
  the columns;
- the `spec` names no flags for that axis;
- a failure whose claim's `spec` the server could not read when it accepted it
  (a success is refused instead, see [httpapi.md §2.7.3](httpapi.md#273-errors)).
  The row looks like the third case; the server logs a warning for the `job_id`.

A synthetic failure reads the authored `spec` directly, so the fourth case
cannot reach it.

**The requested and the effective form differ by design.** An authored group
carries a `benchmark_type` / `runtime_type` / `model_type` discriminant that a
client strips, and a client resolves defaults the plan left unset (`skip_thermal`
`null` authored, true or false executed). The pair separates a mis-authored
plan, a silently resolved default and a stripped discriminant, which look the
same in one column. Group on `requested_*_flags_sha256` for runs asked for the
same setup, on the effective `_sha256` for runs that applied it.

### `client_configuration`

The environment the client forwarded into every runtime process it spawned,
from a `--config` file entry for that runtime type. It is resolved client-side
and appears in no plan, so the row is its only record: a knob like
`GGML_CUDA_ENABLE_UNIFIED_MEMORY=0` moves one box's numbers while every other
field stays byte-identical.

```
{"envs":["CUDA_DEVICE_ORDER","CUDA_VISIBLE_DEVICES","GGML_CUDA_ENABLE_UNIFIED_MEMORY=0"]}
```

Forwards are carried **by name only**, except in the `GGML_*` namespace, which
keeps `NAME=VALUE`: ggml's tuning knobs are part of the measurement, and any
other value could be a credential. The server does not parse the split.

**A measurement condition, not part of the cell**: it enters no cell identity,
dedup key or job-matching key. Stored exactly as sent, neither canonicalized nor
hashed, so a top-level empty object is stored rather than collapsed. NULL when
the client had no entry for the runtime it ran (every client without a
`--config` file and every mobile client). Unverifiable: the value is whatever
the client says.

### `client_version`

The version of the client build that ran the benchmark (the harness), stored
verbatim, NULL when not reported. It is not `runtime_version`: the harness
decides how to warm up, how many repetitions to time and how to gate, and those
change the number while every runtime and model coordinate stays fixed. Without
it a harness change looks like a device regression. Opaque: never parsed or
ordered. Unverifiable: a grouping key, not an authentication signal.

### `client_extras`

Whatever a client reports that no other field names, as a JSON object in a
string, stored exactly as sent on failure rows and measured rows alike, NULL
when absent. Neither canonicalized nor hashed; nothing groups, matches, dedups or
scores on it. A key that turns out to matter becomes a field of its own: the
ASR types' `asr_context_sizing`, `asr_audio_tokens_source` and
`asr_beyond_model_audio_limit` are read at intake and stored again as typed
observation columns (see
[Per-benchmark observation columns](#per-benchmark-observation-columns)). A
retriable failure writes no row, so its extras are not kept.

### `device_time_ms`

The milliseconds the device spent on the claimed job, counted by the client on
a monotonic clock from the claim to the moment the result or failure is ready
to submit. For a phone run driven by pipette-ios-proxy, the clock starts at app
launch. Stored as sent (`Int64`) on failure and measured rows, NULL when absent
and in files written before the column. A measurement condition: part of no cell
identity, dedup key or match, and no score uses it.

### `device_hardware_readings` / `device_os_throttling_readings` / `readiness_gate_waits`

Device telemetry as each OS reports it: what the sensors read, what the OS or
firmware said about its own throttling, and what the client's readiness gate
reported before each rep. Each is a JSON array in a string, NULL when absent and
on every failure row.

Opaque: neither canonicalized nor hashed, and part of no cell identity, dedup
key, match or partition. Nothing in the server reads inside an element, so a new
platform or key needs no server change. The recommended shape is in
[design/device-telemetry.md](design/device-telemetry.md).

A value that is not a JSON array is dropped at ingest, so its column reads
NULL. One over 128 KiB (8 KiB for `readiness_gate_waits`) keeps its last whole
elements that fit. So every stored value parses as a JSON array, and each
element is as sent. The end is kept because heat builds over a run, and every
element names its rep, so what is left still says which reps it covers.
`vl_spec_decode` gates before every sample, so its `readiness_gate_waits`
routinely exceeds 8 KiB and keeps only its last waits.

### Legacy thermal columns

The twelve `device_*_thermal_*`, `device_apple_soc_temp_c_*` and
`device_linux_thermal_zones_*` columns are **legacy**, written only while a
client still sends them. The iOS app sends the three fields above; the CLI sends
`readiness_gate_waits` everywhere and the readings on Windows. The CLI
elsewhere, and the Android app, still send the twelve per-platform thermal
fields. The columns are a reported condition, not a controlled axis,
recorded verbatim to each vendor's API with **no cross-platform mapping**. A row
populates only its platform's families (`device_os_name` tells which).

Each family is a `_before` / `_after` pair bracketing each measured repetition:
`_before` the moment the readiness gate clears, `_after` when the rep's timed
work completes. The scalar families hold one element per repetition; the
sensor and zone families flatten every (iteration, sensor) pair into one list.
**A null list means the family was not captured; an empty list means it was
captured and the source reported nothing.** The two round-trip distinctly. No
"worst" value is stored: derive it from the series.

| Field family | Type | Notes |
|---|---|---|
| `device_apple_thermal_state_*` | `List<string>` | Apple `ProcessInfo.ThermalState`: `nominal`, `fair`, `serious`, `critical` (coolest to hottest). Every Apple OS |
| `device_apple_soc_temp_c_*` | `List<f32>` | Apple SoC die temperature in whole °C, from the hottest private IOHID `PMU tdie*` sensor. iOS (`PIPETTE_PRIVATE_THERMAL` builds) and macOS; null elsewhere or when unreadable |
| `device_android_thermal_status_*` | `List<string>` | `PowerManager.getCurrentThermalStatus()` (API 29+) |
| `device_android_thermal_headroom_*` | `List<f32>` | `getThermalHeadroom()` (API 30+): fraction of the thermal envelope in use, `1.0` at the `SEVERE` threshold, may exceed it; higher is worse |
| `device_android_thermal_sensors_*` | `List<Struct>` | Android thermal-HAL per-sensor readings (privileged) |
| `device_linux_thermal_zones_*` | `List<Struct>` | Linux `/sys/class/thermal` per-zone readings |

Both Android enums (`AndroidThermalStatus` for the device, `AndroidThrottlingSeverity`
per sensor) have the levels `none`, `light`, `moderate`, `severe`, `critical`,
`emergency`, `shutdown`, but are distinct upstream types with no mapping
between them, nor to Apple's: Android `critical` is not Apple `critical`.

`device_android_thermal_sensors_*` element:

| Field | Type | Notes |
|---|---|---|
| `iteration` | int32 | Zero-based measured repetition |
| `type` | string | Lowercased Android `TemperatureType` (`cpu`, `gpu`, `battery`, `skin`, `soc`, …; `unknown` when unmapped). The `bcl_*` sensors are excluded: their value is not °C |
| `name` | string | Vendor sensor name, e.g. `cpu-big` |
| `celsius` | int32 | Whole °C, as reported |
| `throttling_status` | string | Per-sensor `AndroidThrottlingSeverity` |

`device_linux_thermal_zones_*` element:

| Field | Type | Notes |
|---|---|---|
| `iteration` | int32 | Zero-based measured repetition |
| `type` | string | sysfs zone `type`, e.g. `x86_pkg_temp`, `acpitz`, `cpu-thermal` |
| `celsius` | int32 | Whole °C, converted from milli-°C |

Temperatures are stored as reported, with no range check. Caveats when reading
them: a device whose OEM never wired up thermal reporting reads `nominal` /
`none` throughout, indistinguishable from a cool run; the device-level signals
cover the whole device, other apps included; and headroom reads null where the
device does not support it. A scalar series is all-or-nothing, because its index
is the iteration: one rep that failed to read (a headroom `NaN`) leaves the
whole series null. Apple state and Android status are each OS's own advisory
band, not the hottest sensor; derive a temperature maximum from the sensor or
zone arrays.

### `rescored_from`

The `job_id` whose recorded completions this row's job copies to score again,
written by a campaign recalculation
([plan-ingestion.md](plan-ingestion.md#carrying-a-campaigns-jobs-over)) and by
[`requeue-eval`](cli.md#pipette-mgmt-requeue-eval). No device ran the new job,
so its device, model and runtime columns repeat the named job's, and so do its
`requested_*_flags`, except on copies `requeue-eval` wrote before it carried
them. It names one level up: re-scoring A gives B pointing at A, and re-scoring
B gives C pointing at B. Server-stamped. NULL on every row a device ran, and on
copies written before the column existed.

### `rerun_of`

The `job_id` this row's job runs again, on a job a campaign re-run minted
([plan-ingestion.md](plan-ingestion.md#carrying-a-campaigns-jobs-over)). A device
ran the new job, so it is a run of its own, with `rescored_from` NULL. It names
one level up. The scorer reads it from the `rerun_of` map of the plans that link
the job, so a client cannot supply it. NULL on every other row, and on rows
scored before the column existed.

### `run_submitted_at`

The time that a device submitted the run that this row scores. On a row that is
not a copy it equals `submitted_at`, failures included. A copy
([`rescored_from`](#rescored_from)) is filed under the day it was written,
because the warehouse refuses writes to an old day, so its `submitted_at` is the
time of the re-score and `run_submitted_at` the time of the device run, even
through a copy of a copy. Server-stamped. NULL on:

- rows written before the column existed, copies included
- a copy that the server staged before copies carried the time, and that the
  scorer took after
- a copy whose chain of copies reaches a record that is gone or is not a
  success

A copy that `requeue-eval` wrote before `rescored_from` existed names no source,
so it reads as a device run: a newer copy of it gets the older copy's time, not
the device run's.

To place a row on the day of its run, use
`COALESCE(run_submitted_at, submitted_at)`. That gives the re-score day on a copy
with a NULL `run_submitted_at`, and the wrong day on a newer copy of a
`requeue-eval` copy that names no source. To place either, follow its
`rescored_from` chain to the row of the device run.

### Failure rows

A failed job holds a row per metric too. The scorer resolves the metrics the
job's benchmark **intended** to produce, writes one row for each, and leaves
`value` NULL. So "how often does this cell fail?" is the same group-by as "how
fast is this cell?":

```sql
SELECT client_id, benchmark_id, failure_source, count(DISTINCT job_id) AS failures
FROM read_parquet('warehouse/results/**/*.parquet', union_by_name = true)
WHERE failed IS TRUE
GROUP BY 1, 2, 3;
```

**Count distinct `job_id`, never rows.** One failure holds as many rows as its
benchmark type has intended metrics: six for `vl_spec_decode` (nine when the
client resolved speculation on before failing), three for `vl_throughput` and
`asr_throughput`, two for `prefill_throughput` and `vlm_eval`, one for the rest. `count(*)` inflates by a factor that
changes with the type. `count(DISTINCT job_id)` also absorbs the append-only
duplicates. A re-scored copy is a new `job_id` for the same device run, so to
count runs keep `rescored_from IS NULL`; a re-run is a run of its own
([`rerun_of`](#rerun_of)). Add `metric` to the `GROUP BY` for a per-metric
breakdown, still counting distinct `job_id`. For rows written before the
`job_id` column, count `DISTINCT regexp_replace(result_id, '_[0-9]+$', '')`.

| Column | Type | Meaning |
|---|---|---|
| `failed` | boolean | `true` on a failure row, `false` on a measured row. NULL in a file written before the column existed |
| `failed_reason` | string | The client's `failure_reason`, or the message the server wrote when it retired the job. NULL on a measured row |
| `failure_source` | string | Which pass produced the failure: `client`, `expired`, `all_clients_denied`, `unmatchable`, or `lease_lost`. NULL on a measured row. A body stored before the field existed resolves from its `client_id`: the reserved `"system"` id reads NULL, any other reads `client` |
| `failure_retriable` | boolean | The client's `retriable` flag. `false` on every synthetic `"system"` failure. NULL on a measured row |

**Read the flag as `failed IS TRUE` / `failed IS NOT TRUE`.** Rows written
before the column read NULL, so `failed = false` and `failed != true` drop them
all.

**Every pre-existing query must add `WHERE failed IS NOT TRUE`.** `avg`, `min`,
`max`, `sum` and `count(value)` ignore NULL and stay correct; `count(*)`,
`count(result_id)` and any per-cell row tally count the failure row and inflate
silently. The mixed query ("mean throughput, and how many results back it") is
the trap.

On a failure row, `value` and the six device-identity columns (`device_name`,
`device_form_factor`, `device_os_name`, `device_os_version`,
`device_chip_model`, `device_ram_bytes`) are NULL: a client-reported failure
sends no device fields, and no device ran a synthetic one. `benchmark_flags`,
`value_stddev` and `score_runtime_version` are NULL too. The `requested_*_flags`
are populated from the authored `spec`. `unit` stays required: resolving the
intended metric resolves its unit.

**No partition needs a rewrite for nullable columns.** The reader resolves each
column by name and type, never by the file's nullability flag. A column may
widen from required to nullable, never narrow back.

#### Which failures reach the warehouse

The day the body was **submitted** addresses a failure row, as it does a
measured row, so a body scored long after the fact targets an old day partition.
The backend closes a partition to writes once its data passes
`warehouse_lock_age_secs` (72 hours by default), at 24-hour granularity, counted
from midnight UTC of the partition's day. The default therefore leaves three day
partitions open: today and the two days before.

`process-submissions` records a failure only while its partition is open. Past
that it archives the body to `processed/` unrecorded and reports the count in
the run summary. A refused write would abort the whole chunk before any body is
marked processed, so one late body would stall every later run. Two
consequences:

- **The failure table starts at deployment.** Failure counts are not comparable
  across the window that held when the column shipped.
- **A held submission promoted late is archived, not recorded**: `unverified
  promote` re-stages through `incoming/`, and the same window applies.

### Model parameter table

The model parameter table maps normalized model names to parameter counts in
millions. It is the model-params rows ([§3.5](#35-model-params)). A row holds a `total`
and an `active` count: equal for a dense model, apart for MoE and
selective-activation models (`LFM2-8B-A1B`: 8340 total, 1500 active).

Keys are normalized: no `org/` prefix, no `:file.gguf` suffix, no `-GGUF`
suffix, no quant tag, and no quantizer marker (`-UD`, `-i1`, `.i1`).

**A row's counts resolve from its descriptors and nothing else.** The names
the parsed `Model` gives its weights are looked up, most specific first; a
**vault** model names its resource id (`vault_0195…`), so add its row under that
id. A row with a `model_vault_descriptor` also resolves under the public alias.
`model_name` is **not** consulted: a client may set it to anything, so a hit on
it proves only that two strings agreed. A descriptor this server cannot parse
resolves to nothing. `fix-model-param` applies the same rule, so a row carrying
only a `model_name` is not repairable.

An unresolved model takes the client's `model_params_*` values, NULL if it sent
none. That fallback is reported: in the `process-submissions` summary, by
`pipette-mgmt models unresolved` across the warehouse, and by `pipette-mgmt
models resolve` for names before any row exists
([cli.md](cli.md#pipette-mgmt-models-resolveunresolvedimport)).
[`examples/model_params_mapping.toml`](../examples/model_params_mapping.toml)
holds counts for the model families this project routinely benchmarks, in the
form `models import` reads.

### Per-run metadata: `eval_metadata`

Per-run context about an eval submission that does not fit on the metric axis,
as a JSON `{key: value}` object, denormalized onto every row of the submission.
Today it holds the count of samples whose `stop_reason` is `failure`, and only
when there is one:

```json
{"samples_failed": 3}
```

An `asr_eval` row carries the scorer's `samples_excluded` (the samples the
Open ASR Leaderboard's row filter left out), and `error_unit` where mgmt does
not know the task. There is no incomplete `asr_eval` row: a run is scored
only with an answer to every sample, and a partial one writes nothing.

```json
{"samples_excluded": 3}
```

A new key needs no schema change. NULL on non-eval rows and in older files.
Consumers that want accuracy over the samples that could be evaluated compute it
from `samples_failed`; see [scoring-service.md](scoring-service.md).

### Served dataset provenance

`vl_spec_decode` and `vlm_eval` benchmarks serve a corpus the client downloads
([benchmarks.md §2.8.1](benchmarks.md#281-the-dataset-object),
[§2.9](benchmarks.md#the-samples-a-device-runs-the-corpus)).

- `observation_dataset_digest`: the sha256 of the corpus tar the client
  verified.
- `observation_dataset_digest_matched`: whether that equalled the digest the
  catalog pins. **For `vl_spec_decode` a mismatch is recorded, not rejected**;
  a `vlm_eval` is not scored on one, so its scored rows always match.

So **a query that compares results across devices must filter it**:

```sql
SELECT client_id, value
FROM read_parquet('warehouse/results/**/*.parquet', union_by_name = true)
WHERE benchmark_id = 'vl_spec_decode_mmspec_test_gqa_k8'
  AND metric = 'accept_length'
  AND observation_dataset_digest_matched IS NOT FALSE;
```

`IS NOT FALSE`, not `IS TRUE`: NULL means the client reported no digest, and
excluding it drops every row from a client that predates the column.

### Dataset cache: `dataset-cache/artifacts/`

Repacked corpora, keyed by the sha256 of the tar, under the `[storage]` root:

```
dataset-cache/artifacts/
├── {digest}.tar     ← the corpus archive, in its entry's `format`; written once by `dataset-cache build`, never deleted
└── {digest}.json    ← how it was built, and the latest upstream probe verdict
```

`{digest}.json` carries `digest`, `bytes`, `format`, `adapter`, the pinned
`upstream` table, `subset?`, `samples?` (an evals-server corpus's sample
count, as its build counted it), `built_at`, and `probe {state, checked_at,
since, observed_identity?, detail?}`. `dataset-cache probe` rewrites it every minute,
and `serve` hands out a tar only while its probe reads `ok` and is no older than
`dataset_probe_max_age_secs`. The build writes the tar before the JSON, so
metadata always describes a tar that exists. Two benchmarks pinning the same
corpus share one artifact. An organization's private pins share the prefix, so
the catalog writer refuses a digest another organization's entry already pins,
unless a public entry pins the same dataset, and any digest a public entry pins
differently. Nothing deletes from this prefix: an artifact whose
upstream stopped backing it stays, unserved. See
[design/vl-spec-decode-dataset-delivery.md](design/vl-spec-decode-dataset-delivery.md).

**Size the prefix for the asr_eval corpora.** An asr-eval corpus holds every
clip of a set once as a WAV (16-bit PCM where exact, else 32-bit float):
the largest set's is many gigabytes per prompt profile, and the full catalog's
corpora run to well over a hundred gigabytes together. Size the
`[storage]` volume or bucket for that beside the VL corpora. A build streams
the evals server's body and keeps no copy of it, but its scratch (the system
temporary directory, `TMPDIR`) holds the media and the packed tar, about twice
the corpus being built: give it room for twice the largest. asr-eval corpora build
one at a time, after the others, each under `dataset_asr_build_timeout_secs`.

### Schema evolution

Schema evolution is forward-compatible for nullable columns: an older file may
omit a newly added nullable column, and readers treat it as `null`. It is
**not** backward-compatible, and nothing refuses the mismatch: the writers
(`write_partition`, and the S3 store's tail rewrite) read a partition's tail
part through the image's own schema, so a column that image does not know (one
added later, or one removed or renamed since) is dropped silently when the tail
part is rewritten. A part never rewritten keeps it, and a `union_by_name` read
still shows it.

The schema is one for every benchmark type, so a deploy that adds columns is
**roll forward only** once it has scored anything: every file it appends to,
whatever the type, gains the new columns. An older image appends to such a file
without error but strips the new columns from the tail part it rewrites, and it
cannot read a partition holding a benchmark type it does not know (for the ASR
types, `asr_throughput` or `asr_max_memory`). Rolling back past such a deploy
means migrating or deleting those files first.

The ASR columns shipped under their current names. Before they merged,
`observation_asr_throughput_prefill_tokens` was called
`observation_asr_throughput_prompt_tokens` for a while. Only local development
workspaces ran that build, and no shared warehouse did, so no file holds the
earlier name and there is nothing to migrate. A file that does hold it loses
that column when its part is rewritten, like any other renamed column.

## 6. Eval sample results contract

For eval benchmarks, the scorer writes per-sample results beside the warehouse
metrics: the prompt served, the completion produced, and whether it was scored
correct, one row per sample.

`warehouse/eval_sample_results/{job_id}.parquet`

Each file is a self-contained Parquet dataset written once per scored eval job.
The keyspace is flat: `job_id` is globally unique, and both readers
(`pipette-mgmt` and `pipette-duckdb`) resolve a path from `job_id` directly.
Only `process-submissions` writes it.

The invariant is **at most one file per `job_id`**. A retry writes a new file
and atomically replaces the old one. Both the warehouse write and this write
must succeed before the submission moves to `processed/`.

### Parquet schema

| Column | Type | Nullable | Description |
|--------|------|----------|-------------|
| `id` | string | no | Sample ID (matches `completions[].id` in the submission and `samples[].id` from the evals server) |
| `messages` | string | no | JSON-encoded prompt messages array |
| `completion` | string | no | Model-generated text from the submission |
| `is_correct` | boolean | no | Whether the evals server scored this sample as correct |
| `failed` | boolean | yes | `true` if the client-side runtime crashed mid-completion for this sample. Derived from `stop_reason == failure`. Nullable so older files read back as `false` |
| `failed_reason` | string | yes | Description of the failure when known. `null` for `failed = false` rows |
| `stop_reason` | string | yes | Canonical stop reason: `eos` \| `truncated` \| `doom_loop` \| `failure` \| `unknown`. `null` = never labelled (distinct from `unknown`). The sole source of truth for failure. See the [canonical enum](scoring-service.md#per-sample-stop_reason-canonical) |
| `stop_reason_source` | string | yes | Provenance of `stop_reason`: `recorded` (the client sent it) \| `derived` (reconstructed later). `null` when `stop_reason` is `null` |
| `stop_detail` | string | yes | The raw signal behind `stop_reason`: crash detail for `failure`, the unclassified `stop_type` for `unknown`, the trigger for `doom_loop`. `null` when unreported |
| `completion_tokens` | int64 | yes | Output token count for the sample. Pairs with `stop_reason` to separate `eos` (< cap) from `truncated` (== cap, or < cap when the entry's `parameter_max_context` stopped the sample). `null` when unreported |
| `measures` | string | yes | JSON of the scorer's per-sample measurement: an `asr_eval` sample's share of the alignment, sum-safe over a job's rows (a scored unit's owner carries the counts and normalized `reference` / `hypothesis`, an Earnings22 call's other chunks only `{"unit"}`; see [scoring-service.md](scoring-service.md#post-score)). `null` for the accuracy evals and in older files |
| `chat_template_kwargs` | string | yes | JSON of the sample's chat-template arguments as the scorer returned them (an `asr_eval` Voxtral sample's `transcription` mode and `language`), so the row shows the templating request. `null` for a sample served without any, and in older files |

`messages` is the serialized messages array of the evals server's samples
endpoint, `[{"role": "user", "content": "..."}]`; MCQ evals have one user
message, tool-calling evals a system message and a user message. A `vlm_eval`
row keeps each message's `text` parts and drops its `image_url` parts, whose
base64 images are too large to store; an `asr_eval` row keeps its
`input_audio` parts in place with their `data` emptied, so a stored sample
still shows whether the prompt came before or after its clip. How the scorer
joins prompts, completions and scored samples is
[scoring-service.md](scoring-service.md).

## 7. Backend configuration

Four config sections choose where data lives
([`crates/pipette-core/src/config.rs`](../crates/pipette-core/src/config.rs) is
the key reference, [`examples/config.toml`](../examples/config.toml) the
annotated example):

- `[storage]`: the object store for the catalog, submissions, the warehouse,
  eval sample results, blobs and the dataset cache, and, without `[database]`,
  the records of §2 marked `[storage]`.
- `[auth_storage]` (required): without `[database]`, the identity records of §2
  marked `[auth_storage]`.
- `[todo_storage]`: the job queue. Defaults to `[storage]`, which is acceptable
  for `local_fs` only.
- `[database]`: PostgreSQL for every record (§1.1).

```toml
# Local filesystem (default): [todo_storage] not required, shares [storage]
[storage]
backend = "local_fs"
data_dir = "./data"

[auth_storage]
backend = "local_fs"
data_dir = "./data"

# S3 / S3-compatible: separate buckets per section.
# [todo_storage] must be an S3 Express One Zone bucket (§8).
[storage]
backend = "s3"
bucket = "my-data-bucket"

[auth_storage]
backend = "s3"
bucket = "my-auth-bucket"

[todo_storage]
backend = "s3"
bucket = "my-todo-bucket--use1-az4--x-s3"  # must be Express One Zone

# Records on PostgreSQL; the PG* environment variables fill absent keys
[database]
```

S3 credentials are read from the environment (`AWS_ACCESS_KEY_ID`,
`AWS_SECRET_ACCESS_KEY`, IAM roles), never from the config file.

## 8. Architectural decisions

### todo/ requires S3 Express One Zone

The job queue relies on atomic rename for two operations: promoting a complete
job file from `tmp/` to `avail/`, and claiming a job from `avail/` to `leased/`.
On a local filesystem both are single syscalls. On S3, atomic rename exists only
as the `RenameObject` API of
[S3 Express One Zone](https://aws.amazon.com/s3/storage-classes/express-one-zone/).
Standard S3's copy and delete breaks the no-double-assignment guarantee: two
claimers can both copy a job before either deletes it.

Atomic rename is a simple, correct primitive, and the storage-class cost does
not matter for small files at modest request volume. Supporting standard S3
would take a fixed `leased/{job_id}.json` key with conditional writes
(`If-None-Match: *` on claim, `If-Match: <etag>` on heartbeat) through the claim
and heartbeat paths; that redesign is the path if Express One Zone (single-AZ,
limited regions) is unacceptable ([queue.md](queue.md)). The rest of the data
has no such constraint.

A directory bucket also restricts listing: one delimiter, `/`, only a prefix
that ends in it (any other prefix is `InvalidRequest`), and no server-side
`start-after`. So `avail/` gives each job its own directory and `leased/` each
client its own, and a single listing isolates one job's entry or one client's
leases.

**Validate iff you rename.** When `[todo_storage]` is `s3`, a process that
renames against `todo/` (`serve`, `queue-maintenance`, and the `campaigns`
commands that file a plan) probes the bucket at startup: it stages an object
under `todo/tmp/`, renames it, and requires the destination to hold it and the
source to be gone, deleting both keys whatever the verdict. A `NotImplemented`
rename, any other error, an empty destination, a source still present (a copy,
not a move), or a failed read-back refuses the start. The claim path reads a
failed rename as a lost race, so without the probe a misconfigured bucket would
leave every job unclaimable and report nothing. The `clients` admin commands
never rename and skip the probe. On `local_fs` the probe is a no-op.

### `[storage]` and `[auth_storage]` require conditional PUT

Four mechanisms rest on a create-if-absent write being *refused* when the object
exists:

* the storage mutate lock, `locks/mutate.lock` in `[storage]`,
* the exactly-once pre-auth key spend, `client-preauth/{key_id}.spent` in
  `[auth_storage]` (§3),
* the duplicate-job content key, `content-keys/v1/{digest}` in `[storage]` (§2).
  This requirement is why the index is not in `[todo_storage]`, which is probed
  for rename and not for conditional `PUT`, and
* the synthetic failure record, written to `submissions/incoming/{job_id}.json`
  in `[storage]` by `queue-maintenance`.

The fourth carries the highest cost: a synthetic failure and a client's real
submission share one key, and an endpoint that ignores `If-None-Match` lets the
failure overwrite a measurement no later run can reconstruct. The others cost a
duplicate job, a doubly registered pre-auth key or a dropped row.

The lock and the content key also need a compare-and-swap (`If-Match` on the
version) to take over from a crashed holder or a gone job, so that of two
writers only the one whose version still matches wins.

Real S3 honors both headers. S3-compatible endpoints (MinIO, Ceph, R2, older
gateways) vary, and an ignored header answers `200`, which the caller reads as
winning the race: two commands interleave a read-modify-write over one Parquet
partition, or one pre-auth key registers two clients, and nothing reports it.

So a process that depends on these writes probes its backend at startup and
**refuses to start** unless they are honored. The probe writes a randomized key
under `probe/` twice with `If-None-Match: *` and requires the second write to be
refused; where the compare-and-swap is needed too, it rewrites the object with
different bytes and requires an `If-Match` against the superseded version to
fail `Precondition`. An endpoint that returns no ETag is fatal there. Any other
error propagates. The probe object is deleted on every path, so the credentials
need `s3:DeleteObject`.

On a **versioned** bucket each probe leaves two to four noncurrent objects under
`probe/`, and the locked batch commands probe on every invocation. Expire them
with a lifecycle rule on `<prefix>/probe/` covering noncurrent versions and
expired delete markers, or set `probe_backend_capabilities = false` once the
backend is known to conform (the skip is logged; the rename probe is
unaffected).

**Validate iff you depend on a write being exclusive.** The locked batch
commands (`process-submissions`, `score-eval`, `fix-*`, `requeue-eval`) probe
`[storage]` for both writes before taking the lock. `serve` and the `campaigns`
commands that file a plan probe `[storage]` for both, because ingestion claims
and takes over content keys. `serve` also probes `[auth_storage]` for
create-if-absent alone. The auth store's compare-and-swap is best-effort (it
persists a repaired client record and logs every outcome), so an endpoint that
ignores `If-Match` is accepted there, at the cost of a repair that can overwrite
a concurrent client-record change. Read-only commands and the `clients` admin
commands skip the probe. On `local_fs`, exclusive creation is a kernel guarantee
and the probe is a no-op.

Like the rename probe, this shows the endpoint refuses a conflicting write at
startup, not that it does so correctly under concurrency.

### mark_processed is not atomic on S3

S3 has no rename, so `mark_processed` copies and then deletes. A crash between
the two leaves the submission in both `incoming/` and `processed/`, so it is
scored again on the next run and its rows are appended again; reads keep only
the latest scoring run (§5), so the duplicate is invisible to them.

### Credentials in environment, not config

S3 credentials come from environment variables or IAM roles, never the config
file: no secrets in version-controlled config, and every AWS credential source
(env vars, instance profiles, ECS task roles, SSO) works through the standard
`object_store` / AWS SDK chain.

### Multiple collector instances

Multiple `pipette-mgmt serve` instances can accept submissions to the same
bucket concurrently. Each submission has a unique `job_id`, so write keys never
collide, and no coordination is needed between them.
