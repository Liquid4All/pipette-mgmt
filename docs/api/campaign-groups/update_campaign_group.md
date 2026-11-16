# `PATCH /api/v1/campaign-groups/{group_id}`

```json
{ "name": "aa-perf nightly" }
```

Renames the group. `name` is required, trimmed, non-empty and at most 4096
characters. It takes the rights of a campaign rename
([who may do what](../campaign-groups.md#who-may-do-what)). Answers `200` with
the record and `changed`; a rename to the current name writes nothing and
answers `changed: false`, so the write is safe to retry.

## Statuses

| Status | When |
|--------|------|
| 200 | The group, and whether the rename changed it |
| 400 | `group_id` is not a group id; the body is not JSON, names another field, or carries no `name` or an empty or overlong one |
| 403 | The caller may not administer the group ([who may do what](../campaign-groups.md#who-may-do-what)) |
| 404 | No such group, or the server has no `[database]` |
