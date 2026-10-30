# `GET /api/v1/orgs/{org_id}`

One organization and its counts. It answers `OrgView`.
`{org_id}` is the id or the handle
([either spelling](../orgs.md#either-spelling-of-org_id)).

## Statuses

| Status | When |
|--------|------|
| 404 | No such organization, or, under the bare `read_orgs`, any organization other than the caller's own ([who may do what](../orgs.md#who-may-do-what)) |
