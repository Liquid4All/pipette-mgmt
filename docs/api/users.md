# Users

Accounts and their keys: how an account comes to exist and how its holder
signs in, and the administration of accounts, what they hold, and the keys
they sign in with.

## User routes

[Accounts and keys](#accounts-and-keys) describes the user
model, and how a user signs its requests.

An account comes to exist one of three ways, and each ends at an
administrator's decision: `pipette-mgmt users create` mints one directly, an
account holding `manage_orgs` creates one with
[`POST /api/v1/users`](users/create_user.md), or a machine **requests** one at
[`POST /api/v1/users/register`](users/register_user_key.md) and it stays
`pending` until `pipette-mgmt users approve` or a `manage_any_orgs` holder at
[`PATCH /api/v1/users/{user_id}`](users/update_user.md) admits it (a requested
account belongs to no organization, so a scoped caller cannot reach it,
[the rules that make delegation safe](#the-rules-that-make-delegation-safe)).
The first two register no key, so the holder signs in only by registering with
a user pre-auth key ([`POST /api/v1/users/register`](users/register_user_key.md)),
which an administrator of the account mints over HTTP
([`POST /api/v1/users/{user_id}/preauth-keys`](users/mint_preauth_key.md)) or an
operator on the server host mints with `pipette-mgmt users preauth create`. The
routes here **mirror the client routes** (same names, same shapes, same
refusals), and add only what a multi-key principal needs:
`POST /api/v1/users/register` is the user form of `POST /clients/register`, and
`GET /api/v1/users/me` of `GET /clients/me`. The plan and fleet routes are
unchanged; they simply admit a user principal.

[`POST /api/v1/users/me/keys`](users/add_own_key.md) and
[`DELETE /api/v1/users/me/keys/{key_id}`](users/revoke_own_key.md) are additions
that exist because a user holds many keys where a client holds one; the Clerk
registration ([`POST /api/v1/users/register/clerk`](users/register_clerk_key.md))
is the third, and exists because a browser can sign in through Clerk holding no
key at all. No route takes a session or a cookie: the console app signs every
call as any other user does
([signing in from a browser](#signing-in-from-a-browser)),
and none mints a user pre-auth key: minting one is administering the account it
names ([`POST /api/v1/users/{user_id}/preauth-keys`](users/mint_preauth_key.md)).
There is no `PATCH /api/v1/users/me` either, because every field on the record
is either immutable, an operator decision, or the account's key. Reading and
administering other accounts is [account administration](#account-administration).
The routes' bodies are in the OpenAPI document
([The OpenAPI document](../httpapi.md#15-the-openapi-document)).

## Accounts and keys

A **user** (`user_…`) is the identity a person or a service job signs as when
it *authors* work: files plans, reads them, administers. A **client** (`ev1_…`)
is a device that *does* the work. Two kinds, one permission model
([permissions](#permissions)); a host that both runs benchmarks and files plans
holds one of each.

| Field | What it means to a caller |
|---|---|
| `status` | `active`: signs and acts. `pending`: an account a machine requested, which reaches only [`GET /api/v1/users/me`](users/get_own_user.md) until an operator admits it. `disabled`: offboarded, refused everywhere, every key included; enabling restores the keys. An account an operator or an administrator creates, or a Clerk provision rule, starts `active` |
| `account_type` | `human`, or `service` for a job's account, which works for one organization, administers nothing, and never signs in through Clerk ([the rules that make delegation safe](#the-rules-that-make-delegation-safe)) |
| `contact_email` | Stored case-folded and **unique across accounts**: `Operator@` and `operator@` are one account, `operator+ci@` is another. Never verified by this server |
| `campaign_scope` | Only on a campaign reader: the campaigns it reads, and the only ones |

**Keys.** An account holds any number of keys, one per machine or browser. A
`key_id` is `userkey_` and the `sha256` of the public key, and a keypair belongs
to one principal of one kind: registering one another account or a device
already holds is a `409`. `key_details` ("work laptop") is what tells a holder
which key to revoke. Revoking the last key, or the one that signed the request,
is permitted, since a compromised key must go at once; a user pre-auth key
re-registers, though for a person's account that has signed in only the host's
`users preauth create` mints one
([a pre-auth key onboards](#the-rules-that-make-delegation-safe)). A key with
an `expires_at` signs nothing after it, and the refusal (`401 key expired`)
comes after the signature, so only its holder learns it lapsed. A key signs
RFC 9421, or the v2 payload that names the key in `X-User-Key-Id`; an account
is never offered the timestamp-only fallback a device may have.

**Attribution.** A plan records the account that signed the request as
`submitted_by`, and no request can name another account: an unknown query
parameter such as `submitter` is a `400`.

## Calling the API yourself

A script, a service job or a person at a terminal calls `/api/v1` with nothing
but an Ed25519 key registered to their account. There are four steps: make the
key, register it, sign every request, and read what the account may do.

The steps below need nothing but OpenSSL 3, bash and, to read the answers, `jq`.
`mgmt.example` in the examples stands for the server: use the host that serves
the app at `/app/` and this page at `/docs`. The snippets are bash; zsh runs
them too, as long as a URL holding a `?` is quoted, and in fish start `bash`
first. Run them in one shell, since later steps use the variables step 1 sets.

**1. Make a key.** Any Ed25519 implementation will do. The commands here need
OpenSSL 3: the `/usr/bin/openssl` on macOS is LibreSSL, which has no Ed25519
(`brew install openssl@3`, then use its `openssl`).

```sh
openssl genpkey -algorithm ed25519 -out key.pem
pub=$(openssl pkey -in key.pem -pubout -outform DER | tail -c 32 | xxd -p -c 32)
KEY_ID=userkey_$(openssl pkey -in key.pem -pubout -outform DER | tail -c 32 |
  openssl dgst -sha256 -r | cut -d' ' -f1)
echo "public_key $pub"; echo "key id $KEY_ID"
```

The public key goes over the wire as 64 hex characters. The key id is
`userkey_` plus the hex SHA-256 of those 32 bytes, so a signer works it out and
never has to ask for it. `key.pem` is the only secret, and it never leaves the
machine.

**2. Register it to an account.** Use one of four routes:

- **Approving it in the app.** A person who signs in to the app adds the key
  to their own account. Open this link, sign in if asked, and type the code it
  prints:

  ```sh
  echo "https://mgmt.example/app/login/cli?key=$pub&details=$USER@$(hostname -s)"
  echo "${KEY_ID:8:4}-${KEY_ID:12:4}" | tr a-f A-F                   # the code
  ```

  `details` becomes the key's `key_details`, so name the machine; write a space
  as `%20`. The code is the first 8 hex digits of the key id after `userkey_`,
  uppercase, as `XXXX-XXXX`, and it shows the page holds the key this terminal
  made. Then pick no expiry or 30 days and approve. The page registers the key
  with [`POST /api/v1/users/me/keys`](users/add_own_key.md), so the next route's
  limits hold here too.
- **With a user pre-auth key.** An operator creates the account on the host
  (`pipette-mgmt users create <email>`, which prints its `user_…` id) and
  mints a pre-auth key for it (`pipette-mgmt users preauth create <user_id>`).
  An administrator of the account can mint one in the app instead, or at
  [`POST /api/v1/users/{user_id}/preauth-keys`](users/mint_preauth_key.md).
  Send it unsigned to [`POST /api/v1/users/register`](users/register_user_key.md)
  with `{"public_key": "…", "key_details": "ci runner", "user_preauth_key":
  "user_preauth_…"}`. The key signs as that account at once. The response's
  `status` is the account's standing, which registering does not change.
- **With a key the account already holds.** Sign
  [`POST /api/v1/users/me/keys`](users/add_own_key.md) with that key, with
  `{"public_key": "…", "key_details": "…"}`. Add an `expires_at` (RFC 3339, in
  the future and at most 30 days away) for a key that should lapse; without
  one the key signs until it is revoked. Only an `active` account may do this,
  and not a service account or a campaign reader: their administrator mints a
  pre-auth key instead.
- **Asking for an account.** Send `POST /api/v1/users/register` with
  `public_key`, `key_details` and a `contact_email`, and no pre-auth key. The
  account is created `pending` and reaches nothing but `GET /api/v1/users/me`
  until an operator runs `pipette-mgmt users approve <user_id>`
  ([user routes](#user-routes)).

**3. Sign every request** in the RFC 9421 profile that the `userSignature` scheme describes. The signature base
is one line per covered component, then the parameters line, with no newline
after it:

```
"@method": POST
"@authority": mgmt.example
"@path": /api/v1/orgs
"@query": ?dry_run=true
"content-digest": sha-256=:Dmcct3wNRuKmSGlN9J7lDIw9DvE0Xp6DUXXIZHWJ65k=:
"@signature-params": ("@method" "@authority" "@path" "@query" "content-digest");created=1760000000;nonce="4f1c0de5a9b7";keyid="userkey_b16c…";alg="ed25519";tag="pipette"
```

- `@authority` is the `Host` you send to, lowercased. The server must list it
  in its `authorities` setting, or every request is `401`.
- `@query` is `?` alone when the URL has no query.
- A request without a body leaves out the `content-digest` line, and leaves
  `"content-digest"` out of the list.

Sign those bytes with the private key and send three headers: `Signature-Input:
sig=` followed by everything after `"@signature-params": `, `Signature:
sig=:<base64 signature>:`, and, with a body, `Content-Digest` exactly as signed.
Each request needs a new `nonce` and a `created` within five minutes of the
server's clock. A nonce is spent once, so a retry is signed again.

This script does all of it with OpenSSL 3, and reproduces the `userSignature`
scheme's example byte for byte. Save it as `sign.sh` and `chmod +x sign.sh`:

```bash
#!/usr/bin/env bash
# sign.sh METHOD URL [BODY_FILE]: print the RFC 9421 headers for one request.
# KEY (the PEM), KEY_ID, and optionally CREATED and NONCE come from the environment.
set -euo pipefail
method=$1 url=$2 body=${3:-}
rest=${url#*://}
authority=$(printf '%s' "${rest%%/*}" | tr '[:upper:]' '[:lower:]')
target=/${rest#*/}
path=${target%%\?*}
query='?'
[[ $target == *\?* ]] && query="?${target#*\?}"
created=${CREATED:-$(date +%s)}
nonce=${NONCE:-$(openssl rand -hex 16)}
components='"@method" "@authority" "@path" "@query"'
lines=$(printf '"@method": %s\n"@authority": %s\n"@path": %s\n"@query": %s\n' \
  "$method" "$authority" "$path" "$query")
if [[ -n $body ]]; then
  digest="sha-256=:$(openssl dgst -sha256 -binary "$body" | openssl base64 -A):"
  components+=' "content-digest"'
  lines+=$(printf '\n"content-digest": %s\n' "$digest")
  echo "Content-Digest: $digest"
fi
params="($components);created=$created;nonce=\"$nonce\";keyid=\"$KEY_ID\";alg=\"ed25519\";tag=\"pipette\""
base=$(mktemp)
printf '%s\n"@signature-params": %s' "$lines" "$params" > "$base"
sig=$(openssl pkeyutl -sign -rawin -inkey "$KEY" -in "$base" | openssl base64 -A)
rm -f "$base"
echo "Signature-Input: sig=$params"
echo "Signature: sig=:$sig:"
```

Call it once per request and hand its output to curl. This `call` signs and
sends one `GET`:

```bash
call() {
  local headers=()
  while IFS= read -r h; do headers+=(-H "$h"); done < <(KEY=key.pem KEY_ID=$KEY_ID ./sign.sh GET "$1")
  curl -sS --fail-with-body "${headers[@]}" "$1"
}
call 'https://mgmt.example/api/v1/users/me' | jq '{status, may}'
```

A refusal is a `4xx` whose body is `{"error": "…"}`: `call` prints it and
fails, and a `jq` filter reads it as `null` fields, so run the call without
`jq` to see it. A key not approved yet is `401` `key not found` on every route,
whatever else the request gets wrong.

For a request with a body, pass the body file to `sign.sh` too, and send that
same file with `curl --data-binary @body.json -H 'Content-Type: application/json'`.
If `sign.sh` fails, curl still runs, unsigned, and the answer is a `401`, so run
`sign.sh` alone first to see its error.

**4. Know what the account may do.** A key carries no authority of its own.
It signs as its account, and the account's standing and permissions decide every
request:

- **Standing** is checked first. A `pending` account reaches only
  `GET /api/v1/users/me`, and a `disabled` one reaches nothing.
- **Permissions** come next ([permissions](#permissions)). Each is one
  operation at one scope: a bare name such as `read_plans` reaches the
  caller's own organization and campaigns, and its `_any_` form reaches every
  one. Each route's security roles name the permission it requires.

[`GET /api/v1/users/me`](users/get_own_user.md) answers both questions for
the key that signs it. It returns `status`, the expanded `permissions`, and
`may`, which shows how far each of the account's authorities reaches, as the
routes themselves judge it:

| `may` field | What it covers | Value |
|---|---|---|
| `read_roster` | Organizations and the accounts on them | `none`, `own` or `every` |
| `administer_roster` | Accounts, the organization, its private benchmarks and its vault | `none`, `own` or `every` |
| `read_fleet` | Reading the devices in reach | `true` or `false` |
| `administer_fleet` | Administering devices | `none`, `own` or `every` |
| `edit_catalog` | Creating and changing org entries of the descriptor catalog | `none`, `own` or `every` |
| `read_plans` | Reading campaigns and their plans | `none`, `own`, `listed` or `every` |
| `file_plans` | Filing a plan into a campaign | `true` or `false` |
| `create_orgs` | Creating an organization | `true` or `false` |
| `grant` | The permissions this account may grant another account | list of names |
| `grant_service` | The names in `grant` that a service account may hold | list of names |

`own` means the account's own organization and the account itself. `listed`
means only the campaigns a campaign reader's `campaign_scope` names.

A refusal tells you which check failed:

- `401` means the signature or the key: a malformed or stale signature, a
  spent nonce, an `@authority` the server does not answer to, or a key that is
  unknown, expired or revoked.
- `403` means the account: it is `pending` or `disabled`, or it lacks the
  permission the route names, as in `user lacks the read_any_plans permission`
  ([status, then permission](#status-then-permission)).

An operator grants permissions with `pipette-mgmt users permission add`, or an
administrator of the account grants them in the app or at
[`PATCH /api/v1/users/{user_id}/permissions`](users/amend_permissions.md).

**Reading campaigns** takes `read_plans`, which `may.read_plans` other than
`none` shows. With it, list the campaigns in reach, then read the plan one runs
and its jobs and results by the ids the answers carry:

```bash
call 'https://mgmt.example/api/v1/campaigns?limit=200' | jq '.items[] | {campaign_id, name, current_plan_id}'
campaign_id=campaign_…                                    # the one you want, from the listing
plan_id=$(call "https://mgmt.example/api/v1/campaigns/$campaign_id/plan" | jq -r .plan_id)
call "https://mgmt.example/api/v1/plans/$plan_id/jobs?results=true&all=true" > jobs.json
call "https://mgmt.example/api/v1/plans/$plan_id/results.csv" > results.csv
```

The listing is a page of 50 campaigns unless `limit` asks for more, at most
200; its `next_cursor` fetches the next one
([pagination](../httpapi.md#13-pagination)). The routes are
[`GET /api/v1/campaigns`](campaigns/list_campaigns.md),
[`GET /api/v1/campaigns/{campaign_id}/plan`](campaigns/get_campaign_plan.md),
[`GET /api/v1/plans/{plan_id}/jobs`](plans/list_plan_jobs.md) and
[`GET /api/v1/plans/{plan_id}/results.csv`](plans/get_plan_results_csv.md).

## Signing in from a browser

**A browser holds a key, not a session.** The console app at `/app/` makes a
fresh keypair in the browser, keeps the private key non-extractable, and signs
every call RFC 9421 like any other user. No route takes a cookie, and the
browser holds no bearer credential: a Clerk token is spent once, to register
the browser's key ([signing in through Clerk](#signing-in-through-clerk)).
Standing is checked on every request, so disabling the account stops the
browser at its next call, and signing out revokes the key.

**A key may name only itself.** [`GET /api/v1/users/me`](users/get_own_user.md)
accepts a signature with no `X-Client-Id`, and answers the account the key is
registered to, so a holder of nothing but a key learns its account. A key a
device holds, or nobody, is `401 key not found`. An RFC 9421 signature names
only its key on every route.

The browser registers its key one of three ways:

| Path | The key's lifetime |
|---|---|
| Signed by a key the account already holds, at [`POST /api/v1/users/me/keys`](users/add_own_key.md) | The app asks for 7 days; the route refuses an `expires_at` more than 30 days away |
| A Clerk sign-in, at [`POST /api/v1/users/register/clerk`](users/register_clerk_key.md) | 7 days, set by the server |
| Sign-up with a user pre-auth key, at [`POST /api/v1/users/register`](users/register_user_key.md) | No expiry: it may be the account's only key, so the reader saves it first |

A terminal signs in the same way: a command-line client opens
`/app/login/cli` with its public key, and a signed-in reader who types the
code the terminal printed registers that key at `POST /api/v1/users/me/keys`.
The code is derived from the key, so it vouches only that the key shown is the
one the reader's terminal printed.

### Signing in through Clerk

With `[clerk]` set, [`POST /api/v1/users/register/clerk`](users/register_clerk_key.md)
registers a public key to the account a Clerk token names, sent as
`Authorization: Bearer`. The token is verified offline, and is a `401` unless
it carries:

- an RS256 signature under the instance's key, and an `iss` naming it;
- `exp` and `nbf` within five seconds of skew, and an `iat` inside the
  five-minute freshness window, whatever lifetime the template sets;
- an `azp` among the configured `authorized_parties`, and an `aud` of the
  configured JWT template's name (`pipette-mgmt` by default);
- no `act`: an impersonated session signs nobody in;
- an address with `email_verified` true.

Each token signs in once. **The address names the account**, which then passes
the standing check a signed request does. What else is refused:

| Status | When |
|---|---|
| 403 | The address is outside `allowed_domains` (its domain matched exactly), when set; the account is `pending` or `disabled`; it is a service account; no account holds the address and no provision rule covers its domain; or the rule's organization is disabled |
| 409 | The keypair is registered to another principal, or a claim on the address was left unfinished; retry |

An address no account holds, at a domain a provision rule names, gets an
account created `active` in that rule's organization, holding the `org_member`
group ([defaults](#defaults-and-groups)). A sign-in counts as the account's
first, so from then on no administrator can mint a pre-auth key that signs in
as it ([`POST /api/v1/users/{user_id}/preauth-keys`](users/mint_preauth_key.md)),
and a keyless [`POST /api/v1/users/register`](users/register_user_key.md) for
an address a rule covers is refused, so nobody can claim that account first.
Clerk vouches for addresses, so it reaches every account whose address it can
verify within `allowed_domains`, not only the ones it created.

**The first Clerk sign-in surfaces the keys already there.** An account an
administrator created can hold keys before its owner first signs in, such as
one a pre-auth key registered, and those keys sign as the owner. The first
sign-in through Clerk records when it happened and which keys the account held
then, besides the key it registers. [`GET /api/v1/users/me`](users/get_own_user.md)
answers that record as `first_clerk_sign_in`, and the app marks each of those
keys still registered as "registered before your first recorded sign-in", so
the owner can revoke the ones they do not recognise. When there were any, the
audit log gains a `first_clerk_sign_in` event naming them
([Organizations: The audit log](orgs.md#the-audit-log)). Nothing is revoked or
refused, and a failure to record leaves the next sign-in the first. Later
sign-ins change neither the record nor the log, and an account the sign-in
creates holds no key before it. An account that signed in through Clerk before
the record existed gets one at its next sign-in, which lists its own earlier
keys too.

Only the keys held at that sign-in are marked. A key from before it can still
add keys through `POST /api/v1/users/me/keys` afterwards, and those carry no
mark: the audit log names the key that signed each such request.

## Permissions

A **permission** names one operation at one scope, and a principal holds a
**set** of them, never a role. Each route's requirement is its security role in
this document, and each name's meaning is in
[what each name opens](#what-each-name-opens); this section is the model they
follow. Sets travel as sorted `snake_case` arrays, `[]` when empty, never an
absent key, except `stored_permissions`, which a caller that does not
administer the account never reads ([reading a set](#reading-a-set)).

- **Each name belongs to one kind.** `submit_results` and `perform_work` are a
  device's ([device permissions](devices.md#device-permissions)); every other
  name is an account's. A route that requires a name therefore admits one kind:
  a device's key on an account route is refused at the header (`401`, or `403`
  at [`POST /api/v1/plans/validate`](plans/validate_plan.md)), before the body
  is read. A grant naming the other kind's permission is a `400`, and such a
  name stored by an older release grants nothing.
- **Scope.** A bare name acts on the caller's own: its and its organization's
  campaigns and their plans, its organization's roster and devices, the fleet
  its plans can reach. The `_any_` form acts on every one. **A plan has no
  owner; its campaign does**, as the campaign's owner stands now, so handing a
  campaign over hands over its plans, and who filed a plan grants nothing
  ([who may do what](campaigns.md#who-may-do-what)). A plan filed outside every
  campaign is its filer's.
- **Implication.** A name includes the names below, and nothing else is
  implied. `cancel_plans` comes only from `cancel_any_plans`: a cancellation is
  destructive and is granted on its own, so no read or submit grant buys it.

| Name | Also grants |
|---|---|
| `submit_any_plans` | `submit_plans`, `read_plans` |
| `submit_plans` | `read_plans`: a filer reads its plan back |
| `read_any_plans` | `read_plans` |
| `cancel_any_plans` | `cancel_plans` |
| `read_any_clients` | `read_clients` |
| `manage_clients` | `read_clients` |
| `manage_any_clients` | `manage_clients`, `read_clients` |
| `read_any_orgs` | `read_orgs` |
| `manage_orgs` | `read_orgs`: an administrator reads back what it changes |
| `manage_any_orgs` | `manage_orgs`, `read_any_orgs`, `read_orgs` |
| `edit_any_catalog` | `edit_catalog` |

`read_plans`, `cancel_plans`, `read_clients`, `read_orgs` and `edit_catalog`
imply nothing.
`read_any_orgs` does not reach the vault ([the vault](vault.md)), and roster
routes that return device records need `read_clients` as well
([organizations](orgs.md)).

### Status, then permission

Every route checks, in order, and stops at the first refusal:

1. **Authentication**: a missing or failing signature is `401`.
2. **Status**: `403` with `user is pending an operator's approval` or
   `user is disabled` (a device: `client is not approved`). A pending account
   reaches [`GET /api/v1/users/me`](users/get_own_user.md) alone, so it can
   watch for its admission.
3. **Permission**: `403` with `user lacks the {permission} permission`, or, for
   a name the account's type can never hold,
   `a service account cannot hold the {permission} permission` or
   `a campaign reader holds read_plans alone, not {permission}`. A campaign
   route that admits either of two names answers its own text naming both.
   Naming the missing name discloses nothing: requirements are public. Before
   this step, a service account is refused on an administration route and
   whenever it belongs to no organization, and a campaign reader on a route
   that needs no permission.
4. **The object**: scope, rank, and any second grant. A plan, an account or
   an organization outside the caller's scope reads as `404`, the same as one
   that does not exist. A campaign, a vault resource or a device being
   administered is a `403` that names the scope, and `cancel` gives a
   `cancel_plans` holder the same `403` for a plan outside its scope and for an
   absent one, so it cannot probe for plan ids. Each tag states its rule.

The first three read the request headers alone, so a refused caller's body is
never read. The set is read on every request, so a grant or a revocation takes
effect on the next one.

Some rights need no permission: anyone browses the public benchmark catalog,
and an active account reads its own record and keys, revokes its own keys
and, unless it is a service account, adds one, and reaches its own vault
resources and, by membership, its organization's catalog and vault resources.
A campaign reader still reads its own record and gets the public catalog. It
is refused (`403`) on the routes that add or revoke its own keys and on its own
and its organization's vault resources and private catalog, since they scope by
owner or membership rather than by its campaigns.

### Defaults and groups

**An account holds nothing until it is granted.** Nothing is inferred at read
time: a create states its set ([`POST /api/v1/users`](users/create_user.md)), an
administrator changes it
([`PUT`](users/set_permissions.md) and
[`PATCH /api/v1/users/{user_id}/permissions`](users/amend_permissions.md)),
and a requested account holds nothing until an operator admits and grants it. A
user pre-auth key carries no grant, and registering a key never changes the
set. A device's registration holds `submit_results` unless the client
pre-auth key it used carries a template
([client pre-auth keys](devices.md#client-pre-auth-keys)).

A **group** is a spelling of a set, expanded where it is given: the host
commands, and the `org_member` a Clerk provision rule grants. Nothing records
it, so widening a group widens no set already written, and the HTTP grant
routes take names alone.

| Group | Names |
|---|---|
| `org_member` | `submit_plans`, `read_plans`, `cancel_plans`, `read_clients`, `edit_catalog` |
| `org_admin` | `org_member`'s, plus `read_orgs`, `manage_orgs`, `manage_clients` |
| `server_admin` | `submit_any_plans`, `read_any_plans`, `cancel_any_plans`, `read_any_clients`, `manage_any_orgs`, `manage_any_clients`, `edit_any_catalog` |

### Reading a set

[`GET /api/v1/users/me`](users/get_own_user.md) answers the caller's
`permissions` resolved, the table above expanded, and `may`, how far each
authority reaches as the routes judge it; a client draws from `may` rather than
re-deriving the rules. On `UserView`, `permissions` is resolved too, and leaves
out what the account's type cannot hold, so a service account's stray admin
name shows nothing it implies. `stored_permissions`, the set as granted
including names this build does not know, is served only to a caller whose
`manage_orgs` or `manage_any_orgs` reaches the account.

### What each name opens

Each name belongs to exactly one kind, and a name a device holds is never
grantable to an account, nor the reverse.

| Name | Kind | Opens |
|---|---|---|
| `submit_results` | device | Submit an ad-hoc result for the caller's own device, with a server-minted `job_id`. |
| `perform_work` | device | Take queued work: claim, heartbeat and reclaim jobs, fetch their corpus and the vault artifacts the device's owner owns, and report a claimed job. |
| `submit_plans` | account | File and check plans, attributed to the caller. A plan goes into a campaign of the caller or of its organization, and may name the vault resources of the caller and of the organization. Change the note of a plan in a campaign the caller may file into, or of one the caller filed outside a campaign. Create campaigns. Rename, hand over and delete the caller's campaigns. |
| `submit_any_plans` | account | The same, where a job may name any organization's private benchmark and any vault resource. Nothing more: other owners' campaigns take `read_any_plans`, `manage_any_orgs` and `cancel_any_plans`. |
| `read_plans` | account | Read the plans of the campaigns the caller reads, and those it filed outside any campaign. |
| `read_any_plans` | account | Read every plan, and every model-params row. |
| `cancel_plans` | account | Cancel the plans of the caller's own and its organization's campaigns, and those it filed outside any campaign. |
| `cancel_any_plans` | account | Cancel every plan. |
| `read_clients` | account | Read the devices the caller's plans can reach, and the fleet detail in plan reads and plan documents ([fleet disclosure](devices.md#fleet-disclosure)). |
| `read_any_clients` | account | The same over the whole fleet. |
| `read_orgs` | account | Read the **roster** of the caller's own organization: the organization, its accounts, what each holds, and which devices belong where. A roster read that returns device records needs `read_clients` as well. |
| `read_any_orgs` | account | The same for every organization and account, and every organization's private catalog. |
| `manage_orgs` | account | Administer the caller's own organization: its people, its vault resources, its private benchmarks and its campaigns' records. **The delegation grant.** |
| `manage_any_orgs` | account | The same everywhere, plus creating organizations and writing every model-params row. `server_admin` only. |
| `manage_clients` | account | Administer the devices the caller's own organization holds: approve, reject, update, delete, set the owner, grant device permissions, and mint the client pre-auth keys that register a device into it. |
| `manage_any_clients` | account | The same for every device. The only grant that moves a device between organizations. `server_admin` only. |
| `edit_catalog` | account | Create and change org entries of the descriptor catalog in the caller's own organization. A service account may hold it. A member reads the org's entries without this grant. This grant changes no global entry. |
| `edit_any_catalog` | account | Create and change org entries of the descriptor catalog in every organization, and read the entries of every organization. A holder that belongs to an organization sees the entries of that organization with bare names. A service account cannot hold it. This grant changes no global entry. |

## Account administration

Listing, creating, admitting and disabling accounts, granting what they hold,
and the keys they sign in with. **This is the delegation surface**: an
organization onboards its people without a server admin. `pipette-mgmt users`
is the shell-side equivalent.

That covers the credentials too. An administrator mints the pre-auth key a new
account registers its first machine with, so an organization onboards its
people on its own ([`POST /api/v1/users/{user_id}/preauth-keys`](users/mint_preauth_key.md)),
and reads and revokes the keys each account signs in with
([`GET /api/v1/users/{user_id}/keys`](users/list_user_keys.md),
[`DELETE /api/v1/users/{user_id}/keys/{key_id}`](users/revoke_user_key.md)).

These are the **roster** grants, the same ones that serve `/api/v1/orgs`: an
organization and the accounts on it are one thing to read and one thing to
administer, so they share a name rather than each having its own. Both are
scoped to the caller's own organization, with `read_any_orgs` and
`manage_any_orgs` as their server-wide forms.

The routes that answer an account carry `UserView`
([The OpenAPI document](../httpapi.md#15-the-openapi-document)): its record and
two sets, `permissions`, the resolved set, and `stored_permissions`, the set as
stored, which a `remove` reaches
([`PATCH /api/v1/users/{user_id}/permissions`](users/amend_permissions.md)); a
name in the first and not the second is held through a wider one.
[`PATCH /api/v1/users/{user_id}`](users/update_user.md) and
[`PUT /api/v1/users/{user_id}/permissions`](users/set_permissions.md) answer
`ChangedUserView`, the same plus `changed`
([Write answers](../httpapi.md#16-write-answers)).
[`GET /api/v1/users/{user_id}`](users/get_user.md) answers `UserDetail`, which
adds what the reader may do to the account, as the writes judge it
([the rules that make delegation safe](#the-rules-that-make-delegation-safe)). A
reader whose `manage_orgs` covers the account's organization, even one
outranked, also gets `awaits_sign_in`: `true` while nobody has signed in as the
account and it holds no key, what a pre-auth mint for a person
([`POST /api/v1/users/{user_id}/preauth-keys`](users/mint_preauth_key.md)) and a
retype to a service
([the rules that make delegation safe](#the-rules-that-make-delegation-safe))
both need.

## The rules that make delegation safe

**Scope.** A caller holding the bare verb acts only on accounts in its own
organization. An account outside it reads as **absent**, not refused, so the
route cannot be used to probe the server's roster one id at a time. An account
belonging to **no** organization is outside every scoped caller's reach: it may
be a server admin, and letting an org admin adopt one would be a takeover in two
calls. A server admin who is a member is covered by rank, below. Onboarding goes
through [`POST /api/v1/users`](users/create_user.md), which creates into the
caller's own organization, and a pre-auth key minted for the account it created
([`POST /api/v1/users/{user_id}/preauth-keys`](users/mint_preauth_key.md)).

**Rank.** A scoped caller changes no account holding a server-wide (`_any_`)
name it does not hold itself. `PATCH /api/v1/users/{user_id}`, both permission
verbs, [`PUT /api/v1/users/{user_id}/org`](users/set_user_org.md), the pre-auth
key mint and revoke, and a key's revocation answer `403` naming those names,
and write nothing. Membership does not keep a server admin out of an
organization: a Clerk provision rule puts every account at its domain into one,
and a promotion keeps the membership. The account stays readable, so it is
refused rather than absent; its names are already on the caller's roster. A
name that is not server-wide does not count, so an org admin still offboards a
member holding plan grants it lacks, and an unknown name does not count either.
A `manage_any_orgs` caller is not bounded by rank. Both permission verbs weigh
the set again as they swap it, so a server-wide grant landing in between is kept
rather than overwritten.

**Ceiling.** A caller may grant only names it holds, on every path that grants:
create, replace and amend. The check reads the **resolved** set, so a wider verb
authorizes the narrower ones it implies, and the refusal names every name it
refused. Without this, delegation would be privilege escalation with extra
steps: an org admin could hand itself `manage_any_orgs` and own the server.

The ceiling applies to `add` only, never `remove`. Revoking narrows an account,
and an org admin that could not revoke a grant it did not make could never clean
up after one. Rank still limits whose names it may revoke.

**A pre-auth key onboards; it never takes over.** Whoever holds one registers a
key that signs as the account, and some of what an account reaches is its own
rather than a grant: its own vault resources, its campaigns, the plans it
filed. So an administrator mints one only for an account **nobody has signed in
as yet**, and only when the caller holds every name in the account's resolved
set and the account holds no name this build cannot compare. Registration
checks all of it again
([`POST /api/v1/users/{user_id}/preauth-keys`](users/mint_preauth_key.md)).

"Nobody has signed in" is recorded, not inferred from the keys: a Clerk sign-in
registers none
([signing in through Clerk](#signing-in-through-clerk)),
and a holder may revoke its own. An operator's create marks the account, and its
first key registration or Clerk sign-in clears the mark for good. Stripping a
member's grants first therefore does not help. An account that has signed in,
or that predates the mark, is let back in on the host
(`pipette-mgmt users preauth create`) if its holder loses every key.

**A service account takes a key at any time.** It is its organization's, not
a person's, so there is nobody to take it over from: its administrator mints it
a pre-auth key whenever the job needs one, still bounded by the caller's
grants. That is why [`PATCH /api/v1/users/{user_id}`](users/update_user.md)
makes a person's account a service only while nobody has signed in as it (the
record's `awaits_sign_in`), a `409` otherwise; the host's `users set-type` covers the rest. Making a service a
person's is always open.

**A service account administers nothing**
([accounts and keys](#accounts-and-keys)). Every account
administration route but the two reads (`GET /api/v1/users` and
`GET /api/v1/users/{user_id}`), the organization writes of
[Organizations](orgs.md), the device administration of
[Device administration](devices.md), and the private catalog's `PUT` and
`DELETE` refuse it with a `403`. Granting it `manage_orgs`, `manage_clients` or
an `_any_` name is a `400`, and making an account holding one a service is a
`409`. It is created into an organization, and is never released from it or
retyped into none (`400`); any other route refuses it with a `403` while a
record still leaves it in none. On the plan grants it is a member like any
other ([who may do what](campaigns.md#who-may-do-what)).

**A campaign reader reads its campaigns alone.** A service account with a
`campaign_scope` reads the campaigns it names and their plans while its own
organization owns them, and nothing else: not its organization's other
campaigns, not a personal campaign, nor any plan filed outside a campaign. A
campaign handed to another organization drops out of its reach though the scope
still names it. It is what a job that publishes results signs as.
[`POST /api/v1/users`](users/create_user.md) creates one, and
[`PATCH /api/v1/users/{user_id}`](users/update_user.md) makes a service account
one or replaces a reader's campaigns.

- It holds `read_plans` alone. Granting it any other name is a `400`, making an
  account that holds one a reader is a `409`, and a name stored before counts
  for nothing, as an admin name does for a service account.
- The scope is replaced, never removed, and a reader is never retyped as a
  person's (`400`): one write must not widen a reader into a member. A wider
  account is a new account.
- Each campaign it names is one whose plans the caller reads (`403`
  otherwise), so the ceiling holds for campaigns as for names, and one its
  organization owns (`400` otherwise). A campaign that does not exist is a
  `400`, and a scope names at most 100.
- The campaign reads and the plan reads, `results.csv` included, answer the
  listed campaigns' records and plans, `scope=mine` too; `GET /api/v1/users/me`
  answers `listed` for `may.read_plans`. A route open to any active account
  scopes itself by owner or membership, the vault and the private catalog, so
  it refuses a reader with a `403`, and `GET /benchmarks` serves it the public
  catalog alone.
- A pre-auth key signs in as the reader, so only a caller who reads every
  campaign it lists mints one, and registration checks it again (`403`): an
  administrator of its organization does not take over a scope someone wider
  set.

**A rollback past the scope widens every reader.** A build older than the scope
does not read it, and serves a reader as a member of its organization holding
`read_plans`, from its first request. Before you deploy such a build, disable
every reader (`users list` shows the service accounts, `users show` a scope,
`users disable` stops one); a disabled account stays refused on every build.
[Reverting the SQL migration](../storage.md) that adds the scope is refused
until no reader exists.

An account **cannot disable itself**, nor remove its own `manage_orgs` or
`manage_any_orgs`, nor, scoped to an organization, release itself from it
(`400` each), so a caller cannot lock itself out of the surface that would
undo it.

**No route deletes an account.** Offboarding is a disable: the account's
campaigns, vault resources and filed plans keep naming a principal that exists,
where a delete would leave them under an id that never resolves again. The host's
[`users delete`](../cli.md#pipette-mgmt-users-createlistshowapproveset-typeset-campaignsdisableenabledeleterelease-claimset-org)
remains for an account created in error. It refuses while the account owns a
campaign or a vault resource, and while it owns a device unless `--detach`
releases the devices.
