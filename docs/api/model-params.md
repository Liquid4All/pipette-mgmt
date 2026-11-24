# Model parameters

A **model-params row** is one model's parameter counts,
[edited a row at a time](../storage.md#35-model-params). The rows are the model
parameter table. A scoring pass stores a row's counts on the warehouse rows it
writes, and a filing warns `uncatalogued_model` for a model with no row.

The warehouse keeps the counts a result row was scored with: `null` if the
scoring pass ran before its model had a row, and the old counts if the row
changed since. A plan's jobs page
(`GET /api/v1/plans/{plan_id}/jobs?results=true`) and results file
(`GET /api/v1/plans/{plan_id}/results.csv`) take both counts from the row that
its descriptor resolves to, over any stored counts. A model with no row keeps
its stored counts. A result row without a model fills empty counts from the
model that its job requested. Historical rows without a model keep null counts.
Readers of the warehouse itself see stored counts until the `fix-model-param`
command rewrites them.

The results file remains available if a plan-source read fails. Missing counts
that depend on the requested model remain empty.

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
`manage_orgs` and membership, though any member deletes the file
([who may do what](vault.md#who-may-do-what)). Every other row is server-wide,
since two organizations' plans share one result of a public model, and takes
`manage_any_orgs`, which writes any row. A write outside that reach is a `403` with one message, whether the key is
a name, another owner's file or no file.

A vault model's record
([`GET /api/v1/vault/resources/{resource_id}`](vault/get_resource.md)) carries
its row and whether the reader may write it, so its page sets the counts.

No new permission names: a row is its organization's administrators' or the
server's, and reading all of them is for an account that reads every plan.
