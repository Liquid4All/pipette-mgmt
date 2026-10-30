# `GET /api/v1/vault/types`

The model and runtime types an upload takes
([`POST /api/v1/vault/resources`](upload_resources.md)), with their files, as
data, for a form that offers them: the same for every caller, so any active
account reads it. Responds `200` with `VaultTypes`
([OpenAPI document](../../httpapi.md#15-the-openapi-document)), the models and
the runtimes in two lists. A runtime's `flavors` are the ones it is known to be
built for; another is typed in.

## Statuses

| Status | When |
|--------|------|
| 200 | The types |
