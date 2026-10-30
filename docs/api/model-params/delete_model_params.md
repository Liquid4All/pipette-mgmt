# `DELETE /api/v1/model-params/{model_key}`

Removes the row. Who may write which row is
[who writes which row](../model-params.md#who-writes-which-row).

## Statuses

| Status | When |
|--------|------|
| 204 | The row is removed, whether or not a row was there |
| 400 | A key outside `[a-z0-9._-]` |
| 403 | The row is outside the caller's reach, whether the key is a name, another owner's file or no file |
