# `POST /api/v1/plans/{plan_id}/cancel`

Record a cancellation request. The HTTP form of `pipette-mgmt plans cancel`.

**It writes the marker and stops.** `queue-maintenance` owns the `cancelling` /
`cancelled` latch and the job teardown, because a status write from the request
path would race that pass's own status refresh
([plan lifecycle](../plans.md#plan-statuses)). So the plan's
status has not moved when the response arrives, `202` says the work is accepted
rather than done, and the plan's jobs stay claimable until the next pass runs.

Re-cancelling is a no-op: the marker is a plain idempotent write.

## Permissions

| Plan | Permission |
|---|---|
| One filed into the caller's own campaign or its organization's, whoever filed it, or into a disabled member's personal one the caller winds down ([campaigns: who may do what](../campaigns.md#who-may-do-what)) | `cancel_plans` |
| One the caller filed outside every campaign | `cancel_plans` |
| Any other: another owner's campaign's, a deleted campaign's, or one attributed to nobody outside a campaign | `cancel_any_plans` |

`cancel_plans` comes only from `cancel_any_plans`
([the permission model](../users.md#permissions)): a cancellation is destructive
and is granted on its own, so `read_plans` and `submit_plans` buy no part of it.

## Response

The body is `PlanCancelRequested`: what the request did, and nothing of the
plan, since `cancel_plans` reads nothing. A caller that wants the plan asks
[`GET /api/v1/plans/{plan_id}`](get_plan.md) under `read_plans`.

## Who learns that a plan exists

The route checks the permission **before** it reads the manifest, so a caller
that holds no cancel grant gets `403` whether or not the plan exists.

A caller that holds only `cancel_plans` gets `403` for a plan outside its scope
**and** for an id that names no plan. The two answers are identical, so the
caller cannot learn that an id outside its scope resolves. The read routes
conceal the same fact with a `404` ([one `404` for both](get_plan.md#one-404-for-both)). This route conceals
it with the refusal that a plan outside the scope earns anyway.

A caller that holds `cancel_any_plans` gets `404` for an id that names no plan.
Its scope is every plan, so "no such plan" tells it nothing that it cannot
already read.

## Statuses

| Status | When |
|--------|------|
| 202 | The request is recorded; the work is accepted rather than done, and the plan's status has not moved yet |
| 400 | `plan_id` in the path contains characters outside `[A-Za-z0-9-]` |
| 401 | Missing or invalid auth headers, or an `X-Client-Id` that is not a `user_…` |
| 403 | The caller holds no cancel grant; or it holds only `cancel_plans`, and the plan is outside its scope ([permissions](#permissions)) or does not exist |
| 404 | No such plan, for a caller that holds `cancel_any_plans` |
| 409 | The plan is `complete`: every job reached a terminal state, so there is nothing to tear down |
