# `DELETE /api/v1/users/{user_id}/preauth-keys/{key_id}`

Revoke one of the account's pre-auth keys
([`POST /api/v1/users/{user_id}/preauth-keys`](mint_preauth_key.md)). It answers
`204`, and the key registers nothing more. A key id is revoked through the
account it names, so another account's path answers `404`.

## Statuses

| Status | When |
|--------|------|
| 204 | Revoked: the key registers nothing more |
| 403 | The caller does not hold `manage_orgs`; or, for a scoped caller, the account holds a server-wide name it lacks ([the rules that make delegation safe](../users.md#the-rules-that-make-delegation-safe)) |
| 404 | The account is outside the caller's scope or does not exist; or no such key names it |
