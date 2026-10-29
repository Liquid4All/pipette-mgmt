# `PUT /api/v1/clients/{client_id}/owner`

Sets or clears who owns a device, within the caller's scope
([device administration](../devices.md)). It resolves the owner against the
roster before writing, and is a device write under `manage_clients`
([memberships and ownerships](../orgs.md#memberships-and-ownerships)).

It responds with the updated record plus `changed`
([write answers](../../httpapi.md#16-write-answers)). Naming the owner the
device already has (`null` for none) is the no-op.

```json
{ "owner": { "type": "org", "org_id": "org_0195c3e2-…" } }
```

**One owner shape everywhere.** The same object names who owns a device, a
campaign ([campaigns](../campaigns.md)) and a vault resource ([the vault](../vault.md)), and is
answered in the same shape on every read. `type` is the tag. A `null` value
clears the relation.

The route checks the device first, as every device write does
([device administration](../devices.md)), and the destination's scope before its
existence, as [`PUT /api/v1/campaigns/{campaign_id}/owner`](../campaigns/set_campaign_owner.md) does. An unknown device
is a `404` whatever the body names, and a scoped caller cannot tell a real owner
outside its reach from one that does not exist.

## Statuses

| Status | When |
|--------|------|
| 200 | The ownership is written, or already held; the body is the updated record plus `changed` |
| 400 | The body is not JSON, omits `owner`, or holds a key other than `owner`. Nothing is written: only an explicit `null` clears |
| 403 | The caller does not hold `manage_clients`, or the move leaves its scope: without `manage_any_clients`, a user owner other than the caller is outside it |
| 404 | The device named in the path does not exist; or, only for a caller whose scope admits the move, the organization or the account named in the *body* does not exist, a scoped caller getting the `403` |
