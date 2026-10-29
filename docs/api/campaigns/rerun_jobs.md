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

Runs finished or failed jobs of the plan the campaign runs **again**, named by
id or picked by `answered`, in four steps:

1. Takes the document of the plan the campaign runs, as it is.
2. Files it as a revision, as a [variant edit](edit_variants.md) files its result. The
   revision carries over every job of the replaced plan except the named ones,
   finished ones included. Each named job's cell is placed as a **new job**,
   queued afresh, and never joins another plan's run of the cell
   ([carrying jobs into a revision](../campaigns.md#carrying-jobs-into-a-revision)).
3. Moves the campaign onto the revision.
4. Cancels the replaced plan. Its teardown releases every job the revision
   carried. The named jobs have results, so there is nothing of them to stop.

The named job keeps its result: it stays readable under its own `job_id`, and
the new job answers under another. A job of the replaced plan that lapsed for
it, or that teardown retired with no result, is placed afresh by any revision,
so this one places it afresh too.

| Field | Required | Description |
|---|---|---|
| `job_ids` | one of the two | Jobs of the plan the campaign runs, as [`GET /api/v1/plans/{plan_id}/jobs`](../plans/list_plan_jobs.md) lists them. Each must have a **result**, `finished` or `failed`. A job named twice is re-run once |
| `answered` | one of the two | `failed` re-runs every job of the plan whose recorded result is a failure; `all` every job with a result, finished or failed. Judged on each job's record, so a job that lapsed for the plan with no result is not taken. Needs `base_plan_id` |
| `note` | no | The note for the revision, as [the request body of a filing](../campaigns.md#request-body) describes. The server records the change that the revision makes in `change` |

| Query parameter | Required | Description |
|---|---|---|
| `base_plan_id` | with `answered` | The plan the caller read the jobs off. `409` once the campaign runs another, checked before the body is read, so a re-run sent from a page drawn before the campaign moved is refused rather than applied to the plan it moved to |
| `dry_run` | no | `true` runs every check below and expands the document, and files nothing. Not with `answered`, which has no dry run |

A job still queued or running is refused: running it again would duplicate work
already in flight. So is a job that lapsed for the plan and has no result yet,
because it may still run for another plan. A job id names the work rather than
a position in a document, so `job_ids` needs no `base_plan_id`, and takes one.
`answered` names no job a moved campaign would fail to hold, so it needs one. A
job the campaign's plan does not hold is refused, so a retry after the first
attempt landed is refused rather than run twice: the revision holds a new job
for the cell, not the one named.

The document is filed as it is, so the revision must place every cell of the
replaced plan again. A document that now expands to other cells, because the
planner changed since the plan was filed, is refused rather than running cells
nobody named.

Responds `201 Created` with the body of a [variant edit](edit_variants.md).
`rerun_job_count` is the number
of distinct jobs named. `dropped_job_count` is `0` unless the replaced plan
lost jobs to a lapse or a teardown and the revision placed them on other jobs.
`plan.jobs` lists every job the revision holds, the new ones among them. The
jobs routes give each new job's parent in `rerun_of`
([`GET /api/v1/plans/{plan_id}/jobs`](../plans/list_plan_jobs.md)).

A dry run answers `200` as a [variant edit's](edit_variants.md) does: the
[dry-run body](../campaigns.md#response-201-created), plus the `document` it would file and
the counts of a variant edit. It
refuses what the re-run would, a drifted document included, and it files
nothing, and neither moves the campaign nor cancels its plan.

The [console app](../../../ui/README.md#pages) offers the same re-run on each
finished or failed row of the campaign's plan, over the rows a reader checks,
and `answered` over all of them at once. The jobs read
says whom to offer them, by its `rerun_campaign_id`
([the plan page's body](../plans/list_plan_jobs.md#the-plan-pages-body)).

The `403`, `404` and a moved `base_plan_id`'s `409` come **before the body is
read**, as on a [variant edit](edit_variants.md). The writes, their order, and two
revisions at once behave as they do for a variant edit.

The route also answers the errors every filing answers
([filing errors](../campaigns.md#errors)).

## Statuses

| Status | When |
|--------|------|
| 200 | A dry run |
| 201 | The revision is filed, the campaign moved onto it, and the replaced plan cancelled |
| 400 | The body is not JSON, holds a field this route does not list, names both `job_ids` and `answered` or neither, names no job, or names a string that is not a job id. `answered` comes without `base_plan_id`, or with `dry_run`. The query string carries a parameter this route does not list, a `base_plan_id` that is not a plan id, or a `dry_run` that is not a boolean. Or the document no longer expands, for a reason [the filing errors](../campaigns.md#errors) list: something it names changed since it was filed. Or it expands to cells other than the replaced plan's. Nothing is filed |
| 401 | Missing or invalid auth headers, or a device signed ([filing errors](../campaigns.md#errors)) |
| 403 | As on a [variant edit](edit_variants.md): the campaign is outside the caller's scope, the caller lacks `submit_plans`, the caller may not read and cancel the plan the campaign runs, or the caller holds neither `read_clients` nor `read_any_clients` |
| 404 | No such campaign |
| 409 | The campaign is [archived](update_campaign.md): unarchive it first; nothing is filed, dry runs included. Or the campaign runs no plan, or one with no stored document, or runs another than `base_plan_id`. The plan does not hold a named job, or holds it and recorded no digest for it, as a plan filed before digests were recorded. A named job has no result yet. `answered` takes no job of the plan. Nothing is filed. Or another revision moved the campaign, or a campaign delete removed it, while this one was filed: this revision does not become the head, and the server cancels it |
