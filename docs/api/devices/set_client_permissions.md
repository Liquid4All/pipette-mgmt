# `PUT /api/v1/clients/{client_id}/permissions`

Replace the device's permission set whole. What the permissions mean, the
scope, and the answer both permission routes share are
[device permissions](../devices.md#device-permissions).

`PUT` takes `{"permissions": [...]}` and replaces the whole set, dropping a
stored name this build does not define (which [`PATCH`](amend_client_permissions.md)
leaves where it is). An empty list clears it: the device keeps its identity and
may do nothing.

## Statuses

| Status | When |
|--------|------|
| 200 | `ChangedClientPermissions`; `changed: false` means the device already held what was asked for |
| 400 | No `permissions`, or a name a device cannot hold |
| 403 | The caller does not hold `manage_clients`; the device is another organization's; or it is nobody's and the caller is scoped to one |
| 404 | No such device |
