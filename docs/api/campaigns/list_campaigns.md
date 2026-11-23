# `GET /api/v1/campaigns`

The campaigns in the caller's reach, in the standard cursor-paginated envelope
([pagination](../../httpapi.md#13-pagination)), whose `items`
are campaign records. The filters narrow **inside** the caller's reach and never
widen it. Naming an owner outside it gives an empty page rather than a `403`:

| Query parameter | Meaning |
|---|---|
| `owner` | `org` for organization-owned, which is every campaign; any other value is a `400` |
| `org_id` | One organization's. Usable on its own |
| `order` | `oldest` (the default) or `newest` ([pagination](../../httpapi.md#13-pagination)) |
| `names=true` | `names` beside the page, for the owners of its rows ([names](../../httpapi.md#14-names)) |
| `plan=true` | Each row's `plan` and `policy_checks`, below |
| `models=true` | Each row's `models`, below. Needs `plan=true` |
| `state` | `active`, `locked` or `archived`, or several comma-separated: the campaigns in those states ([states](update_campaign.md)). Absent or empty, every state |
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

With `plan=true`, an active campaign in a [group](../campaign-groups.md) also
adds `policy_checks`: each policy of the group that was on when
`queue-maintenance` last read the campaign, in kind order, with `kind` and
`triggered`, `true` while the policy flags a job of the plan or its re-runs
wait for their rows, `false` once it is cleared
([when a policy acts](../campaign-groups.md#when-a-policy-acts)). A row has no
`policy_checks` for a campaign in no group, an archived one, and one the pass
has not read yet. A group state that does not decode fails the page with
`500`.

With `models=true`, a row that has `plan` also adds `models`: the models that
the plan document names, each one time, in document order. Each model is its
source reference, for example `LiquidAI/LFM2-700M-MLX-4bit` or
`org/repo:model.gguf`. A speculative model joins its parts with `+`. A row has
no `models` if it has no `plan`, if the plan document does not read, or if
the document digest differs from the manifest. That costs the row its
`models`, and the page still answers. The console's listing
sends `models=true`, and its search matches each model.

Without `state`, archived campaigns are listed like any other, each row
carrying `"state": "archived"`. The console's listings send `state=active,locked`
unless the reader picks locked, archived or all, and no `state` beside a plan id or a
job id.

## Statuses

| Status | When |
|--------|------|
| 200 | A page of campaigns |
| 400 | An `owner` other than `org`, a `cursor` with `plan_id`, a `limit` outside the allowed range, an unknown `state`, or `models=true` without `plan=true` |
