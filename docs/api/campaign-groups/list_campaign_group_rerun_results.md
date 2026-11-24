# `GET /api/v1/campaign-groups/{group_id}/rerun-results`

The re-runs the group's [policies](../campaign-groups.md#policies) filed in
the members whose plans the caller may read, none of an eval's cell, and how
each answered
([re-run results](../campaign-groups.md#re-run-results)). It reads what
queue-maintenance recorded, and not pipette-duckdb.

- `summary`, one entry per policy with a re-run, in the order `swap`,
  `spread`, `failed`, `ttft_over_e2e`, over every row: its `reruns`, those
  `waiting` for their rows, those `cleared`, still `flagged`, `failed` and
  `dropped`, and the `median_before` and `median_after` of the cleared, flagged
  and failed ones, whose rows were read, `null` for none.
- `rows`, newest filing first, one per re-run job and policy that re-ran it:
  - `campaign_id`, `kind`, `rerun_job_id`, the job the filing queued,
    `rerun_of`, the run it replaces, and `plan_id` and `filed_at`, the filing.
  - `attempt`: the runs in a row the policy flagged, the replaced one
    included. 1 is a job's first re-run, 2 a re-run of a re-run.
  - The cell: `benchmark_id`, and `model_key`, the key the run's model takes
    in the [model params table](../model-params.md), `null` when its rows name
    no model.
  - `before`, the check's value on the replaced run, and `threshold`, what
    the check weighs it against as the policy stood at the filing. For `swap`,
    the largest swap in bytes against 0. For `spread`, the largest spread in
    percent against `percent`, with `short_repetition_ms` and
    `short_repetition_percent` as the policy then had them, `null` when it had
    no short repetition rule; a row under that time is weighed against
    `short_repetition_percent`. For `ttft_over_e2e`, the largest TTFT share of
    E2E in percent against 100, where a share whose excess rounds to 0.0% is
    not over. For `failed`, 1 when a row is a device failure with one of
    `codes`, the list of codes the policy re-ran at the filing, without their
    re-runs, else 0, and no `threshold`; `codes` is `null` for the other kinds.
    `before_code` is the replaced run's failure code, for `failed` only.
  - Once the re-run's rows are read: `after`, the check's value on them,
    `null` when they hold none; `after_code`, the re-run's failure code for
    `failed`; `outcome`, `flagged` when the check still flags the re-run at
    the filing's settings, else `failed` when a row of the re-run failed, else
    `cleared`; and `judged_at`. All four are `null` while the re-run waits.
  - `outcome` is `dropped` for a re-run that the campaign no longer runs and
    that has neither rows nor a recorded result: a later revision dropped it,
    or a person marked it. Its `judged_at` is when queue-maintenance, or the
    backfill, found it so, and its `after` and `after_code` are `null`.

`total` counts the rows before paging. The rows page as
[`GET /api/v1/plans/{plan_id}/jobs`](../plans/list_plan_jobs.md) does: `limit`
is 100 by default, at most 500, and more than that is clamped to 500; `offset`
skips rows; `all=true` answers every row, with `offset` 0 and `limit` the
`total`.

It takes `read_plans` and `read_clients`, as the preview does, and the group
must be in the caller's reach
([who may do what](../campaign-groups.md#who-may-do-what)). It lists only the
rows of the members whose plans the caller reads: every member with
`read_any_plans`, else its organization's campaigns. A caller that reads the
group only through `manage_any_orgs` gets no row. A campaign that left the
group shows no row, and shows its rows again once it joins it again.

## Statuses

| Status | When |
|--------|------|
| 200 | The summary and the page |
| 400 | `group_id` is not a group id, or a query value is not a number or a boolean |
| 403 | The group is outside the caller's reach ([who may do what](../campaign-groups.md#who-may-do-what)), or the caller holds no `read_plans` or no `read_clients` |
