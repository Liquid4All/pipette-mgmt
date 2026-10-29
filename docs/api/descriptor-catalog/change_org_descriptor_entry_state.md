# `POST /api/v1/orgs/{org_id}/descriptor-catalog/{name}/state`

Change a runtime or model entry's state in the named organization. The path takes the
organization identifier (ID) and an unqualified catalog name. The caller needs
`edit_catalog` and membership, or `edit_any_catalog`.

Send one of these bodies:

- `{"action":"pin"}` or `{"action":"hide"}` requires an active entry.
- `{"action":"unpin"}` requires a pinned entry.
- `{"action":"unhide"}` requires a hidden entry.

Each action checks the current state before the change.

To permanently tombstone the entry, send
`{"action":"tombstone","confirmation":"<stored name>"}`. The confirmation
must match the stored name exactly, including its letter case. You cannot
recreate a tombstoned name in the same organization.

## Statuses

| Status | When |
|--------|------|
| 204 | The state changed |
| 400 | The body is invalid or tombstone confirmation is absent or wrong |
| 403 | The caller lacks catalog edit permission |
| 404 | The organization is unreachable, the entry is absent, or the server has no database catalog |
| 409 | The entry is tombstoned or the current state does not allow the action |
