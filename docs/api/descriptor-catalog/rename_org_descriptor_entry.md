# `POST /api/v1/orgs/{org_id}/descriptor-catalog/{name}/rename`

Give a hand-made entry in the named organization a new name. The path takes
the organization identifier (ID) and the current, unqualified catalog name.
The caller needs `edit_catalog` and membership, or `edit_any_catalog`.

The JavaScript Object Notation (JSON) body gives the new name:

```json
{"name": "LFM2.5-Audio-1.5B"}
```

The new name follows the grammar of
[`create_org_descriptor_entry`](create_org_descriptor_entry.md). No
tombstoned entry in the organization can hold it. A change of letter case only
is also a rename, and the old spelling still finds the entry.

If another live hand-made entry in the organization holds the new name, the API
merges the entry into that entry. The merge accepts the members as
[`add_org_descriptor_member`](add_org_descriptor_member.md) would accept them:

- The two entries hold the same kind of descriptor.
- Runtime members have the same runtime type and the same build.
- No two members have the same flavor, the same format and quantization, or the
  same digest.

A member with the digest and the key of a member of the target entry is
already in the target entry, so the merge skips it. The other members go to
the end of the target entry, in their order. Each member keeps the time that it
was added. The target entry keeps its state and its release time. The API then
deletes the renamed entry. Nothing undoes a merge.

The old name is free at once. Lookup does not find the entry by the old name,
and a later entry can take it. In the organization, a bare old name selects
the global entry of that name, if one exists. A stored plan holds descriptors,
not names, so a rename does not change a stored plan. Result rows show the new
name at once.

The audit log keeps the request event. A rename adds a `catalog_rename` event
with the stored old name and the new name. A merge adds a `catalog_merge` event
with the stored old name, the name of the target entry, and the keys of the
moved and the skipped members.

The response is the entry view under the new name. After a merge, it is the
view of the target entry with its new members. A list cursor that names
the old name gives `400` on the next page. Start the list again.

## Statuses

| Status | When |
|--------|------|
| 200 | The API renamed the entry, or merged it into the entry with the new name |
| 400 | The body is invalid, the new name fails the name grammar, or the members of a merge differ in kind, runtime type, or build |
| 403 | The caller lacks catalog edit permission |
| 404 | The organization is unreachable, the entry is absent, or the server has no database catalog |
| 409 | The entry is tombstoned, the new name is identical to the current name, a tombstoned or importer entry holds the new name, or two members of a merge have the same key or digest |
