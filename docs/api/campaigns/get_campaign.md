# `GET /api/v1/campaigns/{campaign_id}`

The campaign record, under the listing's gate
([who may do what](../campaigns.md#who-may-do-what)). Each row of the
[listing](list_campaigns.md) carries the same record, and a create, a rename
and a hand-off answer it with more beside it. It takes the listing's two flags,
and with them answers the listing's row:

| Query parameter | Meaning |
|---|---|
| `names=true` | `names`, the owner's address or handle ([names](../../httpapi.md#14-names)) |
| `plan=true` | `plan`, as a [listing](list_campaigns.md) row carries it |

The body is `NamedCampaignView`; unasked, it
is the record alone.

## Statuses

| Status | When |
|--------|------|
| 200 | The campaign |
| 400 | `campaign_id` is not a campaign id, or a flag is not a boolean |
| 403 | The campaign is outside the caller's reach ([who may do what](../campaigns.md#who-may-do-what)), or the caller holds neither `read_plans` nor `manage_orgs` |
| 404 | No such campaign |
