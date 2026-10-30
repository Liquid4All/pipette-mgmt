# `POST /api/v1/users/register/clerk`

Bind a public key to the account the token names, for a browser that holds no
key. The token is a Clerk token in `Authorization: Bearer`. The key expires in
7 days.

Present when `[clerk]` is set, and a `404` otherwise. The token is the
credential, as the pre-auth key is at
[`POST /api/v1/users/register`](register_user_key.md). It is checked, and the
account provisioned under a rule, as
[signing in through Clerk](../users.md#signing-in-through-clerk)
states.

The body is `RegisterClerkUserKeyRequest`
([The OpenAPI document](../../httpapi.md#15-the-openapi-document)). It is read
before the token, so a malformed one is a `400` that leaves the token unspent.
The answer is
[`POST /api/v1/users/register`'s](register_user_key.md#response-201-created),
and the key carries an `expires_at` 7 days out: the token proved a sign-in, not
a machine, so the key lapses like a session rather than staying for good.

The account's first sign-in here, once its key is registered, records the keys
the account already held and logs them, as
[the first Clerk sign-in](../users.md#signing-in-through-clerk) states.

## Statuses

| Status | When |
|--------|------|
| 201 | Registered, as at [`POST /api/v1/users/register`](register_user_key.md#response-201-created), the key expiring 7 days out |
| 400 | A malformed body, read before the token, so the token is left unspent |
| 404 | `[clerk]` is not set |
