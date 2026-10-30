# `GET /api/v1/descriptor-catalog/devices/{client_id}`

List the catalog members offered to a registered device. The caller needs
`read_clients`, the same fleet-read permission as
`GET /api/v1/clients/{client_id}`. An unknown device returns `404`.

The response is one whole list. `items` contains pinned and active entries
only: first the runtime entries, then the model entries, each newest release
first. Each item has the entry's `name`, `release_time`, `descriptor_kind`
and `members`. Each member contains its selector, structured `member_key`,
digest, and full descriptor.

- **A runtime entry** gives one member: the first member in precedence that
  the device's stored flags and assigned tags satisfy. `member_key.flavor` is
  the selected flavor.
- **A model entry** gives every member that the catalog offers to the
  device. A model has no offer rules. The catalog offers a model member to the
  device if it offers the device a runtime member that can load the model.
  Every offered member of a pinned or active runtime entry counts. This
  includes a member that precedence does not select. A model entry has no
  precedence, so the list does not select one member. Each model member also
  gives its `model_key` and `parameters`.

Offer rules exist for `llamacpp_cli_stock_tools` and `ovms` only. The catalog
can offer a device only the `gguf` members and the `openvino` members of model
entries.

`next_cursor` is always `null`; `unreadable` counts catalog records that could
not be read.

`withheld` reports members that the device cannot use, including members of
entries that still offer another member. `member_count` counts those members;
`entry_count` counts pinned or active entries with no offered member. Each
record in `members` gives the entry name, selector, member key, reason code,
and readable reason. The reason codes are:

- `no_rule`: no offer rule exists for the runtime member's flavor.
- `unsupported`: offer rules do not support the runtime member's type.
- `not_met`: the device does not meet the runtime member's offer rule.
- `no_runtime`: no runtime member offered to the device can load the model
  member. The reason names the model type, for example
  "no offered runtime loads `onnx_genai`".

Hidden and tombstoned entries are absent from both lists and counts.

## Statuses

| Status | When |
|--------|------|
| 200 | The whole short list and withheld report |
| 403 | The caller lacks `read_clients` |
| 404 | The device is unknown, or the server has no `[database]` catalog |
