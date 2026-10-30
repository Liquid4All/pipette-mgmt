# Plans

A **plan** is one ingestion of a plan document. A plan does not change after
filing, except for its note. A campaign files a plan
([filing a plan document](campaigns.md#filing-a-plan-document)). A revision
files the next plan. These routes read a plan, its document and its jobs, say
why its jobs wait, change its note, cancel it, and check a document before it
is filed. The [console app](../../ui/README.md)'s plan pages read them.

## Who may do what

[`GET /api/v1/plans`](plans/list_plans.md),
[`GET /api/v1/plans/{plan_id}`](plans/get_plan.md),
[`GET /api/v1/plans/{plan_id}/jobs`](plans/list_plan_jobs.md),
[`GET /api/v1/plans/{plan_id}/explain`](plans/explain_plan.md),
[`GET /api/v1/plans/{plan_id}/results.csv`](plans/get_plan_results_csv.md) and
[`GET /api/v1/jobs/{job_id}/plans`](plans/list_job_plans.md) take `read_plans`,
widened by `read_any_plans`.
[`GET /api/v1/plans/{plan_id}/document`](plans/get_plan_document.md) and
[`GET /api/v1/plans/{plan_id}/variants`](plans/get_plan_variants.md) take what
the plan reads take, **plus** `read_clients` or `read_any_clients`.
[`POST /api/v1/plans/{plan_id}/cancel`](plans/cancel_plan.md) takes
`cancel_plans`, widened by `cancel_any_plans`.
[`POST /api/v1/plans/validate`](plans/validate_plan.md) takes `submit_plans`.
[`PATCH /api/v1/plans/{plan_id}`](plans/update_plan.md) takes `submit_plans`,
in a campaign the caller files into; `manage_any_orgs` reaches any plan the
caller reads.

The wider grant of each pair implies the narrower one, and the narrower one is
scoped to the caller's campaigns
([the permission model](users.md#permissions)). A plan has no owner:
a plan filed into a campaign is read by whoever reads the campaign and
cancelled by whoever administers it, as its owner stands now, whoever filed
the plan. A plan filed outside every campaign is its filer's, and one attributed
to **nobody** there takes the wider grant.

`GET /api/v1/plans/{plan_id}/explain` carries a second, independent gate:
`read_clients` decides how much of the *fleet* the answer names, and a
submitter with neither fleet grant still reads the scheduling state for its own plan
([two gates](plans/explain_plan.md#two-gates-and-why-they-are-separate)).

## Plan statuses

Filing writes a plan's first `status`; after that it moves on its own.
`queue-maintenance`, a pass the operator runs on a schedule, writes it with
`progress_snapshot` in the same run, so both read as of
`progress_snapshot.computed_at`. Until the first run, `progress_snapshot` is
`null` and `status` is the filing's, which is `active` even when no job is
outstanding.

| `status` | Means | Moves to |
|---|---|---|
| `creating` | Filing is writing its jobs; each is claimable once promoted, and a joined job is already live | `active` or `pending_clients` once filed; `cancelling`, or straight to `cancelled`, if cancelled now or if it is still `creating` after `plan_creating_max_age_secs` (6 hours by default) |
| `active` | At least one outstanding job matches a working client | `pending_clients`, `complete`, `cancelling`, or `cancelled` when a cancel finds nothing left to retire |
| `pending_clients` | Jobs are outstanding, but none matches any working client: the plan is ahead of the fleet. Not an error | `active` when a matching client registers or is granted work; `complete`, `cancelling`, or `cancelled` when a cancel finds nothing left to retire |
| `cancelling` | Teardown is retiring the plan's live jobs. One-way | `cancelled` |
| `complete` | Every job is terminal: succeeded, failed, or lapsed unrun | Never moves |
| `cancelled` | Teardown finished, and no live job is left | Never moves |

Some jobs matched and some not stays `active`, with the unmatched jobs in
`progress_snapshot.starved_groups`. `terminal_at` is when the plan reached
`complete` or `cancelled`.

**The counts.** `progress_snapshot.counts` splits the plan's `total` jobs
into `available` (queued), `running` (leased), `finished` (a success
submitted, scored or not), `failed` and `cancelled` (retired by teardown, with
no result). A job whose deadline for this plan passed counts `failed` before
any other rule. A plan that reached `complete` or `cancelled` keeps its last
snapshot: no later pass rewrites it.

**A cancel lands on the next pass.**
[`POST /api/v1/plans/{plan_id}/cancel`](./plans/cancel_plan.md) records the
request and returns `202`; the plan reads `cancel_requested: true` until the
next pass latches it, or, for a plan cancelled while still `creating`, until it is
`plan_creating_max_age_secs` old. A plan with live jobs then reads `cancelling`
for one pass interval before `cancelled`; one whose jobs all finished goes
straight to `cancelled`. Cancelling a `cancelling` or `cancelled` plan is `202`
with `outcome` `already_cancelling` or `already_cancelled`, and a `complete`
plan answers `409`. A job another plan also lists is *released*, not retired:
it stays claimable and runs for that plan. This plan counts it `available` or
`running` as of the pass that latched `cancelled`, and keeps that count. Once
teardown retires a job, its client's next heartbeat and any result it submits
answer `404`; only a result that landed before the teardown pass is kept.

## Failures no client sent

The server retires a job unrun, and writes a failure for it, in four cases.
`failure_source` names which; a failure a client reported reads `client`.

| `failure_source` | The job |
|---|---|
| `expired` | Reached its `expires_at` with no result |
| `all_clients_denied` | Names its clients by id only, and every one of them reported a retriable failure for it |
| `unmatchable` | Has eligibility clauses no possible client can satisfy |
| `lease_lost` | Was taken and lost by clients too many times (3 by default), as a job that crashes the device does |

`failed_reason` is the server's message: the deadline that passed, the clause
that fired, the clients' reasons, or the clients that lost the job. A cancel
writes no failure: the retired jobs count `cancelled` and have no result.

A synthetic failure's `model_descriptor` and `runtime_descriptor` are the
job's `spec.model` and `spec.runtime` without `auth_token`, canonicalized as
the server stores them. A client spells the same model through its own types,
so the two need not match byte for byte: join a plan's request to what ran by
`job_id`, never by descriptor.

## Reading a plan's results

[`GET /api/v1/plans/{plan_id}/results.csv`](./plans/get_plan_results_csv.md)
and the jobs read with `results=true` answer from pipette-duckdb, which
republishes on its own refresh tick (5 minutes by default). A result scored
since shows on the next tick, and so, in the file, does a plan filed since.
When pipette-duckdb lacks a table or column the read needs, the file answers
`502`, and the jobs read answers its jobs with `results_read` `failed` and no
rows.

- **One row per metric** of each job's latest scoring run. A re-scored job
  shows its newest run only.
- **A failed job has rows too**: one per metric its benchmark meant to
  produce, `failed: true`, `value` and the device fields `null`, `unit` set. So
  count failures by distinct `job_id`, never by row: one failure is one to
  nine rows, by type.
- **A late failure is not recorded.** A failure scored more than two UTC
  days after the day it was submitted (the default) is archived without rows,
  so that job has no results. A held submission promoted late is the usual
  case.

What each `benchmark_type` writes as `metric` (`unit` names the unit on each
row, `value_stddev` is set where the client reported spread):

| `benchmark_type` | `metric` |
|---|---|
| `prefill_throughput` | `ttft` (ms), `prefill_throughput` (tokens/sec) |
| `decode_throughput` | `decode_throughput` (tokens/sec) |
| `end_to_end_latency` | `end_to_end_latency` (ms) |
| `max_memory_usage`, `vl_max_memory`, `asr_max_memory` | `max_host_usage`, and `max_gpu_usage` / `max_npu_usage` where measured (bytes) |
| `eval` | `accuracy` (ratio) |
| `vl_throughput`, `asr_throughput` | `ttft`, `prefill_throughput`, `decode_throughput`, and `end_to_end_latency` when submitted |
| `vl_spec_decode` | `decode_throughput` (tokens/sec), the per-request timings `ttft_ms`, `decode_ms`, `e2e_ms`, `runtime_ttft`, `runtime_e2e_ms` (ms), and, with speculation on only, the acceptance metrics `accept_length`, `accept_length_per_request` (tokens), `draft_acceptance_rate` (ratio) |
| `vlm_eval` | `accuracy`, `headline_score` (the task's official headline, 0-1; its meaning varies by task) |
| `asr_eval` | `wer` (ratio: summed edits over summed reference words; lower is better), or `error_rate` (ratio) for a task this build does not know |

`device` holds the row's device fields without the `device_` prefix.
Unified-memory devices (phones, tablets, Apple Silicon) leave the GPU and NPU
fields out. A scored `eval` or `vlm_eval` row carries `score_runtime_version`,
the version of the scoring service that graded it.
