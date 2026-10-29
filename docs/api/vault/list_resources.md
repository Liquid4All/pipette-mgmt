# `GET /api/v1/vault/resources`

The resources in the caller's reach, in the standard cursor-paginated envelope
([pagination](../../httpapi.md#13-pagination)), whose `items` are resources as
[`GET /api/v1/vault/resources/{resource_id}`](get_resource.md) describes them.
Any active account reads it. Two filters narrow **inside** the caller's reach
and never widen it; naming an owner outside it gives an empty page rather than
a `403`:

| Query parameter | Meaning |
|---|---|
| `owner=mine` | Only the resources this account owns |
| `owner=org` | Only organization-owned resources |
| `org_id=…` | Narrow to one organization |
| `limit`, `cursor`, `order` | [Pagination](../../httpapi.md#13-pagination) |
| `names=true` | `names` beside the page, for the owners of its rows ([names](../../httpapi.md#14-names)) |
| `object=true` | Each row's `object`, as [`GET /api/v1/vault/resources/{resource_id}`](get_resource.md) has it, or `{"state": "unknown"}` where the store did not answer for that file |

## Statuses

| Status | When |
|--------|------|
| 200 | A page of resources |
