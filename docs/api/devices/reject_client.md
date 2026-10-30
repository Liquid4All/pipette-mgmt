# `POST /api/v1/clients/{client_id}/reject`

Refuse a **pending** registration: deletes it and frees its keypair.

Responds `204`, and is **a delete**, matching `clients reject`. There is no
"rejected" state on a client record, and inventing one would leave a row nothing
reads: what an operator means is that the registration was not wanted, so the
identity goes and the keypair is freed for a device that re-registers.

The `409` is the point: the verb for turning away a stranger must not remove a
machine that is doing work. Use [`DELETE`](delete_client.md) deliberately for that.

## Statuses

| Status | When |
|--------|------|
| 204 | The registration is deleted and its keypair freed |
| 404 | No such device |
| 409 | The device is not `pending` |
