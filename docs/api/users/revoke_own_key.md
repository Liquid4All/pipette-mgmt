# `DELETE /api/v1/users/me/keys/{key_id}`

Revoke one registration, including the last one and the one signing the
request. A holder whose only key is compromised must be able to retire it at
once; a fresh pre-auth key re-registers.

An administrator of the account revokes its keys the same way at
[`DELETE /api/v1/users/{user_id}/keys/{key_id}`](revoke_user_key.md), which says
what a revocation does and how a stolen key is retired.

## Statuses

| Status | When |
|--------|------|
| 204 | Revoked |
