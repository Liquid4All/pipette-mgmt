# `POST /api/v1/campaigns/{campaign_id}/tags`

Tags one plan the campaign filed, the current one or a superseded one, as an
annotated git tag does a commit. Needs `submit_plans` and the right to file into
the campaign ([who may do what](../campaigns.md#who-may-do-what)), in any
state: a tag does not move the campaign's plan, so a
[locked or archived](update_campaign.md) campaign takes one too.

The body is `CreatePlanTagRequest` ([the OpenAPI document](../../httpapi.md#15-the-openapi-document)):

- `plan_id`: a plan whose manifest names this campaign.
- `name`: 1 to 64 letters, digits, `.`, `_`, `-` or `/`, with no `..` or `//`
  and no `.` or `/` at either end. Unique within the campaign.
- `message`: why the plan is tagged, at most 4096 characters.

The server adds the caller as `tagger` and the time as `created_at`. A tag made
by `pipette-mgmt campaigns tag` has no `tagger`. No route changes, moves or
deletes a tag. The campaign's delete removes its tags.

Answers `201` with the `PlanTag`. The tags show in
[`GET …/tags`](list_plan_tags.md), in [`GET …/plans`](list_campaign_plans.md),
on each row of the [overview](get_campaign_overview.md)'s `history`, and as
`current_plan_tags` on the [listing](list_campaigns.md)'s rows with `plan=true`.

## Statuses

| Status | When |
|--------|------|
| 201 | The tag |
| 400 | The body is not JSON, names another field, or carries a bad `name`, an overlong `message` or a malformed `plan_id` |
| 403 | The caller may not file into the campaign |
| 404 | No such campaign, or the campaign filed no such plan |
| 409 | The campaign already has a tag of that name |
