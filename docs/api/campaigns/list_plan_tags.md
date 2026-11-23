# `GET /api/v1/campaigns/{campaign_id}/tags`

Every tag of the campaign, newest first, as `PlanTags`
([the OpenAPI document](../../httpapi.md#15-the-openapi-document)). Each
`PlanTag` names its `plan_id`, `name`, `message`, `tagger` and `created_at`.
[`POST`](create_plan_tag.md) on this path makes one.

## Statuses

| Status | When |
|--------|------|
| 200 | The campaign's tags |
