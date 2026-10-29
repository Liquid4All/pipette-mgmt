# `PUT /api/v1/model-params/{model_key}`

Takes `{total_millions, active_millions?, note?}` and replaces the row whole;
one naming the counts and note the row holds writes nothing and answers
`changed: false`, so `updated_at` and `updated_by` stay those of the edit that
set them.
Who may write which row is
[who writes which row](../model-params.md#who-writes-which-row).

## Statuses

| Status | When |
|--------|------|
| 200 | The row plus `changed` |
| 400 | A non-positive count, an active count above the total, a note over 500 characters, or a key outside `[a-z0-9._-]` |
| 403 | The row is outside the caller's reach, whether the key is a name, another owner's file or no file |
| 404 | The key has no row |
