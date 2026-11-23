# `POST /api/v1/orgs/{org_id}/descriptor-catalog/{name}/family`

Set the family of one model entry in the named organization. The model
pickers of the clients group entries by family. The path takes the
organization identifier (ID) and an unqualified catalog name. The caller needs
`edit_catalog` and membership, or `edit_any_catalog`.

The JavaScript Object Notation (JSON) body gives the new family:

```json
{"family": "LFM"}
```

A family follows the catalog name grammar. Omit `family` to return the current
entry without changing it or recording a family-change audit event.
A `null` family clears the override. The default family is the text of the
entry name before its first hyphen, or the whole name if it has no hyphen.

The server commits the change and its `catalog_set_family` audit event in one
transaction. It returns the updated entry with its complete `members` list.
If the audit write fails, the server rolls back the change and returns an
internal server error.

## Statuses

| Status | When |
|--------|------|
| 200 | The server returned the entry |
| 400 | The body is invalid, the family fails the name grammar, or the entry holds runtimes |
| 403 | The caller lacks catalog edit permission |
| 409 | The entry is tombstoned |
