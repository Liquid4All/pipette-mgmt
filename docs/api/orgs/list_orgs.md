# `GET /api/v1/orgs`

The organizations in the caller's scope, each with its member and device
counts. It answers `OrgView`, as
`OrgViewPage`, every row in one page
([pagination](../../httpapi.md#13-pagination)). It carries
**counts, not rosters**: the rosters are
[`GET /api/v1/orgs/{org_id}/users`](list_org_users.md) and
[`GET /api/v1/orgs/{org_id}/clients`](../devices/list_org_clients.md).
`?counts=false` leaves both counts out of every row and spares the reads of
every membership and ownership.

Under the bare `read_orgs` it lists the caller's own organization alone
([who may do what](../orgs.md#who-may-do-what)).

## Statuses

| Status | When |
|--------|------|
| 200 | The organizations in scope, in one page |
| 400 | `counts` is not a boolean |
| 403 | The caller holds no `read_orgs` |
