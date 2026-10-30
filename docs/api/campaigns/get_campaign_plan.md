# `GET /api/v1/campaigns/{campaign_id}/plan`

Resolves the stable id to the plan's status view in one request, returning the
same body [`GET /api/v1/plans/{plan_id}`](../plans/get_plan.md) does. Doing it in two requests would show the
pointer moving in between. It takes that route's `names=true` too, which adds the
submitter's address ([names](../../httpapi.md#14-names)). It takes the listing's gate
([`GET /api/v1/campaigns`](list_campaigns.md)), **plus** `read_plans`.

The plan is read through the **caller's plan scope**, so the answer is the one
the plan route gives. The campaign's own plans are in that scope for every reader
of the campaign holding `read_plans` ([who may do what](../campaigns.md#who-may-do-what)), so an
administrator holding `manage_orgs` alone reads the pointer and is refused the
plan. A pointer naming a plan the campaign did not file answers `404` unless the
caller's scope admits that plan on its own.

**The plan's own reads go by its id.** Its jobs, results and document are the
`/api/v1/plans` routes', addressed by the `plan_id` this answers:
[`GET /api/v1/plans/{plan_id}/jobs`](../plans/list_plan_jobs.md),
[`GET /api/v1/plans/{plan_id}/results.csv`](../plans/get_plan_results_csv.md) and
[`GET /api/v1/plans/{plan_id}/document`](../plans/get_plan_document.md). Resolve the
pointer once and page by that id: a read that resolved it again on every page
could straddle a revision, half one plan and half the next.

## Statuses

| Status | When |
|--------|------|
| 200 | The plan the campaign points at |
| 400 | `campaign_id` is not a campaign id, or `names` is not a boolean |
| 403 | The campaign is outside the caller's scope, or the caller holds no plan read |
| 404 | No such campaign, the campaign points at no plan, or the plan is outside the caller's plan scope |
