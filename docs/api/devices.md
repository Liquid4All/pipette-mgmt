# Device administration

The operator-side device lifecycle. [`pipette-mgmt clients`](../cli.md)
runs the same operations. Reading the fleet is `read_clients`
([`GET /api/v1/clients`](devices/list_clients.md),
[`GET /api/v1/clients/{client_id}`](devices/get_client.md) and
[`GET /api/v1/clients/capabilities`](devices/get_capability_vocabulary.md));
**writing** a device record is `manage_clients`.

**`manage_clients` is a trust boundary, not a convenience.** Approving a device
is the decision that an identity may authenticate and do work
([device status](#device-status)), and a signing key makes it here. That
is why the grant is its own name and is never implied by any read.

## The scope

`manage_clients` is **bounded by the caller's own organization**, the bare
verb's meaning throughout this vocabulary. `manage_any_clients` is the fleet,
and implies it.

| The device is | `manage_clients` | `manage_any_clients` |
|---|---|---|
| held by the caller's organization, directly or through a member | yes | yes |
| held by nobody, or by an account in no organization | **no** | yes |
| held by another organization | **no** | yes |

The same bound applies to where a device may be **put down**: a scoped caller
may assign only into its own organization, or detach, so a move can never widen
another roster. Taking a device out of one organization and into another is
`manage_any_clients` alone.

A scoped caller may name only **itself** as a device's personal owner. A device a
person owns fetches that person's private [vault](vault.md) artifacts, so handing
one to a colleague would hand over the colleague's weights; that is
`manage_any_clients` alone too.

**Unowned devices are a server admin's.** An organization onboards through its
own [client pre-auth keys](#client-pre-auth-keys), which
register a device straight into it. A device that registered without one waits
for `manage_any_clients`, so no organization can take, approve or delete
another tenant's registration.

An account holding `manage_clients` while belonging to **no** organization
administers nothing. That is a state, not an error: the grant can be given
before anyone assigns the account anywhere.

A refusal names the scope, never the organization that holds the device, so the
route cannot be used to map another roster's inventory one probe at a time.

It is separate from `manage_orgs` because the two decide different things about
the same record: ownership says which organization a device rolls up to and is
a roster decision; this says whether the device may act at all. An operator who
administers people need not be the one who admits machines. `manage_clients`
implies `read_clients`, since every operation is a read-modify-write of a record
the caller has to be able to see.

**There is no `POST /api/v1/clients` here.** A device creates itself at
[`POST /clients/register`](../httpapi.md#22-post-clientsregister) with a keypair only
it holds; an operator's create is minting the pre-auth key it registers with
([client pre-auth keys](#client-pre-auth-keys)). Nothing on this surface can bring a
device into existence.

## Fleet disclosure

Which devices exist, what each carries and which one ran a job are **fleet
facts**, and one right gates every surface that tells them: `read_clients`,
widened by `read_any_clients`. A surface left free would be the probe: file a
plan requiring one flag, read its warning, repeat.

`read_clients` reaches the devices the caller's plans can reach, and
`read_any_clients` the whole fleet. No per-submitter device restriction can be
recorded yet, so today both reach every device. A device out of reach reads as
one that does not exist: listings, counts, warnings and starvation reports
leave it out, and
[`GET /api/v1/clients/{client_id}`](devices/get_client.md) answers `404`.

| Surface | Without either grant |
|---|---|
| [`GET /api/v1/clients`](devices/list_clients.md), [`GET /api/v1/clients/{client_id}`](devices/get_client.md), [`GET /api/v1/clients/capabilities`](devices/get_capability_vocabulary.md) | `403`: every field is a fleet fact |
| The `warnings` of a filing route's report, and a plan's `warnings` and `progress_snapshot.starved_groups` | withheld tier |
| [`GET /api/v1/plans/{plan_id}/explain`](plans/explain_plan.md): the `fleet` standing, clause ladder, roster and live match counts | withheld tier |
| [`GET /api/v1/plans/{plan_id}/jobs`](plans/list_plan_jobs.md): lease holders and, with `results=true`, the device that ran each job and why it failed; the same in [`results.csv`](plans/get_plan_results_csv.md) | withheld tier |
| The plan document, which names client ids and requirement sets: [`/document`](plans/get_plan_document.md), [`/variants`](plans/get_plan_variants.md), a revision that starts from it ([variant edit](campaigns/edit_variants.md)), and a job body in [`GET /api/v1/jobs/{job_id}/plans`](plans/list_job_plans.md) | withheld whole; `403` where the document is the whole response |

**The withheld tier keeps the route and drops the fleet**, so a submitter can
still file a plan, read it and ask why it waits. It keeps the caller's own job
ids, its own job counts and fixed reason strings, and shows no requirement set,
no client id, no client count and no hint of which clause failed to match the
fleet. A filing report's `plan_warnings`, which judge the filed body alone and
quote its clauses, are served at both tiers. Each route's response names the
fields it drops. A document is withheld whole, from the
account that filed it too, because one with its values removed would not
round-trip into a revision. A job row's `planned` is served at both tiers: it
is the job's spec without its secrets, and names no client and no requirement
set.

Every tiered body carries `fleet_detail`, `"full"` or `"withheld"`, so a
withheld count reads apart from a zero one. What remains at the withheld tier
is one bit per requirement set, something matches or nothing does, which no
report of starvation can avoid.

## What a device row carries

A `ClientListRow` is the stored record plus what the server reads beside it.

| Field | Meaning |
|---|---|
| `status` | `pending` or `approved` ([device status](#device-status)) |
| `permissions` | The stored set, a name a device cannot hold included; `[]` when it holds none |
| `tags` | Assigned by an operator ([client tags](#client-tags)) |
| `derived_capabilities` | The flags the server derives from the `device_*` profile; with `capabilities` and `tags`, the set a job's clauses test ([capability matching](#capability-matching)) |
| `suspended` | Set when the device holds more than one live lease, as checked by claim, heartbeat and reclaim. A suspended device gets no job, and its heartbeats and reclaims are refused, so its leases lapse, until [unsuspend](devices/unsuspend_client.md) clears it |
| `reindex_pending` | The device takes no work until `queue-maintenance` re-evaluates its eligible jobs |
| `last_seen_at` | The device's last verified, signed request to any device route, a refused one included, to within about a minute; `null` when the server holds no stamp. Age never clears it |
| `backlog` | `job_count`, the queued jobs the device could claim (unleased, before their deadline, not denied by the device), as the `queue-maintenance` run that started at `counted_at` counted them; `null` before the first count or when the count cannot be read |
| `contact_email` | Present only for a caller that administers the device ([The scope](#the-scope)), wherever a user reads a device record, the organization's roster and overview included |

A `backlog` is at most one run old and ignores suspension. A device that is not
a worker counts zero, and one awaiting a reindex shows its count from before.
[`GET /api/v1/clients/{client_id}`](devices/get_client.md) adds
`backlog_by_campaign`: each campaign the caller may read with the distinct jobs
its plans hold (a job two campaigns hold counts in each, so the counts can sum
past `backlog`), `outside_campaign_job_count` for jobs no campaign holds, and
`unreadable_job_count` for the jobs of every other campaign. A campaign holds a
job when one of its plans has a current membership for it and is not being torn
down; `unreadable_job_count` also counts the jobs of a campaign whose record is
gone.

## Device status

A device is `pending` or `approved`; there is no rejected or disabled state.
`POST /clients/register` creates it `pending`, or `approved` when it presents a
[client pre-auth key](#client-pre-auth-keys) or the server's auto-approve rule
admits its self-reported `contact_email`.
[Approving](devices/approve_client.md) admits it,
[rejecting](devices/reject_client.md) a pending one deletes it and frees its
keypair, and [`DELETE`](devices/delete_client.md) removes any device.

Status and permissions are independent. A pending device carries its set from
registration, and the set takes effect the moment it is approved: nothing is
re-granted. A device's request is checked in order, the first two steps from
its headers alone:

1. Authentication: `401`.
2. Status: a pending device gets `403` `client is not approved` on the queue,
   corpus and vault routes, and on the two submission routes unless the server
   keeps unverified submissions, which holds its results instead.
   `GET`/`PATCH /clients/me`, `GET /jobs/{job_id}` and its eval sample results,
   and the catalog take no status check, so a pending device reads and updates
   its own record.
3. Permission: `403` `client lacks the {permission} permission`. On the
   submission routes this step reads the body: a result that names a claimed
   `job_id` needs `perform_work`, any other `submit_results`.

The scheduler asks the same two questions. A plan's fleet match counts only the
**working fleet**, approved devices holding `perform_work`, so a plan that only
other devices match is `pending_clients`, and
[explain](plans/explain_plan.md) names the gate that dropped them. A grant or a
revocation shows on the next pass and the device's next claim.

## Client pre-auth keys

The routes under `/api/v1/orgs/{org_id}/client-preauth-keys`
([mint](devices/mint_client_preauth_key.md),
[list](devices/list_client_preauth_keys.md),
[revoke](devices/revoke_client_preauth_key.md)).

A client pre-auth key is the [pre-auth key](../authentication.md#32-pre-auth-keys)
a device registers with, minted for one organization by an administrator of
its devices. A device that
presents it at [`POST /clients/register`](../httpapi.md#22-post-clientsregister) registers **approved and owned
by the organization**: the two decisions `manage_clients` otherwise makes one
device at a time, made in advance. It is also the only way an organization
brings a device online by itself under `require_client_preauth_key = true`.
The host's `pipette-mgmt clients preauth` mints and lists the same records,
with none of the bounds below.

The mint, list and revoke routes take the `{org_id}` an assignment may put a
device into
([`PUT /api/v1/orgs/{org_id}/clients`](devices/assign_clients_to_org.md)):
the caller's own organization, or any under `manage_any_clients`. Any other
organization answers the same `404` as one that does not exist.

**Registering with one.** The device sends the token as `client_preauth_key` in
the `POST /clients/register` body. It reads `client_preauth_{key_id}.{secret}`,
and the server keeps only the secret's `sha256`. A malformed, unknown, revoked,
spent or expired key, or one whose owner no longer exists, is `401` and creates
no device; the causes read alike, so the route maps no keys. A key whose minter
no longer administers the organization is `403`
([mint](devices/mint_client_preauth_key.md)). Under
`require_client_preauth_key = true`, a registration without a key is `403`. A
keypair already registered gets its record back (`200`) before any of these
checks: the key is not read or spent, and the body is not validated. A
`single_use` key registers exactly one device, simultaneous attempts included;
a `multi_use` key registers any number until it expires or is revoked.

## Device permissions

The routes under `/api/v1/clients/{client_id}/permissions`
([`PATCH`](devices/amend_client_permissions.md) amends,
[`PUT`](devices/set_client_permissions.md) replaces).

What a device may do once it is approved
([permissions](users.md#permissions)): `submit_results`,
`perform_work`, both or neither.
[`pipette-mgmt clients permission`](../cli.md#pipette-mgmt-clients-permission-addremovesetlist)
writes the same object.
The scope is the other writes' ([The scope](#the-scope)): a device the
caller's organization holds, or any under `manage_any_clients`, so every worker
an organization vouches for is on its roster.

**`perform_work` reaches past the organization.** The queue is shared: a device
holding it claims any job whose requirements it matches, whichever
organization's campaign filed it, and reports that job's result. What it
fetches stays bounded by its owner ([the vault](vault.md)) and by the
organization it rolls up to (a private benchmark's corpus,
[`/api/v1/orgs/{org_id}/benchmarks`](orgs/put_org_benchmark.md)). Granting it vouches for a machine to the whole
server, as approving one does.

Both respond `200` with `ChangedClientPermissions` ([the OpenAPI document](../httpapi.md#15-the-openapi-document)),
a write answer ([write answers](../httpapi.md#16-write-answers)) whose no-op is a set the device
already held. A write takes effect on the device's next request, and a pending
device carries its set until it is approved
([device status](#device-status)).

**Defaults.** A set is written when the device registers, and nothing is
inferred at read time. A registration holds `submit_results` alone, which is
what a device that benchmarks itself and submits needs. A key minted here
carries no template, so its devices hold that default too; a host-minted key
may carry a template, an empty one included, and its devices hold exactly that.
`perform_work` is never a default: grant it once a lab device has registered. A
device with no permissions object holds nothing until it is granted.

A name a device cannot hold, any user permission among them, is a `400`. A
device reads its own set at `GET /clients/me`, and cannot write it: a
`permissions` key on `POST /clients/register` or `PATCH /clients/me` is a
`400`.

## Client tags

A device carries any number of flat tags (`team-mobile`, `us-east`). A plan
targets one as the flag `tag:<tag>` ([capability matching](#capability-matching)),
and [`GET /api/v1/clients`](devices/list_clients.md) filters by them. **Only an
operator assigns them**, on the host (`pipette-mgmt clients tag`) or through a
host-minted pre-auth key: no route here writes one, and a key minted here
carries none. A tag decides which work a device is offered, so the device
cannot set its own: `POST /clients/register` and `PATCH /clients/me` ignore a
`tags` key and refuse a `tag:` capability flag.

A tag is not the fleet. Its members are whoever an operator assigned, not
necessarily the devices that take work, so read the working fleet from
`status` and `permissions` rather than from a tag.

## Capability matching

A job reaches a device by **set containment** over the device's effective
capability set, which joins:

- the flags the server derives from the `device_*` profile, in these reserved
  namespaces:
  - `os`, `os_version`, `arch`, `os_family`
  - `device`, `chip`, `form_factor`, `ram_bytes`
  - `gpu`, `gpu_vram_bytes`, `npu`, `npu_vram_bytes`

  The server lowercases a string value and removes its whitespace
  (`iPhone 17 Pro` is `device:iphone17pro`). It writes a byte count as its
  exact decimal value;
- the flags the device reports, such as `runtime:llama_cpp` or
  `client_version:<v>`;
- its tags, as `tag:<tag>`.

A device cannot report a reserved or a `tag:` flag, and what it reports must be
canonical (lowercase, no whitespace). Reported flags are the device's word, not
verified: matching is a scheduling constraint, not hardware authentication.
Each flag compares as a whole string, so `runtime:llama_cpp:b9999` and
`runtime:llama_cpp` are two flags, and a device advertises every level it
supports.

| Clause | Shape | Effect on a device |
|---|---|---|
| `clients` | client ids | eligible when listed, whatever its flags |
| `requires` | flags | eligible when it carries every one |
| `any_of` | groups of flags | on the flag path, eligible when it also carries a flag of each group |
| `exclude_clients` | client ids | ineligible when listed |
| `excludes` | flags | ineligible when it carries any one |
| `not_all_of` | groups of flags | ineligible when it carries every flag of one group |

A device is eligible when it is listed in `clients`, or carries all of
`requires` and a flag of every `any_of` group, **and no exclusion applies**: an
exclusion beats the roster. An empty or absent `requires` matches nobody
through flags, so only `clients` names a device then; the fleet listing's
capability filters follow the same rule. A malformed clause is refused at
filing. In a stored job, a malformed `requires`, `excludes`, `exclude_clients`,
`any_of` or `not_all_of` makes the job match nobody, the roster included, and a
malformed `clients` lists nobody but leaves the flag path working.

`excludes: []` and `exclude_clients: []` exclude nobody. `not_all_of: [[]]`
excludes everybody, the roster included, since every set contains the empty
group: the mirror of `any_of: [[]]`, which matches nobody through flags. A
device whose set is empty (no profile, no reported flags, no tags) matches
through `clients` alone.

- [`GET /api/v1/clients/capabilities`](devices/get_capability_vocabulary.md)
  lists the effective flags the fleet carries, with how many devices carry
  each. Write `requires` from it: a flag no device carries matches nobody.
- A body that matches nobody **today** is a filing warning, since a device can
  register tomorrow. One that no possible device can match is refused: a
  `not_all_of` group is empty, or the roster is dead (`clients` minus
  `exclude_clients` is empty) and so is the flag path, because `requires` is
  empty or absent, `excludes` names a flag `requires` demands, a `not_all_of`
  group holds only flags `requires` demands, `excludes` covers every member of
  an `any_of` group, or an `any_of` group is empty. So is a `not_all_of` group
  naming two flags of one reserved namespace, which excludes nobody. The rules
  are sound but not complete: a contradiction they miss is accepted and matches
  nobody. The test reads the body alone, so a refusal says nothing about the
  fleet.
- [Explain](plans/explain_plan.md) narrows the working fleet one clause at a
  time and names the clause that leaves no device, at the full tier
  ([fleet disclosure](#fleet-disclosure)).
