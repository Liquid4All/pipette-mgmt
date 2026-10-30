# Authentication

This doc covers request signing, the two kinds of identity, the permission
model, signing in from a browser, and what client tags mean for permissions.

## 1. Identity model

Two kinds of principal authenticate here, both with Ed25519 keypairs. The
private key is the only secret, similar to WireGuard or SSH. There are no
tokens, sessions, or passwords.

A **client** (`ev1_…`) is one device: one key, and an id derived from it.
A **user** (`user_…`) is one operator or service job: a minted id and a set of
registered keys, one per machine ([§5.11](#511-users)). This section describes the
client; the differences are all in §5.11.

- **Private key**: stored on the client only. Never sent to the server.
- **Public key**: registered with the server once at `POST /clients/register`.
- **`client_id`**: derived from the public key: `ev1_` + hex(SHA-256(public_key_bytes)).
  This is the stable, human-readable identifier for the client. The `ev1_` prefix
  reads "edge evals 1"; its job is to make an id recognizable at a glance as
  belonging to this system, the way `ghp_` does for GitHub tokens.

  **The digit is not a version mechanism.** Nothing reads it, nothing branches on
  it, and no storage key is scoped by it. The digest is the identity, so
  replacing the hash would give every client in the fleet a new `client_id` and
  a new record: a re-registration, not a migration. Treat the prefix as a
  label, not a version.

Key loss means a new identity. There is no key recovery, and a client holds
exactly one key, which is precisely why an operator identity is a user and not
a client: a person's machine gets re-imaged, and the plans they filed must
survive it.

Re-registering under a new key leaves the old record in place with whatever it
was granted, so a re-imaged or retired host's old identity can still hold
`perform_work`. Delete it with `pipette-mgmt clients delete`.

## 2. Request authentication

Every authenticated request includes four headers:

| Header | Value |
|--------|-------|
| `X-Client-Id` | The client's `client_id` (e.g. `ev1_a3f8...`) |
| `X-Timestamp` | Current UTC time as ISO 8601 (e.g. `2026-03-10T12:00:00Z`) |
| `X-Nonce` | A value unique to this request. 32 hex characters from a CSPRNG is the recommended form |
| `X-Signature` | Ed25519 signature over the signed payload below, hex-encoded |

### 2.1. The signed payload

The client signs the UTF-8 bytes of six newline-separated fields:

```
v1\n{method}\n{path_and_query}\n{timestamp}\n{client_id}\n{nonce}
```

| Field | Value |
|-------|-------|
| `v1` | Literal scheme tag |
| `{method}` | The request's HTTP method, byte-identical to what the server receives (e.g. `GET`, `HEAD`, `POST`) |
| `{path_and_query}` | Request target including the query string, e.g. `/jobs/job-abc` or `/clients/me?page=2` |
| `{timestamp}` | Byte-identical to the `X-Timestamp` header |
| `{client_id}` | Byte-identical to the `X-Client-Id` header |
| `{nonce}` | Byte-identical to the `X-Nonce` header |

For a `GET /clients/me` at `2026-03-10T12:00:00Z`, the signed bytes are:

```
v1
GET
/clients/me
2026-03-10T12:00:00Z
ev1_a3f8...
9f86d081884c7d659a2feaa0c55ad015
```

Covering the method, path, and query binds a signature to one request *shape*:
it does not carry over to another endpoint, another method, or the same endpoint
with different query parameters. The nonce narrows that to a single request, so
a captured signature is worth nothing even inside its freshness window. See
[§2.2](#22-replay-protection). The newline delimiters keep the field boundaries
unambiguous whatever a field contains, and the `v1` tag lets a future scheme be
served alongside this one during a migration.

Each request needs its own nonce. Reusing one across two requests makes their
signatures identical whenever the rest of the payload matches, and the server
rejects the second as a replay.

The payload does **not** cover the request body. Two requests differing only in
body are interchangeable to the verifier for as long as the timestamp stays
fresh.

The payload also does **not** cover a request header other than `X-Client-Id`,
`X-Timestamp`, and `X-Nonce`. `If-None-Match`, `Range`, and `If-Range` are
examples. A change to one of them only changes how much of the response the
server sends. The signer can already read the whole response. The nonce still
limits the signature to one request.

`{path_and_query}` is the request target as the server receives it, signed
byte-for-byte with no normalization. A proxy in front of the server that
rewrites request targets (percent-decoding, merging `//`, resolving dot
segments) leaves every `v1` signature unverifiable, so either keep target
rewriting off the request path or have clients sign the rewritten form.

#### The host-bound payload (`v3`)

`v1` covers no host, so a request signed for one deployment verifies at any
other deployment where the same key is registered, within its window: each
process keeps its own spent-signature record (§2.2). `v3` is `v1` with the
authority added, under the same four headers:

```
v3\n{method}\n{authority}\n{path_and_query}\n{timestamp}\n{client_id}\n{nonce}
```

`{authority}` is the `Host` the request is sent to (`host`, or `host:port` off
the default port), lowercased: the value RFC 9421's `@authority` takes (§2.4).
It must be one of the config's `authorities`, matched case-insensitively, or
the request is `401`; an empty `authorities` refuses every `v3` signature. The
server tries `v3` before `v1`, so a client moves when it is ready, and
everything this section, §2.2 and §2.3 say of `v1` holds for `v3`.

A `v3` signature never verifies as `v1`, so a device that signs `v3` is safe
from a replay at another deployment now. `v1` stays accepted for the devices
in the field, and a device that still signs `v1` stays exposed until it moves.

The server:

1. Looks up the public key by `X-Client-Id`.
2. Rejects the request if the timestamp is outside a **5-minute window** from
   server time, in either direction.
3. Verifies `X-Signature` over the signed payload using the stored public key,
   under strict verification: the signature's `R` and the client's public key
   must both be points of full order. A small-order public key admits a
   signature that verifies against every message, so a client registered with
   one could be signed for by anyone; strictness is what ties a client's
   identity to possession of its private key. Keys produced by any standard
   Ed25519 implementation satisfy this.
4. Rejects the request if that signature has already been spent
   ([§2.2](#22-replay-protection)).

A request that omits `X-Nonce` cannot produce the payload above, so it is
rejected with `missing X-Nonce header` unless it verifies as a timestamp-only
signature ([§2.3](#23-timestamp-only-signatures)).

The server logs each of these refusals at `warn`, under the `client_id` the
request named, so a device that cannot authenticate, a skewed clock included,
shows up in the server's log and not only in its own.

The routes that take no signature are listed once, in
[httpapi.md §1](httpapi.md#1-authentication). The catalog routes among them
also accept one ([§4](#4-access-matrix)).

### 2.2. Replay protection

Each `v1` signature authenticates one request. The server remembers the
signatures it has accepted and rejects any repeat with `401` and `signature
already used`, so capturing a request off the wire and resending it achieves
nothing.

This covers `v1` signatures only. A timestamp-only signature
([§2.3](#23-timestamp-only-signatures)) carries no nonce, so one such signature
is shared by every request that client sends in the same second; the server does
not track them, and they stay replayable within the freshness window.

A signature is remembered until its timestamp leaves the 5-minute window, which
is the moment the freshness check starts rejecting it anyway, so there is no
gap between a signature being forgotten and it becoming unusable. Expired
records are cleared in periodic sweeps rather than the instant they expire, so
memory is bounded by the authenticated request rate over roughly six minutes. A
request whose timestamp the latest sweep has already aged out is refused with
`timestamp expired`, even when its own freshness check ran a moment earlier and
passed.

Two consequences for clients:

- **Retries need a fresh nonce.** Resending a request byte-for-byte after a
  timeout is indistinguishable from a replay and is rejected. Re-sign the retry.
- **The record is per process.** A multi-instance deployment protects each
  instance, not the fleet; a signature spent on one instance is unspent on
  another. Closing that requires shared state and is not implemented.

### 2.3. Timestamp-only signatures

While `accept_legacy_signatures = true` (the default, see
[cli.md](cli.md#configuration-file)), a signature over the bare `X-Timestamp`
value is accepted whenever the `v1` payload fails to verify. This lets clients
migrate to `v1` on their own schedule instead of in lockstep with a server
upgrade.

Such a signature binds nothing but freshness: anyone who captures one can replay
it against any authenticated endpoint until it expires. It is a migration aid,
not a supported mode.

**The fallback is withdrawn per client, automatically.** The first time a client
presents a verified `v1` signature, the server records that fact in the auth
store ([storage.md §3](storage.md#3-metadata-contract)). Every request that
begins after that record lands is refused the timestamp-only payload, whatever
the setting says, so every signature captured from that client before that
point authenticates nothing.

The boundary is one authentication check wide: a timestamp-only request that
read the store just before the record landed is judged on what it read and may
still be accepted. It closes on a payload that was replayable throughout the
window it covers, so nothing is reachable there that was not reachable a moment
earlier.

Two properties make this safe to do without operator involvement. Setting the
record requires a **verified** `v1` signature, so only a party holding the
client's private key can trigger it. And it is never cleared automatically, so
nobody can push a migrated client back onto the replayable payload. The exposure
therefore shrinks on its own, and shrinks exactly where capture-and-replay is
possible: a client that sends no traffic keeps its permission indefinitely, but
produces no signatures to capture.

A client whose software is rolled back to a `v1`-unaware build is locked out by
this, which is the same property working as intended. Recovery is to register
afresh: `client_id` derives from the public key, so a new keypair is a new
client. The server logs a refusal, every acceptance (so the log names the
clients still to migrate), and a record it could not write:

```
WARN client_id=ev1_a3f8... method=GET path=/clients/me refused timestamp-only signature from a migrated client
WARN client_id=ev1_a3f8... method=GET path=/clients/me accepted timestamp-only signature
WARN client_id=ev1_a3f8... error=... failed to record signature migration
```

On a failed write the request still succeeds: it authenticated correctly, and failing it
over a bookkeeping write would deny a client that did everything right. An
isolated occurrence is picked up by that client's next `v1` request. A sustained
stream means something else: the ratchet is engaging for nobody. A store that
serves reads but rejects these writes leaves every client holding its fallback
and every captured timestamp-only signature replayable, while the `Migrated`
column stays empty and reads as "no client has migrated yet". This line is
worth an alert rather than a dashboard.

`pipette-mgmt clients list` reports the same state as a `Migrated` column, which
is the more reliable way to read progress. A client that has migrated but sends
no traffic produces no log lines either way. Once every client shows a date, set
`accept_legacy_signatures = false`; the migration records can then be deleted,
since the setting alone refuses the payload for everyone. `v1` signatures are
unaffected by the setting, so no client-side change accompanies the flip.

### 2.4. RFC 9421 signatures (user accounts)

A user account may sign a request with RFC 9421 HTTP Message Signatures
instead of `X-Signature`, wherever a user signature is accepted: every
`/api/v1` route, and the three root reads a user shares with devices
(`GET /benchmarks` and the vault resource and content reads). Devices keep
`v1`. The two schemes run side by side: `v2` keeps working for now, and the
server logs `user signs v2` once per key and process, which names the callers
still to move. `v2` covers no authority, so it replays against another
deployment that holds the same key; once those lines stop, set
`accept_v2_signatures = false` (the default is `true`) and every user request
is held to `authorities`.

The server reads **one profile** and refuses anything outside it, so a caller
cannot drift into a signature it reads differently from the library that made
it (`crates/pipette-httpsig/src/lib.rs`, `ui/src/lib/httpsig.ts`).

| Part | Value |
|---|---|
| Algorithm | Ed25519 over the RFC 9421 signature base, `alg="ed25519"` |
| Covered components | `"@method" "@authority" "@path" "@query"`, in any order, plus `"content-digest"` when the request has a body. No other component, and no component parameters |
| Parameters | `created` (Unix seconds, within the 5-minute window of §2.2), `nonce` (1 to 256 characters, unique per request), `keyid` (the key's `userkey_…` id), `alg="ed25519"`, `tag="pipette"`; `expires` is optional. No other parameter |
| Labels | One signature per request, under any label, the same in `Signature-Input` and `Signature`, each field on one header line |
| Body | RFC 9530 `Content-Digest` with a single `sha-256` member |

```
Content-Digest: sha-256=:Dmcct3wNRuKmSGlN9J7lDIw9DvE0Xp6DUXXIZHWJ65k=:
Signature-Input: sig=("@method" "@authority" "@path" "@query" "content-digest");created=1760000000;nonce="4f1c0de5a9b7";keyid="userkey_b16c2d1b…";alg="ed25519";tag="pipette"
Signature: sig=:xErs0vVcglQO4jeofP53oNks9pyROP4E0OOttcbMlHt5CAanP85tQV21Sy4jOZEzJg3Bm9gRrGXvqDssOFy+Bg==:
```

That request is `POST /api/v1/orgs?dry_run=true` to `mgmt.example` with the body
`{"slug":"acme"}`, signed by RFC 9421's `test-key-ed25519` (Appendix B.1.4).

**The key names the account.** `keyid` is looked up in the key claim index
([storage.md §3.1.3](storage.md#313-keys-user-keys-and-the-claim-that-indexes-them)),
so the request carries no `X-Client-Id`, and a caller needs only its private
key and the base URL: the key id is `userkey_` plus the SHA-256 of the public
key. A request carrying an RFC 9421 signature and any of `X-Client-Id`,
`X-User-Key-Id` or `X-Signature` is refused, so no header names a principal the
signature did not. A device's key names no account and is `401 key not found`.

**`@authority`** is the `Host` the request arrived with, lowercased; a proxy in
front must keep it, as the load balancer and the app's dev proxy do. It must be
one of the config's `authorities`, matched case-insensitively, or the request
is `401`, so a request signed for another deployment that holds the same key
cannot be replayed here. An empty `authorities` refuses every RFC 9421
signature.
**`@query`** is `?` and the query as sent, or `?` alone.

**The body is held to its digest.** A request whose headers declare a body
must cover `content-digest`, or it is `401`. The body is then hashed and one
that is not the signed digest is refused with `400` before parsing; a vault
upload, which streams, refuses a signed digest other than its path's
`{sha256}` before reading anything. A signature that covers no digest vouches
for an empty body, so a body that arrives anyway, whatever its framing
declared, is refused the same way.

Freshness, the spent-signature record (§2.2), strict verification, the key's
expiry and the account's standing are exactly `v2`'s (§5.11).

### 2.5. Calling the API yourself

[Users: Calling the API yourself](api/users.md#calling-the-api-yourself) walks
through it with nothing but OpenSSL: make a key, register it, sign each request
in the profile of §2.4, and read what the account may do. It lives there so the
API reference at `/docs/` carries it too. The repository also ships signers,
[tools/sign.py](../tools/sign.py) and [tools/sign.ts](../tools/sign.ts), which
can sign with a key in an ssh-agent or 1Password as well.

## 3. Registration

New clients register via `POST /clients/register` (unauthenticated). The client
either provides its own public key or asks the server to generate a keypair.
Both paths return `client_id` and `status: "pending"`. See
[httpapi.md §2.2](httpapi.md#22-post-clientsregister) for the full request/response
spec.

### 3.1. Auto-approve rules

> **⚠️ This feature is not a security control.** `contact_email` is
> self-reported and never verified. Anyone can register with any address.
> An attacker who knows an allowed address or domain can auto-approve
> themselves at will. Auto-approve only saves an operator a manual
> `clients approve` step for trusted, low-risk deployments; it must not be
> relied on to keep anyone out. When approval actually matters, leave it off
> and approve clients manually.

The server can be configured to approve a client immediately at registration
when its `contact_email` matches an allow rule. Rules live under
`[auto_approve]` in the server config (see
[operations.md §1](operations.md#1-configuration)):

```toml
[auto_approve]
# Full addresses — case-insensitive exact match.
emails = ["alice@example.com"]
# Domains — case-insensitive match on the part after `@`.
domains = ["example.org"]
```

Both lists default to empty, so auto-approve is off until configured: every
new client starts `pending`. Matching is case-insensitive; an email matches if
it equals any entry in `emails` or its domain equals any entry in `domains`.
It is evaluated **once, at registration**: changing the config does not
retroactively promote already-registered `pending` clients.

There is no auto-approve for users: nothing here sends mail, so an address is
self-reported on that side too, and an account requested with one comes up
`pending` for an operator to admit ([§5.11](#511-users)). A Clerk sign-in is the
exception, because Clerk verified the address
([§5.12](#signing-in-through-clerk)).

### 3.2. Pre-auth keys

An operator mints a **pre-auth key** and hands it to whoever is bringing a
client online.
[Device administration: Client pre-auth keys](api/devices.md#client-pre-auth-keys)
states what a device presents, what it registers as, and every refusal; this
section records the mechanism and why it is shaped so.

A host-minted key can seed the client with the key's tags, organization label,
owner, and permissions ([§5.3](#53-defaults)). The owner is what puts the
device in an organization
([cli.md](cli.md#pipette-mgmt-clients-preauth-createlistrevokeprune)); minting
the key is the operator's vouching for it. A key an organization's
administrator mints over HTTP seeds the owner alone. Unlike auto-approve, this
**is** a real gate: the key is a high-entropy secret, so a deployment can
require a key for every registration (`require_client_preauth_key = true`).
A user's keys are the same mechanism over its own keyspace, with
`require_user_preauth_key` closing the keyless request for an account
([§5.11](#511-users)). Two settings rather than
one, because they are two decisions: a deployment that mints every operator
identity by hand may still want devices to self-register, and the reverse.

- **Token**: `client_preauth_{key_id}.{secret}`. The older `preauth_{key_id}.{secret}`
  token and `preauth_key` request field are still accepted, so a client holding
  an older key still registers; both field spellings in one body are a
  duplicate-field `400`. The server stores only `sha256(secret)`
  ([storage.md §3](storage.md#3-metadata-contract)); the full token is shown once
  at `clients preauth create` and never again. A user's key carries
  `user_preauth_` instead, so a token presented at the wrong route is refused by
  the grammar
  ([storage.md §3.1.5](storage.md#315-permissions-and-pre-auth-each-kinds-own)).
- **Lifecycle**: single-use or multi-use, expiring after 90 days unless the
  operator sets another window or none
  ([cli.md](cli.md#pipette-mgmt-clients-preauth-createlistrevokeprune)). The
  record is write-once: the only mutation after creation is *deletion*. A
  single-use key deletes itself when spent, `revoke` deletes on demand, and
  `prune` deletes expired keys. A multi-use key is never mutated on consume.
  Single-use is **exactly-once, and holds against simultaneous
  registrations**: spending a key first creates a spent marker with an
  exclusive create, so one of any number of concurrent attempts wins and the
  rest are rejected as unknown. The marker is what holds the key spent, so a
  spend interrupted before the record was deleted still leaves the key
  unusable.
- **Rejection**: an unknown key, a wrong secret and a spent or pruned key all
  read as "invalid", so the endpoint is not an enumeration oracle. A key whose
  owner was deleted is refused with its own message after the consume, when
  the caller has already shown it holds the secret.

## 4. Access matrix

Which route requires what is stated per route: in each chapter's route table in
[httpapi.md](httpapi.md), and as the security roles of the OpenAPI document
([httpapi.md §1.5](httpapi.md#15-the-openapi-document)). This section states the
rules every route follows.

**The catalog is public.** Anyone can browse `GET /benchmarks` and
`GET /benchmarks/{benchmark_id}`, or their `/api/v2` twins, without
authentication. The one thing a signature adds there is the entries **private
to the signer's organization**
([benchmarks.md §1.1](benchmarks.md#11-private-benchmarks)), served to an
approved client or an active user of it; a pending principal is served the
public entries, not refused. A request that presents `X-Client-Id` is held to
it: a signature that fails is a `401`, never the public view. Every route not
listed in [httpapi.md §1](httpapi.md#1-authentication) requires an
authenticated principal.

**Status first, then permission.** Two attributes of the principal decide what
it may do: its **status**, which says whether an operator has vetted the
identity, and its **permissions** ([§5](#5-permissions)), which say which
operations it may perform.
[Users: Status, then permission](api/users.md#status-then-permission) states
the order a route checks them in and each refusal; the two refusals never mix
([§5.4](#54-status-and-permissions)).

**Each route admits one kind.** Each permission is grantable to exactly one
principal kind ([§5.2](#52-the-vocabulary)), so a route that requires one
admits only that kind, and no principal can hold the rights of both. The work
and submission routes serve clients; the plan, fleet, roster and administration
routes serve users ([Users: Permissions](api/users.md#permissions)). The rights
that need no permission at all are listed there for an account; a client reads
its own record and jobs.

**A pending client's submission is held.**\* A pending client that posts to
`POST /benchmarks` or `POST /benchmarks/batch` is held rather than refused.

\* Only when `[unverified_submissions] enabled = true`; otherwise a pending
client's submission is rejected with `403`. Held submissions are written to a
write-only archive partitioned by `client_id`
([storage.md §4.1](storage.md#41-unverified-submissions),
[httpapi.md §2.7.4](httpapi.md#274-unverified-held-submissions)) and are never
scored; the warehouse never sees them. The server returns a `job_id` as a
receipt, but `GET /jobs/{job_id}` does not resolve it. An operator can later
promote a client's held submissions into the normal pipeline or delete them.
The hold applies to a pending client only. An approved client without
`submit_results` or `perform_work` is refused with `403` and nothing is written
([§5.4](#54-status-and-permissions)).

New clients start as `pending`, and an admin approves them out-of-band via
`pipette-mgmt clients approve <client_id>` (see [cli.md](cli.md)), which
prevents abuse and DDoS from unapproved submitters
([Device administration: Device status](api/devices.md#device-status)). A user
has an equivalent gate: an account a machine requests for itself is `pending`
until `users approve` ([Users: Accounts and keys](api/users.md#accounts-and-keys)),
and `require_user_preauth_key = true` closes that path ([§5.11](#511-users)).

The host commands (`campaigns …`, `vault …`, and the maintenance passes) run on
the server host with direct storage access, where shell access to the storage
root is already the authority.

## 5. Permissions

A **permission** names one operation at one scope. A principal holds a set of
permissions. The set decides which routes the principal may call once its
status allows it.

### 5.1. The model

[Users: Permissions](api/users.md#permissions) states the model a caller sees:
a set of names per principal, its wire form, the scope of a bare name and its
`_any_` form, and what each name implies. This section records why it is
shaped so.

- **The term is `permission`.** `capability` names the device and job-matching
  flags ([plan-ingestion.md §5](plan-ingestion.md#5-capability-matching)) and is not
  reused here. A permission is an authorization; a capability is a fact about a
  device.
- **A principal holds permissions.** A principal is an identity the server can
  attribute a request to. There are two kinds: a **client** (`ev1_…`), which is
  one device doing the work, and a **user** (`user_…`), which is one operator or
  service job authoring it ([§5.11](#511-users)). The wire form and the storage
  shape are the same for both. The vocabulary is *shared but partitioned*. Each
  name belongs to one kind ([§5.2](#52-the-vocabulary)).
- **A set, not a role.** A principal holds any subset of the names its kind
  admits. There are no named bundles: a role would make "what does this
  principal hold" a two-hop lookup that a binary predating the role can resolve
  differently from one that follows it, which is exactly the property
  [§5.9](#59-growth) buys. Provisioning several names at once is what a
  permission group does for a user, expanded where it is given
  ([cli.md](cli.md#pipette-mgmt-users-permission-addremovesetlist)), and what the template on a client pre-auth key does
  for a device ([§5.3](#53-defaults)). `permission add --tag` applies one
  grant to a fleet ([§5.6](#56-assignment)).
- **Scope follows the campaign, not the filer.** A plan has no owner; its
  campaign does ([Campaigns: Who may do what](api/campaigns.md#who-may-do-what)).
  The filer is recorded for attribution, and grants scope only over a plan
  filed outside every campaign ([§5.11](#511-users)).
- **Implication.** A name includes narrower names, and nothing else is
  implied. The table in [Users: Permissions](api/users.md#permissions) is held
  by a test to `UserPermission::implied`, and
  [topology.md §5](topology.md#5-permissions) draws the lattice.
  `perform_work` and `submit_results` are independent.
- **Descriptor catalog.** `edit_any_catalog` implies `edit_catalog`. Neither
  name changes a global entry. `edit_catalog` is an authoring right inside one
  organization, and a service account may hold it. `edit_any_catalog` reaches
  every organization, and only a human account may hold it. A member reads the
  entries of its organization without either name. A holder of
  `edit_any_catalog` reads the entries of every organization, also when it
  belongs to an organization.

### 5.2. The vocabulary

Each name belongs to **exactly one principal kind**: a name grantable to a
client is not grantable to a user, and the reverse.
[Users: What each name opens](api/users.md#what-each-name-opens) lists every
name, its kind, and what it opens, and each route's requirement is its security
role in the OpenAPI document ([§4](#4-access-matrix)). This section records
why the vocabulary splits where it does and how the partition is enforced.

**Why the names split where they do.**

- `read_orgs` is one name for organizations and accounts, because they are one
  thing to read. It is not folded into `read_clients`, because a roster says who
  exists and who belongs where, not what any device is or does.
- `read_any_orgs` deliberately does **not** reach the vault: private artifacts
  are not roster data ([Vault](api/vault.md)).
- `manage_orgs` is one name for an organization, its people, its vault
  resources and its private benchmarks, because holding a team's private
  artifacts and definitions is the same authority as holding its roster.
  Reading and uploading the artifacts is the members', on membership alone, as
  filing into the organization's campaigns is; deleting an artifact and
  renaming, handing over or deleting a campaign is this grant's
  ([Campaigns: Who may do what](api/campaigns.md#who-may-do-what)). An account's **own** vault
  resources need no grant at all, the way its own keys do not. It is what lets
  an organization appoint a second administrator without a server admin, and
  granting is bounded to what the caller itself holds, so it can widen nobody.
  It cannot create or delete an organization, set its status, reach an account
  outside its own, or change one holding an `_any_` name it lacks
  ([Users: The rules that make delegation safe](api/users.md#the-rules-that-make-delegation-safe)).
- `manage_clients` is **a trust boundary**: approving a device is the decision
  that an identity may authenticate and do work, and granting `perform_work` is
  the decision that it takes queued work. A client pre-auth key is those two
  decisions made in advance. It is scoped by membership, the bare verb's meaning
  throughout this vocabulary
  ([Device administration: The scope](api/devices.md#the-scope)).

**The partition, and why it is one.** The work verbs describe the machine
holding the key; the authoring, reading, visibility, roster and administration
verbs describe an operator's intent. A device key is the most exposed key in the system (it sits
in a rack, it gets imaged, it is copied into lab automation), and it is the least
worth protecting. Plan authority is the opposite. Annotating the vocabulary and
trusting an operator to grant along the grain would leave the separation as a
convention; partitioning it makes "a device cannot file a plan" true of the
type: each half is its own vocabulary, so a grant of `submit_plans` to a device
is not a check that fails, it does not compile.

A host that both runs benchmarks and files plans therefore holds **two
identities**, a client and a user, rather than one identity holding both halves
of the vocabulary.

The partition is enforced in both directions, independently:

- **Closed on input.** A name is parsed into the vocabulary of the principal it
  is aimed at, so a name from the other half is refused where a typo is: at the
  argument boundary, before anything is read or written. Every input path (the
  admin CLI, the HTTP grant routes and a client pre-auth key's template) refuses
  the other half ([§5.6](#56-assignment)).
- **Inert at rest.** A stored object may still carry such a name, written by a
  release that placed the name differently. A name the object's own vocabulary
  does not define is *unknown* there, whoever else defines it: preserved under
  its stored spelling, granting nothing, and rendered, which is exactly
  [§5.9](#59-growth)'s rule for a name this binary does not define. No reader
  asks which other vocabulary a stored name might belong to, and a misplaced
  name that is visible is one an operator can clear with `permission remove`
  ([§5.6](#56-assignment)).

Implication never crosses the partition, because it cannot be spelled: a user
permission's implications are user permissions. That is what lets a route check
the permission it requires without also checking the caller's kind.

`POST /benchmarks` and `POST /benchmarks/batch` serve two operations under one
route. A body with a client-supplied `job_id` reports a claimed job and requires
`perform_work`. A body without one is an ad-hoc submission and requires
`submit_results`. Both are client rights, so neither route ever admits a user.
In a batch, each item is judged on its own, and a refused item carries the
refusal in its item result.

### 5.3. Defaults

**One default, applied at one moment.** A permission a principal holds was
recorded by something: a registration, a template, an operator. Nothing is
inferred at read time, for either kind: an object that is not there grants
nothing. What each kind holds at creation is stated in
[Users: Defaults and groups](api/users.md#defaults-and-groups) and
[Device administration: Device permissions](api/devices.md#device-permissions);
this section records where each set comes from and why.

Two ways a set gets written at creation, and one state that is never filled in:

- **A new registration** holds `{submit_results}`, because most registrations
  are external devices whose owner installs the app, benchmarks the device, and
  submits the result. That client needs `submit_results` and nothing else. In
  particular it does not hold `perform_work`: only a lab device fetches queued
  work.
- **A pre-auth key** may carry a template.
  `clients preauth create --permission P` (repeatable) stores
  `default_permissions` on the key. A key minted without the flag stores no
  template, and the registration default applies.
  `clients preauth create --no-permissions` stores an empty template. A
  repeatable flag alone cannot spell an empty set, which is why the second flag
  exists. The distinction is why the field on the key is optional rather than
  an empty set. Lab devices register through keys, so the template is where an
  operator grants a fleet its rights in one step.
- **A user** has no registration default. A user pre-auth key carries no
  template because the account exists before its key, so it is granted directly
  (`users create --permission`, `users permission add`, or the grant flags of
  `users approve` for a requested account).
- **A principal with no permissions object holds nothing**
  ([§5.8](#58-storage)). This is not a default. It is the absence of one. The
  state is reachable two ways: a client record registered before permissions
  existed, and an object an operator removed. Both are **granted by hand**, with
  `clients permission add`.

  A default here would read a grant out of a record where the *missing object
  is the evidence that nobody granted anything*, and it would keep doing so,
  silently and at read time, for every record that later lost its object. Being
  told a device holds nothing is recoverable in one command; discovering that
  every un-granted record has been quietly holding a permission is not.
  `clients permission list` shows `-` for a record with no object and `(none)`
  for a cleared one, so the records still needing a grant are a listing, not a
  guess.

### 5.4. Status and permissions

`status` and `permissions` are two independent fields, checked in that order
after authentication ([§2](#2-request-authentication)) and before anything that
depends on the object a route reaches.
[Users: Status, then permission](api/users.md#status-then-permission) states
the order and each refusal, and
[Device administration: Device status](api/devices.md#device-status) a device's,
including that a pending device carries its set and nothing is re-granted at
approval. This section records why.

The hold is for an identity that is not yet vetted, whose work an operator may
later promote. It is not for an identity an operator vetted and chose not to
grant `submit_results` or `perform_work`. That client is refused, and nothing is
archived: holding its data would keep results the operator already declined,
and would hand a client with no work rights an unbounded write channel.

**Authentication, status and permission read the request headers alone**, so
a route settles them before it reads a byte of the body. The routes that file
or check a plan are the case
that makes this matter. Their ceiling is `plan_body_limit_bytes` rather than the
router default, so a refusal that waited for the body would read a plan-sized
body for a caller the server declines. A device is refused there from the
`X-Client-Id` prefix alone, before any signature check ([§5.11](#511-users)).

**The scheduler asks the same two questions, in the same order.** The fleet
match behind a plan's status, its warnings, and `plans status --explain` counts
the **working** fleet: clients that are approved *and* hold `perform_work`
([Device administration: Device status](api/devices.md#device-status)).
Counting every approved client instead would contradict the permission check:
the matcher would report a plan `active` against machines the claim path
refuses, which is the state a fresh registration is in. `--explain` names
which gate dropped them. The match applies no default of its own: a permission
object it cannot read grants nothing, exactly as a route's read does
([§5.3](#53-defaults)). The count is
recomputed against the live store on every pass, so a grant shows on the next
one, and `claim`, `heartbeat` and `reclaim` re-read the caller's set, so a
revocation takes effect on the next request.

### 5.5. Refusal

A permission refusal is `403` with the message
`client lacks the {permission} permission` for a client, or
`user lacks the {permission} permission` for a user, where `{permission}` is the
name the route requires. A service account asked for a permission its type
cannot hold reads `a service account cannot hold the {permission} permission`.
The client message is distinct from `client is not approved`, so a log
line and a client's error handler tell the two causes apart. Naming the missing
permission discloses nothing: each route's requirement is public in
[httpapi.md](httpapi.md) and the OpenAPI document, and the caller reads its own
set at `GET /clients/me` or `GET /api/v1/users/me`.

The server logs each refusal:

```
WARN client_id=ev1_a3f8... required_permission=perform_work method=POST path=/plans/claim refused: client lacks permission
```

An `_any_` scope needs the resource's owner before it can decide, so a refusal
on `GET /api/v1/plans/{plan_id}` for a plan the caller does not own reads the
same as a plan that does not exist: `404`. A `403` there would confirm the plan
exists.

### 5.6. Assignment

An operator assigns permissions, with the `clients permission` and
`users permission` commands
([cli.md](cli.md#pipette-mgmt-clients-permission-addremovesetlist),
[cli.md](cli.md#pipette-mgmt-users-permission-addremovesetlist)) or over HTTP.
A client cannot:

- `POST /clients/register` and `PATCH /clients/me` refuse a body that carries
  `permissions` with `400` and the message
  `permissions are assigned by an operator`. A refusal is preferred to silently
  dropping the field, which would leave the client believing the change took
  effect. This mirrors the reserved capability namespaces, which a client may
  not report.
- A pre-auth key template ([§5.3](#53-defaults)) is an operator's grant made in
  advance, so it is the one path by which a registration sets a non-default set.

A grant that names a permission the principal's kind cannot hold is **refused**,
not filtered, and the message says which kind holds it rather than reporting the
name as unknown: `"submit_plans" is a user permission; a client cannot hold it.
One of: submit_results, perform_work`. The refusal is the parse
([§5.2](#52-the-vocabulary)), so every granting path gets it for free.

`permission remove` deliberately takes **either** half of the vocabulary. A
name a kind cannot hold may still sit in a stored object, and removing it is
how an operator clears one ([§5.2](#52-the-vocabulary)). An *unknown* name (one
no vocabulary in this binary defines) is still refused at the argument
boundary, so a typo cannot pass as a removal; clearing one needs a binary that
knows the name.

A stored object holds the minimal set an operator granted. Every read surface
renders the implication closure over it ([§5.1](#51-the-model)): on a user that
holds `submit_plans`, `permission list` also shows `read_plans`.
`permission set` therefore takes the set to store, not that listing. If the
object stored the closure instead, `permission remove read_plans` on a user
that holds `submit_plans` would leave an object that contradicts itself.

A client whose set is cleared keeps its identity and can do nothing;
`clients delete` revokes the identity itself.

An account holding `manage_clients` makes the same grants over HTTP and from
the device's page in the app
([Device administration: Device permissions](api/devices.md#device-permissions)). A name a device
cannot hold is a `400` there, without the CLI's message. There is no ceiling as
there is for a user's set ([§5.11](#511-users)): the two vocabularies do not
meet, so an administrator holds neither name it grants. What bounds the grant
is the device: one the caller's own organization holds, or any under
`manage_any_clients`, the same scope as every other device write.

The server logs each grant and each revocation at `info!` with `client_id`,
the permissions added or removed, and the resulting set, so an operator can
reconstruct who held what and when from the log stream. Over HTTP the request
span also names who made the change: the signing account as its `client_id`,
and the device as `target_client_id` ([cli.md](cli.md#logging)).

### 5.7. Visibility

- `GET /clients/me` carries `permissions`, always present, `[]` when the client
  holds none. The array is what its object holds, including any inert name a
  client cannot hold ([§5.2](#52-the-vocabulary)). Nothing in the client
  vocabulary implies anything else, so no expansion happens on this route. It is
  the user vocabulary that carries the implication rules ([§5.1](#51-the-model)).
  `PATCH /clients/me` refuses the field on input ([§5.6](#56-assignment)) and
  reports it in the response.
- `pipette-mgmt clients list --columns permissions` renders the stored set.
- What a user reads of a device, its `permissions` and `contact_email`
  included, is
  [Device administration: What a device row carries](api/devices.md#what-a-device-row-carries).
  Which devices can perform work is fleet information under `read_clients`
  ([§5.10](#510-fleet-disclosure)). A record's `contact_email` goes only to a
  caller that administers the device because it is often a personal address,
  and `read_clients` reaches the whole fleet. `GET /clients/me` and the host CLI
  are unaffected.

### 5.8. Storage

A principal's set lives in its own object in the auth store, in a keyspace for
its kind. The keys are in
[storage.md §3.1.5](storage.md#315-permissions-and-pre-auth-each-kinds-own).

**The kind is the keyspace, not a prefix on the id.** The two bodies are
different types ([§5.2](#52-the-vocabulary)), so a reader has to know which it
is deserializing before it opens the object, and the key tells it. A fleet pass
lists the client keyspace and gets exactly the fleet. Telling the kinds apart by
the id's own prefix was the alternative, and it is the one to avoid. `UserId`
refuses a value without `user_`, but `ClientId` enforces only a charset
([§1](#1-identity-model)), so an id carrying no recognisable prefix is still a
client, and a reader that required `ev1_` dropped it. A dropped grant is
indistinguishable from no grant, which silently removes the client from the
fleet a job is matched against.

**Why not a field on the client record.** The client record has two writers
with opposite trust: the device, through `PATCH /clients/me`, and the
operator, through the CLI. Both overwrite the whole object with no
precondition. A device that reads its record just before an operator's
revocation lands, and writes just after, restores the permission. That is a lost
update in the fail-open direction, caused by the party being revoked. A
separate object closes the race by construction: the device never causes this
object to be written. A compare-and-swap on the record would close it too, but
`If-Match` is optional for `[auth_storage]`
([storage.md §8](storage.md#storage-and-auth_storage-require-conditional-put))
and `local_fs` has no equivalent primitive.

**Why not leaf markers like tags.** One marker per permission would make
`add` and `remove` single writes with no read-modify-write anywhere. But a
client with no markers and a client never granted look the same, and an
operator needs the two apart to know which records still need granting
([§5.3](#53-defaults)). An absent object is a record nobody has granted; an
object holding `[]` is a set an operator cleared. The two grant alike. The
distinction is an operator's view, not an authorization rule, which is what
makes it cheap to keep and expensive to lose.

**Writers.** The server writes this object on no client's request:

- **Registration** writes it before the client record, and the write is not
  best-effort. A failed permissions write fails the registration: a lab device
  that lost its template would be unable to claim, and an operator would be
  re-granting by hand. Seeded tags follow the same rule, since a job can
  require one ([§7](#7-client-tags)).
- **The CLI** reads, modifies, and writes. Two operators racing on one client is
  the residual, and the CLI uses a compare-and-swap on the object's version
  where the backend honors it.
- **The administration routes**
  ([Device administration: Device permissions](api/devices.md#device-permissions)) write through
  the same read-modify-write, on an administrator's request.

**Reads.** There is no cache, so a revocation takes effect on the client's next
request. The signature-migration marker can be cached because it only ever
moves one way; a permission set moves both ways.

### 5.9. Growth

The vocabulary is closed on input and open at rest:

- A name from an operator or from the wire must be one of those in
  [§5.2](#52-the-vocabulary). Any other name is rejected. A typo must fail.
- A name read back from the store is carried through unchanged and grants
  nothing. A binary that predates a name must neither drop it nor fail the
  object. The stored name is bounded to `[a-z0-9_]{1,64}` so the object stays
  well-formed whatever wrote it. The retired `export_results` stays this way
  on the accounts granted it, and a remove clears it.

A new permission reaches an existing principal only by an explicit grant.
Adding a name changes no stored object, because a principal holds a name only
when its set contains it. A record with no object holds nothing whatever the
vocabulary gains, so it never picks up a right by a deploy either.

### 5.10. Fleet disclosure

[Device administration: Fleet disclosure](api/devices.md#fleet-disclosure)
states what a caller is served at each tier and what a device out of reach
reads as. This section records the surfaces the rule has to cover, the probes
each one closes, and why the tiers are drawn where they are.

Eight surfaces tell a caller about the fleet:

1. `GET /api/v1/clients` and `GET /api/v1/clients/{client_id}`.
2. The `warnings` in the report of a filing route ([plan-ingestion.md §6.2](plan-ingestion.md#62-ingestion-time)).
3. `warnings` and `progress_snapshot.starved_groups` in a plan as its reads and
   its note edit answer it: `GET /api/v1/plans/{plan_id}`, `GET /api/v1/plans`,
   `PATCH /api/v1/plans/{plan_id}` and `GET /api/v1/campaigns/{campaign_id}/plan`.
4. The `fleet` standing and the per-group clause ladder, roster and live match
   counts in `GET /api/v1/plans/{plan_id}/explain`
   ([`GET /api/v1/plans/{plan_id}/explain`](api/plans/explain_plan.md)).
5. The capability-flag vocabulary and its per-flag client counts in
   `GET /api/v1/clients/capabilities` ([`GET /api/v1/clients/capabilities`](api/devices/get_capability_vocabulary.md)).
6. Lease holders in `GET /api/v1/plans/{plan_id}/jobs` (and the device names
   its `results=true` body carries).
7. The plan document, which names client ids (`clients`, `exclude_clients`) and
   requirement sets (`requires`, `any_of`, `excludes`, `not_all_of`): its
   source, the campaign revision routes and the document a revision starts
   from, and the same six fields of a job body in
   `GET /api/v1/jobs/{job_id}/plans`.
8. The device that ran a job, and why the job failed, in a plan's results:
   `GET /api/v1/plans/{plan_id}/results.csv` and `GET /api/v1/plans/{plan_id}/jobs`
   with `results=true` ([`GET /api/v1/plans/{plan_id}/results.csv`](api/plans/get_plan_results_csv.md)).

One right gates all eight. If any one were free, a submitter that cannot list
the fleet would probe it through the cheapest surface: submit a plan requiring
one flag, read the warning, repeat. Surface 5 returns the complete capability
vocabulary in one request. It therefore requires the same permission. Surface 6
closes another probe. A submitter can name fleet members in a plan without a
fleet list, then read which client took each job. Surface 7 closes the
largest gap. Plan access follows the campaign. Without the right on surface 7,
a teammate can read each client id and requirement set that a colleague named.
Surface 8 names the machine twice: in the row's device, and in the failure
reason, which is free text. The reason of a job that every eligible client
failed lists their ids and their count, and a `lease_lost` reason lists the
clients that lost it.

**Scope.** Reach is the function PIP-525 defines for per-submitter device
restrictions. A submitter with no restriction on record reaches the whole
fleet, so until PIP-526 lands `read_clients` and `read_any_clients` show the
same set, and `read_clients` narrows on its own once restrictions exist. Every
surface above calls that one reach function, and "out of reach" and "does not
exist" are the same answer everywhere, which is the requirement PIP-525 states.

**Tiers.** For surfaces 2, 3, 4, 6 and 8 the right gates the projection, not the
route, because a submitter must be able to file a plan, read its status, and
ask why it is not running, whatever it holds. Surface 7 has a tier too, but it
does not redact the document: a document with its values removed does not
round-trip, so a revision cannot start from it, and the tier withholds the
whole document instead. Surfaces 1 and 5 have **no** withheld tier, because
every field they carry is a fleet fact.

A job row's `planned` on the jobs reads is served at both tiers. It is the
job's `spec` with its secrets removed, and holds no planner warning, no parser
text and no eligibility, so it names no client and no requirement set.
Anything added to it is reviewed against this surface again.

The CLI runs with operator storage access and uses the full tier.

The residual is one bit per requirement set: something matches, or nothing
does. That bit is inseparable from reporting starvation at all. PIP-525 owns
whether to go further.

### 5.11. Users

A **user** (`user_…`) is the identity a person or a service job authenticates as
when it *authors* work. A client (`ev1_…`) is the identity a device
authenticates as when it *does* the work. Two kinds, one permission model,
one signing scheme. The app at `/app/` signs in as a user
([§5.12](#512-signing-in-from-a-browser)).
[Users: Accounts and keys](api/users.md#accounts-and-keys) states what an
account carries and how its keys behave,
[Users: User routes](api/users.md#user-routes) how one comes to exist, and
[the rules that make delegation safe](api/users.md#the-rules-that-make-delegation-safe)
what a service account and a campaign reader may do. This section records why
each is so.

#### Why a second kind rather than a client with different permissions

Four things a client cannot be made to do:

- **Outlive its machine.** A client id is the digest of its one key, so
  re-imaging a laptop mints a new identity, and every plan the old one filed
  falls out of the own scope of `read_plans` and `cancel_plans` permanently.
- **Hold several keys.** One person, three machines. A client is single-key by
  construction ([§1](#1-identity-model)); a user holds a set of registrations and
  revokes one without touching the others.
- **Stay out of the fleet.** A client record is what the matcher narrows. A user
  record is in a different namespace with no device profile and no capabilities,
  so an operator identity can never be counted as a candidate for a job. A
  property of the type rather than a rule someone has to remember.
- **Name a person in an audit.** "Who cancelled this plan" should answer with an
  account, not a laptop.

#### The record

**Five fields, deliberately**, and a sixth on a campaign reader
([storage.md §3.1.1](storage.md#311-the-account-usersuser_idjson)). The record
starts at what every operator question needs and grows on demand. A field that
is only *probably* useful is cheaper to add later than to carry, and the list
below it records what was considered so that adding one is a decision rather
than a rediscovery.

| Field | Why it is collected |
|---|---|
| `user_id` | Minted, not key-derived. A user has many keys, so there is no one key to derive from. `user_` followed by a hyphenated **UUIDv7**, matching the `job-`/`plan-` minted ids. v7 is time-ordered, so accounts list in creation order. |
| `contact_email` | The handle an operator knows the account by, and where a pre-auth key is sent. Stored case-folded. **Unique across accounts** (see below). Nothing here sends mail, so it is an identifier and a contact, never a verified fact. |
| `account_type` | `human`, or `service` for a job's account: one an organization keeps for a program, signing with keys registered from its pre-auth key. A Clerk sign-in never reaches a service account ([§5.12](#signing-in-through-clerk)), since a person who can verify its address is not the job. `human` for a record written before the field. Set at creation and changed by `users set-type` or `PATCH /api/v1/users/{user_id}`. |
| `status` | `active`, `pending` or `disabled`. Which one an account starts at is decided by how it came to exist, not by the kind: created by an operator (`users create`, or the registration an invite admits) it is `active` at once, because the decision has already been made; requested by a machine with nothing but an address it is `pending` until `users approve`, which is the client's `pending`/`approved` split under different words. `disabled` is the reverse transition: offboarding. No route deletes an account, so the plans it filed and the campaigns and vault resources it owns keep resolving to a principal that exists; the host's `users delete` refuses while it owns a campaign or a vault resource, and while it owns a device unless `--detach` releases the devices. |
| `created_at` | — |
| `campaign_scope` | Only on a campaign reader: the ids of the campaigns a service account reads, and the only ones. It is the one field something reads on every request, which is why it is on the record and not beside it: a lost record is a lost account, so the scope cannot be lost while the account still signs. Set at creation or by `PATCH /api/v1/users/{user_id}` and `users set-campaigns`, replaced and never removed ([Users: The rules that make delegation safe](api/users.md#the-rules-that-make-delegation-safe)). |

**A service account works for one organization and administers nothing**, and
a **campaign reader** reads the campaigns it names and nothing else
([Users: The rules that make delegation safe](api/users.md#the-rules-that-make-delegation-safe)).
The reader is the account a publishing job signs as, so a stolen key reads
what is about to be published and no more.

**Uniqueness on the address.** Claiming an address is an **exclusive create**
([storage.md §3.1.2](storage.md#312-address--account-user-email-indexemail)), so
two operators onboarding one person concurrently contend for one key and
exactly one wins, rather than producing two accounts on one address with a
grant on one and the registered key on the other. The address is folded at
creation: `Operator@` and `operator@` are then one mailbox, while
`operator+ci@` is deliberately a separate account.

**Considered and left out.** Each of these was specified and dropped; the
reason is what a later proposal has to argue against.

- *A permissions field.* The set lives in its own object, so a revocation is
  one object write and never a read-modify-write of the account
  ([§5.8](#58-storage)).
- *`display_name`.* The address already names the account, and for a service
  account it says what the account is for. A second free-text name for the same
  principal is one more field to keep true and nothing reads it that the address
  does not.
- *An `owner` for service accounts.* It answers "whose robot is this", which
  has no reader yet, and the address carries the answer by convention: a
  service account uses a plus-tagged form of its owner's address
  (`operator+ci@…`), which the uniqueness rule keeps distinct rather than
  folding away. Add the field when something needs to *query* on it, such as
  offboarding a person's robots with them.
- *`status_changed_at`.* Cheap, and it answers "when was this offboarded". The
  grant and revocation log already carries the event
  ([§5.6](#56-assignment)), so the field is a convenience rather than the only
  record. Worth adding the first time someone has to reconstruct a date from
  the log stream, not before.
- *`notes`.* The pre-auth key has one, and the justification for an identity
  does outlive whoever typed the command. Left out to keep the first version at
  the fields something reads.
- *`last_authenticated_at`.* The field an operator wants for hunting dormant
  accounts, and recording it costs a write on every authenticated request, on
  the hot path. If dormancy hunting becomes real, a day-granularity value
  written only when it changes is the shape to revisit; a per-request timestamp
  is not.
- *Tags.* Grouping users to grant a team at once is the same need
  `permission add --tag` already serves for clients. Left out for the same reason
  a client's tags are off its record (membership is a marker, not a field), and
  because reaching it means widening a live index
  ([storage.md §3.1.4](storage.md#314-tags-not-here)).
- *A password, a second factor, a phone number, an organization, an avatar.*
  Authentication is a keypair; the rest has no reader.

#### Keys

Each registration carries the public key, `key_details` describing the machine,
and `added_at`, under the account that owns it
([storage.md §3.1.3](storage.md#313-keys-user-keys-and-the-claim-that-indexes-them)).

What a caller sees of keys, their ids, revocation and expiry, is
[Users: Accounts and keys](api/users.md#accounts-and-keys); the reasons follow.

A keypair belongs to **one** principal, of one kind. A `key_id` is the `sha256`
of its public key under a `userkey_` prefix, and a client id is the same digest
under `ev1_`, so without a claim spanning both keyspaces one keypair could
authenticate as a device *and* as an operator, and the partition above would be
bypassed by choosing a header. Both registration routes take that claim;
storage.md §3.1.3 states it, with the write order and what a crash leaves.

`key_details` ("work laptop") is what makes a revocation decidable by a human: a
key id is 64 hex characters.

Revoking the last remaining key, or the key the request was signed with, is
permitted because a holder whose only key is compromised must be able to
retire it at once; refusing in order to keep the account reachable would leave
the compromised key live.

The holder is not the only one who revokes. An administrator of the account
revokes one
([`DELETE /api/v1/users/{user_id}/keys/{key_id}`](api/users/revoke_user_key.md)),
and the host with `pipette-mgmt users key revoke`. Disabling the account stops
every key but enabling restores them all, and a stolen key can revoke its
holder's other keys, so retiring one key cannot depend on the holder.

**A key made for one browser expires**
([Users: Signing in from a browser](api/users.md#signing-in-from-a-browser)),
because a browser that is cleared or abandoned without signing out would
otherwise leave a key that signs as the account for good. A machine's key has
no `expires_at` and signs until it is revoked. An expired key's record stays,
listed, until it is revoked.

#### Provisioning and routes

There are three ways in, and each ends at the operator
([Users: User routes](api/users.md#user-routes)):

- **Invited.** An operator creates the account and mints a **user pre-auth key**
  against it. A key signs as the account, so an administrator mints one only
  for an account nobody has signed in as yet, holding nothing the administrator
  does not: it onboards a person and never takes one over
  ([Users: The rules that make delegation safe](api/users.md#the-rules-that-make-delegation-safe)).
  The key grants nothing: the operator grants the account itself, with
  `users create --permission` or `users permission add`. The app shows a
  minted key once, on the page that minted it, never in the address, the
  history or storage.
- **Requested.** A machine registers with a `contact_email` and no invite, and
  `users approve` admits the `pending` account. That is also the moment the
  operator decides what it may do, which is why the grant flags live on that
  command. `require_user_preauth_key = true` closes this path, the way
  `require_client_preauth_key` closes the keyless client registration
  ([§3.2](#32-pre-auth-keys)), and so does a `[clerk]` provision rule for the
  addresses it covers
  ([`POST /api/v1/users/register`](api/users/register_user_key.md)).
- **Provisioned.** A Clerk sign-in at a domain a `[clerk]` provision rule names
  creates the account ([§5.12](#signing-in-through-clerk)). The rule is an
  operator's grant made in advance.

Each grant to an account is logged at `info` with the names it grants, as
`granted`, whether it comes from `POST /api/v1/users` or a provision rule at a
Clerk sign-in, as
`PUT /api/v1/users/{user_id}/permissions` already does.

There is still no self-service `POST /api/v1/users`: a request creates a record that
reaches nothing but `GET /api/v1/users/me`, where it watches for its own admission, so
the decision that an identity may *act* is made on the operator's side, exactly
as client approval is. That is also why a request for
an address some account already holds is refused rather than admitted to it.
Admitting it would be a stranger having an operator approve *their* key on
someone else's account. The one exception is the keypair already registered
there, which is the caller retrying a response it never saw
([`POST /api/v1/users/register`](api/users/register_user_key.md)). A user's pre-auth
key is a client's key's design over its own record and its own keyspace
([storage.md §3.1.5](storage.md#315-permissions-and-pre-auth-each-kinds-own)):
same hashing, same single- or multi-use, same optional expiry, but it names the
account it admits and carries neither a permission template nor any of a
device's attributes. Two records, two token prefixes, two listings, so
`users preauth list` shows exactly the keys an operator minted for accounts.

**The HTTP surface mirrors the client surface**, and adds only what a
multi-key principal needs: registering another machine's key with a key the
account holds, and revoking one
([Users: User routes](api/users.md#user-routes)). A service account cannot add its
own key: its organization decides which machines the job signs from, through
the pre-auth keys its administrator mints, so a leaked key cannot register
company for itself.

#### Signing

A user signs RFC 9421 (§2.4), where the key names the account, or the older
**v2 payload**, which adds the key id:

```
v2\n{method}\n{path_and_query}\n{timestamp}\n{principal_id}\n{key_id}\n{nonce}
```

`X-Client-Id` widens to carry either prefix, and `X-User-Key-Id` is new. It names
which of the account's keys signed, and only a user sends it: a client holds one
key, and its id is a prefix swap from the client id already on the record.

**Why the key id travels rather than being found.** Not to avoid the arithmetic.
Trying five Ed25519 verifications is nothing. Three reasons, none of them CPU:

- **The cost is round trips.** Without the key id the server knows only the
  account, so every request lists the account's keys and reads each
  registration. Caching that listing is not open to us: a cached key set
  is a revocation that does not happen, and `DELETE /api/v1/users/me/keys/{key_id}`
  exists to take effect now.
- **It is a DoS amplifier.** One request with a garbage signature would cost a
  listing, N gets, and N verifications, where N is the victim's machine count.
  Chosen by naming the account, which is not a secret.
- **A deleted key cannot be identified after the fact.** Trial verification
  needs the key to still be registered, so it stops answering "which machine
  signed this" as soon as a revocation deletes the key. The request names the
  key it signed with, so the server holds that id while the request runs. A plan
  manifest records the account, `submitted_by`, and not the key.

The header is a lookup hint, and it is attacker-controlled, so `key_id` is also a
field of the signed payload above. Swapping the header for another key's id
yields a signature over a different string, which does not verify: the hint
picks the key, the signature decides. This is what `kid` in a JWS protected
header does (RFC 7515 §4.1.4); trial verification would be the unusual choice.

**`X-User-Key-Id`, not `X-Key-Id`**: "key" alone does not say which of this
server's keys a value is, and a pre-auth key id is the other one an operator
handles.

A user is never offered the timestamp-only fallback, whatever
`accept_legacy_signatures` says. That mode is a per-client migration aid with a
per-client withdrawal marker ([§2.3](#23-timestamp-only-signatures)); a principal
kind that has never had it starts strict. Clients stay on v1.

#### Attribution

A plan manifest carries one attribution field, `submitted_by`. It names the
account that filed the plan. A plan filed into a campaign belongs to the
campaign, not to its filer ([§5.1](#51-the-model)), so a team campaign records
which colleague filed each revision without giving that colleague the revision.
For a plan filed outside every campaign, which an older build allowed, the own
scope compares against this one field. What a caller can and cannot name is
[Users: Accounts and keys](api/users.md#accounts-and-keys).

**Only a user files a plan**, because a client cannot hold `submit_plans`
([Users: Permissions](api/users.md#permissions)). A plan that records no
`submitted_by`, such as one the `campaigns` commands filed, shows as
`operator` in the app.

A manifest whose `submitted_by` holds an `ev1_…` reads it as absent, so the plan
is attributed to nobody, matches no user in an own-scope check, and is reachable
under `read_any_plans`. Reading it as absent rather than refusing the manifest
keeps the plan in every listing and maintenance pass; the next rewrite of the
manifest drops the value.

### 5.12. Signing in from a browser

The app at `/app/` ([ui/README.md](../ui/README.md#signing-in)) signs every
request it makes. How a reader signs in, page by page, is there; what a caller
sends and gets is
[Users: Signing in from a browser](api/users.md#signing-in-from-a-browser);
this section is the security model.

**A browser holds a key, not a session.** The app is a client of the JSON API
like any other: every call it makes carries an RFC 9421 signature
([§2.4](#24-rfc-9421-signatures-user-accounts)), with the same freshness window
and the same spent-nonce rule, so nothing here is a bearer credential. The
private key is kept in IndexedDB as a non-extractable `CryptoKey`, so no
script, the app's included, can read it back out. The server serves the app's
page and assets to anyone: they hold no data.

**A key may name only itself.** When `GET /api/v1/users/me` is signed with no
`X-Client-Id`, the payload's principal field is empty, and the account is the
one the key's claim names
([storage.md §3.1.3](storage.md#313-keys-user-keys-and-the-claim-that-indexes-them)).
That is one get before the usual two, not a search, and the claim only picks
which registration to verify against: the signature still decides. Because an
RFC 9421 signature (§2.4) names only its key, the app needs no account id to
sign.

**The browser's key is its own.** The app never keeps a key the account uses
elsewhere: it makes a fresh keypair in the browser and registers it. A key
another key or Clerk registers expires ([§5.11](#511-users)); a sign-up key is
the account's first and may be its only one, so it does not expire, and the
reader saves it before it is used.

**A terminal's key is approved here too.** A command-line client passes its
public key to `/app/login/cli` in the query. The code the reader types is
derived from the key itself, so anyone holding the link can work it out: it
vouches only that the key shown is the one the reader's own terminal printed.

What it costs: the key is in the browser, so whoever can run script on the
app's origin can sign as the account while the page is open, though not carry
the key away. The app loads no script from elsewhere
(`Content-Security-Policy: default-src 'self'`), and ClerkJS only at a host
`authorized_parties` names, once a reader asks for it or, on a later signed-out
load, to finish a Clerk sign-out that failed. Signing out of the app signs out
of Clerk too; a failure is reported, and retried on the next signed-out load.

#### Signing in through Clerk

[Users: Signing in through Clerk](api/users.md#signing-in-through-clerk) states
what the token must carry and every refusal. The token is one a Clerk session
issues from the JWT template `[clerk] jwt_template` names. The server holds the
instance's public key and verifies the token offline, with no call to Clerk.
The five-minute `iat` window is the one of [§2](#2-request-authentication).
Three of the checks exist for a reason the claim does not show:

- the `aud` names the template, so a token another app on the same instance
  asks for signs nobody in here;
- no `act`, so a Clerk administrator impersonating a session reaches no
  account this way;
- `allowed_domains`, because a shared Clerk instance verifies addresses for
  every app on it, so this is what keeps its outside users out of a deployment
  meant for one organization, accounts included.

Each token signs in once, through the record [§2.2](#22-replay-protection) keeps
for signatures, and none is logged. Its claims are, at `info`, once its
signature verifies: the ones above and `sub`, as Clerk rendered them, with the
address cut to its domain. A refusal then reads against what the template sent,
while a token that fails its signature logs only the refusal, since its claims
are still whatever the caller wrote.

**The address names the account.** A service account is refused because it
signs with its own keys: a person who can verify its address is not the job.
The sign-in counts as the account's first, as a key registration does, so no
administrator can mint a pre-auth key that takes it over afterwards. A
provisioned account's record is written last, after the address claim, the
membership and the `org_member` grant
([cli.md](cli.md#pipette-mgmt-users-permission-addremovesetlist)), so an
account that exists is complete. A disabled account is never replaced, which is
why offboarding is `users disable`. A keyless `POST /api/v1/users/register` for
an address a rule covers is refused because its unverified claim would hold the
address against this sign-in and win the account for whoever asked first.

**Keys from before the first sign-in are shown, not revoked.** An administrator
who creates an account for an address can register a key to it through a
pre-auth key before the owner first signs in, and that key signs as the owner
afterwards. Revoking such keys at the first sign-in would also take the
machines an owner set up through an invite, so the first Clerk sign-in records
them instead: the owner's account page marks them, and the audit log names
them ([Users: Signing in through Clerk](api/users.md#signing-in-through-clerk)).
Revoking them closes the gap only from then on: a key that one of them added
later through `POST /api/v1/users/me/keys` is unmarked, and the audit log is
where its signer shows.

What it costs: Clerk reaches **every account whose address it can verify**, and
a sign-in registers a key that signs for 7 days, and with it any key the
account's own `POST /api/v1/users/me/keys` adds. A deployment extends that
trust to its Clerk instance and to the accounts behind it, such as a Google
account.

## 6. Device profile

Clients can supply device attributes (hardware, OS, chip model, RAM, etc.) at
registration via `POST /clients/register`, or set and update them later via
`PATCH /clients/me` (see [httpapi.md §2.4](httpapi.md#24-patch-clientsme)).
These attributes form the device profile that the plan assignment system uses
to match jobs to eligible clients. The profile is entirely optional. A client
without one can still receive jobs via the explicit `clients` allowlist in the
job definition.

## 7. Client tags

A client can carry any number of flat **tags** (`team-mobile`, `us-east`). A
plan targets them as `tag:<tag>` flags
([plan-ingestion.md §5](plan-ingestion.md#5-capability-matching)), and
`clients permission add --tag` applies one grant to every client carrying a tag
([§5.6](#56-assignment)). How they are stored is in
[storage.md §2](storage.md#2-logical-layout-and-invariants), and who may set
one and what a device's write of one gets is
[Device administration: Client tags](api/devices.md#client-tags).

**Only an operator assigns them**, with `pipette-mgmt clients tag`
([cli.md](cli.md#pipette-mgmt-clients-tag-addremovelist)), or through the
pre-auth key a client registers with, because a tag decides which work a
client is offered. A client reads its tags at `GET /clients/me`.

**A tag is not the fleet**: a lab tag can hold hosts that never claim a job and
miss some that do. Grant `perform_work` against client ids unless the tag is
known to be exactly the working fleet.
