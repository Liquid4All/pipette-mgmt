# Organizations

An **organization** is a roster first. It holds no permission, and no fleet
read or job claim narrows because a caller belongs to one. What these routes do
is record three facts an operator currently keeps in their head, so a device
listing can be read by owner and an offboarding has something to enumerate.

[`pipette-mgmt orgs`](../cli.md) is the operator surface over the same records.

## What scopes by it

The bare roster grants below and `manage_clients`, the
device-administration grant ([device administration](devices.md)), are bounded by
the caller's own membership; the benchmarks an organization keeps to itself
([private benchmarks](#private-benchmarks)) are served to its principals alone; and an
organization owns campaigns ([campaigns](campaigns.md)) and vault resources
([the vault](vault.md)), which its members read. A campaign's plans go with the
campaign, so a member holding `read_plans` reads the plans of its
organization's campaigns. That is the whole of the tenancy in this system: the
accounts and devices on an organization's roster, and what the organization
owns.

## Who may do what

These grants govern the roster
([the vocabulary](users.md#permissions)). `manage_orgs`
also administers what the holder's organization owns, under
[private benchmarks](#private-benchmarks), [campaigns' who may do what](campaigns.md#who-may-do-what) and
[the vault's who may do what](vault.md#who-may-do-what):

| Grant | Opens |
|---|---|
| `read_orgs` | Every `GET` of this tag, except the audit log. The device inventory, filed under Devices, is the next row |
| `read_orgs` + `read_clients` | [`GET /api/v1/orgs/{org_id}/clients`](devices/list_org_clients.md), which returns whole client records, `GET /api/v1/clients?org_id=` ([`GET /api/v1/clients`](devices/list_clients.md)) and the device highlights of [`GET /api/v1/orgs/{org_id}/overview`](orgs/get_org_overview.md) |
| `manage_orgs` | Every write of this tag except creating an organization, and the membership route `PUT /api/v1/users/{user_id}/org`, plus the organization's audit log ([`GET /api/v1/orgs/{org_id}/audit`](orgs/list_org_audit.md)). Implies `read_orgs`, since administering a roster means reading it back |
| `manage_any_orgs` | The same for every organization, plus `POST /api/v1/orgs` and [`GET /api/v1/audit`](orgs/list_audit.md) |

**None is in the `org_member` permission group**: who exists and who belongs
where is an administrative question about other people. `org_admin` carries
`read_orgs` and `manage_orgs`, bounded by membership, and `server_admin`
carries `manage_any_orgs`, which implies the rest. An operator who needs
read-only roster access is granted `read_orgs` by name.

`read_orgs` is its own name rather than a fold into `read_clients`, because the
two answer different questions and one is far narrower: a roster says who exists
and who belongs where, not what any device is or does. That narrowness is a
promise the routes have to keep, which is why the routes returning device
records (`GET /api/v1/orgs/{org_id}/clients`, the `org_id` filter on `GET /api/v1/clients` and the
device highlights of `GET /api/v1/orgs/{org_id}/overview`) require a fleet grant as well.

**Every route takes an active user.** An organization is not a principal: it
holds no key and authenticates nothing, so there is no client-side surface here
at all.

The bare `read_orgs` and `manage_orgs` are **scoped to the caller's own
organization**: `GET /api/v1/orgs` lists that one organization, and any other
organization reads as `404` rather than `403`, so the roster cannot be mapped by
probing handles ([the rules that make delegation safe](users.md#the-rules-that-make-delegation-safe)).
`read_any_orgs` and `manage_any_orgs` are the server-wide forms, and creating an
organization lives only under the latter. No route deletes one: disable it
(`status`), or delete it on the host with the CLI's `orgs delete`.

## Either spelling of `{org_id}`

On a read, `{org_id}` is **either spelling**: the minted id a listing printed, or the handle an
operator typed. An `org_`-prefixed value is read as an id and anything else as a
handle, so `GET /api/v1/orgs/acme` and `GET /api/v1/orgs/org_0195c3e2-…` reach one record and nobody
has to look an id up before every read.

A mutation (`PATCH`, `PUT`, `POST`, `DELETE`) takes **the id only**: a handle there is a
`400`. A write names the record it changes by what never changes, so a handle
released and claimed by another organization cannot redirect it.

## Memberships and ownerships

The membership and ownership routes,
[`PUT /api/v1/users/{user_id}/org`](users/set_user_org.md) and
[`PUT /api/v1/clients/{client_id}/owner`](devices/set_client_owner.md), write
records of their own, not a key under `/api/v1/orgs`. They belong with the
organizations because both resolve the organization against the roster before
writing, so a membership or an ownership
can only ever name a record that exists. The first is a roster write under
`manage_orgs`; the second is a device write and is `manage_clients`. Neither
grants a permission, yet each moves what an account or a device reaches and who
administers it, so each is an authority write.

A `null` value clears the relation. That is why neither has a `DELETE` twin:
"belongs to nobody" is a value the field takes, not an absence of the relation.

The `404` for an organization or an account named in the *body* is the
load-bearing one. Resolving the target before writing is
what lets an absent `owner` be read as "belongs to no organization" rather than
"points at something gone", and it is why a membership can never name a record
that was never created.

A membership is always an operator's assignment: a registering machine names no
organization, for the same reason it does not approve itself. A self-registering
device likewise arrives unowned, and stays unowned until an operator vouches for
it. The exception is a device registering with a pre-auth key that names an
owner: the operator who minted the key vouched for it in advance.

## Private benchmarks

The benchmarks an organization
[keeps to itself](../benchmarks.md#11-private-benchmarks): the entries
`GET /benchmarks` serves the organization's own devices and members beside the
public catalog. Four routes keep them:
[`GET /api/v1/orgs/{org_id}/benchmarks`](orgs/list_org_benchmarks.md),
[`GET`](orgs/get_org_benchmark.md),
[`PUT`](orgs/put_org_benchmark.md) and
[`DELETE /api/v1/orgs/{org_id}/benchmarks/{benchmark_id}`](orgs/delete_org_benchmark.md).
No permission name of its own:

- The two `GET`s take **membership**. An active account belonging to the
  organization reads its catalog on no grant at all, the way its devices are
  served the entries; `read_any_orgs` reads every organization's.
- `PUT` and `DELETE` take `manage_orgs` **and** membership, or
  `manage_any_orgs`: a team's catalog is its administrators' to curate, the
  line the roster vocabulary already draws.

Another organization's catalog reads as `404` on every verb, as the roster
does, so it cannot be mapped by probing handles. The
[console app](../../ui/README.md#pages) draws these routes for a browser.

The response of the `PUT`, and the body of the two `GET`s, is the entry as
`GET /benchmarks/{benchmark_id}` will serve it to the organization, `org_id`
included, minus the `dataset.url` that route stamps.

## The audit log

Two routes read it:
[`GET /api/v1/orgs/{org_id}/audit`](orgs/list_org_audit.md), what changed in
one organization and who changed it, and
[`GET /api/v1/audit`](orgs/list_audit.md), every audit event.

Each request to a write route (any method but `GET`, `HEAD` and `OPTIONS`)
creates one audit event, for any answer. The audit log also records a refusal,
a failure, and a request whose caller left before the answer. The audit log
does not record these requests:

- A request that succeeded and changed nothing:
  - a dry run;
  - a check;
  - a claim that found no job;
  - a write that answers `changed: false`
    ([write answers](../httpapi.md#16-write-answers));
  - a cancel of a plan that is already cancelled or has a cancel in progress.
- A request that gets a `405`.
- `PUT /plans/{job_id}/heartbeat`. A device renews its lease at each half of
  the lease time. A renewal changes nothing that an operator audits.
- A request that names no caller, claims no id, and does not succeed.

The `status` of an event is the status of the answer. For a streamed answer, it
is the status of the last line. If the caller leaves first, the `status` is
`499`. If writes that the request started finish after that, the `status` is
the status of those writes. The `outcome` is `refused` for a `401`, a `403`, or
the `404` that hides a record outside the caller's scope.

The audit log also records the [`pipette-mgmt` commands](../cli.md#commands)
that change records, and, beside its request's own event, an account's first
Clerk sign-in when the account already held keys: a `first_clerk_sign_in`
action naming each of them
([Users: Signing in through Clerk](users.md#signing-in-through-clerk)). Only a
`[database]` keeps the audit log. Without a
`[database]`, the server records nothing and both routes answer `404`. The
`AuditEvent` schema in the OpenAPI document
([the OpenAPI document](../httpapi.md#15-the-openapi-document)) is the shape of
an event.

The audit log is best effort:

- The server writes an event after the request. For a plain answer, the server
  waits at most one second for the write, and then answers.
- If a write fails, the server logs the failure and the request keeps its
  answer.
- When the server stops, it waits at most ten seconds for the writes that are
  not complete. The server loses an event that it does not write in that time.

An event is in the log of each organization that it concerns. These are the
organization of a verified actor and the organization of each named record,
before and after the request. A device or an account that moves between
organizations is in both logs. An event with an unverified actor is in no
organization's log: the id it claims and the path it names are the caller's
choice. Only [`GET /api/v1/audit`](orgs/list_audit.md) lists it.

### Narrowing the log

`outcome`, `actor` and `target` narrow the audit log that the server reads:

- An outcome or an actor kind that is not in the vocabulary gives a `400`.
- A `target` that cannot be an id gives a `400`.
- The server reads each value that has the shape of an id as the id of a
  record. A device that registered before the `ev1_` prefix has an id with no
  prefix, so the server also accepts that shape.

### Names in the answer

With `names=true`, the answer names ([names](../httpapi.md#14-names)) each id
that the reader may see by another route:

- An acting account, by its address, as a plan names its submitter.
- A target account, only inside the roster of the reader.
- A device, with `read_clients`.
- A campaign or a vault resource, within the reach of the reader.

The answer never names a claimed id. The audit log keeps an event for
[two years](../operations.md#34-audit-log-retention).
