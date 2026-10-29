# `GET /api/v1/plans`

List the plans the caller may read in ascending plan-key order, or newest first
under `order=newest`. A **user** route: it gates on
`read_plans`, which is grantable to a user alone
([the vocabulary](../users.md#permissions)), so a client
presenting an `ev1_…` is refused at the header rather than at the permission.

## Query parameters

| Parameter | Description |
|-----------|-------------|
| `scope` | Omit it for the widest scope the caller's grant allows: `all` for a `read_any_plans` holder, `mine` otherwise. `all` without `read_any_plans` is a `403`, never a silently narrowed listing |
| `submitted_by` | Narrow to the plans one account submitted, the plan's own `submitted_by`. A **filter**, not a permission: the scope settles what the caller may see and this narrows within it, so a holder of plain `read_plans` naming somebody else gets what they filed into the caller's campaigns, possibly nothing, rather than a `403` |
| `campaign_id` | Narrow to the plans filed into one campaign ([campaigns](../campaigns.md)), on `submitted_by`'s rule. A plan filed outside every campaign is selected by no value here |
| `limit` | Maximum rows to return. Defaults to `list_default_limit`; must not exceed `list_max_limit` ([pagination](../../httpapi.md#13-pagination)) |
| `cursor` | Opaque position from the preceding response's `next_cursor`. Omit for the first page |
| `order` | `oldest`, the default, or `newest`: descending plan-key order, which is creation order ([pagination](../../httpapi.md#13-pagination)) |
| `names` | `true` adds `names`, each submitter of the plans served by its address ([names](../../httpapi.md#14-names)). A `submitted_by` filter outside the scope names nobody |

Under `scope=mine` a plan filed into a campaign is the caller's when the caller
reads that campaign: one the caller owns, or one its organization owns. Whoever
filed the plan does not matter, and a deleted campaign's plans are nobody's. A
plan filed outside every campaign is the caller's when its `submitted_by`
equals the caller's `user_id`. One that names **no** submitter (one a client
filed before only users could, or one written before the field existed) is
therefore reachable under `read_any_plans` and under no own scope.

## Response

The [pagination](../../httpapi.md#13-pagination) envelope, whose `items` are
each the body [`GET /api/v1/plans/{plan_id}`](get_plan.md) returns, plus
`names` when asked. An unreadable manifest is counted in `unreadable` rather
than dropped, so it does not read as "no such plan"; its key stays in the
server log.

## Statuses

| Status | When |
|--------|------|
| 200 | A page of the plans in the caller's scope |
| 400 | `status`, `scope`, `order` or `names` is invalid; `submitted_by` is not a user id, or the query carries a parameter this route does not list; `campaign_id` is not a campaign id; `limit` is zero or above the configured maximum; or `cursor` is malformed |
| 401 | Missing or invalid auth headers, or an `X-Client-Id` that is not a `user_…` |
| 403 | Caller holds neither `read_plans` nor `read_any_plans`, or asked for `scope=all` without `read_any_plans` |
