# pipette-mgmt

A Rust/axum service that runs benchmarks on a fleet of edge devices: it files
plans of jobs, hands them to devices, accepts their results, scores them, and
writes the scored metrics to a Parquet warehouse. [README.md](../README.md) is
the map of the docs.

## 1. Processes

One binary, `pipette-mgmt`, runs as these processes:

| Process | Runs | Does |
|---|---|---|
| `serve` | always | the HTTP API: registration, the catalog, claims, heartbeats, result submissions, and the operator routes |
| `process-submissions` (alias `score`) | fast cron | routes evals into the score queue, scores everything else, and finalizes scored evals into the warehouse |
| `score-eval` | slow cron | calls the scoring service for each queued eval and stages the result |
| `queue-maintenance` | cron, every minute | recycles lapsed leases, expires jobs, rebuilds the eligible index, reconciles plan status, collects orphans |
| `dataset-cache` | cron (`probe`), on demand (`build`) | builds and probes the corpora that `vl_spec_decode` and `vlm_eval` benchmarks serve |

## 2. Backends

- `[storage]`: the object store (local filesystem or S3) for the catalog,
  submissions, the warehouse, vault blobs and the dataset cache.
- `[auth_storage]`: without `[database]`, the identity records.
- `[todo_storage]`: the job queue; an S3 Express One Zone bucket in production.
- `[database]`: PostgreSQL for the records (plans, campaigns, identities,
  organizations, vault records). Production runs it.

[storage.md](storage.md) is the layout and the contract of each.

## 3. The flow

1. **Plan.** A campaign write files a plan; ingestion expands it into jobs,
   merges duplicates, and records the plan
   ([plan-ingestion.md](plan-ingestion.md)).
2. **Queue.** The jobs are promoted into `todo/avail/`, and
   `queue-maintenance` writes the eligibility markers that say which device may
   take each ([queue.md](queue.md)).
3. **Claim.** A device claims a job by an atomic rename into `todo/leased/`,
   heartbeats while it runs, and submits the result.
4. **Submit.** The body lands in `submissions/incoming/`, success and failure
   alike ([storage.md §4](storage.md#4-submission-contract)).
5. **Score.** `process-submissions` scores non-eval bodies at once. An eval
   goes through `score-queue/to_do/`, `score-eval` calls the scoring service
   ([scoring-service.md](scoring-service.md)), and the result waits in
   `score-queue/to_finalize/`.
6. **Warehouse.** `process-submissions` appends the metric rows to
   `warehouse/results/`, writes eval samples to
   `warehouse/eval_sample_results/`, and archives the body to
   `submissions/processed/` ([storage.md §5](storage.md#5-warehouse-contract)).

A submission's location is its state: no field inside the JSON records it.

## 4. Two crons

The `/score` call takes minutes, so it has its own pass. `process-submissions`
holds the `mutate` lock briefly and never calls `/score`; `score-eval` holds its
own `score-eval` lock. The two locks are independent: a long `/score` call never
blocks `process-submissions` (or `fix-*` / `requeue-eval`) from writing the
warehouse, and a second `score-eval` run exits at once rather than scoring
twice. `process-submissions` is the only warehouse writer; `score-eval` writes
only the score-queue stages.

## 5. Idempotency

There is no cross-store transaction on S3, so every hop is **at-least-once and
idempotent**: each step writes the next location *before* deleting the previous
one, so a crash leaves a duplicate in progress, never a lost job.

- A re-routed eval overwrites its `to_do` object with the same content and
  deletes `incoming/` again.
- `score-eval` skips the `/score` call when a `to_finalize/{job_id}` entry
  already exists, so the expensive call is not repeated.
- The warehouse is append-only and readers keep the latest scoring run
  (`max(scored_at)` per `job_id`), so finalizing a job twice writes rows that
  supersede the old ones on read.
