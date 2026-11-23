# `POST /api/v1/campaigns/{campaign_id}/rerun`

```text
POST /api/v1/campaigns/campaign_0195c3e2-…/rerun
Content-Type: application/json

{ "job_ids": ["job-0195c400-…"], "note": "nightly-2-rerun" }
```

```text
POST /api/v1/campaigns/campaign_0195c3e2-…/rerun?base_plan_id=plan-0195c3f0-…
Content-Type: application/json

{ "answered": "failed", "note": "thermal throttling on bos-17pro-1" }
```

```text
POST /api/v1/campaigns/campaign_0195c3e2-…/rerun
Content-Type: application/json

{ "job_ids": ["job-0195c400-…"], "fail": { "job-0195c401-…": "swap after 5 runs" } }
```

Runs finished or failed jobs of the plan the campaign runs **again**, named by
id or picked by `answered`, and **marks** the jobs `fail` names failed, whether
they have run or not, in four steps:

1. Takes the document of the plan the campaign runs, as it is.
2. Files it as a revision, as a [variant edit](edit_variants.md) files its result. The
   revision carries over every job of the replaced plan except the named ones,
   finished ones included. Each cell a re-run names is placed as a **new job**,
   queued afresh, and never joins another plan's run of the cell
   ([carrying jobs into a revision](../campaigns.md#carrying-jobs-into-a-revision)).
   Each cell `fail` names is placed as a new job that never queues: a failure
   the server writes answers it ([marking a job failed](#marking-a-job-failed)).
3. Moves the campaign onto the revision.
4. Cancels the replaced plan. Its teardown releases every job the revision
   carried. A job named to run again has a result, so there is nothing of it to
   stop. A job `fail` names may still be queued or running, and the teardown
   stops it ([marking a job failed](#marking-a-job-failed)).

A named job that has a result keeps it: it stays readable under its own
`job_id`, and the new job answers under another. A job of the replaced plan
that lapsed for it, or that teardown retired with no result, is placed afresh
by any revision, so this one places it afresh too, unless `fail` names it.

| Field | Required | Description |
|---|---|---|
| `job_ids` | one of the two, or `fail` alone | Jobs of the plan the campaign runs, as [`GET /api/v1/plans/{plan_id}/jobs`](../plans/list_plan_jobs.md) lists them. Each must have a **result**, `finished` or `failed`. A job named twice is re-run once |
| `answered` | one of the two, or `fail` alone | `failed` re-runs every job of the plan whose recorded result is a failure, except a job a mark answered; `all` every job with a result, finished or failed, marked ones included. Judged on each job's record, so a job that lapsed for the plan with no result is not taken. Needs `base_plan_id` |
| `fail` | no | An object from a job id to the reason it failed, at most 4096 characters: jobs of the plan the campaign runs to [mark failed](#marking-a-job-failed). A job needs no result: one still queued or running, or one that lapsed for the plan, is taken too. Alone, it re-runs nothing; beside `job_ids`, one revision does both, and no job may be in both. Not with `answered` |
| `note` | no | The note for the revision, as [the request body of a filing](../campaigns.md#request-body) describes. The server records the change that the revision makes in `change` |

| Query parameter | Required | Description |
|---|---|---|
| `base_plan_id` | with `answered` | The plan the caller read the jobs off. `409` once the campaign runs another, checked before the body is read, so a re-run sent from a page drawn before the campaign moved is refused rather than applied to the plan it moved to |
| `dry_run` | no | `true` runs every check below and expands the document, and files nothing |

`job_ids` refuses a job still queued or running: running it again would
duplicate work already in flight. It also refuses a job that lapsed for the
plan and has no result yet, because it may still run for another plan. `fail`
takes all three ([marking a job failed](#marking-a-job-failed)). A job id names
the work rather than a position in a document, so `job_ids` needs no
`base_plan_id`, and takes one.
`answered` names no job a moved campaign would fail to hold, so it needs one. A
job the campaign's plan does not hold is refused, so a retry after the first
attempt landed is refused rather than run twice: the revision holds a new job
for the cell, not the one named.

`answered: failed` also takes each job the server failed unrun after a smaller
run of its family failed (`smaller_run_failed`,
[Plans: Failures no client sent](../plans.md#failures-no-client-sent)). It
takes the smaller run's failure too, so that run's new job goes first, and a
timeout or out of memory there fails the larger ones again
([queue.md](../../queue.md#client-reported-failure)).

The document is filed as it is, so the revision must place every cell of the
replaced plan again. A document that now expands to other cells, because the
planner changed since the plan was filed, is refused rather than running cells
nobody named. A body that this build only reads differently, with a retired
field dropped or a default filled in, is the same cell
([plan-ingestion.md](../../plan-ingestion.md#carrying-a-campaigns-jobs-over)).

Responds `201 Created` with the body of a [variant edit](edit_variants.md).
`rerun_job_count` is the number of distinct jobs named to run again, and
`marked_job_count` the number `fail` marks. `dropped_job_count` is `0` unless
the replaced plan lost jobs to a lapse or a teardown and the revision placed
them on other jobs.
`plan.jobs` lists every job the revision holds, the new ones among them. The
jobs routes give each new job's parent in `rerun_of`
([`GET /api/v1/plans/{plan_id}/jobs`](../plans/list_plan_jobs.md)).

A dry run answers `200` as a [variant edit's](edit_variants.md) does: the
[dry-run body](../campaigns.md#response-201-created), plus the `document` it would file and
the counts of a variant edit. It
refuses what the re-run would, a drifted document included, and it files
nothing, and neither moves the campaign nor cancels its plan. `rerun_job_ids`
names the jobs it would run again, the ones `answered` picks included, and
`marked_job_ids` the jobs it would mark failed. A marked job still queued or
running would stop too, and `dropped_job_ids` does not list it
([marking a job failed](#marking-a-job-failed)).

To file exactly what a dry run of `answered` listed, send `rerun_job_ids` back
as `job_ids` with the same `base_plan_id`. `answered` picks again when it files,
so it would also take a job that failed between the two requests. A re-run filed
by `job_ids` records no `answered` in its `change`, so name the rule in `note`.
The body takes `default_body_limit_bytes`, 64 KB by default
([request size limits](../../httpapi.md#12-request-size-limits)), about 1,500
job ids. A longer list files in parts, each its own revision. Each part moves
the campaign, so send each later part with the plan the previous part filed as
`base_plan_id`. A refused part leaves the earlier parts filed: a dry run on the
new plan shows which jobs remain.

The [console app](../../../ui/README.md#pages) offers the same re-run on each
finished or failed row of the campaign's plan, over the rows a reader checks,
and `answered` over all of them at once. It offers a mark on each such row too,
whose confirmation takes the reason. The jobs read
says whom to offer them, by its `rerun_campaign_id`
([the plan page's body](../plans/list_plan_jobs.md#the-plan-pages-body)).

The `403`, `404` and a moved `base_plan_id`'s `409` come **before the body is
read**, as on a [variant edit](edit_variants.md). The writes, their order, and two
revisions at once behave as they do for a variant edit.

The route also answers the errors every filing answers
([filing errors](../campaigns.md#errors)).

## Marking a job failed

`fail` records that a job's cell failed, as the caller judges it. The job may
have run: too slow, swapping, or too big for the device. Or it may not have run
yet, where the caller knows how it would end: the longer prompts of a model
whose shorter prompt ran past its attempt limit, or the remaining cells of a
model that the device kills at load. The revision answers each named job's
cell with a failure under a new job that no device runs, as a
[recalculation](recalculate_jobs.md) answers its cell with a copy
([plan-ingestion.md](../../plan-ingestion.md#carrying-a-campaigns-jobs-over)).
The failure is the server's
([Plans: Failures no client sent](../plans.md#failures-no-client-sent)):
`failure_source` `marked`, `failure_code` `unspecified`, and the reason as
`failed_reason`. The new job's `rerun_of` names the job it replaces, which keeps
its own result where it has one. The revision's `change` counts the marks in
`marked_job_count`.
The failures are written while the revision is filed, so a filing that fails
after them, or does not advance (a `409` that cancels the plan), leaves them
scored under jobs of a plan that never runs, as a recalculation's copies are.

The revision does not carry a named job. What happens to one with no result
yet depends on where it stands:

- **Queued.** The replaced plan's teardown retires it at the next
  `queue-maintenance` pass. A device that claims it before that pass is
  stopped as a running one is.
- **Running.** The filing does not stop it. The teardown retires it at the same
  pass, and from then its device's heartbeat and any result it submits answer
  `404` ([Plans: Plan statuses](../plans.md#plan-statuses)). A result that
  lands before the pass is kept under the job's own `job_id`. The cell stays
  marked either way: the revision holds the new job.
- **Lapsed for the plan**, with no result. The plan stopped waiting for it at
  the lapse. It may still run for another plan, and the mark does not stop it
  there. Without the mark, the revision would queue its cell again.

A job that another plan also holds is released, not retired, and runs for that
plan.

A mark reaches the readers that go through a plan: the [plan's reads](../plans.md),
its [results CSV](../plans/get_plan_results_csv.md) and the console's Jobs
table show the cell failed. A replaced job that ran keeps its measured rows in
the warehouse, and nothing on them records the mark, so a read of the benchmark
across the warehouse still sees that run.

The jobs read shows the new job `failed` once the next `queue-maintenance`
pass reads its record, or once pipette-duckdb publishes its rows. A later
revision carries it while the cell keeps its digest. One that places the cell
under a new digest, as a change of requirements or of the attempt limit does,
runs the cell again, as it runs any failed cell again. Name it in `job_ids` to
run the cell again.
That re-run does not join the job the mark replaced, so the cell runs twice
where another plan still runs that job.
`answered: failed` leaves it out, so a re-run of the failed jobs keeps the
mark, and `answered: all` takes it.

## Statuses

| Status | When |
|--------|------|
| 200 | A dry run |
| 201 | The revision is filed, the campaign moved onto it, and the replaced plan cancelled |
| 400 | The body is not JSON, holds a field this route does not list, names both `job_ids` and `answered`, or none of `job_ids`, `answered` and `fail`, names `fail` with `answered`, names one job in both `job_ids` and `fail`, names no job, or names a string that is not a job id. A reason in `fail` is blank, longer than 4096 characters, or holds a NUL character. `answered` comes without `base_plan_id`. The query string carries a parameter this route does not list, a `base_plan_id` that is not a plan id, or a `dry_run` that is not a boolean. Or the document no longer expands, for a reason [the filing errors](../campaigns.md#errors) list: something it names changed since it was filed. Or it expands to cells other than the replaced plan's. Nothing is filed |
| 401 | Missing or invalid auth headers, or a device signed ([filing errors](../campaigns.md#errors)) |
| 403 | As on a [variant edit](edit_variants.md): the campaign is outside the caller's scope, the caller lacks `submit_plans`, the caller may not read and cancel the plan the campaign runs, or the caller holds neither `read_clients` nor `read_any_clients` |
| 409 | The campaign is [locked or archived](update_campaign.md): unlock or unarchive it first; nothing is filed, dry runs included. Or the campaign runs no plan, or one with no stored document, or runs another than `base_plan_id`. The plan does not hold a named job, or holds it and recorded no digest for it, as a plan filed before digests were recorded. A job `job_ids` names has no result yet. `answered` takes no job of the plan. Nothing is filed. Or another revision moved the campaign, or a campaign delete removed it, while this one was filed: this revision does not become the head, and the server cancels it |
