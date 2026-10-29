# `POST /api/v1/users`

Create an account, optionally with a set.

The body is `CreateUserRequest`
([The OpenAPI document](../../httpapi.md#15-the-openapi-document)). Its
`permissions` are bounded by the ceiling
([the rules that make delegation safe](../users.md#the-rules-that-make-delegation-safe)),
and a scoped caller may name only its own `org_id`, which omitting it means.
Nor may it create an address a Clerk provision rule gives to an organization
other than the caller's, or to one that does not exist, whose first sign-in
would land in that account. A
`service` account is a job's, and never signs in through Clerk
([accounts and keys](../users.md#accounts-and-keys)). A
`campaign_scope` makes it a campaign reader, which reads those campaigns alone
and holds `read_plans` alone
([A campaign reader reads its campaigns alone](../users.md#the-rules-that-make-delegation-safe)).

Responds `201` with the record and its resolved set. The account is created
`active`: it exists because somebody decided it should, unlike the `pending` one
a keyless [`POST /api/v1/users/register`](register_user_key.md) asks for.

The account holds **no key**. Its holder signs in by registering one with a
user pre-auth key minted for it, over HTTP by its administrator
([`POST /api/v1/users/{user_id}/preauth-keys`](mint_preauth_key.md)) or on the
server host
([`pipette-mgmt users preauth create`](../../cli.md#pipette-mgmt-users-preauth-createlistrevokeprune)), and
presented at [`POST /api/v1/users/register`](register_user_key.md). Registering
with the account's address instead is refused: a `409`, since the account
already holds it, or the `403` of
[`POST /api/v1/users/register`](register_user_key.md#statuses) where
`require_user_preauth_key = true` or a Clerk provision rule covers the address.

## Statuses

| Status | When |
|--------|------|
| 201 | The record and its resolved set, created `active` |
| 400 | A service account with no `org_id` from a caller over the fleet: a service account belongs to an organization; or a service account granted `manage_orgs`, `manage_clients` or an `_any_` name; or a `campaign_scope` on a person's account, one granting a name beside `read_plans`, one naming a campaign that does not exist or that the account's organization does not own, or more than 100 |
| 403 | The set exceeds what the caller holds, or the organization is not the caller's; or the caller is scoped and a Clerk provision rule gives the address's domain to an organization other than the caller's, or to one that does not exist; or the caller is scoped and belongs to no organization, so there is nowhere to create; or the caller is a service account; or the `campaign_scope` names a campaign whose plans the caller does not read |
| 409 | An account already holds that address; or the organization is disabled, and takes no new member |
