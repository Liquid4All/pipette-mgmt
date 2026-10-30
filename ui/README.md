# Console app

A Svelte app, served by pipette-mgmt at `/app/`, and a client of the JSON
routes like any other: each page is the routes it reads, and the server makes
every permission and scope decision. Route contracts are in
[httpapi.md](../docs/httpapi.md); building and the dev server in
[development.md](../docs/development.md#3-the-console-app).

## Serving

`pipette-httpapi`'s `build.rs` embeds `ui/dist` into the binary. The server
answers every path under `/app/` with the same page, which matches the address
against `src/lib/routes.ts` and draws that page, so a page is a bookmark and a
reload stays on it. `GET /` and `GET /app` answer `308` to `/app/`, keeping the
query, and `/app/` draws `/app/me/campaigns` in place, so Clerk's `?clerk=1`
survives. The page and its assets hold no data and are served to anyone.

## Signing in

There is no cookie and no session. The app makes an Ed25519 key for this
browser, keeps it in IndexedDB as a non-extractable `CryptoKey`, and signs
every call with it (RFC 9421, `src/lib/httpsig.ts`). `GET /api/v1/users/me`,
signed with the key alone, names the account. A `401` on any call means the
key lapsed or was revoked, and the frame says so in place of the page. A
pending account reads nothing else, so the frame draws one notice for it.

The browser's key is registered one of four ways (`src/lib/signin.ts`): signed
by a private key file the reader picks, by the
`pipette-ctl auth keys add --expires-in 7d` command the page prints, through
Clerk where `[clerk]` is set and the host is one `authorized_parties` names, or
at sign-up with a `user_preauth_…` token. The first three keys expire in 7 days
and signing out revokes them; **Sign in again with Clerk** replaces a lapsed
one. A `pipette-ctl` too old for the flag adds a key that never expires, so the
page registers a second that does and revokes the first. A sign-up key does not
expire, so the reader saves it as `id_ed25519`. `/app/account/keys` registers
and revokes the account's keys. `/app/login/cli` approves the key
`pipette-ctl auth login` names, once the reader types the terminal's code (the
key's first 8 SHA-256 hex digits, as `XXXX-XXXX`), with no expiry or 30 days;
`pipette-ctl` polls `GET /api/v1/users/me` with that key until then.

A key the classic console left in `localStorage` (for a sign-up key, perhaps
the only copy) is forgotten once the server answers `401` `key not found`,
`key expired` or `user not found`, or once losing it costs nothing; otherwise
the app offers to sign in with it or save it, as `id_ed25519` first.

The security model is
[authentication.md §5.12](../docs/authentication.md#512-signing-in-from-a-browser).
Ed25519 in WebCrypto needs Chrome 137+, Safari 17+ or Firefox 129+, in a
secure context.

## Pages

| Address | Draws |
|---|---|
| `/app/me/campaigns`, `…/vault`, `…/vault/runtimes`, `…/devices` | The campaigns, vault models and runtimes, and devices the reader owns |
| `/app/account`, `…/keys` | The reader's record and session, and its keys |
| `/app/login/cli` | The approval of a `pipette-ctl auth login` key |
| `/app/campaigns`, `…/new`, `…/{campaign_id}`, `…/{campaign_id}/document` | The campaigns in reach, the create form, one campaign (it opens on the plan it runs), and its plan document editor. Each campaign listing hides archived campaigns unless its `state` filter asks for `archived` or `all`; a campaign is archived and unarchived on its Settings |
| `/app/campaigns/{campaign_id}/plans/{plan_id}/…` | That plan's tabs, below, with the campaign's record on History and its writes on Settings. An earlier plan offers a revert to it, as each earlier row of History does. The page also lets the reader restore a variant into the current plan. The reader can append it or replace a variant |
| `/app/plans`, `…/{plan_id}`, `…/cells`, `…/analysis`, `…/analysis/{report}`, `…/revisions`, `…/settings` | The plan listing, and a plan's Overview, Jobs, Results (one report's charts, or `swap` or `missing`), History and Settings. `…/results` and `…/reports…` redirect |
| `/app/vault`, `…/runtimes`, `…/new`, `…/{resource_id}` | The vault resources in reach, the upload page, one resource |
| `/app/benchmarks`, `…/new`, `…/{org}/{benchmark_id}` | The private benchmarks the reader reads, the form, one entry |
| `/app/clients`, `…/{client_id}` | The fleet, and one device with its forms. For a fleet reader, the device page also shows the catalog short list |
| `/app/users`, `…/new`, `…/{user_id}`, `…/{user_id}/plans`, `…/permissions`, `…/keys`, `…/preauth-keys`, `…/settings` | The roster, the create form, and one account's tabs |
| `/app/orgs`, `…/new`, `…/{org}` | The organizations, the create form, one organization's overview |
| `/app/orgs/{org}/campaigns`, `…/vault`, `…/vault/runtimes`, `…/benchmarks`, `…/members`, `…/devices` | One organization's listings; its device pre-auth keys sit on `…/devices` |
| `/app/server/users`, `…/devices`, `…/model-params` | Accounts in no organization, devices nobody owns, and the parameter counts in the model parameter table |
| `/app/server/catalog`, `…/catalog?kind=model`, `…/catalog/{name}` | The global catalog's runtime entries and model entries, and one entry. A model entry shows its members as a table of formats and quantizations, with their parameter counts |

`{org}` takes a handle or an id. The tree (`src/lib/tree.ts`) groups the pages
by owner: **Me**, each **Organization**, and **Server**. A branch narrows inside
the reader's reach and never past it. The listings over the whole reach
(`/app/campaigns`, `/app/vault`, `/app/benchmarks`, `/app/clients`,
`/app/users`) are not in the tree, but still open. A record's page sits under
the branch that holds it.

## Rules every page follows

- A refusal is drawn in place, as the route words it, at its status: in place
  of the page for a read, above or under the form for a write, in the dialog
  for a danger-zone write. The app enforces nothing. A hidden leaf or button
  and the `may_*` flags only save the reader a refusal
  ([Campaigns: Who may do what](../docs/api/campaigns.md#who-may-do-what)).
- A listing's filters and sort (`sort`, `-` first for down) live in the
  address, so a bookmark reproduces it. An empty value is no filter, as on the
  routes ([httpapi.md §1.3](../docs/httpapi.md#13-pagination)). A chosen value
  no row has stays listed at 0, so it can be let go of.
- A listing reads every page of its route, then filters, sorts and pages in
  the browser; a filter the server reads (a plan's scope, the fleet's clauses)
  reads it again. It is one table: a column is a record field under its own
  name, a list or an object as JSON, the first column the id linking to its
  page. The reader picks the columns, and the browser keeps the pick. The
  reader drags the right edge of a header to set the width of a column, and
  the browser keeps the widths. Text past the edge of a column ends in an
  ellipsis. **Default widths** gives the table its own layout again.
- An id is a link. A submitter or an owner reads as its address or handle from
  `names`, with the id underneath
  ([httpapi.md §1.4](../docs/httpapi.md#14-names)).
- An owner field is an autocomplete over the record's `…/owners` lookup
  ([`GET /api/v1/clients/{client_id}/owners`](../docs/api/devices/list_client_owners.md)),
  and posts only the id of the owner picked or named exactly.
- A record's writes are settings rows: a General section, and a danger zone
  whose write runs once the reader types its phrase. Each write reads the
  record again.
- A plan's tabs are one screen, fed by one read of its jobs with
  `results=true&all=true` ([`GET /api/v1/plans/{plan_id}/jobs`](../docs/api/plans/list_plan_jobs.md#the-plan-pages-body)).
  The Overview counts its summaries over that read in the browser. Its
  **Scorer** row lists each scorer version on the eval rows, with the number
  of jobs that the version scored. The Jobs tab shows the version of each row
  in its **scorer version** column.
