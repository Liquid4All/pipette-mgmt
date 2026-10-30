# Model parameters

A **model-params row** is one model's parameter counts,
[edited a row at a time](../storage.md#35-model-params). The rows are the model
parameter table. A scoring pass stores a row's counts on the warehouse rows it
writes, and a filing warns `uncatalogued_model` for a model with no row.

A result row has a `null` `model_params_total_millions` if the scoring pass
ran before its model had a row. A plan's jobs page
(`GET /api/v1/plans/{plan_id}/jobs?results=true`) and results file
(`GET /api/v1/plans/{plan_id}/results.csv`) then take both counts from the row
that its descriptor resolves to. A stored count wins. The `fix-model-param`
command rewrites the warehouse.

## Rows

A row is `{model_key, total_millions, active_millions, note, updated_at, updated_by}`;
`updated_by` is `null` on a seeded or imported row, and `note` on a row without
one. An omitted `active_millions` is the total. `note` says where the counts
come from: it is trimmed, and an omitted or blank one leaves the row without a
note.

## Who writes which row

Reading the rows takes `read_any_plans`; each write takes `manage_orgs` and a
person, never a service account.

A row keyed by a vault resource id holds the counts of one private model, and
is written by the administrators of the organization that owns the file:
`manage_orgs` and membership, the line the vault's delete draws
([who may do what](vault.md#who-may-do-what)). Every other row is server-wide,
since two organizations' plans share one result of a public model, and takes
`manage_any_orgs`, which writes any row. So does the row of a file a person
owns. A write outside that reach is a `403` with one message, whether the key is
a name, another owner's file or no file.

A vault model's record
([`GET /api/v1/vault/resources/{resource_id}`](vault/get_resource.md)) carries
its row and whether the reader may write it, so its page sets the counts.

No new permission names: a row is its organization's administrators' or the
server's, and reading all of them is for an account that reads every plan.
