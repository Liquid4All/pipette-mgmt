# `POST /api/v1/users/{user_id}/preauth-keys`

Mint the pre-auth key that registers a machine.

A user pre-auth key ([accounts and keys](../users.md#accounts-and-keys)),
minted for one account by an administrator of it: the key its holder presents at
[`POST /api/v1/users/register`](register_user_key.md) to register the account's
**first** machine, admitted at once. The host's `pipette-mgmt users preauth`
mints and lists the same records, with none of the bounds below.
[`GET /api/v1/users/{user_id}/preauth-keys`](list_preauth_keys.md) lists them and
[`DELETE /api/v1/users/{user_id}/preauth-keys/{key_id}`](revoke_preauth_key.md)
revokes one.

It mints one for an account nobody has signed in as yet, or for a service
account at any time, holding nothing the caller does not
([the rules that make delegation safe](../users.md#the-rules-that-make-delegation-safe)).
It takes `MintUserPreauthRequest`
([The OpenAPI document](../../httpapi.md#15-the-openapi-document)): no key
minted here outlives 90 days, and none is permanent. Any other field is a
`400`, rather than a key without what it asked for. A key minted here is
single-use, so there is no `usage`: the account's holder adds its other
machines itself ([`POST /api/v1/users/me/keys`](add_own_key.md)), except a
service account's, which takes a key minted here for each. No user pre-auth key
carries a permission template: an account's set is granted on the account
([`PATCH /api/v1/users/{user_id}/permissions`](amend_permissions.md)).

The key records the caller as `minted_by`, and it **replaces** every other key
minted here for the account, so one is outstanding. Registering with it
([`POST /api/v1/users/register`](register_user_key.md)) checks the minting
again, against the state at registration: it is refused with `409` once anybody
has signed in as a person's account, so a copy kept back after the person signed
in admits nothing, and with `403` once the minter could not mint it: the minter
is no longer an active account that administers the account, or the account now
holds a name the minter lacks or, a campaign reader, reads a campaign the minter
does not. The key is spent either way.

Those checks bound the moment of registration, not what follows it. Whoever
holds the token registers the account's first machine, so an administrator who
keeps it rather than handing it over signs in as the account, and that key
stays on it when the person later signs in through Clerk and reaches whatever
the account is granted afterwards. The account's keys are listed on its own
page and at [`GET /api/v1/users/me`](get_own_user.md), and its holder revokes
one it does not recognize
([`DELETE /api/v1/users/me/keys/{key_id}`](revoke_own_key.md)), or its
administrator does
([`DELETE /api/v1/users/{user_id}/keys/{key_id}`](revoke_user_key.md)). Two
mints racing can also each revoke the other's key and leave none outstanding;
minting again settles it.

Responds `201` with `UserPreauthKeyMinted`, the key and its token, **the one time
the token is shown**: only its `sha256` is stored, and the answer carries
`Cache-Control: no-store`.

## Statuses

| Status | When |
|--------|------|
| 201 | `UserPreauthKeyMinted`, the key and its token, shown this once |
| 400 | An unknown field, `expires_in_days` outside 1 to 90, or a `note` longer than 4096 characters |
| 403 | The caller does not hold `manage_orgs`; or the account holds a name the caller does not, or one this build does not know; or the account is a campaign reader reading a campaign whose plans the caller does not; or, for a scoped caller, the account holds a server-wide name it lacks ([the rules that make delegation safe](../users.md#the-rules-that-make-delegation-safe)) |
| 404 | The account is outside the caller's scope or does not exist |
| 409 | A person's account that somebody has signed in as, that predates the record of first sign-ins, or that already holds a key |
