# `DELETE /api/v1/users/{user_id}/keys/{key_id}`

Revokes one of the keys the account signs in with
([`GET /api/v1/users/{user_id}/keys`](list_user_keys.md)), as its holder does at
[`DELETE /api/v1/users/me/keys/{key_id}`](revoke_own_key.md), and answers `204`.
The key stops signing on the next request, its keypair is free to register
again, and the account's other keys go on signing. The host's
[`pipette-mgmt users key revoke`](../../cli.md#pipette-mgmt-users-key-listrevoke)
does the same.

This is how a stolen key is retired. Disabling the account
([`PATCH /api/v1/users/{user_id}`](update_user.md)) stops every key and enabling
it restores them all, including any the thief registered with the stolen one.
So list the account's keys and revoke the stolen one, and every key added since
it was stolen (the listing runs oldest first), before the account is restored. A
revocation retires a key, not the person: its holder can register another,
through Clerk or with a key it still holds, so disable the account to stop the
person.

A key id is revoked through the account that holds it: another account's path
answers `404`, and that key goes on signing. The caller's own account is no
exception: its keys revoke here as at `DELETE /api/v1/users/me/keys/{key_id}`,
the one signing the request included.

## Statuses

| Status | When |
|--------|------|
| 204 | Revoked: the key stops signing on the next request |
| 403 | The caller does not hold `manage_orgs`; or, for a scoped caller, the account holds a server-wide name it lacks ([the rules that make delegation safe](../users.md#the-rules-that-make-delegation-safe)) |
| 404 | The account is outside the caller's scope or does not exist; or it holds no such key |
