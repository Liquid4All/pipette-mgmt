# `GET /api/v1/plans/{plan_id}/explain`

Why the plan's jobs are not scheduled. The HTTP form of
`plans status --explain`: it walks the gates a job clears to reach a client and
names the first one that blocks every outstanding job.

A separate route rather than a field on [`GET /api/v1/plans/{plan_id}`](get_plan.md)
because of what it costs: it lists the queue, the leases, the denials, the
eligible index and the fleet.

## Two gates, and why they are separate

`read_plans` decides whether the caller may ask about **this plan**.
`read_clients` decides how much of the **fleet** the answer may name
([fleet disclosure](../devices.md#fleet-disclosure)). A submitter
must be able to ask why its own plan is stuck whatever it holds, so the fleet
right gates the *projection*, not the route, exactly as it does for the
`warnings` in the report of a filing route.

| Field | Without `read_clients` | With `read_clients` or `read_any_clients` |
|---|---|---|
| `scheduling_state`, `headline`, `census`, `read_at`, `snapshot_computed_at`, `starved_by_live_scan` | present | present |
| `groups[].job_count` | present | present |
| `groups[].description`, `groups[].never_matches` | absent | present (diagnostic only when applicable) |
| `fleet` | absent | present |
| `groups[].ladder`, `groups[].roster`, `groups[].live_*_client_count` | absent | present |

`fleet_detail` is `"withheld"` or `"full"`, so a reader can tell an absent count
from a zero one.

`census` counts the **caller's own jobs**, so it is not gated. At the withheld
tier the headline uses one fixed sentence per fleet-dependent state: it does
not name which fleet gate blocks clients, count unblocked clients, or reveal
whether cached starved groups now match a working client. `description` and `never_matches`
are omitted because both can quote the clause set.

The residual at the withheld tier is one bit per requirement set (something
matches, or nothing does), and that bit is inseparable from reporting starvation
at all.

## Response

The body is `PlanExplanation`. Its `scheduling_state` is the first
gate that blocks every outstanding job, and `headline` says it in one sentence at the
caller's tier, the string `plans status --explain` prints at the full tier. A
`null` `snapshot_computed_at` means no `queue-maintenance` pass has run for the
plan, which by itself explains a plan that reads `active` and runs nothing.
`groups` holds every cached starved group whatever the state: a partially
starved plan clears the chain and still holds groups worth reading.

A `scheduling_state` of `schedulable` means at least one claimable job matches an unblocked
client holding no denial marker for it. The plan is not blocked, and the answer
is to wait.

## Statuses

| Status | When |
|--------|------|
| 200 | The explanation; a caller with `read_plans` and no fleet right gets the withheld projection, never a `403` |
| 400 | `plan_id` in the path contains characters outside `[A-Za-z0-9-]` |
| 401 | Missing or invalid auth headers, or an `X-Client-Id` that is not a `user_…` |
| 403 | Caller holds neither `read_plans` nor `read_any_plans` |
| 404 | No such plan, **or** a plan the caller's scope does not admit ([one `404` for both](get_plan.md#one-404-for-both)) |
