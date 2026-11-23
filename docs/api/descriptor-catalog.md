# Descriptor catalog

The descriptor catalog gives a runtime descriptor or a model descriptor a
readable name. An active account can use these read routes:

- [`GET /api/v1/descriptor-catalog`](descriptor-catalog/list_descriptor_catalog.md)
  lists entries.
- [`GET /api/v1/descriptor-catalog/names`](descriptor-catalog/lookup_descriptor_names.md)
  looks up entries by name.
- [`GET /api/v1/descriptor-catalog/by-digest`](descriptor-catalog/lookup_descriptor_digests.md)
  finds the names of a descriptor digest.
- [`GET /api/v1/descriptor-catalog/family-logos/{sha256}`](descriptor-catalog/get_descriptor_family_logo.md)
  gets the logo of a model family.

The host command-line interface (CLI) writes global entries. An
organization member with `edit_catalog`, or a holder of `edit_any_catalog`,
can write organization entries through the Hypertext Transfer Protocol (HTTP)
application programming interface (API). Each request reads the store, so
the next request shows a change that either path makes.

Organization entry writes use these routes:

- [`POST /api/v1/orgs/{org_id}/descriptor-catalog`](descriptor-catalog/create_org_descriptor_entry.md)
  creates an entry.
- [`POST /api/v1/orgs/{org_id}/descriptor-catalog/{name}/members`](descriptor-catalog/add_org_descriptor_member.md)
  appends a member.
- [`POST /api/v1/orgs/{org_id}/descriptor-catalog/{name}/rename`](descriptor-catalog/rename_org_descriptor_entry.md)
  gives a hand-made entry a new name.
- [`POST /api/v1/orgs/{org_id}/descriptor-catalog/{name}/members/relabel`](descriptor-catalog/relabel_org_descriptor_member.md)
  changes the quantization label of a model member.
- [`POST /api/v1/orgs/{org_id}/descriptor-catalog/{name}/members/remove`](descriptor-catalog/remove_org_descriptor_member.md)
  removes a member from a hand-made entry.
- [`POST /api/v1/orgs/{org_id}/descriptor-catalog/{name}/family`](descriptor-catalog/set_org_descriptor_entry_family.md)
  sets the family of a model entry.
- [`POST /api/v1/orgs/{org_id}/descriptor-families/{family}/logo`](descriptor-catalog/set_org_descriptor_family_logo.md)
  sets or removes the logo of a model family.
- [`POST /api/v1/orgs/{org_id}/descriptor-catalog/{name}/state`](descriptor-catalog/change_org_descriptor_entry_state.md)
  changes state or tombstones an entry with confirmation.

These routes accept an organization identifier (ID). The create and append
routes write runtime entries and model entries. Global entries have no HTTP
write route.

Each entry has a scope: `global`, or one organization. Every active account
reads the global entries and its own organization's, and a holder of
`edit_any_catalog` reads every organization's. For any other account, an entry
of an organization does not exist. Each scope has its own namespace, so an org
entry can have the name of a global entry; each answer gives an entry's
`scope` and `display_name`.

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

Each model entry has a `family`, such as `LFM2.5`. The model pickers of the
clients group entries by family. An operator can set the family of an entry.
Otherwise, the family is the text of the entry name before its first hyphen,
or the whole name if it has no hyphen. A runtime entry has no `family`.

A model entry whose family has a logo also gives `family_logo_sha256`: the
SHA-256 of the logo Uniform Resource Identifier (URI). The logo comes from the
scope of the entry. An organization entry with no logo of its own for its
family gets the global logo for that family. The server ignores case in family
names when it selects a logo. A logo URI is under
`https://pipette.liquid.ai/logos/`, or it is a data
URI that holds the image. A data URI can be large, so a read gives only the
digest.

Get the logo URI with
[`GET /api/v1/descriptor-catalog/family-logos/{sha256}`](descriptor-catalog/get_descriptor_family_logo.md).
A device uses
[`GET /clients/me/descriptor-catalog/family-logos/{sha256}`](../httpapi.md#220-get-clientsmedescriptor-catalogfamily-logossha256).
A digest identifies one logo URI. When an operator changes a logo, the next
catalog read gives a new digest. Each signed logo request checks the caller's
current access.

A model member gives its download size in bytes in `size_bytes`. When the
size is unknown, including for vault model members, the value is `null`.
Runtime members omit the field. For a Hugging Face model, the server reads
the file sizes at the pinned commit when it adds the member. For GGUF, it
adds the sizes of the files named in the descriptor. For directory formats,
it adds the sizes of all files under the source's prefix.

To see what one registered device can use now, read
[`GET /api/v1/descriptor-catalog/devices/{client_id}`](descriptor-catalog/list_descriptor_catalog_for_device.md).
For each pinned or active runtime entry, it selects one offered member. For
each model entry, it gives every member that an offered runtime member can
load. It also reports every member that the device cannot use, and gives
each offered member its run evidence on this device and on every device. This
read needs `read_clients`. The short list contains the global entries and the
entries of the caller's organization. Each item and each withheld member gives
its `scope` and its `display_name`, and its selectors use that name.

The [import-run history](descriptor-catalog/list_catalog_import_runs.md) gives
the newest import and refresh results and the last successful import time.

For a runtime member, `coverage` has one of these values:

- `covered`: an offer rule exists for the member.
- `no_rule`: the runtime type has a flavor table, but no rule exists for the
  member's flavor.
- `unsupported`: the runtime type has no flavor table.

`coverage` does not say whether a device meets the rule.

Hidden entries remain readable. No response includes a tombstoned entry.
