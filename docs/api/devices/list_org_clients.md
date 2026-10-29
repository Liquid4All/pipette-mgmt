# `GET /api/v1/orgs/{org_id}/clients`

The devices the organization owns, directly or through its members. The body
is `ClientViewPage`, every row
in one page ([pagination](../../httpapi.md#13-pagination)). It does not repeat
the organization's id, so a caller that named it by handle reads the id from
[`GET /api/v1/orgs/{org_id}`](../orgs/get_org.md), or from the `org_id` every
member row of [`GET /api/v1/orgs/{org_id}/users`](../orgs/list_org_users.md)
carries.

The device listing is gated on a **fleet grant as well as a roster one**
([who may do what](../orgs.md#who-may-do-what)). Its
rows are client records, `contact_email` omitted unless the caller's
`manage_clients` reaches the organization
([visibility](../devices.md#what-a-device-row-carries)), so it is a fleet read filtered by organization:
serving it on `read_orgs` alone would let an account that administers people
enumerate device hardware and public keys without ever being granted fleet
access.

It includes **both owner kinds**, told apart by each row's `owner`: "who do I
talk to about this machine" has different answers for a device the organization
owns and one a member owns.

A user-owned device rolls up to whichever organization its owner belongs to
**now**, so moving an account moves its devices. That join is derived rather
than stored a second time, which is why there is no third route to keep in step.

## Statuses

| Status | When |
|--------|------|
| 200 | The organization's devices, in one page |
| 400 | `org_id` is neither an organization id nor a handle |
| 403 | The caller lacks `read_orgs` or `read_clients` |
| 404 | No such organization, or, under the bare `read_orgs`, any organization other than the caller's own ([who may do what](../orgs.md#who-may-do-what)) |
