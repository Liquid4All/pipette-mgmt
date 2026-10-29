# `GET /api/v1/campaigns`

The campaigns in the caller's reach, in the standard cursor-paginated envelope
([pagination](../../httpapi.md#13-pagination)), whose `items`
are campaign records. The filters narrow **inside** the caller's reach and never
widen it. Naming an owner outside it gives an empty page rather than a `403`:

| Query parameter | Meaning |
|---|---|
| `owner` | `mine` for the caller's own, `org` for organization-owned |
| `org_id` | One organization's. Usable on its own |
| `order` | `oldest` (the default) or `newest` ([pagination](../../httpapi.md#13-pagination)) |
| `names=true` | `names` beside the page, for the owners of its rows ([names](../../httpapi.md#14-names)) |
| `plan=true` | Each row's `plan`, below |
| `state` | `active` or `archived`: the campaigns in that state ([archiving](update_campaign.md)). Absent, every state |
| `plan_id` | The one campaign this plan was filed into, current or superseded: one page, no cursor. An unknown plan, one filed into none, or a campaign outside the reach or the owner filter is an empty page; a malformed id, a `cursor` beside it, or a `limit` the listing would refuse is a `400` |

Unasked, a row is the campaign record. With `plan=true`, a row whose campaign
runs a plan the caller's plan scope admits ([who may do what](../campaigns.md#who-may-do-what)) adds
`plan`, `PlanSummary`: each key as
[`GET /api/v1/plans/{plan_id}`](../plans/get_plan.md) answers it, and
`counts` its `progress_snapshot`'s (`null` until `queue-maintenance` counts the
plan).
The scope is judged by the campaign the plan itself names, not by the row,
since a campaign's pointer is no proof the plan is its own. A row has no `plan`
for a campaign that names none, for a plan outside the scope (a `manage_orgs`
holder without `read_plans` reads none), and for one whose manifest does not
read, which costs that row and not the page. A campaign that does not read
fails the page with `500`.

Without `state`, archived campaigns are listed like any other, each row
carrying `"state": "archived"`. The console's listings read every state and
hide archived rows themselves, so they can count what they hide.

## Statuses

| Status | When |
|--------|------|
| 200 | A page of campaigns |
| 400 | A malformed `plan_id`, a `cursor` beside it, a `limit` the listing would refuse, or an unknown `state` |
