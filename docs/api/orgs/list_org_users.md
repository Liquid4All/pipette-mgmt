# `GET /api/v1/orgs/{org_id}/users`

The accounts belonging to the organization. The body is `OrgUserRowPage`, every row
in one page ([pagination](../../httpapi.md#13-pagination)). It does not repeat
the organization's id, so a caller that named it by handle reads the id from
[`GET /api/v1/orgs/{org_id}`](get_org.md), or from the `org_id` every member
row carries.

## Statuses

| Status | When |
|--------|------|
| 200 | The organization's members, in one page |
| 400 | `org_id` is neither an organization id nor a handle |
| 403 | The caller holds no `read_orgs` |
| 404 | No such organization, or, under the bare `read_orgs`, any organization other than the caller's own ([who may do what](../orgs.md#who-may-do-what)) |
