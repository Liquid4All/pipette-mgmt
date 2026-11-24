# `PATCH /api/v1/campaigns/{campaign_id}`

Renames the campaign, sets its labels, or locks, archives or reactivates it. Needs
membership of the owning organization with `submit_plans` (a person's account)
or `manage_orgs`, or `manage_any_orgs`
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
delete still work. The record carries `state` only while it is not `active`. It
is the campaign's own state, beside the `status` of the plan it runs.

`state` is also `locked`, which needs `state_note`: why the campaign is locked,
at most 4096 characters. A locked campaign takes no new plan, as an archived
one, and also refuses a cancel of the plan it runs and a delete; each `409`
quotes the note. The listings show it. A rename, a relabel and a hand-off still
work. `state_note` goes with `locked` alone, any other state clears it, and
the record carries it only while locked. Sending `locked` again with another
note replaces the note. A locked campaign is not archived directly: unlock it
first.

`name` is trimmed, non-empty and at most 4096 characters, as on a
[create](create_campaign.md). A body with no field, or one asking for
the state the record is already in, writes nothing. Answers `200` with the
[record](get_campaign.md) and `changed`, whether the
write changed it; `changed: false` is a success, so the write is safe to retry.

The edit lands only on the record as read, so it never undoes a write that
landed meanwhile, such as a revision moving the campaign's plan. If the record
changed since it was read, the answer is `409` and nothing was written: send it
again.

## Statuses

| Status | When |
|--------|------|
| 200 | The campaign, and whether the write changed it |
| 400 | `campaign_id` is not a campaign id; the body is not JSON, names another field, or carries an empty or overlong `name` or an unknown `state`, `locked` without a `state_note`, or a `state_note` without `locked`; or `labels` breaks a tag rule (the message names the key where there is one) |
| 403 | The caller may not administer the record ([who may do what](../campaigns.md#who-may-do-what)) |
| 409 | Another write changed the campaign while this edit was made: nothing was written; send it again. Or the body archives a locked campaign |
