# `GET /api/v1/orgs/{org_id}/client-preauth-keys`

The pre-auth keys naming the organization, without their secrets. What such a
key is, and the scope these routes share, is
[client pre-auth keys](../devices.md#client-pre-auth-keys).

Answers the keys in one page ([pagination](../../httpapi.md#13-pagination)), newest first, each
the body [`POST`](mint_client_preauth_key.md) answers without `token`. It is every key whose owner is the organization, so
a key the host minted with `--owner-org` is on it: that one has no `minted_by`,
and carries what it seeds as `default_tags`, `default_organization` and
`default_permissions`. A key naming one of the organization's members is that
member's and is not here. A spent single-use key is gone, and an expired one
stays, marked `expired`, until `clients preauth prune` reaps it.

## Statuses

| Status | When |
|--------|------|
| 403 | The caller does not hold `manage_clients` |
| 404 | The organization is outside the caller's scope or does not exist |
