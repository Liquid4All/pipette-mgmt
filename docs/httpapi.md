# HTTP API

Base URL: `http://localhost:3000` (configurable via `listen_addr` in config TOML)

What every route shares (§1), and a device's own routes (§2, with the `405`
that `POST /api/v1/plans` gives a pipette-plan too old to file into a campaign).

The operator API under `/api/v1`, which a person or a script calls, is
described by `GET /api/v1/openapi.json` ([§1.5](#15-the-openapi-document)). Its
prose lives under [api/](api/): one file per operation, which the document
carries as the operation's description and the meaning of each answer, and one
intro per tag:

- [Users](api/users.md): the signed-in account, and account administration
- [Organizations](api/orgs.md): organizations, their members, catalogs and overviews
- [Devices](api/devices.md): the fleet, its administration, and the pre-auth keys that enroll devices
- [Campaigns](api/campaigns.md): campaigns, filing their plans and their revisions
- [Plans](api/plans.md): checking, reading and cancelling plans and their jobs
- [Vault](api/vault.md): private models and runtimes
- [Model parameters](api/model-params.md): the parameter counts scoring reads for each model
- [Descriptor catalog](api/descriptor-catalog.md): readable names for runtime and model descriptors

The console app at `/app/` is a client of these routes, and
[ui/README.md](../ui/README.md) describes its pages.

## 1. Authentication

Most endpoints require authentication. The following are unauthenticated:
`GET /health`, `POST /clients/register`, `POST /api/v1/users/register` (and its Clerk
form, `POST /api/v1/users/register/clerk`, which a Clerk token authenticates), `GET /benchmarks`,
`GET /benchmarks/{benchmark_id}`, their `/api/v2` twins
([§2.16](#216-get-apiv2benchmarks-and-get-apiv2benchmarksbenchmark_id)), the
console app's page and assets under [`/app/`](../ui/README.md), `GET /` and
`GET /app`, which answer `308` to `/app/`, each keeping its query,
`GET /api/v1/openapi.json`, and the API reference at `GET /docs/` with its
stylesheet, where `GET /docs` answers `308` ([§1.5](#15-the-openapi-document)).
The catalog
routes also **accept** a signature, and answer a signed caller with the entries
private to its organization as well ([§2.5](#25-get-benchmarks)). See
[authentication.md](authentication.md) for the identity model, required headers,
and signing details. A user account may sign with RFC 9421 instead of
`X-Signature` ([authentication.md §2.4](authentication.md#24-rfc-9421-signatures-user-accounts));
such a request's body is held to its signed `Content-Digest`, and one that does
not match is a `400`.

Each verified client signature records the client's last-seen time, which the
console shows ([queue.md](queue.md#device-liveness)). The server also
records the time if the route refuses the request.

Two kinds of principal authenticate: a **client** (`ev1_…`, one device) and a
**user** (`user_…`, one operator or service job). Each route admits one kind
(the work routes admit clients, the plan and fleet routes admit users), because
each permission is grantable to one kind
([authentication.md §5.2](authentication.md#52-the-vocabulary)).

A device's routes sit at the root. Every route an operator calls, from the app
or a script that signs its own requests
([Users: Calling the API yourself](api/users.md#calling-the-api-yourself)), sits
under `/api/v1`. The exception is the vault record's two
reads, `GET /vault/resources/{resource_id}` and its `/content`: a plan names
that address as an artifact URL, so it stays at the root and serves both kinds.
`/api/v1/vault/resources/{resource_id}` serves an operator's read beside the
delete ([Vault](api/vault.md)).

## 1.1. Error responses

Every error response carries the same envelope, a single `error` string:

```json
{"error": "contact_email is not a valid email address"}
```

For a `4xx`, that string describes what the caller got wrong and is safe to
surface to a user. For a `5xx` it is a fixed string: `"internal server error"`
(`500`) or `"upstream request failed"` (`502`), carrying no detail about the
failure. Diagnosing a `5xx` requires the server log, which records the error in
full; the per-endpoint error tables below list the conditions each status
covers.

A `412` answers a precondition the request set that does not hold:
`If-None-Match: *` on a write that must create, or `If-Match: *` on one that
must replace. Only `PUT /api/v1/orgs/{org_id}/benchmarks/{benchmark_id}` answers one
([Organizations: Private benchmarks](api/orgs.md#private-benchmarks)); `GET /benchmarks` honours `If-None-Match` with
a `304` instead ([§2.5](#25-get-benchmarks)).

A path no route serves answers `404` with `{"error": "no such route"}`, and a
method its path does not serve answers `405` with
`{"error": "method not allowed"}` and an `Allow` header naming the methods it
does serve.

An `/api/v1` JSON body naming a key its route does not take is a `400`; the
device routes' bodies ignore one instead ([§2.7](#27-post-benchmarks)).

A user route checks the permission it requires before it reads its path or
body, so a caller lacking it is answered `403` even where the path id is
malformed, and learns nothing about whether the record exists
([authentication.md §5.4](authentication.md#54-status-and-permissions)).

The one place an error appears outside this envelope is the per-item `error`
field of a `POST /benchmarks/batch` result (see
[§2.8](#28-post-benchmarksbatch)). It holds the same string, under the same
rules.

## 1.2. Request size limits

A request body is read in full before it is parsed, so each route caps how much
it will read. A body past the cap is rejected with `413` and
`{"error": "request body is too large"}`. The body is never parsed, so a
request that is both oversized and malformed reports only its size. A route
that takes no body never reads one: whatever a request sends there is dropped
unread, and neither its size nor its content is refused.

| Routes | Limit |
|--------|-------|
| `POST /benchmarks`, `POST /benchmarks/batch` | `submission_body_limit_bytes` (default 500 MB) |
| `POST /api/v1/plans`, `POST /api/v1/plans/validate`, `POST /api/v1/campaigns`, `POST /api/v1/campaigns/{campaign_id}/plans`, `POST /api/v1/campaigns/{campaign_id}/variants`, `PUT /api/v1/campaigns/{campaign_id}/document` | `plan_body_limit_bytes` (default 128 MB) |
| `PUT /api/v1/vault/blobs/{principal_id}/{sha256}` | `vault_blob_limit_bytes` (default 128 GiB), streamed rather than read in full ([`PUT /api/v1/vault/blobs/{principal_id}/{sha256}`](api/vault/put_blob.md)) |
| Every other route | `default_body_limit_bytes` (default 64 KB) |

The routes above carry a body that scales with the workload, and each kind
scales on a different thing. A `POST /benchmarks` submission carries one
`completions` entry per eval sample. A `POST /benchmarks/batch` request carries
up to 1000 submissions. A plan request carries the source document
([Campaigns: Request body](api/campaigns.md#request-body)), and
`plan_max_jobs` separately limits its server-side expansion. Each ceiling is a
key in [`config.rs`](../crates/pipette-core/src/config.rs), so an operator can
raise or lower it. Everything else carries a fixed
set of identity and device fields, for which 64 KB is far beyond any legitimate
request.

A submission body is also costed before it is parsed, since its parsed form can
take far more memory than its text: a `0` takes a 32-byte value, and a
one-key object a whole map node of several hundred bytes. One whose parsed form
would take more than `submission_body_limit_bytes` is rejected with `413` and
`{"error": "request body would take more than N bytes once parsed"}`, where N is
the ceiling. Text-heavy bodies, such as eval completions, parse to about their
size; a ceiling under a few kilobytes refuses even a single small submission.

Between them, the two submission routes hold at most four ceilings of bodies
at once: a body reserves its declared length, or the whole ceiling when it
declares none, before it is read, and further requests wait unread. A device
has one body read at a time; its next submission waits for that one to be
parsed. A body that sends nothing for 60 seconds is rejected with `408` and
`{"error": "the request body sent nothing for 60 seconds"}`. Once read, a
body's parsed form takes its share of another four ceilings until its request
is answered, so small submissions are handled side by side while a large one
waits for room. The bodies of pending devices' submissions, which
`[unverified_submissions] enabled = true` holds rather than refuses, share
one of the four ceilings, so slow uploads from pending devices leave approved
devices at least three. Three approved devices that keep uploads open slowly,
with a pending one, can still delay every other device's large submissions,
and one pending device's slow upload can delay every other pending device's.

At worst the two routes hold about eight times `submission_body_limit_bytes`
at once (4 GB at the default), plus each handler's typed copy of its
submission; size the ceiling to the host's memory.

A response is gzipped (`Content-Encoding: gzip`) when the request sends
`Accept-Encoding: gzip`, and sent as is otherwise. An event stream, a body
under 32 bytes, and an image are never compressed. A plan's jobs with their
results run to megabytes of JSON and shrink about twentyfold.

## 1.3. Pagination

All paginated list routes use the same query parameters and response format.
Each route sorts rows by the complete object key in ascending lexicographic
order, unless it takes `order=newest` and the caller asks for it.

**Listings answered whole.** A listing whose section says it answers in one
page uses the same format. Its `next_cursor` is always `null`; it takes no
`limit` or `cursor` and ignores either if sent. The key order above does not
bind it: a route that promises an order says so in its own section. Its
`unreadable` is `0` unless its section says it counts: the pre-auth key
listings fail the request on an invalid object, and the rest skip it without
counting it. The rest of this section is about the paginated routes.

| Query parameter | Required | Meaning |
|-----------------|----------|---------|
| `limit` | no | Maximum number of rows to return. The default is `list_default_limit` (default 50). The value must not exceed `list_max_limit` (default 200). The server returns `400` for `0` or a value above the maximum. |
| `cursor` | no | Position returned in `next_cursor` by the preceding page. Treat this value as opaque. Omit it for the first page. The server returns `400` for a malformed value. |

All responses use the same format. Thus, one parser can process plan and client
lists.

```json
{
  "items": [],
  "next_cursor": "opaque-value",
  "unreadable": 0
}
```

Request pages until `next_cursor` is `null`. Do not use the page length to find
the last page. A full page can be the last page.

Send each non-null cursor unchanged in the next request. Do not create or parse
a cursor. A cursor is an exclusive lower limit for the object key. The cursor
remains valid if its object is deleted between requests.

The cursor identifies the last key that the server examined. This key can
identify an unreadable object or a row that does not match a filter. Thus, the
next page does not examine or count the same unreadable object again. Use the
same filters for all pages of one list operation. Results are not specified if
you change a filter between pages.

`limit` controls the number of returned rows. It does not control the number of
objects that the server reads. An object at a valid key can contain invalid
data. Such an object does not occur in `items` and does not use a row. It does
not cause the request to fail or the page to be short. The server can read more
objects than it returns to fill a page.

`unreadable` is the number of invalid objects in the key range for the page. A
value of `0` applies only to that page. To check the complete keyspace, request
all pages and add their `unreadable` values.

The server applies filters after it reads each object. Therefore, a selective
filter can make the server read many more objects than it returns. The
configured limits control returned rows. They do not control scanned objects.

**Newest first.** `GET /api/v1/campaigns`, `GET /api/v1/vault/resources` and
[`GET /api/v1/plans`](api/plans/list_plans.md) also take `order`: `oldest`, the
default above, or `newest`, which sorts rows by object key in **descending**
order. Ids end in a UUIDv7, so this is creation order, newest first.
`next_cursor` is then an exclusive **upper** limit for the key: it remains
valid if its object is deleted or stops matching a filter, and it is `null`
exactly on the last page. A cursor holds only under the order that issued it.
Every newest-first page reads the whole keyspace, so `unreadable` counts the
keyspace's invalid objects on the first page and is `0` on every later one; the
sum over all pages is still the keyspace's count. Any other value of `order`
gets `400`.

**An empty value reads as no filter.** Every list route reads `status=` or
`cursor=` as absent, so "any status" is "no filter". A wrong value is still a
`400`.

## 1.4. Names

A body names accounts and organizations by id. A route that takes `names=true`
(the campaign reads, [`GET /api/v1/campaigns`](api/campaigns/list_campaigns.md) and
[`GET /api/v1/campaigns/{campaign_id}`](api/campaigns/get_campaign.md), the campaign's plan,
[`GET /api/v1/campaigns/{campaign_id}/plan`](api/campaigns/get_campaign_plan.md), the vault reads,
[§2.17](#217-get-vaultresourcesresource_id) and
[`GET /api/v1/vault/resources`](api/vault/list_resources.md), and the plan reads,
[`GET /api/v1/plans`](api/plans/list_plans.md) and
[`GET /api/v1/plans/{plan_id}`](api/plans/get_plan.md)) adds `names`, an object
from each id to a readable name: an account's address, an organization's
handle. It names only the owners of the records the body carries, never an id
the query names, so it tells the caller nothing the body did not already. An
id whose record is gone, or did not read, is left out, and the caller shows the
id. A campaign's overview ([`GET /api/v1/campaigns/{campaign_id}/overview`](api/campaigns/get_campaign_overview.md))
always carries `names`, on the same rule. The plan page's jobs body
([`GET /api/v1/plans/{plan_id}/jobs`](api/plans/list_plan_jobs.md#the-plan-pages-body)) carries
`names` with `results=true`: at the full tier, each device it names by its
device name, and each vault resource a plan cell names by the `name` of its
record ([`POST /api/v1/vault/resources`](api/vault/upload_resources.md)). An uploaded source carries that
name itself as `custom_label`; `names` covers a source without one, whose
descriptor shows an id and not what it holds. There the name says more than the
body does, so it is given only to a reader the vault's own reach admits
([Vault: Who may do what](api/vault.md#who-may-do-what)). The audit listings
([Organizations: The audit log](api/orgs.md#the-audit-log)) name the ids in their
events by the rule in that section. Otherwise a body has no `names` without
`names=true`.

## 1.5. The OpenAPI document

`GET /api/v1/openapi.json` answers the operator API, every route under
`/api/v1`, as an OpenAPI 3.1 document: each operation's parameters, request
body, answers and error statuses, the JSON Schema of every body (of a TOML
body's parsed data, so `SchedulerPlan` is a plan document's every field), and the
permissions its gate requires, as the roles of its security requirement. A
route that refuses a service account says so, and carries `x-human-only`. A
device's routes (§2) are not in it. The route is open, since the document
describes routes and no data, and it answers with
`Access-Control-Allow-Origin: *`, so a viewer on another origin can load it. Its
`ETag` lets a viewer revalidate with `If-None-Match` for a `304` while the server
runs the same build.

`GET /docs/` serves the document as the API reference: one static page and its
stylesheet, open, rendered at the first request. The app at `/app/` keeps its
signing keys on this origin, where any script can sign with them, so the page
runs no script and is served with `Content-Security-Policy: sandbox`, which
makes it an origin of its own. `pipette-mgmt openapi --site` writes the same
files for another host ([CLI](cli.md#pipette-mgmt-openapi---site-dir)).

## 1.6. Write answers

A write that edits one record answers the record as stored after it, plus
`changed`. `changed: false` is a success that wrote nothing, since the record
already held what was asked, so a retry is safe. Each route's section says what
its no-op is.

## 2. Endpoints

### 2.1. `GET /health`

Health check. **Unauthenticated.**

**Response** `200 OK`

```json
{"status": "ok"}
```

### 2.2. `POST /clients/register`

Register a new client. **Unauthenticated.**

#### 2.2.1. Request body

| Field | Type | Required | Description | Example | Suggested source |
|-------|------|----------|-------------|---------|-----------------|
| `public_key` | string | no* | Hex-encoded Ed25519 public key | `"a3f8b2c1..."` | `ed25519` key generation library |
| `generate_key` | bool | no* | If `true`, the server generates a keypair | `true` | — |
| `organization` | string | yes | Organization operating this client | `"LiquidAI"` | Operator-configured |
| `client_details` | string | yes | Freeform description of the client | `"Lab Jetson Orin"` | Operator-configured |
| `contact_email` | string | yes | Contact email for admin approval. An ordinary `local@domain.tld` address: no quoted local part (`"…"@example.com`), no display name (`Ops <ops@example.com>`), no domain literal (`user@[192.0.2.1]`), domain must have a TLD, 254 octets max (a non-ASCII address therefore allows fewer characters) | `"lab@example.com"` | Operator-configured |
| `client_preauth_key` | string | no | Pre-auth key (`client_preauth_{key_id}.{secret}`, or the earlier `preauth_` spelling) from `clients preauth create`, or from an organization's administrator ([Device administration: Client pre-auth keys](api/devices.md#client-pre-auth-keys)). Also accepted under the pre-rename field name `preauth_key`; both in one body is a `400`. Valid key → auto-approved (+ seeded tags, org label and owner); invalid, or naming an owner deleted since → `401`, no client created. See [authentication.md §3.2](authentication.md#32-pre-auth-keys) | `"client_preauth_a1b2….c3d4…"` | `pipette-mgmt clients preauth create` |
| `device_name` | string | no | Device model / marketing name | `"MacBook Pro 14-inch (2023)"` | macOS: `sysctl hw.model`; Linux: `/sys/devices/virtual/dmi/id/product_name`; Windows: WMI `Win32_ComputerSystem.Model`; embedded: `/proc/device-tree/model` |
| `device_form_factor` | string | no | One of `phone`, `tablet`, `laptop`, `desktop`, `server`, `embedded` | `"laptop"` | Operator-configured (hardcoded per deployment) |
| `device_os_name` | string | no | OS name (a Linux distribution when available) | `"macOS"` | macOS: `sw_vers -productName`; Linux: `NAME` in `/etc/os-release`; Windows: `platform.system()` |
| `device_os_version` | string | no | OS version string | `"15.3"` | macOS: `sw_vers -productVersion`; Linux: `VERSION_ID` in `/etc/os-release`; Windows: `platform.version()` |
| `device_cpu_arch` | string | no | CPU architecture. The server maps known aliases to the `arch:` flag value | `"x86_64"` | `std::env::consts::ARCH` |
| `device_os_family` | string | no | OS family. The server maps known aliases to the `os_family:` flag value | `"linux"` | `std::env::consts::OS` |
| `device_chip_model` | string | no | Chip / SoC model | `"Apple M3 Pro"` | macOS (Apple Silicon): `sysctl hw.chip_model`; macOS (Intel): `sysctl machdep.cpu.brand_string`; Linux: `model name` in `/proc/cpuinfo`; Windows: WMI `Win32_Processor.Name` |
| `device_ram_bytes` | int | no | System RAM in bytes | `36000000000` | macOS: `sysctl hw.memsize`; Linux: `MemTotal` in `/proc/meminfo`; Windows: WMI `Win32_ComputerSystem.TotalPhysicalMemory` |
| `device_gpu_model` | string | no | GPU model; omit or `null` if none | `"NVIDIA RTX 4090"` | NVIDIA: `nvidia-smi --query-gpu=name`; macOS: Metal device API; Linux: `lspci`; AMD on Linux: `product_name` under `/sys/class/drm/card<N>/device` |
| `device_gpu_vram_bytes` | int | no | GPU VRAM in bytes | `24000000000` | NVIDIA: `nvidia-smi --query-gpu=memory.total`; macOS: Metal device API; AMD on Linux: `mem_info_vram_total` under `/sys/class/drm/card<N>/device` |
| `device_npu_model` | string | no | NPU / neural accelerator model; omit or `null` if none | `"Apple Neural Engine"` | Usually hardcoded from chip (Apple Silicon → `"Apple Neural Engine"`; Qualcomm → `"Hexagon DSP"`); vendor SDK otherwise |
| `device_npu_vram_bytes` | int | no | NPU VRAM in bytes; `null` when not separately addressable (e.g. unified memory) | `null` | Vendor SDK; `null` for most unified-memory architectures |
| `capabilities` | string[] | no | Free-form capability flags the client reports directly, e.g. installed runtimes (`"runtime:llama_cpp"`). Must not use a server-owned reserved namespace (see below) | `["runtime:llama_cpp"]` | Client runtime inventory |

\* Exactly one of `public_key` or `generate_key: true` must be provided.

The server rejects a body that carries `permissions` with `400`. An operator
assigns permissions. A pre-auth key template is the one path by which a
registration sets a non-default set (see
[authentication.md §5.6](authentication.md#56-assignment)).

Capability flags participate in job matching (see
[plan-ingestion.md §5](plan-ingestion.md#5-capability-matching)): the server unions them with the
flags it derives from the `device_*` profile to form the client's effective
capability set. Each flag must be in **canonical form**: lowercase with no
whitespace (`400` otherwise), since matching is exact and the device-derived
flags are canonical. The server derives flags from the `device_*` profile in
these **reserved** namespaces:

- `os:`, `os_version:`, `arch:`, `os_family:`
- `device:`, `chip:`, `form_factor:`, `ram_bytes:`
- `gpu:`, `gpu_vram_bytes:`, `npu:`, `npu_vram_bytes:`

The server owns these namespaces, so a client may not report them in
`capabilities` (doing so is a `400`). Nor may it report a
`tag:` flag: an operator assigns tags. Report installed runtimes as
`runtime:<name>` (optionally versioned, e.g. `runtime:llama_cpp:b9999`);
any other canonical string is a free-form flag. For guidance on *which* flags a
client should report, and how this set relates to the one sent to
[`PATCH /clients/me`](#24-patch-clientsme). See
[client-integration.md §3](client-integration.md#3-choosing-a-capability-set).

#### 2.2.2. Response `201 Created`

```json
{
  "client_id": "ev1_a3f8...",
  "status": "pending",
  "private_key": "hex-encoded-ed25519-private-key"
}
```

`private_key` is present only for `generate_key: true`. That form is the only response in the API that returns a
long-lived credential, and the server holds no TLS itself, so this endpoint
depends on the deployment terminating TLS in front of it
([operations.md §5.6](operations.md#56-network-exposure-and-tls)). Generating
the keypair client-side and sending `public_key` keeps the private key off the
wire entirely, which is what the supported clients do.

`status` is normally `"pending"`. It is `"approved"` when the request carries a
valid `client_preauth_key` ([§3.2](authentication.md#32-pre-auth-keys)) or its
`contact_email` matches an `[auto_approve]` rule
([§3.1](authentication.md#31-auto-approve-rules)).

Registration is **idempotent on the public key**. A repeat request for an
already-registered public key returns `200 OK` with the existing `client_id` and
`status`: it creates nothing and consumes no `client_preauth_key`. This lets a
client that registered but failed to persist its identity locally retry safely
with the same keypair (and, if it used one, the now-spent
`client_preauth_key`) and recover its `client_id` without a fresh key.

#### 2.2.3. Errors

| Status | Condition |
|--------|-----------|
| 400 | Missing required fields or both/neither `public_key` and `generate_key` |
| 400 | Invalid `device_form_factor` (must be one of: `phone`, `tablet`, `laptop`, `desktop`, `server`, `embedded`) |
| 400 | `device_gpu_vram_bytes` present without `device_gpu_model` |
| 400 | `device_npu_vram_bytes` present without `device_npu_model` |
| 400 | `device_os_version` present without `device_os_name` |
| 400 | A `capabilities` flag is empty, not canonical (lowercase + no whitespace), or uses a reserved namespace |
| 400 | The body carries `permissions` (message: `permissions are assigned by an operator`) |
| 401 | `client_preauth_key` is malformed, unknown, expired or spent, carries the wrong secret, or names an owner that no longer exists |
| 403 | `require_client_preauth_key = true` and the body carries no `client_preauth_key` |
| 403 | The key was minted over HTTP, and its minter could not mint it now ([Device administration: Client pre-auth keys](api/devices.md#client-pre-auth-keys)) |
| 409 | The keypair is already registered to another principal |
| 429 | No `client_preauth_key`, and this process has admitted 20 registrations without a pre-auth key, devices and accounts together, or 1 from this client's address, in the current minute; `Retry-After` gives the seconds to wait ([operations.md §5.6](operations.md#56-network-exposure-and-tls)) |


### 2.3. `GET /clients/me`

Get the authenticated client's profile.

#### 2.3.1. Response `200 OK`

```json
{
  "client_id": "ev1_a3f8...",
  "organization": "LiquidAI",
  "client_details": "Lab Jetson Orin",
  "contact_email": "lab@example.com",
  "status": "approved",
  "tags": ["team-mobile", "us-east"],
  "permissions": ["perform_work", "submit_results"],
  "reindex_pending": false,
  "capabilities": ["runtime:llama_cpp"],
  "device_name": "MacBook Pro 14-inch (2023)",
  "device_form_factor": "laptop",
  "device_os_name": "macOS",
  "device_os_version": "15.3",
  "device_cpu_arch": "arm64",
  "device_os_family": "macOS",
  "device_chip_model": "Apple M3 Pro",
  "device_ram_bytes": 36000000000,
  "device_gpu_model": null,
  "device_gpu_vram_bytes": null,
  "device_npu_model": null,
  "device_npu_vram_bytes": null
}
```

Device profile fields are `null` when the client has not yet registered a device
profile via `PATCH /clients/me`. `capabilities` is the set the client reported
directly (empty array when none); the server-derived `device_*` flags are **not**
echoed here. They are visible through the `device_*` fields themselves.

`tags` is read-only and always present (`[]` when the client is untagged). Tags
are assigned by an operator on the mgmt side (`pipette-mgmt clients tag …`); a
client cannot set them via `POST /clients/register` or `PATCH /clients/me`. See
[authentication.md §7](authentication.md#7-client-tags).

`permissions` is read-only and always present (`[]` when the client holds none).
It is the stored set, sorted: the operations the client may call, plus any
inert name described below. An operator assigns them
(`pipette-mgmt clients permission …`, or
[Device administration: Device permissions](api/devices.md#device-permissions)). A body that carries the field on either
client route gets a `400`. A stored name a client cannot hold, such as a plan
or fleet right, is rendered under its stored spelling and grants nothing, so an
operator can see it and remove it. Nothing in the client vocabulary implies
anything else, so no expansion happens here. See
[authentication.md §5.2](authentication.md#52-the-vocabulary).

`reindex_pending` is `true` while the client's eligible-index re-evaluation is
pending (after a device-profile change or a fresh registration, until the next
`queue-maintenance` run). While `true`, the client has no standing in the
queue (see [§2.4](#24-patch-clientsme)).

#### 2.3.2. Errors

| Status | Condition |
|--------|-----------|
| 401 | Missing or invalid auth headers |

### 2.4. `PATCH /clients/me`

Update the authenticated client's profile. `client_details` and the `device_*`
profile fields are mutable; `organization` and `contact_email` are set at
registration and cannot be changed.

Device profile fields may be sent at registration or here. The plan
assignment system matches jobs against the profile (see
[plan-ingestion.md §5](plan-ingestion.md#5-capability-matching)); a client without one can
still be listed in a job's `clients` roster.

The eligible index is not updated inline. The handler writes the profile and
sets a reindex flag that `queue-maintenance` processes on its next run, so the
new profile reaches job offers within one cron interval
([operations.md §3.2](operations.md#32-eligible-index-maintenance)).

A device-profile change **voids the client's standing in the queue**: jobs are
matched against the profile, so the server relinquishes every lease the client
holds (each job returns to the queue) and refuses all plan operations until the
reindex completes. `claim` returns `204`, and `heartbeat`, `reclaim`, and
plan-attached submissions return `404` (see §2.7.3, §2.10, §2.11).

Only the server can tell whether a PATCH actually *changed* the profile. The
response's `reindex_pending` field ([§2.3.1](#231-response-200-ok)) carries the
verdict: `true` means the client's standing was voided and work claimed under
the old profile is forfeited; `false` means nothing was voided. What a client
does with each is [client-integration.md §2](client-integration.md#2-startup).

#### 2.4.1. Request body

All fields are optional; only fields that are present with a value are updated. A
field that is absent **or** `null` leaves the stored value unchanged. Device
fields cannot be individually cleared once set (the profile is fixed device
hardware metadata). To reset a profile, re-register the client.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `client_details` | string | no | Updated client description |
| `device_name` | string | no | Device model / marketing name |
| `device_form_factor` | string | no | One of `phone`, `tablet`, `laptop`, `desktop`, `server`, `embedded` |
| `device_os_name` | string | no | OS name (e.g. `"macOS"`, `"Ubuntu"`) |
| `device_os_version` | string | no | OS version string |
| `device_cpu_arch` | string | no | CPU architecture. The server maps known aliases to the `arch:` flag value |
| `device_os_family` | string | no | OS family. The server maps known aliases to the `os_family:` flag value |
| `device_chip_model` | string | no | Chip / SoC model (e.g. `"Apple M3 Pro"`) |
| `device_ram_bytes` | int | no | System RAM in bytes |
| `device_gpu_model` | string | no | GPU model |
| `device_gpu_vram_bytes` | int | no | GPU VRAM in bytes |
| `device_npu_model` | string | no | NPU model |
| `device_npu_vram_bytes` | int | no | NPU VRAM in bytes |
| `capabilities` | string[] | no | Replacement capability set (see note) |

`permissions` is read-only. The server rejects a body that carries it with
`400`, even when its value is `null`. An explicit `null` still carries the key,
so it is still a request to set the field. The server refuses the field rather
than drop it. A silent drop would leave the client with the false belief that
the change took effect. Any other key outside the table, `organization`,
`contact_email`, `status` and `tags` included, is ignored: the `200` body
echoes the stored values, so the client can see that nothing changed.

Unlike the per-field `device_*` merge, `capabilities` is **set-granular**: when
present it *replaces* the stored set wholesale (report the full current set),
and when absent (or `null`) the stored set is left unchanged. Note that `[]` is a
*present* value and therefore **clears** the stored set; "leave unchanged" is an
absent key or `null`. Reserved-namespace and empty flags are rejected exactly as
at registration ([§2.2.1](#221-request-body)). A capabilities change voids the
client's queue standing the same way a device-profile change does
(`reindex_pending`). See
[client-integration.md §3](client-integration.md#3-choosing-a-capability-set) for
how a client should choose and maintain the set across registration and PATCH.

#### 2.4.2. Response `200 OK`

Returns the updated client profile (same shape as `GET /clients/me`), with the
read-only `permissions` array. `reindex_pending: true` means this PATCH (or an
earlier, still-unsettled change) voided the client's queue standing.

#### 2.4.3. Errors

| Status | Condition |
|--------|-----------|
| 400 | Invalid `device_form_factor` (must be one of: `phone`, `tablet`, `laptop`, `desktop`, `server`, `embedded`) |
| 400 | `device_gpu_vram_bytes` present without `device_gpu_model` |
| 400 | `device_npu_vram_bytes` present without `device_npu_model` |
| 400 | `device_os_version` present without `device_os_name` |
| 400 | A `capabilities` flag is empty, not canonical (lowercase + no whitespace), or uses a reserved namespace |
| 400 | The body carries `permissions` (message: `permissions are assigned by an operator`) |
| 401 | Missing or invalid auth headers |
| 409 | The record changed (an operator edit, an approval, or another PATCH from this client) or was deleted while the request was in flight. The record was not updated, but a profile or capability change may already have released the client's leases. Resend: a deleted client then gets `401` |

### 2.5. `GET /benchmarks`

List the benchmark catalog. **Unauthenticated, and optionally signed.** Each
entry includes its `benchmark_id`, `benchmark_type`, and `parameter_`-prefixed
fields. Eval-type benchmarks omit `samples`. See [benchmarks.md](benchmarks.md)
for per-type parameter details, and
[§2.16](#216-get-apiv2benchmarks-and-get-apiv2benchmarksbenchmark_id) for the same
catalog as the pair a submission declares.

An unsigned request is served the **public** entries. A request signed by a
client or a user is served those plus the entries **the caller's organization
keeps** ([benchmarks.md §1.1](benchmarks.md#11-private-benchmarks)), read from
the organization's own keyspace on this request: a client belongs through its
owner, a user through its membership
([storage.md §3.2](storage.md#32-organizations)). A private entry carries an
`org_id` field naming that organization; a public one carries none. An id both
hold is served as the public entry. The signature
is checked the way every authenticated route checks it, so a request that
carries `X-Client-Id` and fails to verify is a `401`, not the public view.
Standing decides what is added, not whether the route answers: a pending client
or a pending user is served the public catalog, an approved client or an active
user its organization's private entries as well. A disabled user is refused
with `403` as it is everywhere.

The response carries an `ETag`, the quoted SHA-256 of its body. A request whose
`If-None-Match` names that tag is answered `304 Not Modified` with no body.

#### 2.5.1. Query parameters

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `type` | string | no | Filter by `benchmark_type` (e.g. `prefill_throughput`). The kebab-case spelling (`prefill-throughput`) is accepted too. Absent or empty means every type. An unknown value is a `400` that lists the known types |

#### 2.5.2. Response `200 OK`

| Field | Type | Description |
|-------|------|-------------|
| `benchmark_id` | string | Unique benchmark identifier |
| `benchmark_type` | string | Type of one of the configured benchmarks (see [`GET /benchmarks`](#25-get-benchmarks) for the live catalog) |
| `org_id` | string | The id of the organization the entry is private to; **absent** on a public entry |
| `parameter_*` | varies | Benchmark parameters (type-specific, see [benchmarks.md](benchmarks.md)) |

#### 2.5.3. Errors

| Status | Condition |
|--------|-----------|
| 400 | `type` names no benchmark type |
| 401 | `X-Client-Id` present and the signature does not verify |
| 403 | The signer is a disabled user |

### 2.6. `GET /benchmarks/{benchmark_id}`

Get benchmark details. **Unauthenticated, and optionally signed**, under the
rule [§2.5](#25-get-benchmarks) states: the public catalog is consulted first,
then, for a signed caller that belongs to an organization, that organization's
own keyspace. A private entry is served to its organization's principals and
reads as `404` to everyone else, the same `404` an unknown id draws, so the id
discloses nothing. For `eval`
benchmarks, samples are fetched from the evals server and included in the
response. Parameters come from the local catalog. The `ETag` and `304` rule of
[§2.5](#25-get-benchmarks) applies here too.

For `vl_spec_decode`, `vlm_eval` and `asr_eval` benchmarks the response carries
a `dataset` object instead: the catalog's pin plus the `url` the corpus
downloads from ([§2.15](#215-get-datasetsbenchmark_id)). It is read from the
local catalog like any parameter (see
[benchmarks.md §2.8.1](benchmarks.md#281-the-dataset-object),
[§2.9](benchmarks.md#the-samples-a-device-runs-the-corpus) and
[§2.12](benchmarks.md#the-samples-a-device-runs-the-asr-eval-corpus)). A
`vlm_eval`'s and an `asr_eval`'s samples are not inline: each image would cross
the wire once per question that shows it, and an ASR set is gigabytes of audio.
Both also carry the entry's `recipe`.

#### 2.6.1. Path parameters

| Parameter | Type | Description |
|-----------|------|-------------|
| `benchmark_id` | string | Benchmark identifier (e.g. `prefill_throughput_256`) |

#### 2.6.2. Response `200 OK`

Same fields as section 2.5, plus for `prefill_throughput`,
`decode_throughput` and `end_to_end_latency` benchmarks:

| Field | Type | Description |
|-------|------|-------------|
| `parameter_batch_size` | int | Requests kept in flight at once; `1` unless the catalog says otherwise |

for eval benchmarks:

| Field | Type | Description |
|-------|------|-------------|
| `parameter_max_tokens` | int | Maximum generation length |
| `parameter_max_context` | int | Tokens a sample's prompt and output may reach together; omitted when the entry sets no such bound (see [benchmarks.md](benchmarks.md#the-context-bound)) |
| `parameter_mcq_choices` | string[] | Valid answer choices (MCQ evals only); omitted when the entry sets none |
| `parameter_temperature` | float | Sampling temperature the entry pins; omitted when it pins none |
| `parameter_top_p` | float | Nucleus sampling mass; omitted when the entry pins none |
| `parameter_top_k` | int | Top-k cut (`0` disables it); omitted when the entry pins none |
| `parameter_min_p` | float | Minimum-probability cutoff; omitted when the entry pins none |
| `parameter_seed` | int | Base RNG seed; omitted when the entry pins none |
| `parameter_seed_rule` | string | `vlm_eval` and `asr_eval` only, beside `parameter_seed`: how each sample's seed derives from it (`per_sample_sha256`); omitted when the entry names none |
| `samples` | array | `eval` only. Samples from the upstream evals server; only the `samples` field is proxied into this response (see [benchmarks.md](benchmarks.md#25-eval-accuracy)) |

Each sample is `{id, messages}`, and each message exactly `{role, content}`:
`role` is `system`, `user` or `assistant`, and `content` a string or an array
of `{"type": "text", "text"}` and `{"type": "image_url", "image_url": {"url"}}`
parts (the vision messages a `vlm_eval`'s corpus rebuilds). An upstream sample with any other role
or part type answers `502`. A `vlm_eval`'s `parameter_lmms_task` is one of
`mme`, `mmstar`, `ocrbench`, `mmmu_val`, `realworldqa`, `mmbench_en_dev`,
`pope`, or a newer task kept as written. A `vlm_eval` that sets
`parameter_prompt_profile` is served its samples in that model family's
wording ([benchmarks.md](benchmarks.md#29-vlm-eval-accuracy)), and the field
is omitted when it names none.

An `asr_eval` carries **no `samples`**, and its entry is served as it is, with
no upstream call, so a fetch and its `304` revalidation read no audio: its
clips are its corpus, which a working client downloads from
[§2.15](#215-get-datasetsbenchmark_id). Their count is the entry's own
`parameter_sample_count`. Every `asr_eval` entry is private to an
organization (its datasets may not be redistributed), so only that
organization's principals read it. Its sampling fields are an eval's, plus
`parameter_prompt_profile` (always present), `parameter_num_beams`,
`parameter_beam_repetition_penalty` and `parameter_beam_length_penalty`
([benchmarks.md](benchmarks.md#212-asr-eval-error-rate)).

Each sampling field is **omitted, not null**, when the entry pins nothing, and
the client applies its own default for that knob; an `asr_eval`'s is its
prompt profile's card value, which pipette-plan-types resolves
([benchmarks.md](benchmarks.md#212-asr-eval-error-rate)). See
[benchmarks.md §2.5](benchmarks.md#25-eval-accuracy) for the fallbacks and for
how `parameter_seed` derives a per-attempt seed: an entry's sample `<id>#k`
draws with `parameter_seed + k` modulo `2^32`, and `<id>` alone with
`parameter_seed`. A `vlm_eval` or `asr_eval` entry with
`parameter_seed_rule = "per_sample_sha256"` sends each sample instead the
first four bytes, read big-endian, of the SHA-256 of the UTF-8
bytes of `<parameter_seed>|<sample id>`, the seed in decimal and the id as
served, suffix included: seed `0` and `élément-3#1` give `1252465368`, seed
`7` and `图像-2` give `225674150`
([benchmarks.md](benchmarks.md#29-vlm-eval-accuracy)).

and for `vl_spec_decode` benchmarks:

| Field | Type | Description |
|-------|------|-------------|
| `parameter_draft_length` | int | Tokens drafted per verification step |
| `parameter_max_tokens` | int | Generation cap per turn |
| `parameter_batch_size` | int | Samples kept in flight at once; `1` unless the catalog says otherwise |
| `parameter_temperature` | float | Sampling temperature for every turn; `0.0` (greedy) unless the catalog says otherwise |
| `dataset` | object | The catalog's pin (`format`, `adapter`, `digest`, `bytes`, `upstream`, and `subset` when the corpus keeps one upstream subset) plus `url`, the path of [`GET /datasets/{benchmark_id}`](#215-get-datasetsbenchmark_id) |

Throughput benchmark:

```json
{
  "benchmark_id": "prefill_throughput_256",
  "benchmark_type": "prefill_throughput",
  "parameter_prefill_tokens": 256
}
```

A speculative-decoding benchmark carries its `dataset` as
[benchmarks.md §2.8.1](benchmarks.md#281-the-dataset-object) shows it, plus
`url`.

Eval benchmark:

```json
{
  "benchmark_id": "eval_gpqa_diamond_2026.06.1_t1",
  "benchmark_type": "eval",
  "parameter_eval_id": "gpqa_diamond",
  "parameter_dataset_name": "2026.06.1",
  "parameter_max_tokens": 8192,
  "parameter_temperature": 1.0,
  "samples": [
    {
      "id": "a1b2c3d4e5f6",
      "messages": [{"role": "user", "content": "..."}]
    }
  ]
}
```

#### 2.6.3. Errors

| Status | Condition |
|--------|-----------|
| 400 | `benchmark_id` in the path does not start with a letter or digit, or contains characters outside `[A-Za-z0-9_.-]` |
| 401 | `X-Client-Id` present and the signature does not verify |
| 403 | The signer is a disabled user |
| 404 | Benchmark not found, or private to an organization the caller is not in |
| 502 | Upstream unreachable (eval benchmarks only) |

### 2.7. `POST /benchmarks`

Submit a benchmark result. The single submission endpoint for both
ad-hoc runs and plan-attached runs. Requires an approved client holding the
permission the body's own operation requires. Returns the assigned `job_id`.
See [benchmarks.md](benchmarks.md) for per-type payload fields.

Valid auth headers are always required. A *pending* client is rejected with
`403`, unless the server sets `[unverified_submissions] enabled = true`, which
**holds** its submissions instead (§2.7.4).

**Two operations, one route.** A body carrying a `job_id` reports a job the
client claimed and requires `perform_work`. A body without one is an ad-hoc
result for the caller's own device and requires `submit_results`. The
permission a submission needs therefore follows its body, and an approved
client that lacks it is `403` with
`client lacks the {permission} permission`. Nothing is written, and the claim
this body echoes is not renewed
([authentication.md §5.2](authentication.md#52-the-vocabulary)). A *held*
submission consults no permission: the hold is for an identity nobody has vetted
yet, and a grant is what an operator makes after vetting one.

**Which benchmarks a body may name.** The public catalog's, and those the
client's organization keeps to itself
([benchmarks.md §1.1](benchmarks.md#11-private-benchmarks)): the rule
[§2.6](#26-get-benchmarksbenchmark_id) serves a definition by, so an approved
client its organization holds, directly or through a member owner. Any other
private id is `404 benchmark not found`, the answer an unknown id gets. The
exception is a `"failure"` reporting the client's own live claim on a job that
runs the id, accepted from any client. A device outside the organization
claims such a job, is refused the definition, and hands the job back with a
retriable failure, which denies it the job and returns it to the queue.

The body is a tagged union discriminated by `message_type`:

- `"success"` *(default)*: the benchmark completed. Body carries the
  device / model / runtime context plus the type-specific result
  fields.
- `"failure"`: the benchmark could not be executed. Body records only
  the model / runtime configuration and a human-readable
  `failure_reason`. No device or result fields are required.

`message_type` may be omitted on a success body. When `job_id` is set the submission is tied to the
plan task obtained via [`POST /plans/claim`](#29-post-plansclaim); the
descriptors are serialized from that claim's `spec.model` / `spec.runtime`.

Unknown fields in a submission body are ignored, never rejected. This is
a compatibility guarantee: a retired field an older client may still send
(e.g. the legacy `plan_id`) must not turn its submission into a `400`, so
the submission parsers deliberately do not reject unknown keys.

The first exception is a body that mixes the two variants: a `"failure"`
carrying device or measurement fields other than `device_time_ms`, or a
`"success"` carrying `failure_reason` / `retriable`. Those keys are not *unknown*; they are the
other variant's, and their presence means the client built the wrong shape:
a measurement it never took, or a reason for a run that succeeded. Ignoring
them would hide the bug, so they are a `400` naming the offending fields
([§2.7.3](#273-errors)). Configuration a failure legitimately knows
(`model_params_*`, `benchmark_flags`, `client_configuration`,
`runtime_cpu_variant`) is **not** covered, and stays accepted and ignored.

The second is `benchmark_type`, read with `benchmark_params`: a malformed or
disagreeing pair is a `400` ([§2.7.1.1](#2711-request-body)).

The server stamps `client_id` and `submitted_at` itself, and the
`requested_*_flags` from the lease it issued. A body's own values for those are
ignored.

#### 2.7.1. Success variant

Used when `message_type` is `"success"` or absent. Carries the
device / model / runtime context plus the type-specific result fields.

##### 2.7.1.1. Request body

Per-type result fields (`prefill_time_ms`, `decode_time_ms`,
`completions[]`, etc.) are listed in [benchmarks.md](benchmarks.md).
Every reported number is held to a range, and one outside it is a `400`: a
time a metric divides by or a whole measured span (`prefill_time_ms`,
`prompt_ms`, `total_time_ms`, …) is positive and finite, a standard deviation
or byte count is not negative, and a token or turn count is a positive `i32`.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `message_type` | string | no | `"success"` (default). May be omitted on success bodies |
| `job_id` | string or null | no | Job identifier assigned by the planner; included when this submission is tied to a [`POST /plans/claim`](#29-post-plansclaim). Omit for ad-hoc submissions |
| `benchmark_id` | string | yes | Benchmark identifier (e.g. `"prefill_throughput_256"`) |
| `benchmark_type` | string or null | no | The type the client ran (e.g. `"prefill_throughput"`). Sent together with `benchmark_params`, or neither. The catalog's entry for `benchmark_id` stays authoritative, so the pair is a check: one that disagrees is a `400`. A success for a `vlm_eval` or `asr_eval` entry that names a `parameter_seed_rule` must send the pair: without it the result is a `400` |
| `benchmark_params` | object or null | no | The parameters the client ran with: the catalog entry's `parameter_*` fields without the prefix, as [`GET /api/v2/benchmarks`](#216-get-apiv2benchmarks-and-get-apiv2benchmarksbenchmark_id) serves them (e.g. `{"prefill_tokens": 256, "batch_size": 1}`). Requires `benchmark_type`, which supplies its tag. Every field the entry pins must match, an eval's `temperature` included; a sampling field the entry leaves unpinned is the client's to state. An eval's `max_context` is no sampling field: it must be the entry's, so a run declaring one the entry does not set disagrees. A field left out reads as the catalog's, so a client that predates one, such as a sampling setting or an image count, still submits; the server logs what it filled. `image_tokens`, `decode_tokens` and `seed_rule` are never filled: each changes what the client runs, so leaving one out against an entry that sets it disagrees |
| `device_name` | string | yes | Device model / marketing name |
| `device_form_factor` | string | yes | One of `phone`, `tablet`, `laptop`, `desktop`, `server`, `embedded` |
| `device_os_name` | string | yes | OS name (e.g. `"Ubuntu"`) |
| `device_os_version` | string | yes | OS version (e.g. `"Ubuntu 22.04"`) |
| `device_os_build` | string or null | no | Precise OS build, finer than `device_os_version` (iOS `22F76`, macOS `24F74`, Windows `26100.1234`, Android `AP3A.240905.015.A2`, Linux `uname -r`). Null when unavailable |
| `device_os_security_patch` | string or null | no | OS security-patch level where the platform exposes one (Android, e.g. `2025-06-01`). Null elsewhere |
| `device_cpu_arch` | string or null | no | CPU architecture as the device reports it. The server maps known aliases to the `arch:` flag value |
| `device_os_family` | string or null | no | OS family as the device reports it. The server maps known aliases to the `os_family:` flag value |
| `device_chip_model` | string | yes | Chip / SoC model |
| `device_ram_bytes` | int | yes | System RAM in bytes; not negative |
| `device_gpu_model` | string or null | no | GPU model; required if `device_gpu_vram_bytes` is set |
| `device_gpu_vram_bytes` | int or null | no | GPU VRAM in bytes; requires `device_gpu_model` |
| `device_npu_model` | string or null | no | NPU model; required if `device_npu_vram_bytes` is set |
| `device_npu_vram_bytes` | int or null | no | NPU VRAM in bytes; requires `device_npu_model` |
| `device_battery_level` | int or null | no | Battery charge (0–100) at run time; null if unavailable. Outside 0–100 is a `400` |
| `device_power_state` | string or null | no | Run-environment power state: `charging`, `not_charging`, or `plugged_in_not_charging`. Null if unavailable |
| `device_power_save_mode` | bool or null | no | Whether OS low-power / battery-saver mode was active (can lower CPU clocks) |
| `device_apple_thermal_state_{before,after}` | array or null | no | Apple `ProcessInfo.thermalState`, one per repetition (Apple devices) |
| `device_apple_soc_temp_c_{before,after}` | array or null | no | iOS SoC die temperature in °C, one per repetition, from builds that can read it |
| `device_android_thermal_status_{before,after}` | array or null | no | Android `getCurrentThermalStatus()`, one per repetition |
| `device_android_thermal_headroom_{before,after}` | array or null | no | Android `getThermalHeadroom()`, one per repetition |
| `device_android_thermal_sensors_{before,after}` | array or null | no | Android thermal-HAL readings per sensor, each tagged with its `iteration` |
| `device_linux_thermal_zones_{before,after}` | array or null | no | Linux `/sys/class/thermal` readings per zone, each tagged with its `iteration` |
| `device_android_cpuset` | string or null | no | Android only. The cpuset cgroup the benchmark process ran under (`/top-app`, `/foreground`, `/moderate`, …); surfaces OEM scheduling demotion |
| `device_android_cpu_affinity_list` | string or null | no | Android only. Allowed CPU list in Linux CPU-list syntax (e.g. `0-5`, `0-3,6-7`) |
| `device_android_cpu_affinity_excludes_top_tier` | bool or null | no | Android only. True when the highest-frequency core tier is absent from the allowed set (the demotion signal) |
| `device_hardware_readings` | string or null | no | What the device's sensors reported around each measured rep, in the records the OS returned, as a JSON array in a **string**. Opaque: never refuses a submission, nothing looks inside an element or groups, matches, or scores on it, and stored as sent, but a value over 128 KiB keeps only its last elements that fit, and one that is not a JSON array is dropped. The recommended shape is in [design/device-telemetry.md](design/device-telemetry.md) ([why](storage.md#device_hardware_readings--device_os_throttling_readings--readiness_gate_waits)) |
| `device_os_throttling_readings` | string or null | no | What the OS or firmware reported about its own throttling around each rep: states, levels, bitmasks, headroom. Same rules as `device_hardware_readings` |
| `readiness_gate_waits` | string or null | no | What the client's readiness gate reported before each rep. Same rules as `device_hardware_readings`, but cut at 8 KiB |
| `model_name` | string or null | no | Optional grouping label (e.g. `"llama-3.2-1b"`); `model_descriptor` is the model's identity |
| `model_quant` | string or null | no | Optional quantization label for the primary artifact (e.g. `"q4_0"`); per-piece quant lives in `model_descriptor` |
| `model_descriptor` | string (JSON) or null | no | Full, lossless model specification as a JSON **string**. Handles single- and multi-artifact models. Opaque to the server: schema never interpreted, only key-order/whitespace normalized. When present, must be valid JSON (`400` otherwise). Recommended shape: the serialized `pipette-plan-types` `Model` variant ([examples](storage.md#model_descriptor--runtime_descriptor)) |
| `model_flags` | string or null | no | Opaque configuration affecting model behavior. Typically a JSON string going forward (e.g. `{"enable_thinking":true}`) but a plain string is equally valid; never validated or interpreted. Key-order/whitespace normalized when it parses as JSON, stored trimmed-but-verbatim when it doesn't ([why](storage.md#model_flags--runtime_flags)) |
| `model_params_total_millions` | int or null | no | Total parameter count in millions; positive `i32`. The scorer prefers the value from the model parameter table |
| `model_params_active_millions` | int or null | no | Active parameter count in millions; positive `i32`, must be ≤ `model_params_total_millions` |
| `runtime_name` | string or null | no | Optional grouping label (e.g. `"llama.cpp"`); `runtime_descriptor` is the runtime's identity |
| `runtime_version` | string or null | no | Optional version label (e.g. `"b5000"`), stored verbatim and never interpreted |
| `runtime_descriptor` | string (JSON) or null | no | Full, lossless runtime specification as a JSON **string**, version baked in. Opaque to the server and normalized like `model_descriptor`. When present, must be valid JSON (`400` otherwise). Recommended shape: the serialized `pipette-plan-types` `Runtime` variant ([examples](storage.md#model_descriptor--runtime_descriptor)) |
| `runtime_flags` | string or null | no | Opaque configuration affecting the runtime itself. Typically a JSON string going forward but a plain string is equally valid; never validated or interpreted. Normalized exactly like `model_flags` ([why](storage.md#model_flags--runtime_flags)) |
| `benchmark_flags` | string (JSON) or null | no | The **resolved** harness configuration the run actually executed under (readiness gating, timeouts, loop detection) as a JSON **string**. Opaque to the server: schema never interpreted, only key-order/whitespace normalized like `model_descriptor`. When present, must be a valid JSON object (`400` otherwise). Resolved, not authored: a client that left a setting unset submits the value it ran with, never `null` ([why](storage.md#benchmark_flags)) |
| `client_configuration` | string (JSON) or null | no | The environment the client forwarded into the runtime process it spawned, as a JSON **string**: `{"envs":[...]}`. Opaque and stored verbatim, unlike `benchmark_flags`, which is normalized and hashed. When present, must be a valid JSON object (`400` otherwise). Absent, not null, when the client had no entry for the runtime it ran ([why](storage.md#client_configuration)) |
| `runtime_cpu_variant` | string or null | no | Runtime-selected CPU kernel variant, interpreted per `runtime_name`; for llama.cpp/ggml the `ggml-cpu-<tag>` variant (e.g. `"armv8.2_1"`, `"apple_m2_m3"`). Null on single-static-backend builds |
| `client_version` | string or null | no | Version of the **client build** that produced the run: the harness, not the inference runtime it drove (`runtime_version`). Opaque to the server: stored verbatim, never parsed, ordered, or compared. Must be non-blank when present (`400` otherwise) |
| `device_time_ms` | int or null | no | The milliseconds that the device spent on the claimed job. Stored as sent, and nothing scores on it. Must be from 0 to 9223372036854775807 (`i64::MAX`) (`400` otherwise). Null if the client does not measure the time ([why](storage.md#device_time_ms)) |
| `client_extras` | string (JSON) or null | no | Anything the client reports that no other field names, as a JSON **string** holding an object. Opaque: stored verbatim, and nothing groups, matches, or scores on it. When present, must be a valid JSON object of at most 16 KiB (`400` otherwise) ([why](storage.md#client_extras)). Three keys are read: an `asr_throughput` or `asr_max_memory` success must carry `asr_context_sizing` (`exact`, `upper_bound` or `pinned`), an `asr_throughput` one also `asr_audio_tokens_source` (`calibrated` or `measured`), and either must carry `asr_beyond_model_audio_limit` (a boolean); mgmt stores them again as typed observation columns ([benchmarks.md §2.10](benchmarks.md#210-asr-throughput)) |
| `runtime_stdout` | string or null | no | What the runtime wrote to stdout: the subprocess the client spawned, or the app itself when the runtime runs in-process. The server keeps the last 1 MiB and starts a cut text with `…`. `""` records that the process printed nothing, which absent does not. Stored apart from the submission, and returned by no route ([storage.md §4.2](storage.md#42-runtime-output)) |
| `runtime_stderr` | string or null | no | The same for stderr |
| `observation_max_swap_bytes` | int or null | no | Most the kernel held of the run in swap at once, in bytes. Every benchmark type but `eval` reports it. A term of its own: not part of `observation_max_host_bytes`, and never added to it. Must not be negative (`400` otherwise). A value of `0` is a real reading: the platform sampled swap and the run stayed resident. Null means nothing sampled it |
| `observation_max_host_bytes` | int or null | no | Peak resident memory the run held, in bytes, as the platform counts it; swapped-out pages are not counted. Every benchmark type but `eval` reports it. Must not be negative (`400` otherwise). Distinct from `max_host_bytes`, the measurement the peak-memory benchmarks require: the two can differ by design ([why](benchmarks.md#reading-the-memory-fields)). Null when nothing sampled it |

A peak-memory result's `max_host_bytes` and `max_gpu_bytes` may be spelt
`max_ram_bytes` and `max_vram_bytes`, the names the Rust worker sends. Both
spellings of one field in one body is a `400`.

A throughput submission, as the Rust worker sends it for a claimed job:

```json
{
  "job_id": "job-550e8400-e29b-41d4-a716-446655440000",
  "benchmark_id": "prefill_throughput_256",
  "benchmark_type": "prefill_throughput",
  "benchmark_params": {"prefill_tokens": 256, "batch_size": 1},
  "device_name": "MacBook Pro 14-inch (2023)",
  "device_form_factor": "laptop",
  "device_os_name": "macOS",
  "device_os_version": "15.3",
  "device_chip_model": "Apple M3 Pro",
  "device_ram_bytes": 36000000000,
  "model_descriptor": "{\"org\":\"unsloth\",\"path\":\"Qwen3.5-0.8B-Q4_0.gguf\",\"repo_name\":\"Qwen3.5-0.8B-GGUF\",\"source\":\"huggingface\",\"type\":\"gguf_text\"}",
  "runtime_descriptor": "{\"flavor\":\"macos-arm64\",\"repository_url\":\"github.com/ggml-org/llama.cpp\",\"repository_version\":\"b9050\",\"source\":\"github_release\",\"type\":\"llamacpp_cli_stock_tools\"}",
  "runtime_flags": "{\"number_gpu_layers\":99,\"threads\":8}",
  "client_version": "2026.10.02-430a0bf-internal",
  "device_time_ms": 41250,
  "prefill_time_ms": 34.7,
  "prefill_time_ms_stddev": 1.2
}
```

An eval carries `completions` in place of the timing fields:

```json
{
  "completions": [
    {"id": "a1b2c3d4e5f6", "completion": "The answer is B", "stop_reason": "eos", "completion_tokens": 412},
    {
      "id": "51cd2cdc9277",
      "completion": "",
      "stop_reason": "failure",
      "stop_detail": "[2026-03-10T12:04:51Z] llama-server crashed mid-completion: exit signal: 11 (SIGSEGV)",
      "completion_tokens": 0
    }
  ]
}
```

###### 2.7.1.1.1. Per-sample generation metadata

Each `completions[]` entry may carry optional metadata about how that
sample's generation ended. All of it is re-injected onto the per-sample
warehouse rows after scoring
([scoring-service.md](scoring-service.md#per-sample-stop_reason-canonical)).
Only `stop_reason` also reaches the scoring service, which reads it to tell
a sample cut at the output-token cap or the context bound from a finished
one; the rest is **stripped** from the `/score` request.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `stop_reason` | string or absent | no | How generation ended, from the canonical enum: `eos`, `truncated`, `doom_loop`, `failure`, `unknown`. Absent when the client does not report one. Stored as reported: a value outside the enum is persisted rather than rejected, so a client-side addition needs no server release |
| `stop_detail` | string or absent | no | Free-form observation behind `stop_reason`, the *why*: crash detail for `failure`, the unclassified stop type for `unknown`, the trigger for `doom_loop`. Normally omitted for a clean `eos` / `truncated` |
| `completion_tokens` | int or absent | no | Output tokens generated for this sample. Paired with `stop_reason` to tell `eos` (below the cap) from `truncated` (at it, or below it when the entry's `parameter_max_context` stopped the sample) |
| `failed` | bool | no | **Legacy.** Stored verbatim on the submission, but never read: the failure columns are derived from `stop_reason`. See below |
| `failed_reason` | string or absent | no | **Legacy.** See below |

**`stop_reason == "failure"` is the sole signal for a failed sample.** It is
what marks the sample failed in the warehouse and what counts it toward the
`samples_failed` metric.

The server still accepts the legacy `failed` / `failed_reason` pair, but never
reads `failed`: the stored `failed` and `failed_reason` are derived from
`stop_reason`. A sample sent as `{"failed": true, "failed_reason": "…"}` with no
`stop_reason` is recorded as **not failed**, and its reason is dropped.
`failed_reason` still stands in for `stop_detail` when `stop_reason` *is*
`failure`.

##### 2.7.1.2. Response `202 Accepted`

```json
{"job_id": "job-550e8400-e29b-41d4-a716-446655440000"}
```

An ad-hoc submission (no `job_id`) is given a fresh `job-{uuid}`; a
plan-attached one echoes the claim's.

**Resubmitting under the same `job_id`.** An **approved** client's
plan-attached body is bound to its live lease before anything is written
(§2.7.3), and that lease is what decides a repeat submission. There is no
separate "already submitted" check. (A held submission binds no lease; see
§2.7.4.)

- While the lease is still live (the client resubmits before the server has
  torn the job down, or after a successful
  [`reclaim`](#211-post-plansjob_idreclaim)), the write **supersedes** the
  caller's own earlier body, unless the scorer has already taken that body, in
  which case both are kept.
- Once the first submission has completed the job, the lease no longer exists,
  so a repeat submission is refused with `404` (the claim-binding row in
  §2.7.3). That `404` means the earlier attempt landed: a following `reclaim`
  also `404`s.

A client superseded meanwhile, with the job leased to somebody else, gets `409`
instead. Every refusal is reported; no result is dropped silently.

An ad-hoc submission binds no lease and has no dedup key: the server mints a
fresh id per request, so retrying one records a second independent result
([client-integration.md §5](client-integration.md#5-error-handling-and-retries)).

#### 2.7.2. Failure variant

Used when `message_type` is `"failure"`. Only plan-attached benchmarks
produce failure submissions. Ad-hoc runs (no `job_id`) report only
on success, so `job_id` is required here.

##### 2.7.2.1. Request body

A failure body identifies, by `job_id`, which (benchmark, model, runtime)
tuple could not be executed and captures a human-readable reason.

It carries none of the success variant's device or measurement fields except
`device_time_ms`. The run produced no result to describe or measure, but the
device spent time on the job before it failed. Sending any other device or
measurement field is a `400` ([§2.7.3](#273-errors)) naming the offenders,
because the client built the wrong shape and dropping the fields silently would
hide that. Any *other* field
beyond the list below is still ignored, per §2.7. The required
`retriable` flag tells the server whether the failure is specific to the
reporting client (others may still attempt the job) or inherent to the
job itself (a terminal result).

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `message_type` | string | yes | Must be `"failure"` |
| `job_id` | string | yes | Job identifier from the [`POST /plans/claim`](#29-post-plansclaim) response, echoed verbatim. The descriptors are serialized from the claim's `spec.model` / `spec.runtime` (with any local HuggingFace token stripped) |
| `failure_reason` | string | yes | Why the benchmark could not be run. Typically includes a timestamp and the runtime's own error output |
| `retriable` | bool | yes | Whether **another** client may still attempt this job. `true`: the failure is specific to this client (e.g. out of disk, thermal throttle, transient local fault): the server denies the job to this client and keeps it available to others. `false`: the failure is inherent to the benchmark/model/runtime and would recur anywhere: the server records it as the job's terminal result and tears the job down. See [queue.md](queue.md#consequences-of-failure) |
| `benchmark_id` | string | yes | Benchmark that was being attempted |
| `benchmark_type` | string or null | no | With `benchmark_params`, checked like the success variant's pair |
| `benchmark_params` | object or null | no | See `benchmark_type`. A client reporting a claim on a benchmark it may not read has its pair ignored, as if it sent none, so the answer reveals nothing about the definition |
| `model_descriptor`, `runtime_descriptor` | string (JSON) or null | no | As on the success variant: valid JSON when present |
| `model_flags`, `runtime_flags` | string or null | no | As on the success variant |
| `model_name`, `model_quant`, `runtime_name`, `runtime_version` | string or null | no | Optional grouping labels, as on the success variant |
| `client_version` | string or null | no | Version of the client build reporting the failure; see the success table above. Worth sending here especially: a failure raises the question of which harness build produced it |
| `device_time_ms` | int or null | no | The milliseconds from the claim to the moment the failure is ready to submit. See the success table above. This is the only `device_*` field that a failure carries |
| `client_extras` | string (JSON) or null | no | See the success table above |
| `runtime_stdout`, `runtime_stderr` | string or null | no | What the runtime printed before it failed; see the success table above. Kept for a retriable failure too, which records no result |

Example:

```json
{
  "message_type": "failure",
  "job_id": "job-550e8400-e29b-41d4-a716-446655440000",
  "benchmark_id": "prefill_throughput_256",
  "failure_reason": "[2026-03-10T12:04:51Z] llama-server failed to load model: out of memory",
  "retriable": true,
  "model_descriptor": "{\"org\":\"unsloth\",\"path\":\"Qwen3.5-0.8B-Q4_0.gguf\",\"repo_name\":\"Qwen3.5-0.8B-GGUF\",\"source\":\"huggingface\",\"type\":\"gguf_text\"}",
  "runtime_descriptor": "{\"flavor\":\"macos-arm64\",\"repository_url\":\"github.com/ggml-org/llama.cpp\",\"repository_version\":\"b9050\",\"source\":\"github_release\",\"type\":\"llamacpp_cli_stock_tools\"}",
  "client_version": "2026.10.02-430a0bf-internal",
  "device_time_ms": 3120
}
```

A **non-retriable** failure (`retriable: false`) is the job's terminal result.
The scorer records one result row per metric the job intended to measure, each
with a null `value` and the failure fields set
([storage.md](storage.md#failure-rows)), and no eval sample results.
`GET /jobs/{job_id}` reports `status: "failed"` with the `failure_reason`
verbatim, before and after scoring.

A **retriable** failure (`retriable: true`) is *not* a job result. The server
denies the job to the reporting client and keeps it available to other eligible
clients ([queue.md](queue.md#consequences-of-failure)). The denial keeps the
`failure_reason`, and the runtime output is kept as for a recorded result. No
submission record is written, so a later `GET /jobs/{job_id}` answers `404`. If
every eligible client has since reported a retriable failure,
`queue-maintenance` converts the job to a terminal failure
whose `failure_reason` starts with `"All eligible clients reported failure"`
and summarizes the clients' reasons (see
[queue.md](queue.md#the-all-denied-failure-reason)). That record belongs to
the reserved client id `"system"`, so `GET /jobs/{job_id}` answers `404` to
every client for it too ([§2.13](#213-get-jobsjob_id)). A user reads the
outcome in the plan's jobs and results
([`GET /api/v1/plans/{plan_id}/jobs`](api/plans/list_plan_jobs.md),
[`GET /api/v1/plans/{plan_id}/results.csv`](api/plans/get_plan_results_csv.md)).

##### 2.7.2.2. Response `202 Accepted`

The body of [§2.7.1.2](#2712-response-202-accepted), echoing the claim's
`job_id`. Resubmission works as it does for a success body: the lease decides.
A **non-retriable** failure completes the job, so a repeat submission meets the
same `404` a repeat success does. A **retriable** failure returns the job to the
queue, so the caller no longer holds it, and a repeat submission is a `404` too.

#### 2.7.3. Errors

| Status | Condition |
|--------|-----------|
| 400 | Invalid `message_type` (must be `"success"` or `"failure"` when present) |
| 400 | `job_id` present but not a well-formed job id (contains characters outside `[A-Za-z0-9-]`) |
| 400 | Missing or invalid `benchmark_id` |
| 400 | Only one of `benchmark_type` and `benchmark_params` present, either malformed, or the pair disagrees with the catalog's entry for `benchmark_id` |
| 400 | Failure: missing `job_id`, `failure_reason`, or `retriable` |
| 400 | Failure: `retriable` present but not a boolean |
| 400 | Success: missing required field (`device_name`, `device_form_factor`, `device_os_name`, `device_os_version`, `device_chip_model`, `device_ram_bytes`, or type-specific fields) |
| 400 | Success: `model_params_total_millions` or `model_params_active_millions` present but not a positive `i32`, or `_active > _total` |
| 400 | Success: a reported number outside its range ([§2.7.1.1](#2711-request-body)) |
| 400 | Success: invalid `device_form_factor` (must be one of: `phone`, `tablet`, `laptop`, `desktop`, `server`, `embedded`) |
| 400 | Success: `device_gpu_vram_bytes` present without `device_gpu_model` |
| 400 | Success: `device_npu_vram_bytes` present without `device_npu_model` |
| 400 | Success: eval submission: `completions` missing or empty |
| 400 | Success: vlm_eval or asr_eval submission for an entry that pins a corpus: `observation_dataset_digest` missing or not that corpus |
| 400 | Success: eval submission: `completions[].id` missing or not a string |
| 400 | Success: eval submission: duplicate `completions[].id` (each id must appear at most once) |
| 400 | Failure: the body carries a `device_*` field other than `device_time_ms`, or a measurement: any benchmark's result field (`prefill_time_ms`, `completions`, `max_host_bytes`, …) or an `observation_max_*_bytes` count. The error names the offenders |
| 400 | Success: the body carries `failure_reason` or `retriable`. The error names them |
| 400 | `client_extras` present but not a JSON object, or over 16 KiB |
| 400 | Success: an `asr_throughput` or `asr_max_memory` body whose `client_extras` lacks `asr_context_sizing` or `asr_beyond_model_audio_limit`, an `asr_throughput` one lacking `asr_audio_tokens_source`, either of the first two keys holding a value mgmt does not know, or an `asr_beyond_model_audio_limit` that is not a boolean. The error names the key. Also an `asr_throughput` body whose two keys are a pair no client produces (`exact` with `measured`, `upper_bound` with `calibrated`), or whose `asr_audio_tokens` plus the entry's `parameter_text_tokens` is not less than `asr_prompt_tokens` ([benchmarks.md §2.10](benchmarks.md#210-asr-throughput)) |
| 400 | `device_time_ms` present but negative, or greater than `i64::MAX` |
| 400 | Both spellings of one memory field, such as `max_host_bytes` and `max_ram_bytes` |
| 401 | Missing or invalid auth headers |
| 403 | Client is pending (not approved) and `[unverified_submissions] enabled = false` on this server |
| 403 | Approved client, `job_id` supplied, and it does not hold `perform_work`: `client lacks the perform_work permission` |
| 403 | Approved client, `job_id` absent (an ad-hoc result), and it does not hold `submit_results`: `client lacks the submit_results permission` |
| 403 | Success on a claimed job that names a vault resource the client may not fetch ([Vault](api/vault.md#who-may-do-what)): `the job names a vault resource this client may not fetch; only a failure may be reported` |
| 404 | Benchmark not found: neither the public catalog nor the client's organization keeps the id, and the body is not a failure reporting the client's own claim on a job that runs it ([§2.7](#27-post-benchmarks)) |
| 404 | `job_id` supplied but the client holds no active claim on it (the lease was recycled, the job completed or expired, or it was never claimed) |
| 404 | `job_id` supplied while the client's profile re-evaluation is pending (`reindex_pending`). A profile change ([§2.4](#24-patch-clientsme)) relinquishes the client's leases, voiding the claim; the result is forfeited and the claim is never renewed |
| 409 | `job_id` supplied but the job is leased to a **different** client: the caller was superseded |

The pending `403` is decided from the headers, so it comes before any `400` or
`413` ([§1.2](#12-request-size-limits)): the server never reads the body of a
client it refuses for its status.

A submission that omits `job_id` (an ad-hoc run; the server mints one) is never
subject to the `404`/`409` claim-binding checks: there is no claim to bind. Held
submissions from a pending client (§2.7.4) skip them too: they are kept apart
per client, so there is nothing to hijack.

#### 2.7.4. Unverified (held) submissions

When the server is configured with `[unverified_submissions] enabled = true`,
a submission from a **pending** (unapproved but validly-signed) client is
*held* instead of rejected with `403`. The body is validated with exactly the
same rules as an approved submission (§2.7.1 / §2.7.2), stamped as usual, and
kept apart per client, **never scored** while held
([storage.md §4.1](storage.md#41-unverified-submissions)). An operator resolves
a client's held submissions once it is approved or rejected:
`pipette-mgmt unverified promote` re-stages them for the scorer, and
`pipette-mgmt unverified delete` discards them ([cli.md](cli.md)).

##### 2.7.4.1. Response `202 Accepted`

```json
{"job_id": "job-550e8400-e29b-41d4-a716-446655440000"}
```

The `job_id` is a receipt for operator triage. It is **not** resolvable via
[`GET /jobs/{job_id}`](#213-get-jobsjob_id) until the submission is promoted.

##### 2.7.4.2. Errors

Held submissions use the same `400` validation table as
[§2.7.3](#273-errors); the claim-binding `404` / `409` rows do not apply. A
pending client is served no organization's entries, so a private id is the
`404` an unknown one is.

### 2.8. `POST /benchmarks/batch`

Submit multiple benchmark results in a single request. Each element of
the `submissions` array is validated and written independently with the
same rules as [`POST /benchmarks`](#27-post-benchmarks).

Authentication and disposition follow [`POST /benchmarks`](#27-post-benchmarks):
the whole batch is processed under the single authenticated client, and a
pending client's items are all **held** (§2.7.4) when that is enabled. The
*permission* is the one thing decided per item, because it follows each body's
own operation (see §2.8.3).

**Per-item failures are swallowed.** Unlike `POST /benchmarks`, this
endpoint returns `200 OK` even when some submissions fail validation or
fail to write. Each element of the returned `results` array reports
either a `job_id` (success) or an `error` string (failure) tagged with
its original `index`. Callers must inspect every item to know what
actually succeeded.

The whole request only fails (4xx) when the request envelope itself is
bad: missing/invalid auth headers, a pending client when
`[unverified_submissions] enabled = false`, missing `submissions`
array, empty array, or more than 1000 items.

#### 2.8.1. Request body

| Field | Type | Description |
|-------|------|-------------|
| `submissions` | array | 1–1000 submission objects, each with the same shape as the body of `POST /benchmarks` |

#### 2.8.2. Response `200 OK`

| Field | Type | Description |
|-------|------|-------------|
| `results` | array | One entry per input submission, in the same order |
| `results[].index` | integer | Position of the submission in the request array |
| `results[].job_id` | string or absent | Assigned job id (`job-{uuid}`) when the submission was accepted |
| `results[].error` | string or absent | Human-readable reason when the submission was rejected |

Exactly one of `job_id` or `error` is present per entry.

Example with a mix of successes and failures:

```json
{
  "results": [
    {"index": 0, "job_id": "job-550e8400-e29b-41d4-a716-446655440000"},
    {"index": 1, "error": "invalid submission: missing field `device_name`"},
    {"index": 2, "error": "benchmark not found"}
  ]
}
```

#### 2.8.3. Errors

These apply to the request envelope; per-item failures are reported
inside `results` (see above) rather than as a 4xx status.

| Status | Condition |
|--------|-----------|
| 400 | Missing `submissions` array |
| 400 | Empty `submissions` array |
| 400 | More than 1000 submissions in one request |
| 401 | Missing or invalid auth headers |
| 403 | Client is pending (not approved) and `[unverified_submissions] enabled = false` on this server |

As on [`POST /benchmarks`](#273-errors), the pending `403` is decided from the
headers and comes before any `400` or `413`.

The permission a submission requires follows its body
([§2.7](#27-post-benchmarks)), so it is decided **per item**: a batch mixing a
claimed-job report with an ad-hoc result is judged a name at a time, and an item
the caller may not perform carries
`client lacks the {permission} permission` in its own `error` field while the
rest of the batch is written. The caller's permission set is read once for the
request, whatever the item count.

### 2.9. `POST /plans/claim`

Claim the next available job for execution. Requires an approved client
holding `perform_work` (403 if pending, 403 if the permission is missing). The
server checks the client's eligibility index, selects
one available job, leases it to the calling client, and returns the lease
envelope and the job's run specification. See [queue.md](queue.md) for the
job lifecycle and storage layout.

The client must run the benchmark and report the outcome via
[`POST /benchmarks`](#27-post-benchmarks), echoing the `job_id`. Its
`model_descriptor` / `runtime_descriptor` are serialized from the same
`spec.model` / `spec.runtime` the claim carried, so they are the echo the server's
claim-binding check expects. Failures use the same endpoint with
`message_type: "failure"`. While running, the client renews the lease with
[`PUT /plans/{job_id}/heartbeat`](#210-put-plansjob_idheartbeat).

#### 2.9.1. Request body

Empty (`{}` or no body).

#### 2.9.2. Response `200 OK`

A server-owned **envelope** wrapped around the **`spec`** the planner authored.
The envelope is the small set of fields the management server acts on (identity,
lease, expiry), and `spec` is the run specification, forwarded verbatim: the
server stores it without interpreting it. Clients must tolerate unrecognized
fields in both. Benchmark parameters and eval samples are not included. Call
[`GET /benchmarks/{benchmark_id}`](#26-get-benchmarksbenchmark_id) if needed.

The job's eligibility fields (`clients`, `requires`, `any_of` and the negative
`exclude_clients`, `excludes`, `not_all_of`) are **not** returned. They are
inputs to selection and already spent by the time a job is handed out. What the
client needs from them is `client_versions`: which of the app builds it reports
the job admits.

| Field | Type | Description |
|-------|------|-------------|
| `job_id` | string | Job identifier, assigned by the server at ingestion |
| `benchmark_id` | string | Benchmark identifier. Always equal to `spec.benchmark`: the server lifts it from there, so the two cannot disagree |
| `time_window` | string | Lease increment as an ISO 8601 duration (e.g. `"PT5M"`), **set by the server** from its configured lease duration. Each heartbeat extends the lease by this much. When the claim hands back a job the client already holds (below), it is instead the *remaining* life of that lease |
| `expires_at` | string, optional | ISO 8601 basic-format timestamp (`20240908T000000Z`) after which the job will no longer be assigned. **Absent** when the job never auto-expires |
| `client_versions` | array of strings | The builds the client may run the job on, ascending: each `<v>` of the client's own `client_version:<v>` flags such that the job admits the client when that flag is the only `client_version:` flag it carries. A job with no version clause admits every reported version, as does a `clients` roster entry. `[]` when the client reports no version, or the job admits it at no single one. Always present, on the idempotent path too |
| `spec` | object | The run specification (see below) |

`spec` is `pipette-plan-types`' `ClientRunSpec`. The server-side planner
constructs it for a plan document. The server passes it through unchanged:

| Field | Type | Description |
|-------|------|-------------|
| `benchmark` | string | Benchmark identifier; must equal the envelope's `benchmark_id` |
| `model` | object | Full, lossless model specification. The `type` field tags it as `gguf_text`, `gguf_vision`, `gguf_vision_speculative`, `mlx`, `mlx_speculative`, `torch`, `openvino`, `onnx_genai`, or `apple_foundation_text`. The object flattens the source coordinates alongside it |
| `runtime` | object | Full, lossless runtime specification. The `type` field tags it. The object flattens the version, build, and flavor coordinates alongside it |
| `model_flags` | object, optional | Model-generation flags for this `(benchmark, model)`: `enable_thinking`, and on `gguf_text` and `torch` a `reasoning_effort` of `low`, `medium` or `high`. Eval and vlm_eval only (a vlm_eval takes `enable_thinking` alone). Omitted when unset |
| `runtime_flags` | object, optional | Runtime load flags for this `(benchmark, runtime, model)`. Omitted when unset |
| `benchmark_flags` | object, optional | HTTP timeout, doom-loop, and readiness settings for this cell. Omitted when unset |

Each flag group carries its own `benchmark_type` / `runtime_type` / `model_type`
discriminants, which must agree with the `benchmark`, `model`, and `runtime` the
spec names. The flag groups **reject unrecognized keys**, and so do some runtime
variants: `uv_openvino`, `uv_onnx_genai`, `ovms` and `trt_edgellm`, and the
nested `source` object on every uv-provisioned variant. `spec` and `model`
tolerate them.

`trt_edgellm` is a TensorRT Edge-LLM server archive for one SoC and CUDA major
(`flavor`, e.g. `jetson-orin-cuda-13`), from a `remote_archive`,
`vault_remote_archive`, `relative_dir` or `absolute_dir` source. It runs a
`gguf_vision` model on the `vl_throughput` and `vl_max_memory` benchmarks only,
and its jobs require `accel:gpu` and `cpu_vendor:nvidia`, which a Jetson
reports. Its `runtime_flags` take `precision` (`fp16`, `bf16` or `w8a16`, the
language model's engine precision), `envs` and `raw`; `raw` may not carry
`--model`, `-m`, `--mmproj`, `--host`, `--port`, `--cache-dir`,
`--build-only` or `--precision`.

Example:

```json
{
  "job_id": "job-550e8400-e29b-41d4-a716-446655440000",
  "benchmark_id": "eval_ifbench_2026.06.1",
  "time_window": "PT10M",
  "expires_at": "20240908T000000Z",
  "client_versions": ["2026.10.02-430a0bf-internal"],
  "spec": {
    "benchmark": "eval_ifbench_2026.06.1",
    "model": {
      "type": "gguf_text",
      "source": "huggingface",
      "org": "unsloth",
      "repo_name": "Qwen3.5-0.8B-GGUF",
      "path": "Qwen3.5-0.8B-Q4_0.gguf"
    },
    "runtime": {
      "type": "llamacpp_cli_stock_tools",
      "source": "github_release",
      "repository_url": "github.com/ggml-org/llama.cpp",
      "repository_version": "b9050",
      "flavor": "macos-arm64"
    },
    "model_flags": {
      "model_type": "gguf_text",
      "benchmark_type": "eval",
      "enable_thinking": true
    },
    "runtime_flags": {
      "runtime_type": "llamacpp_cli_stock_tools",
      "model_type": "gguf_text",
      "benchmark_type": "eval",
      "number_gpu_layers": 99,
      "threads": 8
    }
  }
}
```

A plan pins builds with a variant's `client_versions`, which the planner writes
as one `any_of` group of `client_version:` flags. A client that reports
`client_version:` flags runs the job on one of the builds `client_versions`
names, so the submission's `client_version` is that build. `[]` then means no
single build it reports satisfies the job
([client-integration.md §4](client-integration.md#4-the-work-loop)).

A claim whose `spec` the client cannot read (absent, unparseable, naming a
benchmark that disagrees with the envelope, or pairing an incompatible model and
runtime) is **mis-authored, not transient**: it would fail identically anywhere
([client-integration.md §6](client-integration.md#6-retriable-vs-non-retriable-failures)).

A claim from a client that already holds exactly one live lease returns that
same job, so a claim whose response was lost is recovered by claiming again.
`time_window` is then the lease's remaining life, and the lease is not renewed
(see [queue.md](queue.md#claiming-a-job)). The server
counts that lease as lost to the job, once per lease, and withholds a job whose
lost leases reach `plan_lost_lease_limit` (see
[queue.md](queue.md#heartbeat-timeout-dead-device)).

#### 2.9.3. Response `204 No Content`

Returned when no job is currently available for this client. Possible reasons:

- No jobs in the queue list this client as eligible.
- All eligible jobs are currently leased to other clients.
- All eligible jobs have passed their `expires_at`. An expired job is never
  handed out, even if `queue-maintenance` has not yet removed it from the queue
  (see [queue.md](queue.md#expiration)).
- Every eligible job, or the job the client already holds, names a vault
  resource the client may not fetch ([Vault](api/vault.md#who-may-do-what)).
  Such a job, granted or already held, goes back to the queue for a device
  that may, and is denied to this client.
- The client already holds a lease on a job that has lost
  `plan_lost_lease_limit` leases. The lease lapses and `queue-maintenance`
  retires the job.
- The client is suspended (it held more than one unexpired lease; see
  [queue.md](queue.md#claiming-a-job)), or this claim would have given it a
  second one, which suspends it. The response is identical to the
  no-job case. An operator clears the flag with
  `pipette-mgmt clients unsuspend` before the client can claim again.

The body is empty. How long to wait before claiming again is
[client-integration.md §5](client-integration.md#idle-polling-cadence).

#### 2.9.4. Errors

| Status | Condition |
|--------|-----------|
| 401 | Missing or invalid auth headers |
| 403 | Client is pending (not approved) |
| 403 | Client does not hold `perform_work`: `client lacks the perform_work permission` |

### 2.10. `PUT /plans/{job_id}/heartbeat`

Renew the lease on an active job. Requires an approved client holding
`perform_work` (403 if pending, 403 if the permission is missing). Each
successful heartbeat extends the lease by the full lease length from now, so a
client heartbeats well inside it
([client-integration.md §4](client-integration.md#4-the-work-loop)). A claim
handed back idempotently reports only the lease's remaining life as
`time_window`.

Two responses say the lease is no longer the caller's:

- **`404 Not Found`**: no active lease exists for this `job_id`. The lease
  expired and the job returned to the queue, or the client's profile
  re-evaluation is pending: a profile change ([§2.4](#24-patch-clientsme))
  relinquishes every lease, so there is nothing to renew. A job still
  unclaimed can be taken back with
  [`POST /plans/{job_id}/reclaim`](#211-post-plansjob_idreclaim).
- **`409 Conflict`**: the job is leased to a different client, which
  superseded this one. A result it submits anyway is refused with `409` by
  the claim-binding check on [`POST /benchmarks`](#273-errors), and never
  displaces the result of the client that holds the lease.

What a client does about each is
[client-integration.md §5](client-integration.md#5-error-handling-and-retries).

A suspended client gets `403` without a renewal, so its leases lapse. A
renewal that leaves the client a second live lease, as renewing a lapsed lease
can, suspends it and answers `403`
([queue.md](queue.md#claiming-a-job)). So does a lease on a job that names a
vault resource the client may not fetch
([Vault](api/vault.md#who-may-do-what)): it could never run the job, so its
lease goes back to the queue at once, denied to the client, for a device that
can.

A heartbeat never fails because the job has passed its `expires_at`. The
deadline governs when a job may be *assigned*, not how long a client that
already holds it may run, so a benchmark in progress is allowed to finish
([queue.md](queue.md#expiration)). Nor is a lease's total life capped: one
lease may be renewed indefinitely. An operator stops a device that stalls a
job by revoking its `perform_work` or unapproving it, which refuses its
heartbeats, so the lease lapses.

#### 2.10.1. Path parameters

| Parameter | Type | Description |
|-----------|------|-------------|
| `job_id` | string | Job identifier assigned by the planner |

#### 2.10.2. Request body

Empty (`{}` or no body).

#### 2.10.3. Response `200 OK`

Empty body.

#### 2.10.4. Errors

| Status | Condition |
|--------|-----------|
| 400 | `job_id` in the path contains characters outside `[A-Za-z0-9-]` |
| 401 | Missing or invalid auth headers |
| 403 | Client is pending (not approved) |
| 403 | Client does not hold `perform_work`: `client lacks the perform_work permission` |
| 403 | Client is suspended, or this renewal left it a second live lease and suspended it: `client is suspended` |
| 403 | The job names a vault resource the client may not fetch; the lease goes back to the queue, denied to the client: `the job names a vault resource this client may not fetch` |
| 404 | No active lease found for this `job_id`: the lease was reaped |
| 404 | Client's profile re-evaluation is pending. Its leases were relinquished by the profile change ([§2.4](#24-patch-clientsme)); the lease is never renewed |
| 409 | Lease belongs to a different client |

### 2.11. `POST /plans/{job_id}/reclaim`

Re-acquire the lease on a job the client was previously running, for recovery
after its heartbeats could not be sent while it ran the benchmark and the lease
was reaped meanwhile.

If the calling client still holds a lease on the job (expired, but not yet
recycled by `queue-maintenance`), the server renews it and returns `200`.
Otherwise it re-acquires the job from the queue under the same eligibility and
denial checks as [`POST /plans/claim`](#29-post-plansclaim). Requires an
approved client holding `perform_work` (403 if pending, 403 if the permission
is missing); a suspended client receives `403`. A reclaim that would leave the
client a second live lease suspends it: a renewal answers `403`, and a
re-acquire puts the job back and answers `404`. A job that names a vault
resource the client may not fetch is neither renewed nor re-acquired, and
answers `403`; a re-acquired one goes back to the queue.

While the client's profile re-evaluation is pending (`reindex_pending`, see
[§2.4](#24-patch-clientsme)) the whole endpoint returns `404` without renewing
anything: the profile change relinquished the client's leases and its
eligibility is not yet known. Work from before the profile change is forfeited.

A job past its `expires_at` is treated as gone and returns `404`, even if
`queue-maintenance` has not yet removed it
([queue.md](queue.md#expiration)). This applies only to the
re-acquire path; a lease the client still holds is allowed to finish
([§2.10](#210-put-plansjob_idheartbeat)).

#### 2.11.1. Path parameters

| Parameter | Type | Description |
|-----------|------|-------------|
| `job_id` | string | Job identifier the client wishes to reclaim |

#### 2.11.2. Request body

Empty (`{}` or no body).

#### 2.11.3. Response `200 OK`

Lease re-acquired; heartbeats renew it from here. The response body is empty:
the client already has the job JSON from the original claim.

#### 2.11.4. Errors

| Status | Condition |
|--------|-----------|
| 400 | `job_id` in the path contains characters outside `[A-Za-z0-9-]` |
| 401 | Missing or invalid auth headers |
| 403 | Client is pending (not approved), or client is suspended, or this renewal left it a second live lease and suspended it |
| 403 | The job names a vault resource the client may not fetch; nothing is renewed and the lease, held or re-acquired, goes back to the queue: `the job names a vault resource this client may not fetch` |
| 403 | Client does not hold `perform_work`: `client lacks the perform_work permission` |
| 404 | Job cannot be re-acquired and is not leased to anyone. It is past its `expires_at`, was completed, or was re-acquired and finished by another client |
| 404 | The re-acquire would have left the client a second live lease; the job goes back to the queue and the client is suspended |
| 404 | Client's profile re-evaluation is pending (`reindex_pending`). Its leases were relinquished by the profile change ([§2.4](#24-patch-clientsme)); prior work is forfeited |
| 409 | Job is currently leased to a different client |

### 2.12. `POST /api/v1/plans`

This route files no plan. The server files a plan only into a campaign
([Campaigns: Filing a plan document](api/campaigns.md#filing-a-plan-document)). A `POST` with any body answers `405` in the error
envelope, with an `Allow` header, and stages nothing:

```json
{"error": "This server files a plan only into a campaign. File the plan document with `POST /api/v1/campaigns` or `POST /api/v1/campaigns/{campaign_id}/plans`."}
```

A `pipette-plan` release that has no `campaigns` verbs posts its plan to this
route, and prints the message. The server answers before it reads the
credentials, so a device and an unsigned caller get the same `405`.
`GET /api/v1/plans` lists the plans in the caller's scope
([`GET /api/v1/plans`](api/plans/list_plans.md)).

### 2.13. `GET /jobs/{job_id}`

Get the status of a job. The server verifies the job belongs to the authenticated
client. If the job is processed, metrics are included in the response.

For plan-attached submissions, `job_id` is the same identifier returned by
[`POST /plans/claim`](#29-post-plansclaim), so no separate tracking is needed.

Held submissions (see §2.7.4) are not retrievable through this endpoint until
promoted: their `job_id` is a receipt, not a lookup key.

#### 2.13.1. Path parameters

| Parameter | Type | Description |
|-----------|------|-------------|
| `job_id` | string | Job id (`job-{uuid}` for server-minted jobs) |

#### 2.13.2. Response `200 OK`

| Field | Type | Description |
|-------|------|-------------|
| `job_id` | string | Job id (`job-{uuid}` for server-minted jobs) |
| `benchmark_id` | string | Benchmark identifier |
| `benchmark_type` | string | Benchmark type |
| `status` | string | `incoming` (awaiting processing), `scoring` (an eval in the score-queue awaiting/finishing the scoring-service call), `processed` (scored), or `failed` (a failure record; its metrics are not surfaced here) |
| `submitted_at` | string | ISO 8601 submission time |
| `scored_at` | string or null | ISO 8601 scoring time (null unless `status: "processed"`; absent on a failed job) |
| `score_runtime_version` | string or null | Version of the evals server that scored this job (non-null only for eval benchmarks when `status: "processed"`; absent on a failed job) |
| `metrics` | array or null | Benchmark metrics (null unless `status: "processed"`; absent on a failed job) |
| `failure_reason` | string | Client-supplied failure reason (only when `status: "failed"`) |
| `model_name`, `model_quant`, `runtime_name`, `runtime_version` | string or null | Model / runtime identity echoed verbatim from the failure record (only when `status: "failed"`); each is null when the failure record omitted it |
| `client_version` | string or null | Version of the client build that reported the failure, echoed verbatim (only when `status: "failed"`); null when the failure record omitted it. The warehouse `client_version` column carries the same value on the failure's rows |
| `metrics[].metric` | string | Metric name (e.g. `prefill_throughput`). See [benchmarks.md](benchmarks.md) for per-type metric definitions |
| `metrics[].value` | number | Metric value |
| `metrics[].value_stddev` | number or null | Standard deviation, propagated from the submitted per-field stddev. Present on both direct and derived metrics when the input stddev is available. See [benchmarks.md](benchmarks.md) for propagation formulas. Null when the submission did not include stddev for the relevant timing field |
| `metrics[].unit` | string | Unit of measurement (e.g. `tokens/sec`) |

Processed job:

```json
{
  "job_id": "job-550e8400-...",
  "benchmark_id": "prefill_throughput_256",
  "benchmark_type": "prefill_throughput",
  "status": "processed",
  "submitted_at": "2026-03-10T12:01:00Z",
  "scored_at": "2026-03-10T12:02:30Z",
  "score_runtime_version": null,
  "metrics": [
    {"metric": "ttft", "value": 34.7, "value_stddev": 1.2, "unit": "ms"},
    {"metric": "prefill_throughput", "value": 7373.0, "value_stddev": 255.1, "unit": "tokens/sec"}
  ]
}
```

`failed` is terminal for the *job*: no client runs it again. The scorer still
records its failure rows ([storage.md](storage.md#failure-rows)), which this
response does not surface: a failed job has no `scored_at`,
`score_runtime_version` or `metrics`. `status` is `"failed"` before and after
scoring: it comes from the body's `message_type`.

#### 2.13.3. Errors

| Status | Condition |
|--------|-----------|
| 400 | `job_id` in the path contains characters outside `[A-Za-z0-9-]` |
| 401 | Missing or invalid auth headers |
| 404 | Job not found |

### 2.14. `GET /jobs/{job_id}/eval-sample-results`

Get the eval sample results for a processed eval job. Returns the prompt,
completion, and correctness for each sample in the dataset. Only available
for eval-type benchmarks that have been scored. A `vlm_eval` or `asr_eval` job
is scored only with an answer to every sample, none failed, so its results
cover the whole dataset; a partial one is never scored, and stays a `404` here.

#### 2.14.1. Path parameters

| Parameter | Type | Description |
|-----------|------|-------------|
| `job_id` | string | Job id (`job-{uuid}` for server-minted jobs) |

#### 2.14.2. Response `200 OK`

```json
[
  {
    "id": "a1b2c3d4e5f6",
    "messages": [{"role": "user", "content": "..."}],
    "completion": "The answer is B",
    "is_correct": true,
    "failed": false,
    "failed_reason": null
  },
  …
]
```

| Field | Type | Description |
|-------|------|-------------|
| `id` | string | Sample ID |
| `messages` | array | Prompt messages as served by the evals server, shaped as in [§2.6](#26-get-benchmarksbenchmark_id), with a `vlm_eval`'s `image_url` parts dropped and their text kept, and an `asr_eval`'s `input_audio` parts kept in place with their `data` emptied. Stored as a JSON-encoded string in Parquet; the API deserializes it to an array in the response |
| `completion` | string | Model-generated text from the submission |
| `is_correct` | boolean | Whether the evals server scored this sample as correct. Failed samples (`failed: true`) almost always come back `false` because their completion was empty |
| `failed` | boolean | `true` if the client-side runtime crashed mid-completion for this sample. **Derived** from the submission's `stop_reason == "failure"` ([§2.7.1.1.1](#27111-per-sample-generation-metadata)), not copied from the client's legacy `failed` flag, so a sample submitted with only that flag reads back `false`. Pre-feature parquet files read back as `false` |
| `failed_reason` | string \| null | Free-form, human-readable description of the failure when known: the sample's legacy `failed_reason` if the submission sent one, else its `stop_detail` ([§2.7.1.1.1](#27111-per-sample-generation-metadata)). `null` on any row where `failed` is `false` |
| `measures` | object | Present only where the scorer measured the sample: an `asr_eval` clip's `hits`, `substitutions`, `deletions`, `insertions`, `reference_units`, and normalized `reference` and `hypothesis` ([benchmarks.md](benchmarks.md#212-asr-eval-error-rate)) |

#### 2.14.3. Errors

| Status | Condition |
|--------|-----------|
| 400 | `job_id` in the path contains characters outside `[A-Za-z0-9-]` |
| 401 | Missing or invalid auth headers |
| 404 | Job not found, job does not belong to client, job is not processed, job is not an eval benchmark, or the job failed |

### 2.15. `GET /datasets/{benchmark_id}`

Download a benchmark's corpus as a tar: a `vl_spec_decode`'s
`vl-spec-decode-corpus/1` ([benchmarks.md §2.8.1](benchmarks.md#281-the-dataset-object)),
a `vlm_eval`'s `vlm-eval-corpus/1`
([§2.9](benchmarks.md#the-samples-a-device-runs-the-corpus)) or an `asr_eval`'s
`asr-eval-corpus/1`
([§2.12](benchmarks.md#the-samples-a-device-runs-the-asr-eval-corpus)). Requires an
approved client holding `perform_work`: a corpus is work input.

The upstream owns the corpus. The response is served from mgmt's cache **only
while the latest upstream probe, at most `dataset_probe_max_age_secs` old
(default 180), found the upstream publishing the pinned content**. The request
never contacts the upstream: the `dataset-cache probe` task records a verdict on
the cached artifact every minute and this route reads it
([design](design/vl-spec-decode-dataset-delivery.md)).

#### 2.15.1. Response `200 OK`

```http
HTTP/1.1 200 OK
Content-Type: application/x-tar
Content-Length: 79619072
ETag: "7f5693b172a03d00c59e0fdf0b5b9d490d7198f1dc031567cdc8010c16a975c4"
Cache-Control: no-store
```

The body is the tar whose sha256 is `dataset.digest`; verify it before use. The
`ETag` is that digest. `Cache-Control: no-store` means the client keeps the
corpus for the job, not beyond it.

#### 2.15.2. Refusals

Every refusal is decided before the first byte and carries a fixed message; the
detail goes to the server log. A `503` is transient and carries
`Retry-After: 60`; a `502` does not clear on its own.

| Status | Body | Condition |
|--------|------|-----------|
| 401 | standard auth error | Missing or invalid auth headers |
| 403 | standard auth error | Client not approved, or lacks `perform_work` |
| 404 | `{"error": "benchmark not found"}` | Unknown `benchmark_id`, or one private to an organization the client is not in ([§2.6](#26-get-benchmarksbenchmark_id)) |
| 404 | `{"error": "benchmark has no dataset"}` | The benchmark serves no corpus |
| 503 | `{"error": "dataset not built"}` | No cached artifact for the pinned digest yet |
| 503 | `{"error": "upstream check stale"}` | The probe verdict is older than `dataset_probe_max_age_secs` |
| 503 | `{"error": "upstream unavailable"}` | The probe could not reach the upstream |
| 502 | `{"error": "upstream no longer backs the pin"}` | The upstream no longer publishes the pinned revision (or, for the evals server, those samples), or publishes different content under it |
| 502 | `{"error": "artifact does not match the pin"}` | The artifact cached under the pinned digest was built from another pin (an entry copied another's digest); logged as `dataset_pin_conflict` |
| 502 | `{"error": "artifact does not hold the entry's samples"}` | An `asr_eval` entry's `parameter_sample_count` is not the count its corpus's build recorded; logged as `dataset_sample_count_mismatch` |

### 2.16. `GET /api/v2/benchmarks` and `GET /api/v2/benchmarks/{benchmark_id}`

The catalog of [§2.5](#25-get-benchmarks) and
[§2.6](#26-get-benchmarksbenchmark_id), each entry reduced to its id and the
`benchmark_type` + `benchmark_params` pair a submission declares
([§2.7.1.1](#2711-request-body)). A client copies the two keys into its
`POST /benchmarks` body as they are.

Both routes serve the entries the v1 routes serve, by the same rule: open to
anyone, optionally signed, with a private entry served only to its
organization's principals. The listing takes the same `type` query parameter
and is sorted by `benchmark_id`. Neither route calls an upstream, so an eval
entry carries no `samples`.

An entry carries every `parameter_*` field its definition sets, sampling and
image count included, so entries that differ only in such a knob carry
different pairs. An eval always carries the `temperature` it runs at: the
entry's, or where it pins none the eval-id rule's that every client applies. Its
other sampling fields are omitted when the entry pins none, which leaves them to
the client. Only the corpus `dataset` stays on
[§2.6](#26-get-benchmarksbenchmark_id), so entries differing only in corpus
carry the same pair.

#### 2.16.1. Response `200 OK`

The listing is an array of entries; the lookup is one.

| Field | Type | Description |
|-------|------|-------------|
| `benchmark_id` | string | Unique benchmark identifier |
| `benchmark_type` | string | The entry's type |
| `benchmark_params` | object | The entry's `parameter_*` fields without the prefix |
| `org_id` | string | The id of the organization the entry is private to; **absent** on a public entry |

```json
{
  "benchmark_id": "eval_gpqa_diamond_2026.06.1_t1",
  "benchmark_type": "eval",
  "benchmark_params": {"eval_id": "gpqa_diamond", "dataset_name": "2026.06.1", "max_tokens": 8192, "temperature": 1.0}
}
```

#### 2.16.2. Errors

| Status | Condition |
|--------|-----------|
| 400 | Listing only: `type` names no benchmark type |
| 401 | `X-Client-Id` present and the signature does not verify |
| 403 | The signer is a disabled user |
| 404 | Lookup only: benchmark not found, or private to an organization the caller is not in |

### 2.17. `GET /vault/resources/{resource_id}`

One resource, as `NamedVaultResourceDetail` in the OpenAPI document
([§1.5](#15-the-openapi-document)): the record, and for an uploaded file the
plan entry it belongs to. An operator reads the same answer at
`GET /api/v1/vault/resources/{resource_id}`. An imported file's entry carries
no `custom_label`: an import names each file after its own origin
([storage.md §3.4](storage.md#34-the-vault)).

An account's read also carries `may_administer`, whether
[`DELETE`](api/vault/delete_resource.md) would admit the reader,
and for a model `model_params`, the parameter counts its results take
([model params](api/model-params.md)). A device's read carries neither. A row of the listing
([`GET /api/v1/vault/resources`](api/vault/list_resources.md)) carries both the same.

An account may ask for two more fields, which a device's read ignores:

| Query parameter | Adds |
|---|---|
| `names=true` | `names`, the owner's address or handle ([§1.4](#14-names)) |
| `object=true` | `object`, whether the file is still stored, from a stat of it: `{"state": "present"}`, `{"state": "resized", "bytes": …}` for a file of another size than `bytes` (replaced behind the record), or `{"state": "missing"}`. A store that does not answer is a `500` |

### 2.18. `GET /vault/resources/{resource_id}/content`

The route the surface exists for, and the second half of the pair
[who may do what](api/vault.md#who-may-do-what) describes: the record answers *what* the bytes are,
this answers *with* them. Both apply the same owner match, so they can never
disagree about who may have it.

Streams the file with:

| Header | Value |
|---|---|
| `Content-Type` | `application/octet-stream` |
| `Content-Length` | the file's size, read from the store |
| `ETag` | the record's `sha256`, quoted |
| `Accept-Ranges` | `bytes` |
| `Cache-Control` | `private, no-store` |

**The client verifies the bytes.** The length is the file's and the `ETag` is
the record's digest, so a download that arrives complete and hashes to something
else has found the file replaced behind the record.

**A broken download resumes.** To get the rest of a file from byte `N`, send
`Range: bytes=N-`. Also send `If-Range` with the `ETag` from the first response.
The server sends `206 Partial Content` with the bytes from `N` to the end. The
response has the headers above, with these changes:

| Header | Value |
|---|---|
| `Content-Length` | the size of the part: the file's size minus `N` |
| `Content-Range` | `bytes N-{last}/{size}`, where `{last}` is the size minus 1 |

The server sends the whole file with `200` in these cases:

- The `If-Range` is not the `ETag` byte for byte. A weak tag (`W/…`) and a date
  never match.
- The `Range` is a closed range (`bytes=0-99`).
- The `Range` is a suffix range (`bytes=-100`).
- The `Range` names more than one range.
- The `Range` has a unit that is not `bytes`.

A `Range` with no `If-Range` gets a `206`. The `ETag` is the digest of the file,
and it does not change. The `If-Range` only protects a client that kept a part
of a different file.

Every refusal happens **before the first byte**: the record is read and judged,
and only then is the file opened. A `Range` does not change a refusal.

| Status | Condition |
|--------|-----------|
| 403 | The caller's owner is not the record's owner; a device that is unapproved, lacks `perform_work`, or belongs to nobody |
| 404 | No such resource, or the record names a file that is no longer stored |
| 416 | `Range: bytes=N-` with `N` at or past the end of the file. `Content-Range: bytes */{size}` gives the size. Start again from byte 0 |

A missing file is a `404` with a logged `vault_object_missing` event: the record
outlives its bytes only if the file went missing from the store, which is an
operator's problem rather than the caller's.
