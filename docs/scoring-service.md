# Scoring service contract

The mgmt server delegates eval scoring to an external **scoring service**
configured via `evals_server_url`. Any service that implements the two
endpoints below can be plugged in.

## Endpoints

| Method | Path | Purpose |
|---|---|---|
| `POST` | `/score` | Score completions — the only endpoint on the scoring hot path |
| `GET`  | `/evals/{eval_id}/datasets/{dataset_name}/samples` | Prompt fetch for `GET /benchmarks/{id}` (client-facing) |

---

### `POST /score`

Scores client completions against ground truth. This is the core scoring
endpoint used by `pipette-mgmt score-eval`.

**Request**

```json
{
  "eval_id": "math_500",
  "dataset_name": "2026.06.1",
  "completions": [
    {"id": "sample_001", "completion": "The answer is \\boxed{4}."},
    {"id": "sample_002", "completion": "\\boxed{42}"}
  ]
}
```

| Field | Type | Description |
|---|---|---|
| `eval_id` | string | Eval identifier, from `parameter_eval_id` in the benchmark catalog |
| `dataset_name` | string | Dataset name, from `parameter_dataset_name` in the benchmark catalog |
| `completions` | array | Completions to score; ids must be unique within the array |
| `completions[].id` | string | Must match a sample `id` from the samples endpoint |
| `completions[].completion` | string | The model's output text |

