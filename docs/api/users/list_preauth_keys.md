# `GET /api/v1/users/{user_id}/preauth-keys`

The pre-auth keys naming the account, without their secrets. What a user pre-auth key is,
and what bounds its mint, is at
[`POST /api/v1/users/{user_id}/preauth-keys`](mint_preauth_key.md).

It answers the keys in one page
([Pagination](../../httpapi.md#13-pagination)), newest first, each the mint's
answer without `token`. A spent single-use key is gone from the listing, and an
expired one stays, marked `expired`, until `users preauth prune` reaps it. A key
the host minted has no `minted_by`. Whether a mint could admit the account's
first machine is the account record's `awaits_sign_in`
([`GET /api/v1/users/{user_id}`](get_user.md)), so a page offers the mint then,
or for a service account. The `POST` still answers `403` for an account wider
than the caller.

## Statuses

| Status | When |
|--------|------|
| 200 | The keys, newest first |
| 403 | The caller does not hold `manage_orgs` |
| 404 | The account is outside the caller's scope or does not exist |
