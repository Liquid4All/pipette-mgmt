# `GET /api/v1/campaign-groups/{group_id}/policy-preview`

What the group's [policies](../campaign-groups.md#policies) would re-run and
mark failed in each member campaign now, its result rows that lack model
params, and its failed jobs below a clean larger run. It reads each member as
[queue-maintenance](../campaign-groups.md#when-a-policy-acts) does, and from
what queue-maintenance keeps of a member it takes only what holds it and its
last filing: it reads every answered job of the plan, those queue-maintenance
has already decided included. It files, writes, marks and cancels nothing.
`members` holds one entry per member whose plans the caller may read, in
member order:

- `campaign_id`, and `plan_id`, the plan the campaign runs, `null` for none.
- `state`: `evaluated` when the plan's answered jobs were checked. Otherwise
  `other_owner`, as the campaign's owner is not the group's and no policy acts
  on it; `no_plan`, as the campaign is archived, runs no plan, or runs one
  filed without a document; `not_ready` while the plan is `creating`;
  `cancelled` once someone stopped the plan; `unreadable`, as pipette-duckdb
  failed or timed out on its rows or an earlier member's;
  `waiting_for_devices`, as jobs the policies' last revision of it queued wait
  for a device; `filing_not_advanced`, as that revision did not move the
  campaign onto its plan, which still runs; or `refused`, as queue-maintenance
  could not file the plan again under this release and the same policies that
  re-run or mark.
- `filing_plan_id`, the plan of that last revision while it holds the member
  (`waiting_for_devices` or `filing_not_advanced`), else `null`, and
  `unclaimed_job_count`, its jobs that wait for a device.
- `answered_job_count`, the plan's jobs whose outcome queue-maintenance
  recorded, and `waiting_job_count`, those with no rows in pipette-duckdb yet,
  which wait.
- `earliest_deadline`, the earliest deadline of the plan's jobs, and
  `deadline_passed`. If the plan's document sets a deadline that has passed,
  the server refuses a re-run of the plan.
- `checks`, one per check in the order `swap`, `spread`, `failed`,
  `ttft_over_e2e`, `model_params`, `smaller_failed`: whether its policy is
  `on`, and what that policy alone would do with its own settings, on or off.
  Of the jobs it has `flagged`, it would `rerun`, mark failed (`marked`) or keep
  (`kept`) each; a skipped job counts in `flagged` only. `model_params` flags
  each job with a row in `missing_model_params`, and `smaller_failed` each job
  in `smaller_failed`, and both keep it.
- `triggered`, in kind order, the policies that are on and either flag a job
  of the plan or have a re-run they filed into the campaign that waits for its
  rows; every other policy that is on is cleared. A held member reads no rows,
  so only its waiting re-runs trigger its policies.
- What the policies that are on would do together: `rerun_job_ids`,
  `marked_job_ids`, and `kept_job_count`, the answered jobs with rows they
  leave as they are, including those none of them flags. `skipped` maps each
  flagged job they leave aside to why: `no_content_key` and `unreadable_plan`
  for good, `chain_read_failed` and `earlier_run_without_rows` until the next
  read.
- `missing_model_params`, the answered jobs' rows that the plan pages show
  without a total or active parameter count, as their model has no entry in
  the [model params table](../model-params.md): one entry per `key` an entry
  for the model would take, in key order, with its `row_count` and
  `job_count`. A row that records no model, or whose model descriptors give no
  key, is left out.
- `smaller_failed`, the answered jobs with a failed row, whatever the failure,
  whose [family](../campaign-groups.md#policies) holds a larger answered job
  with rows and no failed row, in `benchmark_id` then `job_id` order. Each
  names its `job_id`, `benchmark_id`, the `failure_source` and `failure_code`
  of its first failed row, and the smallest such clean job: `clean_job_id`,
  `clean_benchmark_id`, and `model_key`, its rows' model by the key a model
  params entry takes, `null` where they name none. A job whose source is gone
  has no family and is left out.
- `last_filing`, the policies' last revision of the campaign, whatever the
  `state`: its `plan_id`, `filed_at`, and `queued_job_count`, the jobs it
  queued that the plan it revised did not hold. `null` for none.

On a member that is not `evaluated` every count is 0 and every list is empty,
except that an `unreadable`, `waiting_for_devices` or `refused` one keeps its
`answered_job_count` and its deadline, and a `waiting_for_devices` or
`refused` one its `model_params` and `smaller_failed` checks,
`missing_model_params` and `smaller_failed`.

A check's attempts on a job are its runs in a row that it flags, following
each re-run back to the job it ran again. A job is re-run when a policy that
flags it counts at most `max_reruns` attempts, for `failed` the re-runs of the
job's failure code, and the job's re-run chain holds fewer than 16 runs. Otherwise `swap` marks it failed if it flags it, and the
others keep its result.

An evaluated member costs one pipette-duckdb read over its plan's answered
jobs, and one more for each step back along the re-run chains of its flagged
jobs, up to 15; the model params check reads no more, and the smaller failed
check reads the answered jobs' sources from the plan store. Every member costs
one database read of its waiting re-runs. A
`waiting_for_devices` or `refused` member costs those first reads alone. Members are read one
after another. The first pipette-duckdb failure or timeout ends the reads:
each later member that would read rows is `unreadable`. The model params table
and the benchmark catalog are read once per preview.

It takes `read_plans` and `read_clients`, as the failed check counts the
device failures a caller without `read_clients` does not see, and the group
must be in the caller's reach
([who may do what](../campaign-groups.md#who-may-do-what)). It lists only the
members whose plans the caller reads: every member with `read_any_plans`, else
the caller's own campaigns and its organization's. A caller that reads the
group only through `manage_any_orgs`, or as the administrator of a disabled
member, gets none of its members.

## Statuses

| Status | When |
|--------|------|
| 200 | Each member's preview |
| 400 | `group_id` is not a group id |
| 403 | The group is outside the caller's reach ([who may do what](../campaign-groups.md#who-may-do-what)), or the caller holds no `read_plans` or no `read_clients` |
| 404 | No such group, or the server has no `[database]` or no `[duckdb]` |
