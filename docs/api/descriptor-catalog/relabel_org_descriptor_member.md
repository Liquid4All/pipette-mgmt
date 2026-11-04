# `POST /api/v1/orgs/{org_id}/descriptor-catalog/{name}/members/relabel`

Change the quantization label of one model member of an entry in the named
organization. The path takes the organization identifier (ID) and an
unqualified catalog name. The caller needs `edit_catalog` and membership, or
`edit_any_catalog`.

The JavaScript Object Notation (JSON) body names the member by its format and
its current label, and gives the new label:

```json
{"format": "gguf", "from": "f16_q4_0", "to": "q4_0"}
```

A label has the characters `[a-z0-9_]`. No other member of the entry with the
same format can hold the new label. The old label is free at once, so a member
that the entry gets later can take it.

The label is not part of the descriptor or its digest. The member keeps its
digest, its position, and its descriptor. A stored plan holds descriptors, so
a label change does not change a stored plan. The selector
`name::format::label` with the old label no longer resolves.

The response is the updated entry view with its complete `members` list.

## Statuses

| Status | When |
|--------|------|
| 200 | The API changed the label |
| 400 | The body is invalid, a label fails the label grammar, or the entry holds runtimes |
| 403 | The caller lacks catalog edit permission |
| 404 | The organization is unreachable. |
| 404 | The entry or matching member does not exist. |
| 404 | The server has no database catalog. |
| 409 | The entry is tombstoned. The new label equals the old label. A member with the same format holds the new label. |
