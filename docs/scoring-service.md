# Scoring service contract

The mgmt server delegates eval scoring to an external **scoring service**
configured via `evals_server_url`. Any service that implements the two
endpoints below can be plugged in. The pipeline that calls them is in
[architecture.md §3](architecture.md#3-the-flow).

## Endpoints

| Method | Path | Purpose |
|---|---|---|
| `POST` | `/score` | Score completions: the only endpoint on the scoring hot path |
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
    {"id": "sample_001", "completion": "The answer is \\boxed{4}.", "stop_reason": "eos"},
    {"id": "sample_002", "completion": "\\boxed{42}"}
  ],
  "enable_thinking": true
}
```

| Field | Type | Description |
|---|---|---|
| `eval_id` | string | Eval identifier, from `parameter_eval_id` in the benchmark catalog |
| `dataset_name` | string | Dataset name, from `parameter_dataset_name` in the benchmark catalog |
| `completions` | array | Completions to score; ids must be unique within the array |
| `completions[].id` | string | Must match a sample `id` from the samples endpoint |
| `completions[].completion` | string | The model's output text |
| `completions[].stop_reason` | string, optional | How generation ended, as the client recorded it (see [the stop_reason enum](#per-sample-stop_reason-canonical)). Omitted when the client reported none |
| `enable_thinking` | bool, optional | The `enable_thinking` the job's model flags asked for, read from the submission's server-stamped [`requested_model_flags`](storage.md#requested_model_flags--requested_runtime_flags--requested_benchmark_flags): an `eval`'s and a `vlm_eval`'s alike. Omitted when the job set none (a model with no reasoning mode), or when no request was recorded (an ad-hoc submission, for one). An `asr_eval`'s is its recipe's, always `false`: every ASR run has thinking off, and the scorer refuses `true` |
| `prompt_profile` | string, optional | A `vlm_eval`'s, when its entry names one; always an `asr_eval`'s, whose scorer cleans each answer as that family's runtime does |
| `samples_digest` | string, optional | A `vlm_eval`'s and an `asr_eval`'s: the entry's pinned samples digest (`sha256:` and its `[dataset.upstream] identity`), sent once the submission reported the pinned corpus. The scorer answers `409` when it no longer serves those samples, and the job stays queued ([benchmarks.md](benchmarks.md#the-samples-a-device-runs-the-corpus)) |

The two optional keys say how a sample was generated, which its text alone
cannot. When the prompt opens a reasoning block and generation stops at the
output-token cap or the context bound, the completion is raw reasoning with no
marker to strip; only `enable_thinking` with `stop_reason: "truncated"` tells it
apart from a finished answer. A scoring service treats both as optional, and absent means
"grade without it". It also ignores keys it does not know, so mgmt can add one
ahead of the scorers that read it.

Only `id`, `completion` and, when the client recorded one, `stop_reason` are
sent per completion. mgmt forwards every completion, a failed one included,
and strips the rest of its per-sample metadata (`stop_detail`,
`completion_tokens`, the legacy `failed` / `failed_reason`), which the scorer
has no contract for. A failed sample typically arrives with `completion: ""`,
so the scorer returns `is_correct: false` for it, as for any other empty
response.

Ids are unique within `completions`; a scoring service may refuse duplicates
with a `400`.

#### Per-sample `stop_reason` (canonical)

pipette-mgmt owns the canonical `stop_reason` enum. Every other repo in
the pipette suite references this definition (pipette-clients emits it,
pipette-scores receives it on each `/score` sample, pipette-datasheet /
pipette-duckdb / pipette-dashboard consume it). The column lives on
`eval_sample_results` and is **nullable**.

`stop_reason ∈ { eos, truncated, doom_loop, failure, unknown }`:

| Value | Meaning |
|---|---|
| `eos` | Model emitted EOS before either bound: completion tokens **< cap**, and on an entry with `parameter_max_context`, prompt plus completion **< that bound**. |
| `truncated` | Hit the output-token cap: completion tokens **== cap** (`n_predict = parameter_max_tokens`, e.g. 8192 = max **output** tokens, not the context window). Or, on an entry with `parameter_max_context`, hit that context bound: prompt plus completion **== `parameter_max_context`**, so completion tokens are **< cap** ([benchmarks.md](benchmarks.md#the-context-bound)). |
| `doom_loop` | Generation stopped on runaway repetition. vLLM reports it as `finish_reason` `repetition` on an eval cell whose `repetition_detection` runtime flag is set; older clients' own detectors wrote it too. |
| `failure` | Empty completion / runtime crash. The sole source of truth for a failed sample; the legacy `failed` flag is not consulted. |
| `unknown` | Labelling was attempted but the reason is indeterminate. |

`NULL` (distinct from `unknown`) means the sample was **never
labelled**: e.g. a client that didn't report a stop reason.

The companion columns (`stop_reason_source`, `stop_detail`,
`completion_tokens`) and how mgmt fills them are in
[storage.md §6](storage.md#6-eval-sample-results-contract); the wire fields a
client sends are in
[httpapi.md §2.7.1.1.1](httpapi.md#27111-per-sample-generation-metadata).
`stop_reason` is the sole source of truth for a failed sample. There is **no**
eval-level aggregate: "% truncated per eval" is a query-time `GROUP BY`
downstream.

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
| `scored_samples[].is_correct` | bool | Scoring verdict; an `asr_eval` clip's is "its unit scored with no error", which is no ASR metric and from which mgmt computes nothing |
| `scored_samples[].measures` | object, optional | An `asr_eval` sample's share of the alignment, stored as the row's `measures`. Sum-safe: summing `hits`, `substitutions`, `deletions`, `insertions` and `reference_units` over a job's samples (absent = 0) gives the context's totals. The owner of a scored unit (an utterance, or an Earnings22 call's chunk 0) carries the counts and the normalized `reference` and `hypothesis` with its `unit`; another chunk of a scored call carries `{"unit": <call id>}` alone; an excluded sample `{"excluded": true}` |
| `scored_samples[].chat_template_kwargs` | object, optional | The sample's chat-template arguments as the evals server serves them (an `asr_eval` Voxtral sample's transcription mode and language), stored as the row's `chat_template_kwargs` JSON. Absent for a sample served without any |

An `asr_eval`'s `context` names its headline: `error_unit` (`word`, the only
unit; mgmt refuses any other) and `error_rate`, the corpus rate as a ratio,
which mgmt stores as the `wer` metric (`error_rate` for a task it does not
know; [benchmarks.md](benchmarks.md#212-asr-eval-error-rate)). mgmt sends an
`asr_eval` to `/score` only with one completion per dataset sample, none
failed, and the service refuses any other request with a `400`, as for a
`vlm_eval`: a partial run stays queued and is never published. A response
without `error_unit` or a numeric `error_rate`, whose unit disagrees with a
known task's, or that counts samples unanswered (`samples_missing`), is
refused. The context's `samples_excluded` goes to the row's `eval_metadata`.

**The scorer must echo every requested id exactly once.** mgmt derives
`accuracy = correct / total` from `scored_samples`, so a dropped or unknown id
would skew it; mgmt refuses such a response and keeps the submission for
investigation. Failed completions stay in the denominator. Per-sample rows,
`eval_metadata` and the accuracy metric are described in
[storage.md §6](storage.md#6-eval-sample-results-contract) and
[storage.md § Per-run metadata](storage.md#per-run-metadata-eval_metadata).

---

### `GET /evals/{eval_id}/datasets/{dataset_name}/samples`

Returns prompts for a given eval/dataset. Called by the mgmt server when a
client fetches `GET /benchmarks/{benchmark_id}` for an eval-type benchmark.
The `samples` array is spliced into the benchmark response so clients know
what to complete.

**Query**

| Parameter | Type | Description |
|---|---|---|
| `prompt_profile` | string, optional | Sent for a `vlm_eval` entry that sets `parameter_prompt_profile`, and always for an `asr_eval`. The service rebuilds each prompt's text the way VLMEvalKit prompts that model family ([benchmarks.md](benchmarks.md#29-vlm-eval-accuracy)), or builds each clip's request in that family's shape, and answers `400` for a profile it does not build |

Only a text eval's samples are spliced into `GET /benchmarks/{benchmark_id}`.
A `vlm_eval`'s and an `asr_eval`'s are read only by `pipette-mgmt
dataset-cache`, which builds the entry's corpus from one `/samples` stream
(with `X-Samples-Digest`) and probes `GET …/samples/digest`
([benchmarks.md](benchmarks.md#the-samples-a-device-runs-the-corpus)). An
`asr_eval`'s samples are never paged: the whole set streams in one body.

Both callers percent-encode `eval_id` and the dataset name as one path segment
each, and refuse an empty, `.` or `..` value, or one with a control character,
without a request.

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

A dataset that sets `metadata.repeats = N` is served with each base id expanded
into `<id>#0 … <id>#(N-1)`. mgmt passes these through as ordinary distinct
samples, and stores one warehouse row per id. `EvalBenchmark::sample_seed`
offsets a pinned seed by `k`.

---

## Error handling

- A non-success status, a timeout or an unreachable service on `POST /score`
  leaves the submission queued, and the next `score-eval` run retries it. A
  refused connection, `502` or `503` pauses the run and counts against no job;
  a timeout or `504` pauses it and counts against the job, as does any other
  non-success status without pausing, until the job is parked
  ([storage.md](storage.md#4-submission-contract)). A non-success status from
  the samples endpoint fails that benchmark fetch.
- The HTTP timeout is `http_timeout_secs` (default 600 seconds).
- `serve` starts while the scorer is down; eval sample proxying and scoring
  wait for it.

## Design notes

- **Stateless**: the scoring service receives completions, scores them, and
  returns results. It does not need to track submissions or jobs.
- **`id` is the join key**: scored samples must echo the ids submitted in
  the request, each exactly once.
- **`eval_id` and `dataset_name` come from the catalog**: they are defined
  in the benchmark TOML, not by the client. A scoring service implementation
  only needs to handle the `(eval_id, dataset_name)` pairs that are configured.
- **One round trip per submission**: the POST response contains everything
  needed for persistence (metrics + per-sample audit rows + runtime version),
  so no follow-up calls are required.
