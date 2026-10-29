# `GET /api/v1/users/{user_id}/keys`

The keys the account signs in with, one per machine, as
[`GET /api/v1/users/me`](get_own_user.md) lists its own: it answers them in one
page ([Pagination](../../httpapi.md#13-pagination)), oldest first. Listing them is the first step
in retiring a stolen key
([`DELETE /api/v1/users/{user_id}/keys/{key_id}`](revoke_user_key.md)).

## Statuses

| Status | When |
|--------|------|
| 200 | The keys, oldest first |
| 403 | The caller does not hold `manage_orgs` |
| 404 | The account is outside the caller's scope or does not exist |
