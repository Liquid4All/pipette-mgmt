# `PATCH /api/v1/campaigns/{campaign_id}`

Renames the campaign, sets its labels, or archives or unarchives it. Needs
`submit_plans` for your own campaign, or `manage_orgs` for an organization's
([who may do what](../campaigns.md#who-may-do-what)).

`PATCH` takes `UpdateCampaignRequest` ([the OpenAPI document](../../httpapi.md#15-the-openapi-document)) and sets each field its body names and leaves the rest: `name`, and
`labels`, the whole set as an object of `key: value` strings, so a key the
object leaves out is dropped and `{}` clears them. Labels are free pairs to
group and filter campaigns by, on the rules of an AWS resource tag
([the campaign record](../campaigns.md#the-campaign-record)); a set outside them is a `400`
naming the key where there is one, and writes nothing. Nothing on the server
reads a label, and no route filters by one: a reader of
[`GET /api/v1/campaigns`](list_campaigns.md) narrows the rows itself.

`state` is `archived` or `active`. An archived campaign is hidden from the
console's listings unless their state filter asks for it, and
[`GET /api/v1/campaigns`](list_campaigns.md) filters on it with `state`. It
takes no new plan: filing one, a variant edit, a document revision, a re-run, a
recalculation and a revert are each a `409` until `active` unarchives it. The
plan it runs keeps running. Every read, a rename, a relabel, a hand-off and a
delete still work. The record carries `state` only while it is `archived`. It
is the campaign's own state, beside the `status` of the plan it runs.

`name` is trimmed, non-empty and at most 4096 characters, as on a
[create](create_campaign.md). A body with no field, or one asking for
the state the record is already in, writes nothing. Answers `200` with the
[record](get_campaign.md) and `changed`, whether the
write changed it; `changed: false` is a success, so the write is safe to retry.

## Statuses

| Status | When |
|--------|------|
| 200 | The campaign, and whether the write changed it |
| 400 | `campaign_id` is not a campaign id; the body is not JSON, names another field, or carries an empty or overlong `name` or an unknown `state`; or `labels` breaks a tag rule (the message names the key where there is one) |
| 403 | The caller may not administer the record ([who may do what](../campaigns.md#who-may-do-what)) |
| 404 | No such campaign |
