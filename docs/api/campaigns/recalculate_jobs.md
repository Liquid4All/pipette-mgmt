# `POST /api/v1/campaigns/{campaign_id}/recalculate`

```text
POST /api/v1/campaigns/campaign_0195c3e2-…/recalculate
Content-Type: application/json

{ "job_ids": ["job-0195c400-…"], "note": "nightly-2-rescored" }
```

Scores **again** the completions that finished evals of the plan the campaign
runs recorded, and runs nothing on a device. It is a [re-run](rerun_jobs.md)
in every step but one: each named
job's cell is placed as a **new job** that is never queued. The revision answers
it at once with a copy of the named job's record, under the new `job_id` and
submitted now, and the scorer takes the copy as it takes any submission. The
copy is dated now because the warehouse files a row under the day it was
submitted, and an old day refuses writes
([reading a plan's results](../plans.md#reading-a-plans-results)).

The named job keeps its record and its score. The new score lands under the new
`job_id` once `process-submissions` and `score-eval` have run, so the revision
counts the new job `finished` from the start while its result reads empty until
then. A scorer that changed since the job was first scored is the reason to
recalculate: the copy is scored by the scorer that runs now.

| Field | Required | Description |
|---|---|---|
| `job_ids` | one of the two | Jobs of the plan the campaign runs, as [`GET /api/v1/plans/{plan_id}/jobs`](../plans/list_plan_jobs.md) lists them. Each must be an eval that **finished**. A job named twice is recalculated once |
| `all_evals` | one of the two | `true` recalculates every job of the plan whose record holds completions to score again; `false` is no selector. Needs `base_plan_id` |
| `note` | no | The note for the revision, as [the request body of a filing](../campaigns.md#request-body) describes. The server records the change that the revision makes in `change` |

| Query parameter | Required | Description |
|---|---|---|
| `base_plan_id` | with `all_evals` | As on a [re-run](rerun_jobs.md) |
| `dry_run` | no | `true` runs every check below and expands the document, and files nothing. Not with `all_evals` |

A job that failed is refused, and so is one of a benchmark other than an eval:
neither recorded completions to score again. A job with no result yet is
refused as a re-run refuses it. The rest of the rules are the re-run's:
`base_plan_id` as there, a job the plan does not hold is refused, and a
document that now expands to other cells is refused.

Responds `201 Created` with the body of a [variant edit](edit_variants.md).
`recalculated_job_count` is the
number of distinct jobs named, and `rerun_job_count` is `0`. A dry run answers
`200` as a re-run's does, and writes no copy.

The [console app](../../../ui/README.md#pages) offers the same recalculation
on each finished eval row of the campaign's plan, and `all_evals` at once.

The `403`, `404` and a moved `base_plan_id`'s `409` come **before the body is
read**, and the writes and their order are a [variant edit's](edit_variants.md), with the copies written while the revision is filed.
A filing that fails after some copies are written does not withdraw them: they
are scored under jobs of a plan that never runs, and filing again writes new
copies of the same records.

The route also answers the errors every filing answers
([filing errors](../campaigns.md#errors)).

## Statuses

| Status | When |
|--------|------|
| 200 | A dry run |
| 201 | The revision is filed, the campaign moved onto it, and the replaced plan cancelled |
| 400 | As on a [re-run](rerun_jobs.md): the body or the query string is malformed, names both `job_ids` and `all_evals` or neither, names no job, or names a string that is not a job id, `all_evals` comes without `base_plan_id` or with `dry_run`, or the document no longer expands to the replaced plan's cells. Nothing is filed |
| 401 | Missing or invalid auth headers, or a device signed ([filing errors](../campaigns.md#errors)) |
| 403 | As on a [variant edit](edit_variants.md) |
| 404 | No such campaign |
| 409 | The campaign is [archived](update_campaign.md): unarchive it first; nothing is filed, dry runs included. Or as on a [re-run](rerun_jobs.md): the campaign runs no plan, one with no stored document, or another than `base_plan_id`, the plan does not hold a named job, or a named job has no result yet. Or a named job failed, or is not an eval. Or `all_evals` takes no job. Nothing is filed. Or another revision moved the campaign while this one was filed, or a campaign delete removed it: this revision does not become the head, and the server cancels it. The copies it wrote are scored all the same, under jobs only the cancelled revision holds |
