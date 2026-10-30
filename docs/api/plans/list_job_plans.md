# `GET /api/v1/jobs/{job_id}/plans`

List the plans that asked for a job, and read the body of that job. Each plan
that asks for a job writes a link from the job to the plan. The first plan also
writes the [job source](../../plan-ingestion.md#8-ingestion-flow-atomic-ish):
the body that the job ran, with `job_id` and `expires_at` stamped. The
job source stays after the job leaves the queue, so this route also answers
for a finished job.

A **user** route. It gates on `read_plans`, widened by `read_any_plans`
([permissions](../users.md#permissions)). It admits each
plan exactly as
[`GET /api/v1/plans/{plan_id}`](get_plan.md) does.
The response leaves out each plan that the caller's scope does not admit, so it
names no plan of another tenant. If no plan remains, the route answers `404`.
The same `404` answers for a job that no plan links, so a caller cannot learn
that a job exists from a plan that it cannot read.

The `200 OK` body is `JobPlans`: the plans in the caller's scope that asked
for the job, oldest first and in any status, each as a campaign row's `plan`
([`GET /api/v1/campaigns`](../campaigns/list_campaigns.md)), and `body`, the job source's
`spec`, eligibility, `job_id` and `expires_at`, with every `auth_token` removed
at every depth.

`body` is `null` only when the job source was deleted from storage by hand: no
code path deletes one.

The eligibility is the six fields `requires`, `any_of`, `clients`, `excludes`,
`not_all_of`, and `exclude_clients`. They name client ids and requirement sets,
so the full tier gets them and the withheld tier does not
([fleet disclosure](../devices.md#fleet-disclosure)). A withheld
`body` keeps `job_id`, `expires_at`, and `spec`. The route omits other top-level
fields for every job source, so a plan that joined under the v1 merge rule
cannot read top-level data only the first submitter supplied.

Each error answers in the JSON `{"error"}` envelope. A `500` answers when the
store failed to list links, a linked manifest cannot be read after another
linked plan establishes scope, the job source is not JSON, or the pending
cancel markers do not list.

## Statuses

| Status | When |
|--------|------|
| 200 | The plans in the caller's scope that asked for the job, and the job's body |
| 400 | `job_id` in the path contains characters outside `[A-Za-z0-9-]` |
| 401 | Missing or invalid auth headers, or an `X-Client-Id` that is not a `user_…` |
| 403 | Caller holds neither `read_plans` nor `read_any_plans` |
| 404 | No readable linked plan can be confirmed in the caller's scope, including when the only linked manifests cannot be read. The message is `no readable plan holds this job` |
