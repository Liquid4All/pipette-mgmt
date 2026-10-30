# `GET /api/v1/vault/resources/{resource_id}`

One resource, as `NamedVaultResourceDetail` in the
[OpenAPI document](../../httpapi.md#15-the-openapi-document): the record, and
for an uploaded file the plan entry it belongs to. A device reads the same
answer at `GET /vault/resources/{resource_id}`
([device route](../../httpapi.md#217-get-vaultresourcesresource_id)). An imported file's entry
carries no `custom_label`: an import
[names each file after its own origin](../../storage.md#34-the-vault).

Any active account reads it, **and** the owner match
([who may do what](../vault.md#who-may-do-what)).

An account's read also carries `may_administer`, whether
[`DELETE`](delete_resource.md) would admit the reader, and for a model
`model_params`, the parameter counts its results take
([model parameters](../model-params.md)). A device's read carries neither. A
row of the [listing](list_resources.md) carries both the same.

An account may ask for two more fields, which a device's read ignores:

| Query parameter | Adds |
|---|---|
| `names=true` | `names`, the owner's address or handle ([names](../../httpapi.md#14-names)) |
| `object=true` | `object`, whether the file is still stored, from a stat of it: `{"state": "present"}`, `{"state": "resized", "bytes": …}` for a file of another size than `bytes` (replaced behind the record), or `{"state": "missing"}`. A store that does not answer is a `500` |

## Statuses

| Status | When |
|--------|------|
| 200 | The resource |
| 403 | The resource is outside the caller's scope: it belongs to another owner |
| 404 | No such resource |
