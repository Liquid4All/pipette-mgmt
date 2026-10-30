# `PATCH /api/v1/clients/{client_id}`

Update `organization`, `client_details`, `contact_email`. The body is
`UpdateClientRequest`, with at least one field, or the response is
a `400`. `organization` is the free-text label the device sent about itself,
**not** the ownership relation of
[`PUT /api/v1/clients/{client_id}/owner`](set_client_owner.md). `status`, `public_key` and
`registered_at` are not here: the first has its own verbs, the other two are the
identity.

Responds `200` with the record plus `changed`. A field set to the value it
already held is **not** a change, so `changed: false` means the device already
matched what was asked for.

## Statuses

| Status | When |
|--------|------|
| 200 | The record plus `changed`; `changed: false` means the device already matched what was asked for |
| 400 | The body sets no field |
| 409 | The record kept changing under the write (another operator, or the device's own `PATCH /clients/me`); nothing was written, resend |
