# `DELETE /api/v1/orgs/{org_id}/client-preauth-keys/{key_id}`

Revoke one. What such a key is, and the scope these routes share, is
[client pre-auth keys](../devices.md#client-pre-auth-keys).

Answers `204`, and the key registers nothing more. A key id
is revoked through the organization it names, so another organization's path
answers `404`.

## Statuses

| Status | When |
|--------|------|
| 204 | The key registers nothing more |
| 400 | `org_id` is not an organization id |
| 403 | The caller does not hold `manage_clients` |
| 404 | The organization is outside the caller's scope or does not exist; or no such key names it |
