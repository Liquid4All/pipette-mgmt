# `GET /api/v1/campaign-groups`

The groups in the caller's reach, in the standard cursor-paginated envelope
([pagination](../../httpapi.md#13-pagination)), whose `items` are group records
without their members. The reach is the campaign listing's: the groups of every
owner whose campaigns the caller reads
([who may do what](../campaign-groups.md#who-may-do-what)). A campaign reader's
page is always empty. The filters narrow **inside** that reach and never widen
it, so naming an owner outside it gives an empty page rather than a `403`:

| Query parameter | Meaning |
|---|---|
| `owner` | `org` for organization-owned, which is every group; any other value is a `400` |
| `org_id` | One organization's. Usable on its own |
| `limit`, `cursor`, `order` | [Pagination](../../httpapi.md#13-pagination) |

## Statuses

| Status | When |
|--------|------|
| 200 | A page of groups |
| 400 | A malformed `owner`, `org_id`, `cursor` or `order`, or a `limit` outside the allowed range |
| 403 | The caller holds neither `read_plans` nor `manage_orgs` |
