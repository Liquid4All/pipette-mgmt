# `DELETE /api/v1/campaigns/{campaign_id}`

Deletes the campaign record, which is only a pointer, and keeps every plan it
named: each plan keeps its manifest, its jobs and its lifecycle, and is read
and cancelled from then on under the `_any_` grants alone
([who may do what](../campaigns.md#who-may-do-what)). It takes the rename's gate
([`PATCH`](update_campaign.md)). Answers `204` with no body.

The route refuses the delete while a plan the campaign filed still runs:
`creating`, `active` or `pending_clients` with no cancel requested, or a head
filed while the route read the plans. Cancel each first
([`POST /api/v1/plans/{plan_id}/cancel`](../plans/cancel_plan.md)). The `409` names each such plan by
id, whatever the caller's plan scope, as
[`GET …/plans`](list_campaign_plans.md) does.

## Statuses

| Status | When |
|--------|------|
| 204 | The campaign record is deleted; its plans are kept |
| 400 | `campaign_id` is not a campaign id |
| 403 | The caller may not administer the record ([who may do what](../campaigns.md#who-may-do-what)) |
| 404 | No such campaign |
| 409 | A plan the campaign filed still runs. The message names each one |
