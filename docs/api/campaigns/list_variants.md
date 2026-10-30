# `GET /api/v1/campaigns/{campaign_id}/variants`

The plan a variant edit revises and its `[[variants]]` blocks, in the order
`remove` numbers them, for a form to draw before it sends the edit: the body is
`CampaignVariants` ([the OpenAPI document](../../httpapi.md#15-the-openapi-document)).

`base.plan_id` is the `base_plan_id` the edit then names. The read takes the
edit's gate ([`POST …/variants`](edit_variants.md)), so it refuses
where the edit would.

## Statuses

| Status | When |
|--------|------|
| 200 | The plan the edit revises, and its variants |
| 400 | `campaign_id` is not a campaign id |
| 403 | The caller holds no campaign authority, the campaign is outside its scope, or it lacks the grants a [variant edit](edit_variants.md) names over the plan the campaign runs |
| 404 | No such campaign |
| 409 | The campaign runs no plan, or runs one with no stored document |
