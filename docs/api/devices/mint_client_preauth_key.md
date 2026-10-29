# `POST /api/v1/orgs/{org_id}/client-preauth-keys`

Mint the pre-auth key that registers a device approved and owned by the
organization. What such a key is, and the scope these routes share, is
[client pre-auth keys](../devices.md#client-pre-auth-keys).

Takes `MintClientPreauthRequest` ([the OpenAPI document](../../httpapi.md#15-the-openapi-document)):
no key minted here outlives 90 days, and none is permanent. Any other field is a
`400`, rather than a key without what it asked for. **A
key minted here seeds the owner and nothing else.** It carries no tags and no
permission template: a device registered with it holds the registration
default, `submit_results`
([defaults](../users.md#defaults-and-groups)), and one that claims
queued work is granted `perform_work` once it has registered
([device permissions](../devices.md#device-permissions)). Tags stay the host's. The device's
`organization` label stays what it sent.

The key records the caller as `minted_by`, and registering with it checks the
minting again, against the state at registration: it is refused with `403` once
the minter is no longer an active account whose `manage_clients` reaches the
organization. So a copy kept by an administrator who has since left registers
nothing. The refusal spends no key and creates no device; whoever administers
the organization now revokes it. Unlike a user's key
([`POST /api/v1/users/{user_id}/preauth-keys`](../users/mint_preauth_key.md)), a mint replaces nothing: an organization
keeps one key per rack if it wants to.

Responds `201` with `ClientPreauthKeyMinted`, the key and its token, **the one
time the token is shown**: only its `sha256` is stored, and the answer carries
`Cache-Control: no-store`.

## Statuses

| Status | When |
|--------|------|
| 201 | The key and its token, the one time the token is shown |
| 400 | An unknown field, a `usage` other than `single_use` or `multi_use`, `expires_in_days` outside 1 to 90, or a `note` past 4096 characters; or `org_id` is not an organization id |
| 403 | The caller does not hold `manage_clients` |
| 404 | The organization is outside the caller's scope or does not exist |
