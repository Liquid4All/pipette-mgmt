# `GET /api/v1/plans/{plan_id}`

One plan's status: the manifest's client-facing projection, the same body
[`pipette-mgmt plans status`](../../cli.md#pipette-mgmt-plans-liststatusresultscancel)
prints.

`plan_id` in the path, and `names=true` to add the submitter's address
([names](../../httpapi.md#14-names)).

## Response

The body is `PlanView`.
`fleet_detail` is `"full"` with `read_clients` or `read_any_clients`, and
`"withheld"` otherwise. `warnings` are the **frozen ingestion-time** fleet-match
groups, where `progress_snapshot.starved_groups` is the live form; the two can
legitimately disagree once clients register after ingestion.

At the withheld tier, each warning keeps its `job_ids` and a reason rebuilt
from their count, such as `1 job matches no working client`. Each
`progress_snapshot.starved_groups[]` keeps only `job_ids`; `requires`, `any_of`,
`clients`, `exclude_clients`, `excludes`, and `not_all_of` are absent. At the full
tier, the clause fields retain their stored shape, including omission of empty
negative clauses. `counts` and `computed_at` are present at both tiers.

`progress_snapshot` and `terminal_at` are rendered as `null` rather than omitted,
so a caller can tell "not computed yet" / "still live" from a field the schema
lacks. `note`, `change`, `submitted_by`, `campaign_id` and `previous_plan_id` are
omitted instead, because absence there is a final answer.

The two campaign fields carry **ids only, and are not gated on the campaign's own
scope**: a caller reading this plan is already inside the plan scope, and an id
says nothing `GET /api/v1/campaigns/{campaign_id}` would not refuse to say itself. They
are what makes a campaign's history walkable from either end, so a caller holding
a plan can follow it back without reading the campaign at all.

The plan's job list is not here;
[`GET /api/v1/plans/{plan_id}/jobs`](list_plan_jobs.md) answers it.

## One `404` for both

The `404` covers both an absent plan and one outside the caller's scope
deliberately. A `403` on a plan the caller cannot see would confirm that the id
names a real plan, which is the one thing an own-scoped caller must not learn.
`GET /jobs/{job_id}` follows the same rule for another client's job.

## Statuses

| Status | When |
|--------|------|
| 200 | The plan |
| 400 | `plan_id` in the path contains characters outside `[A-Za-z0-9-]`, or `names` is not a boolean |
| 401 | Missing or invalid auth headers, or an `X-Client-Id` that is not a `user_…` |
| 403 | Caller holds neither `read_plans` nor `read_any_plans` |
| 404 | No such plan, **or** a plan the caller's scope does not admit ([one `404` for both](#one-404-for-both)) |
