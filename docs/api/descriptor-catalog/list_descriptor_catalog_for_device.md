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

Each member also gives its run `evidence`. The evidence tells what happened
when the member ran. It never selects an entry or a member. It is about the
member that the list gives, not about the other members of the entry. For
example, if the device resolves the CUDA member, the evidence is about the
CUDA member, also when the CPU member succeeded on the device.

- `this_device` gives `last_success` and `last_failure` on this device. Each
  is `null`, or an object with the time `at` that the device submitted the
  run and the `client_version` that ran it.
- `any_device` is weaker evidence. It gives `last_success_at`, the time of
  the last success on any device, and `devices_with_success`, the number of
  devices with a success. A success on other hardware says little about this
  device.

A run can fail because of the model or the benchmark. So a success suggests
that a build works, and does not prove it. A member matches a run through its
digest, or through its verified alias. The scorer records each run as it
scores it. A run that the server failed, and a run that a recalculation
scored again, give no evidence. Without a `[database]`, `evidence` is absent.

Offer rules can offer stock llama.cpp, OVMS, and the three runtimes built into
the iOS app. The catalog offers a model member when the device can use an
offered runtime that loads it.

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
- `no_build`: the device meets the offer rule of a runtime that the iOS app
  builds in, but does not report the member's build flag. The reason names
  the flag, for example "device does not report build flag
  `runtime:descriptor:<digest>`".
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
