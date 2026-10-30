# `POST /api/v1/clients/{client_id}/approve`

Admit the device, so it may claim and submit work.

Answers `ClientApproved` ([the OpenAPI document](../../httpapi.md#15-the-openapi-document)): the device as
[`PATCH /api/v1/clients/{client_id}`](update_client.md) answers it, plus the `previous_status`
it moved from. **Idempotent**: approving an approved device answers `200` with
`changed: false` and no `previous_status`, rather than refusing. A retry whose
response was lost costs nothing.

## Statuses

| Status | When |
|--------|------|
| 200 | The device is approved; `changed: false` and no `previous_status` if it already was |
| 409 | The record kept changing under the write (another operator, or the device's own `PATCH /clients/me`); nothing was written, resend |
