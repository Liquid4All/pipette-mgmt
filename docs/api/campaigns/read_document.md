# `GET /api/v1/campaigns/{campaign_id}/document`

The plan a document revision replaces and the document that plan was filed
from, for a form to start from before it sends the revision: the body is
`CampaignDocument` ([the OpenAPI document](../../httpapi.md#15-the-openapi-document)).

`base.plan_id` is the `base_plan_id` the revision then names. The read takes
the revision's gate ([`PUT …/document`](revise_document.md)), so it
refuses where the revision would, with the statuses of
[`GET …/variants`](list_variants.md). It also answers `409` when
the campaign runs no plan, or runs one with no stored document.

In that order: the caller's authority first, so a caller holding none reads
`403` whether or not the campaign exists.

## Statuses

| Status | When |
|--------|------|
| 200 | The plan the revision replaces, and its document |
| 400 | `campaign_id` is not a campaign id |
| 403 | The caller holds no campaign authority, the campaign is outside its scope, or it lacks the grants a [variant edit](edit_variants.md) names over the plan the campaign runs |
| 404 | No such campaign |
| 409 | The campaign runs no plan, or runs one with no stored document |
