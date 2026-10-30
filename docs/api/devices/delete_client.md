# `DELETE /api/v1/clients/{client_id}`

Responds `204`, removing the record, its keypair claim and its queue state.
Once the record is gone the device authenticates nothing; the rest is
best-effort, with `queue-maintenance` as the backstop.

**Idempotent** under `manage_any_clients`: deleting a device that is not there
is also a `204`, not a `404`. A teardown that stopped part-way is finished by
repeating the call, which is worth more than distinguishing a mistyped id. A
caller bounded by `manage_clients` gets `404` instead: its scope check needs
the record, so it cannot show that an absent device was ever its to delete.

## Statuses

| Status | When |
|--------|------|
| 204 | The record, its keypair claim and its queue state are removed; under `manage_any_clients`, also when there was no such device |
| 404 | No such device, for a caller bounded by `manage_clients` |