A completion may also carry the mgmt-internal per-sample metadata
`failed` / `failed_reason` (see below) and `stop_reason` /
`completion_tokens` (see [the stop_reason enum](#per-sample-stop_reason-canonical)).
These are stripped before the `/score` call and re-injected onto the
per-sample parquet rows afterward.

Duplicate ids in `completions` are a 400 from the scoring service. The mgmt
server rejects duplicates at submission time as well (`POST /benchmarks`), so
this case should not occur on the hot path. As a safety net for legacy
submissions written before that gateway check existed, the scoring cron
deduplicates locally (first occurrence wins) and logs a `warn` with
`job_id`, `original`, and `unique` before posting to `/score`.

**Failed completions are forwarded but stripped.** Client submissions may
include completions with `failed: true` and an optional `failed_reason`
(see [pipette-clients#103](https://github.com/Liquid4All/pipette-clients/pull/103))
to flag samples where the local runtime crashed mid-completion. These
fields are mgmt-internal metadata; the scoring service has no contract
for them. mgmt **forwards every completion** to `/score`, including
failed ones, but **strips** the `failed` / `failed_reason` fields from
the wire request — only `id` and `completion` are sent. Failed samples
typically arrive with `completion: ""`, so the scorer returns
`is_correct: false` for them, just like any other empty response. The
mgmt server re-injects the `failed` / `failed_reason` metadata onto the
per-sample parquet rows by id lookup after the scoring round-trip.

#### Per-sample `stop_reason` (canonical)

pipette-mgmt owns the canonical `stop_reason` enum. Every other repo in
the pipette suite references this definition (pipette-clients emits it,
pipette-scores backfills it, pipette-datasheet / pipette-duckdb /
pipette-dashboard consume it). The column lives on
`eval_sample_results` and is **nullable**.

`stop_reason ∈ { eos, truncated, doom_loop, failure, unknown }`:

| Value | Meaning |
|---|---|
| `eos` | Model emitted EOS — completion tokens **< cap**. |
| `truncated` | Hit the output-token cap — completion tokens **== cap** (`n_predict = parameter_max_tokens`, e.g. 8192 = max **output** tokens, not the context window). |
| `doom_loop` | Client aborted the generation on runaway repetition (client-only signal). |
| `failure` | Empty completion / runtime crash. The sole source of truth for a failed sample; the legacy `failed` flag is not consulted. |
| `unknown` | Labelling was attempted but the reason is indeterminate. |

`NULL` (distinct from `unknown`) means the sample was **never
labelled** — e.g. a client that didn't report a stop reason.

Companion columns:

- `stop_reason_source ∈ { recorded, derived }` — `recorded` when the
  reason was captured at generation by the client (or derived from the
  client's `failed` flag), `derived` when reconstructed after the fact
  (e.g. a tokenizer-based backfill). `NULL` whenever `stop_reason` is
  `NULL`.
- `stop_detail` (string, nullable) — free-form observation behind
  `stop_reason`: the crash detail for `failure`, the unclassified
  `stop_type` for `unknown`, the trigger for `doom_loop`; normally empty
  for a clean `eos` / `truncated`. Generalizes `failed_reason`.
- `completion_tokens` (int, nullable) — output token count for the
  sample, paired with `stop_reason` to distinguish `eos` from
  `truncated`.

On the submission wire these ride on each `completions[]` entry as
optional `stop_reason` / `stop_detail` / `completion_tokens` keys added
client-side. All default to absent so submissions from clients that don't
report them still deserialize — the columns simply land as `NULL`. Like
`failed` / `failed_reason`, they are **not** part of the `/score`
contract: mgmt strips them from the request and re-injects them onto the
per-sample parquet rows, additionally stamping `stop_reason_source =
recorded`. `stop_reason` is the sole source of truth for a failed sample;
the legacy `failed` flag is not consulted, and `failed` / `failed_reason`
are derived from `stop_reason == failure`. `stop_detail` prefers the
client's value, falling back to `failed_reason` on a failure so the crash
detail is never lost. There is **no** eval-level aggregate — "% truncated
per eval" is a query-time `GROUP BY` downstream.

**Response** `200 OK`

`context` is a flat `string → JSON value` map of eval-specific aggregates (numbers typical, other scalars allowed).

```json
{
  "runtime_version": "1.2.3",
  "context": {"accuracy_EN": 0.95, "accuracy_FR_FR": 0.82},
  "scored_samples": [
    {
      "id": "sample_001",
      "messages": [{"role": "user", "content": "What is 2+2?"}],
      "completion": "4",
      "is_correct": true
    },
    {
      "id": "sample_002",
      "messages": [
        {"role": "system", "content": "You are a helpful assistant."},
        {"role": "user", "content": "Capital of France?"}
      ],
      "completion": "Paris",
      "is_correct": true
    }
  ]
}
```

| Field | Type | Description |
|---|---|---|
| `runtime_version` | string | Scorer version, stored as `score_runtime_version` in the warehouse |
| `scored_samples[].id` | string | Matches a request `completions[].id` |
| `scored_samples[].messages` | array | Prompt in chat-message format from the dataset |
| `scored_samples[].completion` | string | Echoed from the request |
| `scored_samples[].is_correct` | bool | Scoring verdict |

The mgmt server derives `total`, `correct`, and `accuracy = correct / total`
(0 when `total` is 0) from `scored_samples`, and stores accuracy as a metric
with unit `ratio`. **Failed completions are not excluded from the
denominator** — they're scored alongside everything else and almost
always come back `is_correct: false` because their `completion` was
empty, which is what we want. Consumers that need an "accuracy over
samples that could actually be evaluated" can compute it themselves
from `samples_failed` (see `eval_metadata` below).

The `messages`, `completion`, and `is_correct` fields are written
verbatim to `warehouse/eval_sample_results/{job_id}.parquet`,
along with the client-side `stop_reason` / `stop_detail` / `completion_tokens`
and the retiring `failed` / `failed_reason` metadata re-injected by id — no
client-side join needed downstream.

**`eval_metadata`** is a free-form `{key: value}` JSON object
that mgmt stamps onto every warehouse row of an eval submission when
there's per-run metadata worth recording but it isn't a scored metric
on its own. Currently it carries `{"samples_failed": N}` when any
completion ended in `failure` — counted solely by `stop_reason ==
failure`; future per-run metadata can be added without a warehouse schema
change. Stored as a string in the `eval_metadata` parquet column; nullable
so non-eval rows and rows without metadata read back as `NULL`.

**Verification: one-to-one between request and response.** mgmt
builds a set of every requested completion id before the `/score`
call and removes from it as each `scored_samples[].id` is processed.
If any requested id is missing from the response, or if the
response includes an id that was never requested, the cron `bail`s
with `scorer response mismatch: dropped {...}, unknown {...}` and
the job stays in `incoming/` for human investigation. No silent
recovery — a dropped row would skew the accuracy denominator
(`correct / (n - k)`) with no warning, and an unknown id signals a
contract violation. The verification covers every completion, not
just failed ones — failed rows aren't a special case here.

**Empty submission rejection.** If a submission's `completions`
array is empty after dedup, mgmt bails before calling `/score` with
`eval submission has no completions to score`. Without this guard
`accuracy = correct / total` would silently emit `0 / 0 = 0.0`, an
unrecoverable masking of the anomaly downstream.

---

### `GET /evals/{eval_id}/datasets/{dataset_name}/samples`

Returns prompts for a given eval/dataset. Called by the mgmt server when a
client fetches `GET /benchmarks/{benchmark_id}` for an eval-type benchmark —
the `samples` array is spliced into the benchmark response so clients know
what to complete.

**Response** `200 OK`

```json
{
  "samples": [
    {
      "id": "sample_001",
      "messages": [{"role": "user", "content": "What is 2+2?"}]
    }
  ]
}
```

| Field | Type | Description |
|---|---|---|
| `samples[].id` | string | Unique sample identifier |
| `samples[].messages` | array | Prompt in chat-message format |

---

## How the mgmt server uses the contract

1. `GET /benchmarks/{benchmark_id}` fetches eval samples from
   `/evals/{eval_id}/datasets/{dataset_name}/samples`.
2. `POST /benchmarks` stores completions in `submissions/incoming/`.
3. `process-submissions` routes eval submissions to
   `submissions/score-queue/to_do/`.
4. `score-eval` posts each queued eval to `/score` and stages the response in
   `submissions/score-queue/to_finalize/`.
5. A later `process-submissions` finalizes the staged result: warehouse
   `accuracy`, per-sample `eval_sample_results/`, `score_runtime_version`, and
   any `eval_metadata`. Per-sample stop metadata is re-injected from the client
   submission; see [the stop_reason enum](#per-sample-stop_reason-canonical).

## Error handling

- Non-success status from `POST /score` causes the submission to stay in
  `score-queue/to_do/` for retry on the next cron run. Non-success status from
  the samples endpoint fails that benchmark fetch request; no submission has
  been queued yet.
- HTTP timeout is configurable via `http_timeout_secs` (default 600 seconds).
- **Service down.** Connection, DNS, or timeout failures on `POST /score` pause
  `score-eval` after staging any completed work. The command exits
  successfully; the next run resumes from `to_do/`. `serve` still starts while
  the scorer is down, though eval sample proxying and `score-eval` depend on it.

## Design notes

- **Stateless** — the scoring service receives completions, scores them, and
  returns results. It does not need to track submissions or jobs.
- **`id` is the join key** — scored samples must echo the ids submitted in
  the request. Mismatches cause the submission to fail.
- **`eval_id` and `dataset_name` come from the catalog** — they are defined
  in the benchmark TOML, not by the client. A scoring service implementation
  only needs to handle the `(eval_id, dataset_name)` pairs that are configured.
- **One round trip per submission** — the POST response contains everything
  needed for persistence (metrics + per-sample audit rows + runtime version),
  so no follow-up calls are required.
