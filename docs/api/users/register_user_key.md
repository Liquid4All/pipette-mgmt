# `POST /api/v1/users/register`

Bind a public key to an account. With a `user_preauth_…` secret the key names
an existing account and the registration is admitted `active`; with a
`contact_email` instead it asks for one, which is created `pending`. This is the
user form of `POST /clients/register`.

**Unauthenticated**, on both paths: a registration is the one call with no key
bound to an account yet to sign with. The user pre-auth key's secret, when
there is one, is the credential.

The body is `RegisterUserKeyRequest`
([The OpenAPI document](../../httpapi.md#15-the-openapi-document)). It carries
exactly one of `user_preauth_key` or `contact_email`: both together would be an
invite naming one account beside an address describing another, and neither is
no path at all. Each is a `400`. `contact_email` takes a client's address rules,
folded to lower case because it becomes the account's key.

**With an invite**, the key names the account, so the body sets no field of the
record: a holder cannot redirect the registration at an identity the operator
did not choose. The account is whatever it already was, normally `active`. The
invite is minted by an administrator of the account
([`POST /api/v1/users/{user_id}/preauth-keys`](mint_preauth_key.md)) or on the
host (`pipette-mgmt users preauth create`).

**With an address**, there is no account yet and this asks for one. The address
is the whole of what it asks with, because it is the whole of what an operator
decides on. The account is created `pending`, holds no permissions, and
authenticates nothing but [`GET /api/v1/users/me`](get_own_user.md) until
`users approve` admits it, which is how it watches for that. The key registered
here is the account's from the start, so nothing is registered a second time
once that happens. This is `POST /clients/register` without a
`client_preauth_key`, over the other keyspace, and
`require_user_preauth_key = true` closes it the way `require_client_preauth_key`
closes that one.

## Response `201 Created`

The body is `UserKeyRegistered`
([The OpenAPI document](../../httpapi.md#15-the-openapi-document)). Its `status`
is `active` on the invited path and `pending` on the request path, so it tells a
caller which one it took.

A request for an address **this same keypair is already registered at** returns
`200 OK` with that account instead: it is the caller retrying a `201` whose
response it never saw. Nothing else is idempotent here: an unregistered key
asking for a taken address is a `409`, because admitting it would be a stranger
having an operator approve *their* key on someone else's account. The invited
path has no fast path at all: the pre-auth key is consumed before anything is
looked up, so an interrupted invited registration needs a freshly minted invite
to retry.

## A failed request

A request that fails after claiming the address releases the claim, so the
address stays usable and a retry with a fresh keypair is one round trip. A
claim left stranded by a crash is an operator's to clear
(`users release-claim`), and is reported as a `409` naming that fact rather than
adopted: a stranded claim and a request still in flight are indistinguishable
from here.

## Statuses

| Status | When |
|--------|------|
| 200 | A request for an address this same keypair is already registered at: the caller retrying a `201` whose response it never saw |
| 201 | Registered: `status` is `active` on the invited path, `pending` on the request path |
| 400 | Both `user_preauth_key` and `contact_email`, or neither; or `key_details` longer than 4096 characters |
| 401 | The pre-auth key is malformed, unknown, expired, spent, or names an account that no longer exists |
| 403 | `require_user_preauth_key = true` and the body carries no invite; or no invite, and `contact_email` is at a domain a `[clerk]` provision rule names: that account comes from a Clerk sign-in ([signing in through Clerk](../users.md#signing-in-through-clerk)); or the key was minted over HTTP, and its minter could not mint it now ([`POST /api/v1/users/{user_id}/preauth-keys`](mint_preauth_key.md)) |
| 409 | The key was minted over HTTP, and somebody has signed in as the account since; or the keypair is already registered to another principal ([accounts and keys](../users.md#accounts-and-keys)); or an account already holds that address, and this keypair is not registered to it; or a claim on the address was left stranded by a crash |
| 429 | No invite, and this process has admitted 20 registrations without a pre-auth key, devices and accounts together, or 1 from this client's address, in the current minute; `Retry-After` gives the seconds to wait ([operations.md §5.6](../../operations.md#56-network-exposure-and-tls)) |
