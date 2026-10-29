# Descriptor catalog

The descriptor catalog gives a runtime descriptor or a model descriptor a
readable name. An active
account can list entries, look up names, and find the names of a descriptor
digest. The host command-line interface (CLI) writes global entries. An
organization member with `edit_catalog`, or a holder of `edit_any_catalog`,
can write organization entries through the Hypertext Transfer Protocol (HTTP)
application programming interface (API). Each request reads the store, so
the next request shows a change that either path makes.

Organization entry writes use these routes:

- [`POST /api/v1/orgs/{org_id}/descriptor-catalog`](descriptor-catalog/create_org_descriptor_entry.md)
  creates an entry.
- [`POST /api/v1/orgs/{org_id}/descriptor-catalog/{name}/members`](descriptor-catalog/add_org_descriptor_member.md)
  appends a member.
- [`POST /api/v1/orgs/{org_id}/descriptor-catalog/{name}/state`](descriptor-catalog/change_org_descriptor_entry_state.md)
  changes state or tombstones an entry with confirmation.

These routes accept an organization identifier (ID). The create and append
routes write runtime entries and model entries. Global entries have no HTTP
write route.

Each entry has a scope: `global`, or one organization. Every active account
reads the global entries. A member of an organization also reads the entries
of that organization. For every other account, an entry of an organization
does not exist.

Each scope has its own namespace, so an org entry can have the name of a
global entry. Each response gives the `scope` of an entry and its
`display_name`. The `display_name` is the name as the caller sees it:

- An entry of the caller's organization shows as the bare name.
- A global entry shows as `global/<name>` if a live entry of the caller's
  organization has the same name.
- Every other global entry shows as the bare name.

A tombstoned org entry does not change the name of a global entry.

Entries contain members. A runtime member's key is its flavor. A model
member's key is its format and quantization. The list shows each entry's member
count and member keys. Use `members=true` to include full descriptors. Name
lookup always includes them. A `selector` gives the complete member name. It
starts with the `display_name`, then `::flavor` for a runtime or
`::format::quantization` for a model.

The response fields describe both runtime entries and model entries. A model
member with its full descriptor also gives `model_key` and `parameters`. The
`model_key` field is the key of the member's row in the
[model parameter table](model-params.md). The `parameters` field holds the
counts in that row. The members of one model entry can have different keys,
because they come from different repositories.

To see what one registered device can use now, read
[`GET /api/v1/descriptor-catalog/devices/{client_id}`](descriptor-catalog/list_descriptor_catalog_for_device.md).
For each pinned or active runtime entry, it selects one offered member. For
each model entry, it gives every member that an offered runtime member can
load. It also reports every member that the device cannot use. This read
needs `read_clients`. The short list contains global entries only. Its
selectors use the bare name.

The [import-run history](descriptor-catalog/list_catalog_import_runs.md) gives
the newest import and refresh results and the last successful import time.

For a runtime member, `coverage` has one of these values:

- `covered`: an offer rule exists for the member.
- `no_rule`: the runtime type has a flavor table, but no rule exists for the
  member's flavor.
- `unsupported`: the runtime type has no flavor table.

`coverage` does not say whether a device meets the rule.

Hidden entries remain readable. No response includes a tombstoned entry.
Only a `[database]` store supports these routes.
