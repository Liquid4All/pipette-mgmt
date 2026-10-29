# `GET /api/v1/campaigns/{campaign_id}/plans`

The counterpart of the [`POST`](file_plan.md) on this path: you file plans into a campaign, and
this is what you filed, its head and then what that supersedes, as `CampaignPlans`
([the OpenAPI document](../../httpapi.md#15-the-openapi-document)).
The walk is bounded, and `history_truncated` says when it stopped with revisions
still behind it.

**Ids, not rows, and not scope-checked.** A plan id discloses nothing on its own,
and resolving each through the reader's plan scope would be one read per revision
to produce a list with holes in it. For a plan's state ask
[`GET /api/v1/plans/{plan_id}`](../plans/get_plan.md), which
applies that scope itself; for all of them with their state ask
[`GET /api/v1/plans?campaign_id=…`](../plans/list_plans.md) ([pagination](../../httpapi.md#13-pagination)), which filters inside the
caller's plan scope and so simply omits what they may not see.

## Statuses

| Status | When |
|--------|------|
| 200 | What the campaign has run: its head, then what that supersedes |
