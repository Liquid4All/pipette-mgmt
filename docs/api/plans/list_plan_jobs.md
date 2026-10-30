# `GET /api/v1/plans/{plan_id}/jobs`

The plan's jobs and where each one stands. Scoped exactly as
[`GET /api/v1/plans/{plan_id}`](get_plan.md) is: a caller that cannot read the plan
cannot read its membership list either.

## Query parameters

[The OpenAPI document](../../httpapi.md#15-the-openapi-document) lists the parameters:
`limit` and `offset`, `results=true` for
[the plan page's body](#the-plan-pages-body), and `all=true` for
every job in one page, what the plan page's overview draws.

A plan can hold tens of thousands of jobs, which is what the 500 ceiling stands
between, and what `all=true` waives.

## Response

The body is `NamedPlanJobs`,
with every key `results=true` adds ([the plan page's body](#the-plan-pages-body))
absent. Its `jobs` follow the order the plan's document lists its jobs (variant, then
benchmark, then model), or the manifest's own order for a plan with no document
that still parses. Either is fixed for the plan, so paging is stable while the
queue moves underneath.

**A row has two sources**, kept apart. Its top level is the job's plan cell and
where the job stands; what a client reported is its run's, under `result` with
`results=true` ([the plan page's body](#the-plan-pages-body)). So a row reads the same
whether the job is queued, answered, lapsed or retired.

**`planned` is read off the job's stored body**, or for a job filed before
those were stored, off its queued body while it is queued or held. It is the
job's `spec` as a submitted record carries it: `model` and `runtime` in the plan
document's form without an `auth_token`, and the flag groups as authored with
their axis keys, an `envs` entry keeping its value only under `GGML_*`. A
client's reported descriptor can spell the same model differently
([failures no client sent](../plans.md#failures-no-client-sent)), so compare
a request with what ran by job id, not by value. It names no machine, so both
tiers get it.

`state` is settled in the order `queue-maintenance` counts a plan's progress in:

1. A lapse recorded on this plan wins outright: `failed`. The plan's deadline
   for the job, `plan.lapsed.expires_at`, passed with no answer, even while a
   shared job runs on for another plan.
2. Queued → `available`; under a lease → `running`.
3. Otherwise the job's run in pipette-duckdb: a failed run → `failed`, any
   other → `finished`.
4. Otherwise the outcome the plan records for the job, once `queue-maintenance`
   has read its record; its result reads `unknown` until the run is scored and
   republished.
5. Otherwise the plan's status: on a `complete`, `cancelling` or `cancelled`
   plan → `cancelled`, since only teardown takes a job out unanswered; on any
   other → `unknown`, as is every such job when the runs failed to read.
6. On a server with no `[duckdb]`, the submission record decides in place of 3
   and 5: a `failure` body → `failed`, any other → `finished`, none →
   `cancelled`.

## The plan page's body

With `results=true` the page carries what the plan's page draws beside the
jobs, under the same scope and nothing else (not the campaign's reach): each
job's result, the re-runs and recalculations offered on it, and the cancel.

`jobs[].result` is the job's latest scoring run, read from pipette-duckdb by
the ids of the page's jobs, or why there is none to show, by its `status`:
`scored` with the run's `rows`; `not_run` for a job queued, running, cancelled
or lapsed for this plan and not answered since (one that lapsed and was later
answered for another plan shows that result); `unknown` for one that may have a run the page did not
read, or one not republished yet. A job a re-run carried over shows its result as soon
as it has one, without waiting for pipette-duckdb's `plan_jobs` to republish.

The body adds `results_read`, how the runs were read: pipette-duckdb times out at
3 s (or `[duckdb] timeout_secs`, if shorter; with `all=true`, `timeout_secs`
alone), and a page whose results were not read still answers its jobs. The
`earlier_runs` read within that same wait: a re-run's beside the runs, as its
plan names the job it ran again, and a recalculation's after them, as its run
names the one it scores again. A run not read by then is left out. A
`scored` result's `rows` are [the result rows](get_plan_results_csv.md#result-rows), cut to the
reader's tier; a failed run's rows each say `failed`, with its
`failed_reason` and `failure_source`, and measure nothing. `recalculable` is
judged only beside a `rerun_campaign_id`, and the route still decides.

Each server keeps the runs and the `earlier_runs` of a `complete` plan for
5 minutes after an `all=true` read. In that time, a read of the plan with
`results=true` uses the kept runs and sends no query. A page read uses kept
runs, but does not keep them. The server keeps only a read whose
`results_read` is `ok` and whose earlier runs all read. Thus, after a rescore
or a `fix-model-param` run, the rows can be up to 5 minutes old.

At each read, the server cuts the rows to the reader's tier. The server also
finds these values again:

* names
* `rerun_campaign_id`
* `may_cancel`
* `may_edit_note`
* `document_state`

`jobs[].planned_runtime_sha256` is the digest of the job's planned runtime. It
equals the digest that the scorer stores for a row that reports the same
runtime, and it leaves out a vault `custom_label`
([storage.md](../../storage.md#model_descriptor--runtime_descriptor)). A row's
`runtime_descriptor_sha256` is the stored digest of the runtime that the row
reports. Both digests are keys of `descriptor_names`.

`jobs[].planned_model_sha256` is the digest of the job's planned model,
without an `auth_token`. A row's `model_descriptor_sha256` is the stored
digest of the model that the row reports. Both digests are also keys of
`descriptor_names`. For a vault run, the row records the Hugging Face alias of
the vault model, so its digest is the alias's. The planned digest of a vault
model is the digest of the vault descriptor. A catalog name thus shows on the
rows of a vault run, but not on its planned cell.

| Field | Type | Description |
|-------|------|-------------|
| `rerun_campaign_id` | string | The campaign this plan heads, when the caller may re-run and recalculate its jobs there ([`POST /api/v1/campaigns/{campaign_id}/rerun`](../campaigns/rerun_jobs.md)): it may file into the campaign and holds what a revision needs, over a plan with content digests. Absent otherwise, as on every plan the campaign no longer runs |
| `may_cancel`, `cancel_permission` | | The grant cancelling this plan takes from the caller ([permissions](cancel_plan.md#permissions)), and whether it holds it. A `complete` or `cancelled` plan takes no cancel either way |
| `document_state` | string | What the store holds of the plan's document ([`GET /api/v1/plans/{plan_id}/document`](get_plan_document.md)). Below the full tier, `present` says only that the plan names one |
| `descriptor_names` | object | The catalog names of the runtime and model digests that the page shows, as the reader sees them ([the descriptor catalog](../descriptor-catalog.md)). Each digest is a key. `display` is the member selector that a row shows, for example `llamacpp-v0.7.0::linux-cuda-12-8` or `AcmeLM-VL-3B::gguf::q4_0`. `name` is the entry's name in `display`: bare, `global/<name>`, or `<org-slug>/<name>`. `synonyms` holds the selectors of the other names, in rank order. A digest with no name that the reader can read has no key. Absent without a `[database]`, and when no digest has a name |
| `names` | object | At the full tier, each device the page names, as a lease holder or on a run's rows, `client_id` to its name, the `user@host` word of its details or else the details whole. A device whose record is gone is left out. At either tier, each vault resource a job's `planned` model or runtime names, `resource_id` to its record's `name`, for a reader whose vault reach admits the record ([vault: who may do what](../vault.md#who-may-do-what)); a reader outside it gets the descriptor alone, which says the same name where its source carries a `custom_label`. Absent when it would name nothing below the full tier |

## Statuses

| Status | When |
|--------|------|
| 200 | The page of jobs; with `results=true`, [the plan page's body](#the-plan-pages-body) |
| 400 | `plan_id` in the path contains characters outside `[A-Za-z0-9-]`; a `limit` or `offset` that is not a non-negative integer; or a `results` or `all` that is not a boolean |
| 401 | Missing or invalid auth headers, or an `X-Client-Id` that is not a `user_…` |
| 403 | Caller holds neither `read_plans` nor `read_any_plans` |
| 404 | No such plan, **or** a plan the caller's scope does not admit ([one `404` for both](get_plan.md#one-404-for-both)) |
