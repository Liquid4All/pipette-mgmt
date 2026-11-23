# `POST /api/v1/users/register`

Bind a public key to the account a `user_preauth_…` invite names. This is the
user form of `POST /clients/register`. Nothing here asks for an account: one
comes from a Clerk sign-in ([signing in through Clerk](../users.md#signing-in-through-clerk))
or an administrator ([`POST /api/v1/users`](create_user.md), `pipette-mgmt users create`).

**Unauthenticated**: a registration is the one call with no key bound to an
account yet to sign with. The user pre-auth key's secret is the credential.

The body is `RegisterUserKeyRequest`
([The OpenAPI document](../../httpapi.md#15-the-openapi-document)):
`public_key`, `key_details` and `user_preauth_key`, all required. The key names
the account, so the body sets no field of the record: a holder cannot redirect
the registration at an identity the operator did not choose. The invite is
minted by an administrator of the account
([`POST /api/v1/users/{user_id}/preauth-keys`](mint_preauth_key.md)) or on the
host (`pipette-mgmt users preauth create`).

## Response

The body is `UserKeyRegistered`
([The OpenAPI document](../../httpapi.md#15-the-openapi-document)). Its `status`
is the account's standing, which registering does not change.

There is no fast path: the pre-auth key is consumed before anything is looked
up, so an interrupted registration needs a freshly minted invite to retry.

## Statuses

| Status | When |
|--------|------|
| 201 | Registered |
| 400 | The body lacks `user_preauth_key` or holds another field, such as `contact_email`; or `key_details` longer than 4096 characters |
| 401 | The pre-auth key is malformed, unknown, expired, spent, or names an account that no longer exists |
| 403 | The key was minted over HTTP, and its minter could not mint it now ([`POST /api/v1/users/{user_id}/preauth-keys`](mint_preauth_key.md)) |
| 409 | The key was minted over HTTP, and somebody has signed in as the account since; or the keypair is already registered to another principal ([accounts and keys](../users.md#accounts-and-keys)) |
