# `GET /api/v1/model-params`

Every row in one page
([pagination](../../httpapi.md#13-pagination)) in `{model_key}.json` byte order
(`lfm2-8b` before `lfm2`), with `unreadable` counting the rows that did not
read.

## Statuses

| Status | When |
|--------|------|
| 200 | The rows |
