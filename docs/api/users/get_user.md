# `GET /api/v1/users/{user_id}`

One account. It answers `UserDetail`, which adds what the reader may do to the
account, as the writes judge it
([the rules that make delegation safe](../users.md#the-rules-that-make-delegation-safe)),
to the account's record and sets
([account administration](../users.md#account-administration)). A reader whose
`manage_orgs` covers the account's organization, even one outranked, also gets
`awaits_sign_in`: `true` while nobody has signed in as the account and it holds
no key, what a pre-auth mint for a person
([`POST /api/v1/users/{user_id}/preauth-keys`](mint_preauth_key.md)) and a
retype to a service
([`PATCH /api/v1/users/{user_id}`](update_user.md)) both need.

## Statuses

| Status | When |
|--------|------|
| 200 | The account, what the reader may do to it, and, to a reader whose `manage_orgs` covers its organization, `awaits_sign_in` |
| 404 | The account is outside the caller's scope or does not exist |
