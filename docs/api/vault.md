# Vault

The **vault** is how a plan names a private artifact: a customer's own
checkpoint, an internal build. A **vault resource** is one uploaded file of a
model or a prebuilt runtime: it says whose it is, what it hashes to, and which
plan variant it was uploaded as. A device fetches the bytes through mgmt and
gets them only if the organization that owns the device owns the resource.

**Files first, then what they are.** A client uploads each file by its digest
([`PUT /api/v1/vault/blobs/{principal_id}/{sha256}`](vault/put_blob.md)), then
names the plan variant the files make up
([`POST /api/v1/vault/resources`](vault/upload_resources.md)), and gets back the
plan entry that points at them. Only a downloadable model or a prebuilt
llama.cpp, OVMS or TensorRT Edge-LLM runtime can be uploaded.

**No answer names where a file is stored.** A client names a file by its
principal and digest and a resource by its id. No storage key appears in an
answer or an error, and a store's own error text never reaches a caller.

**A record is immutable.** It is uploaded and it is deleted, and nothing edits
it in between: no rename, no repoint, no hand-off. A plan carries the id in a
URL, and a record that could be changed underneath would make that URL mean
something else later. Different bytes or a different owner are a new upload
under a new id.

The [console app](../../ui/README.md#pages) draws the same records, and
[`pipette-mgmt vault`](../cli.md) is the operator surface.

**The writes take an active user; the two reads of one record take either
kind.** Uploading an artifact is an operator's decision, and a device can hold
no user permission.
[`GET /vault/resources/{resource_id}`](../httpapi.md#217-get-vaultresourcesresource_id)
and [`…/content`](../httpapi.md#218-get-vaultresourcesresource_idcontent) are
the exception and the reason the surface exists: an approved device holding `perform_work` reaches both, and
so does any active account. A device reads the record first to learn the digest
and the size, and pulls the bytes only if it does not already hold them. That
is the split [`GET /benchmarks/{benchmark_id}`](../httpapi.md#26-get-benchmarksbenchmark_id)
and [`GET /datasets/{benchmark_id}`](../httpapi.md#215-get-datasetsbenchmark_id)
already make for a corpus.

**Ingestion imports a gated model.** A plan document with a gated HuggingFace
model makes the server download the model and write its records, with a
`huggingface` origin ([filing a plan document](campaigns.md#filing-a-plan-document)).
No route of this tag is called. The owner is the campaign's owner. A later
import of the same commit and files by the same owner reuses the records.

## Who may do what

**There are no new permission names.** Only an organization owns a resource.
Its resources are read, uploaded and deleted by its members; setting a model's
parameter counts takes `manage_orgs`.

Uploading a file, asking whether one is uploaded, and naming the variant
uploaded files make up take the same scope: any active account for its
organization's. Reading one resource, or its bytes, takes `perform_work`
(device) or any active account, **and** the owner match. Deleting one takes
membership of the owning organization.

The **ownership rule** is the whole of the model, and it is read twice: once for
operators, once for devices.

- A resource is **read**, **uploaded** and **deleted** by every account on
  its organization, as it did the ones it once owned itself
  ([Campaigns' "Who may do what"](campaigns.md#who-may-do-what) states the
  campaign rule). Uploading runs nothing, and a worker fetches a record when a job
  it claimed names it; the server checks the owner, not the job, so the bytes
  are fetchable by every working device the organization owns.
- `manage_any_orgs` reaches every resource, read and write. **`read_any_orgs`
  does not**: that name is the roster read, documented as who exists and who
  belongs where rather than what anything holds, and private weights are not
  roster data.

Any other device is not granted a job naming the resource:
a claim puts it back in the queue and denies it to the device, and a heartbeat
or reclaim on a lease it already holds is refused with `403` and puts that
lease back the same way
([`POST /plans/claim`](../httpapi.md#29-post-plansclaim)). A success it
reports for such a job is refused with `403`
([`POST /benchmarks`](../httpapi.md#273-errors)); a failure is taken.

**A resource may only ever name its own owner's files.** A file is uploaded for
one principal, and a description finds its files under the owner it names and
nowhere else. Knowing a digest is not holding the file: an account that
describes another organization's digest as its own is told no such file is
uploaded for it, exactly as for a digest nobody holds.

An **unowned device reaches nothing.** Registration is open, so a machine that
registered itself and was never claimed is exactly the identity the vault must
not serve.

A resource outside the caller's scope is a **`403`**, for the reason a campaign
outside one is: the id is a minted UUIDv7 under a `vault_` prefix, so a caller
that can name one was given it. A resource outside the caller's scope is
refused as belonging to another owner.

**Naming a resource in a plan takes reach, not a grant.** A plan may name the
filer's organization's resources; `submit_any_plans` names any,
and `manage_any_orgs` does not widen it. This holds on every door that files a
plan, `POST /api/v1/campaigns` and every revision of
[Campaigns](campaigns.md), dry runs included. Ingestion refuses a resource
outside the reach as one that does not exist, a `400` that names no owner, as
it refuses a private benchmark outside the filer's organization
([what the server checks](campaigns.md#what-the-server-checks)).
