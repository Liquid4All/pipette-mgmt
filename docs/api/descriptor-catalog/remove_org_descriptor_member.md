# `POST /api/v1/orgs/{org_id}/descriptor-catalog/{name}/members/remove`

Remove one member from a hand-made entry in the named organization. The path
takes the organization identifier (ID) and an unqualified catalog name. The
caller needs `edit_catalog` and membership, or `edit_any_catalog`.

The JavaScript Object Notation (JSON) body names the member by its key, in the
form that `catalog list` prints:

```json
{"member": "gguf::q4_0"}
```

- A model member has the key `format::quantization`.
- A runtime member has its flavor as the key.
- A runtime member without a flavor has the key `-`.

The other members keep their positions, their digests, and their descriptors.
A member that the entry gets later goes to the end. A stored plan holds
descriptors, so a removal does not change a stored plan. After the removal, the
selector for the member does not resolve. A result that only the removed member named
loses that name.

The importer owns the members of stable and daily entries. The application
programming interface (API) removes members only from hand-made entries.
An entry keeps at least one member. To
retire the whole entry, tombstone it.

The response is the updated entry view with its complete `members` list. The
server writes an audit event of the type `catalog_remove_member` with the key
and the digest of the removed member.

## Statuses

| Status | When |
|--------|------|
| 200 | The API removed the member |
| 400 | The body is invalid, or the key does not have the form for the descriptor kind of the entry |
| 403 | The caller lacks catalog edit permission |
| 404 | The entry or specified member does not exist |
| 409 | The entry is tombstoned or is a stable or daily entry, or the member is its last member |
