# `PATCH /api/v1/clients/{client_id}`

Update `organization`, `client_details`, `contact_email`, or `name`. In the
`UpdateClientRequest` body, set at least one field to a non-null value.
The server returns `400` if the request sets no field.
`organization` is the free-text label the device sent about itself,
**not** the ownership relation of
[`PUT /api/v1/clients/{client_id}/owner`](set_client_owner.md). `status`, `public_key` and
`registered_at` are not here: the first has its own verbs, the other two are the
identity.

`name` is the operator's name for the device. The device cannot set it, and
its own `PATCH /clients/me` keeps it. The server trims the value. A blank value
clears the name. If `name` is absent or `null`, the name does not change.

Responds `200` with the record plus `changed`. A field set to the value it
already held is **not** a change, so `changed: false` means the device already
matched what was asked for.

## Statuses

| Status | When |
|--------|------|
| 200 | The record plus `changed`; `changed: false` means the device already matched what was asked for |
| 400 | The body sets no field |
| 409 | The record kept changing under the write (another operator, or the device's own `PATCH /clients/me`); nothing was written, resend |
