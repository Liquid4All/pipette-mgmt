# `POST /api/v1/clients/{client_id}/unsuspend`

Clear the `todo/` suspension set when a client held more than one live lease.

Answers the device as
[`PATCH /api/v1/clients/{client_id}`](update_client.md) does, with `changed: false` for a device
that was not suspended. Under `manage_any_clients` it also clears a suspension
whose device record is gone, so no shell access is needed; with no device to
answer with, that call and its repeat answer `204`.

## Statuses

| Status | When |
|--------|------|
| 200 | The suspension is cleared; `changed: false` when there was none |
| 204 | No device record answers to the id; its suspension is cleared |
