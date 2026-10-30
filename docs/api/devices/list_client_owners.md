# `GET /api/v1/clients/{client_id}/owners`

Whom the caller may hand the device to: the choices of the device's owner field,
for the app
to post the one picked to [`PUT /api/v1/clients/{client_id}/owner`](set_client_owner.md),
which does not draw that boundary itself. It takes `read_clients`, then
`manage_clients` whose scope reaches the device.

A device inside an organization, owned by it or by a member of it, is offered
to that organization and its members only, whoever asks, a server admin
included: moving one to another organization is to detach it and claim it from
the unclaimed devices. A device in none is offered every organization and
account the caller's roster grant reads. Either way the caller's roster grant
bounds who is offered. Of the accounts, a caller without `manage_any_clients` is
offered only itself, the one person
[`PUT /api/v1/clients/{client_id}/owner`](set_client_owner.md) lets it name.

| Parameter | Type | Description |
|-----------|------|-------------|
| `q` | string | Keeps the owners whose handle, address or id holds it, case aside. Absent or empty keeps every one |
| `type` | string | `org` or `user` keeps that owner `type` alone. Absent or empty keeps both |

```json
{"owners": [{"value": "org_…", "name": "acme", "label": "acme", "type": "org"}]}
```

At most 20, organizations by handle, then accounts by address, an account's
`label` naming its organization. One whose `value`, `name` or `label` is `q`,
case aside, comes first, so a typed handle or address resolves however many
others hold it.

The statuses come in the order below: the device is read first, so an unknown
one is a `404` whatever the caller administers.

## Statuses

| Status | When |
|--------|------|
| 400 | `client_id` contains characters outside the id charset, or `type` is neither `org` nor `user` |
| 401 | Missing or invalid auth headers, or an `X-Client-Id` that is not a `user_…` |
| 403 | Caller holds neither `read_clients` nor `read_any_clients`; or `this device is outside your device scope`: no `manage_clients`, or a scope that does not reach the device |
| 404 | No such client, **or** one outside the caller's reach |
