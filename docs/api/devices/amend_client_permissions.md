# `PATCH /api/v1/clients/{client_id}/permissions`

Grant or revoke what the device may do: `submit_results`, `perform_work`. What
the permissions mean, the scope, and the answer both permission routes share are
[device permissions](../devices.md#device-permissions).

`PATCH` amends, with `add` and `remove`: at least one, and no name in both. It goes through the
store's read-modify-write, so two administrators amending at once keep each
other's work, and a stored name this build does not define stays where it is.

## Statuses

| Status | When |
|--------|------|
| 200 | `ChangedClientPermissions`; `changed: false` means the device already held what was asked for |
| 400 | Neither `add` nor `remove`, a name in both, or a name a device cannot hold |
| 403 | The caller does not hold `manage_clients`; the device is another organization's; or it is nobody's and the caller is scoped to one |
| 404 | No such device |
