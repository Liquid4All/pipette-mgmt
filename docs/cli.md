# CLI Reference

## Binary

The single binary is called `pipette-mgmt`. `pipette-mgmt <command> --help` is
the reference for each command and flag. This page holds what the help cannot:
how the config is found, the behaviour that spans commands, and the contracts a
script or an operator relies on.

The CLI runs on the host. Commands that read or change records use direct storage
access, except `plans results`, which reads over HTTP from pipette-duckdb. None
signs a request or checks a permission: shell access to the storage root is
already the authority. `config check` reads only the config file, and `openapi`
reads neither the config nor storage.

## Global flags

`--config <path>` (env `PIPETTE_MGMT_CONFIG`) names the config file. It can go
before or after a subcommand. Without either, the binary reads
`/etc/pipette-mgmt/config.toml` if it exists, otherwise
`pipette-mgmt/config.toml` in the platform config directory (`~/.config` on
Linux, `~/Library/Application Support` on macOS).

`--log-format <text|json>` (env `PIPETTE_MGMT_LOG_FORMAT`, default `text`) sets
the shape of the log lines; see [Logging](#logging).

## Logging

Every command writes its log to stderr, so stdout carries only what the command
prints: a table, a summary, or a JSON report a script can parse.

`RUST_LOG` takes
[`EnvFilter` directives](https://docs.rs/tracing-subscriber/0.3/tracing_subscriber/filter/struct.EnvFilter.html#directives)
and defaults to `info`. A malformed value fails the command. A filter that drops
`info` also drops the request span, and with it the identifiers that every event
written during a request inherits.

`--log-format json` writes one JSON object per line, for a log collector. An
event's own fields sit beside `message`, and `spans` lists the spans it was
written in, outermost first. A request's span carries `method`, `path`, and the
identifiers its handler resolved:

```json
{"timestamp":"2026-09-28T18:02:51.834549Z","level":"WARN","message":"returned a server-generated private key in the registration response","target":"pipette_httpapi::client::clients","spans":[{"client_id":"ev1_6302…","method":"POST","path":"/clients/register","name":"request"}]}
```

To see the queries a slow request made, set
`RUST_LOG=info,sqlx::query=debug,pipette::duckdb=debug`: sqlx logs each SQL
statement with its `elapsed`, and `pipette::duckdb` each pipette-duckdb query
with its `elapsed_ms`, row count and SQL cut to 300 characters. With
`--log-format json`, each line's `spans` names the request it belongs to.
pipette-duckdb logs the queries it serves on its own side too, at
`RUST_LOG=pipette_duckdb=debug`.

The request span's `client_id` is always the caller: the device or account that
signed, or the one registering. A route that acts on a device, such as an
approval, a permission write or an owner change, names the device in
`target_client_id`. An event's own `client_id`, beside `message`, is a separate
field and may name the device.

## Configuration file

`pipette-mgmt` reads every setting from one TOML file, found as
[Global flags](#global-flags) describes. The doc comments on `Config` in
[`crates/pipette-core/src/config.rs`](../crates/pipette-core/src/config.rs) are
the reference for each key: its meaning, default and bounds.
[`examples/config.toml`](../examples/config.toml) is the annotated example, with
every key at its default.

A value outside its bounds fails the load, before the command does anything. So
does a key this build renamed (`require_preauth_key`), with an error naming the
new spelling: the renamed key defaults to the open setting, so reading past it
would silently reopen keyless registration.

Credentials stay out of the file, with one exception:

- S3 sections read the environment: `AWS_ACCESS_KEY_ID`,
  `AWS_SECRET_ACCESS_KEY`, `AWS_REGION` / `AWS_DEFAULT_REGION`, or an EC2/ECS
  instance or task role.
- `[database]` takes its password from `PGPASSWORD` alone. Any other connection
  key it omits falls back to its libpq `PG*` variable.
- `[duckdb]` is the exception: its `user` and `password` are read from the file.

## Commands

| Command | For |
|---|---|
| [`config check`](#pipette-mgmt-config-check---strict) | Validate the server config offline and report missing catalog offer rules |
| [`serve`](#pipette-mgmt-serve) | The HTTP server |
| [`process-submissions`](#pipette-mgmt-process-submissions-alias-score) (`score`) | The fast scoring pass, from cron |
| [`score-eval`](#pipette-mgmt-score-eval) | The slow eval scoring pass, from cron |
| [`queue-maintenance`](#pipette-mgmt-queue-maintenance) | Reconcile the `todo/` queue, from cron |
| [`dataset-cache`](#pipette-mgmt-dataset-cache-probebuildpins) | Keep upstream dataset corpora servable, and author their pins |
| [`plans`](#pipette-mgmt-plans-liststatusresultscancel) | Read and cancel plans |
| [`campaigns`](#pipette-mgmt-campaigns-createingestvariantsrevisererunrevertlistshowrenameownerdelete) | File and revise plans through campaigns |
| [`catalog`](#pipette-mgmt-catalog) | Manage readable names for runtime and model descriptors |
| [`clients`](#pipette-mgmt-clients-list---tag-tag) | Devices: lifecycle, tags, permissions, owners, pre-auth keys, suspensions |
| [`users`](#pipette-mgmt-users-createlistshowapproveset-typeset-campaignsdisableenabledeleterelease-claimset-org) | Accounts: lifecycle, permissions, keys, pre-auth keys |
| [`orgs`](#pipette-mgmt-orgs-createlistshowupdatedisableenabledeleterelease-claimmembersclientsbenchmarks) | Organizations, their rosters and private benchmarks |
| [`vault`](#pipette-mgmt-vault-listshowdeleteprune) | Read and delete vault records and files |
| [`models`](#pipette-mgmt-models-resolveunresolvedimport) | Coverage of the model parameter table, and rows from a file |
| [`fix-model-param`](#pipette-mgmt-fix-model-param), [`fix-canonical`](#pipette-mgmt-fix-canonical), [`fix-vault-labels`](#pipette-mgmt-fix-vault-labels) | Rewrite warehouse columns in place |
| [`requeue-eval`](#pipette-mgmt-requeue-eval) | Re-score an eval benchmark's history |
| [`unverified`](#pipette-mgmt-unverified) | The held-submission archive |
| [`runtime-output`](#pipette-mgmt-runtime-output-job_id) | A job's runtime stdout and stderr |
| [`reindex`](#pipette-mgmt-reindex) | Rebuild the reverse tag index |
| [`flag-reindex`](#pipette-mgmt-flag-reindex) | Force an eligible-index rebuild |
| [`unlock`](#pipette-mgmt-unlock) | Inspect or clear the storage mutate lock |
| [`db`](#pipette-mgmt-db-migrate---to-version---dry-run-and-db-check) | The `[database]` schema, and record copies to and from it |
| [`audit`](#pipette-mgmt-audit-listprune) | Read the audit log, and prune it past its retention |
| [`openapi`](#pipette-mgmt-openapi---site-dir) | Print the OpenAPI document, or write the API reference page |

If `[database]` is set, each run of a command that changes records creates an
audit event ([Organizations: The audit log](api/orgs.md#the-audit-log)).
The event contains the subcommand and its arguments, the operator and hostname
of the host, and the outcome. The audit log keeps the arguments for two years.
A reader with `manage_any_orgs` can read them. The audit log also records the
cron passes.
These commands make no audit event:

- A read.
- `config check`, which reads only the config file.
- A `--dry-run`.
- `serve`, because the server records its own requests.
- `openapi`, which prints the binary's own document.
- `unlock`, `db migrate`, `db check` and `db export`, because a storage lock,
  the schema and an export change no records. `db import` and
  `db backfill-job-cells` change records, so they make an event.

An event is in the log of each organization that `--org` names. It is also in
the log of each organization that owns a record the arguments name by id.

The command writes the audit event when it ends. A command ends when it
returns, fails, or panics, or when a `SIGINT` or a `SIGTERM` stops it. If the
write fails, the command logs the failure and keeps its result.

A signal stops a command that changes records at its next wait. The command
then writes its event, which records a failure, and exits with status 130. It
waits at most ten seconds for the write, and a second signal stops it at once.
A `SIGKILL` leaves no event. A command that makes no audit event stops on a
signal as usual.

## Subcommands

### `pipette-mgmt config check [--strict]`

Parse and validate the server config. The command does not start services. It
does not contact storage, databases, or other services, and it needs no runtime
credentials. Use `pipette-mgmt config check --config <path>` before you deploy a
config change. Run the check with the same `pipette-mgmt` build that the
deployment runs. The known flavors and the config fields are part of the build.

The command exits with a nonzero code on invalid TOML and on every error from
`Config::load`. Examples are an unknown flavor in the descriptor catalog and a
capability flag that is not canonical. A pass does not run the startup checks
that `serve` runs against storage and the database.

`Config::load` ignores a key that no config field reads, and the default for that
field applies. The check produces a warning for each such key, for example
`evals_server_ur` or a section header `[storag]`. The check cannot see a key
inside `[storage]`, `[auth_storage]`, or `[todo_storage]`.

Each known flavor without an offer rule produces a warning that names the runtime
type and flavor. To offer that flavor, add a `[[descriptor_catalog.offer_rules]]`
entry. A flavor without a rule stays unavailable, because the catalog denies each
flavor that has no rule. Warnings do not fail the default check. Pass `--strict`
to fail if any known flavor lacks a rule or any key is unknown. The command
reports every warning before it exits.

### `pipette-mgmt serve`

Starts the HTTP server. Before it binds, it loads the benchmark catalog, runs
`db check` when `[database]` is set, and probes the backends it renames and
writes against ([operations.md §5.2](operations.md#52-aws-s3)). Stopping it:
[operations.md §2.1](operations.md#21-stopping-the-server).

### `pipette-mgmt process-submissions` (alias `score`)

The fast pass. It routes eval submissions to the score-queue, scores the rest,
records failures, and finalizes evals `score-eval` has scored. It never calls
the scoring service, so it holds the [mutate lock](#concurrency) only briefly.
It prints a summary line naming each disposition and exits non-zero if any job
failed. `score` is a visible alias, so an existing cron keeps working.

A failure submission gets one warehouse row per metric its job intended to
measure, with a null `value` ([storage.md §5](storage.md#failure-rows)). Rows
copy the submitted `model_name` and `model_quant` for every runtime; nothing
derives them from a repo id.

A pass that wrote rows for a model with no model-params row ends by naming it,
once per pass, on stdout and as a `warn`:

```
2 model(s) have no model-params row, so 1284 row(s) kept the parameter counts the
client reported: k2-horizon-3.7b, minicpm5-2b. Add a row for each, then backfill the
rows with `fix-model-param`.
```

Models are tallied under their normalized model key and counted in warehouse
rows, as `models unresolved` counts them. The pass still succeeds: an unknown
model falls back to the client's counts
([storage.md §5](storage.md#model-parameter-table)). The fix is the
[models repair loop](#models-unresolved-the-audit-after-scoring).

### `pipette-mgmt score-eval`

The slow pass, and the only scorer of eval benchmarks: without it, eval jobs
queue but are never scored. It drains the score-queue, calls the scoring service
for each job, and stages the result for the next `process-submissions` to
finalize. It holds its own `score-eval` lock, never the mutate lock
([Concurrency](#concurrency)).

### `pipette-mgmt queue-maintenance`

Reconciles the `todo/` queue and exits. [operations.md §3.1](operations.md#31-job-queue-maintenance)
covers its cadence, why runs must not overlap, and its log lines;
[queue.md](queue.md) is the protocol it maintains. Each run, in order:

1. Recycles expired leases to `avail/`, counting each as a lost lease of its
   job, and retires a job whose losses reach `plan_lost_lease_limit` as
   `lease_lost`.
2. Ends the jobs of every plan that owes teardown: one latched `cancelling`, and
   one whose manifest is stranded in `creating`. A job another plan still wants
   is released and stays claimable.
3. Converts jobs past their `expires_at` into synthetic `"system"` failure
   records, written create-if-absent to `submissions/incoming/` for the next
   `process-submissions`.
4. Retires the same way a job no possible client can match: a malformed clause,
   or a positive clause contradicting a negative one.
5. Escalates the same way a job whose closed `clients` roster is fully denied.
6. Indexes new jobs into `eligible/`, and rebuilds flagged clients.
7. Collects orphaned `eligible/` and `denied/` markers.
8. Refreshes each plan's `status` and `progress_snapshot`.
9. Collects the content keys and membership markers of jobs and plans that are
   gone, after `plan_creating_max_age_secs` and a second sighting, since a
   mid-flight ingest's keys can name a job not queued yet.
10. Deletes `todo/tmp/` files older than `todo_tmp_max_age_secs`.

It never deletes a plan manifest or the document a plan was filed from
([plan-ingestion.md §9](plan-ingestion.md#retention)). It takes no mutate lock:
it writes only `todo/`, `plans/`, and a synthetic failure under a job's own key
in `submissions/incoming/`.

Passes 1 to 5, 8 and 9 log a failed item, skip it and retry it on the next run.
A failure in passes 6, 7 or 10 aborts the run instead; a rerun is safe. Either
way the command exits non-zero, so cron monitoring sees a persistent fault.

### `pipette-mgmt fix-model-param`

The three `fix-*` commands rewrite warehouse columns in place, and share their
behaviour. Each walks every Parquet file under `warehouse/results/`, rewrites
only the rows that change (a file with none keeps its mtime), writes each file
atomically with `parquet_zstd_level`, and holds the [mutate lock](#concurrency)
unless given `--dry-run`. A crash leaves the data consistent, and a re-run
finishes the job. Each prints a one-line summary on stdout and logs the same
totals at `info`.

`fix-model-param` sets `model_params_total_millions` and
`model_params_active_millions` from the model-params rows
([storage.md §3.5](storage.md#35-model-params)), so a table edit reaches rows
already written without re-scoring. It is the last step of the
[models repair loop](#models-unresolved-the-audit-after-scoring), and is
permanent: the scorer can always write a row before its model has a row.

A row resolves by its `model_descriptor`, as the scorer resolves it, so a row
carrying only a `model_name` is never repaired. A row nothing resolves is left
as stored and counted, each distinct identity logged once. An empty table is
an error: there is nothing to fix from.

### `pipette-mgmt models resolve|unresolved|import`

`resolve` and `unresolved` ask about table coverage from either side of
ingestion; `import` adds rows. None takes the mutate lock. A model the table
does not know is not an error: the scorer keeps the counts the client reported,
or NULL. These commands make that gap visible.

#### `models resolve` (the gate, before ingestion)

Resolves model names against the model parameter table of the deployment and
**exits non-zero if any name has no entry**, so a script can gate a plan on coverage before its rows
land unresolved.

```
MISSING  NANI-Nithin/K2-Horizon-3.7B-GGUF -> k2-horizon-3.7b
MISSING  K2-Horizon-3.7B-Q4_K_M.gguf -> k2-horizon-3.7b
MISSING  openbmb/MiniCPM5-2B-GGUF -> minicpm5-2b
Error: 2 model(s) have no entry in the model parameter table: k2-horizon-3.7b, minicpm5-2b. ...
```

Each name prints with the key it folds to. On a miss that key is the row to
add, and two spellings of one model fold to one key: the two `K2-Horizon-3.7B`
names above are one entry. Names are looked up directly, since a plan names
models before any descriptor exists. Normalization strips `org/` prefixes,
`:file.gguf` suffixes, distribution tags (`-GGUF`, `-MLX`, `-ONNX`), quantizer
markers (`-UD`, `-i1`, `.i1`) and quant tags, so one entry covers every mirror
of a model.

#### `models unresolved` (the audit, after scoring)

Scans every warehouse row and reports each model the table cannot resolve,
with the rows waiting on it, by the rule `fix-model-param` applies: what this
reports is what that command would leave unchanged. It exits zero either way.

```
1284 of 91043 warehouse row(s) name a model that the model parameter table (167 entries) cannot resolve:
  k2-horizon-3.7b  (612 row(s))
      sample model_descriptor: {"org":"someorg","repo_name":"K2-Horizon-3.7B-GGUF",...}
  minicpm5-2b  (672 row(s))
      sample model_descriptor: {"org":"otherorg","repo_name":"MiniCPM5-2B-GGUF",...}
Add a model-params row for each, then run `fix-model-param` to repair the rows.
```

`--json` emits the same report, with `catalog_entries` beside it, because
"nothing resolved" means one thing against 167 entries and another against
zero. Its three buckets each send you somewhere different: `named` holds the
coverage gaps, each with its row count and a sample descriptor (add a row);
`unparsed` counts rows whose descriptor this build cannot parse (teach it the
shape); `no_descriptor` counts rows carrying none, which nothing server-side
fixes.

The repair loop is:

1. `models unresolved` to see what is missing.
2. Add a row for each model: **Server → Model params** in the app, or
   `POST /api/v1/model-params` ([Model parameters](api/model-params.md)).
3. [`fix-model-param --model <names>`](#pipette-mgmt-fix-model-param) to
   backfill the rows written during the gap.

#### `models import` (rows from a file)

Adds a model-params row for each entry of a TOML file that has none: a bare
integer for a dense model (`"LFM2-700M" = 742`), an inline table for the others
(`"LFM2-8B-A1B" = { total = 8340, active = 1500 }`). Each entry lands under its
normalized key with no `updated_by` and no note. A key that already has a row
keeps it, so an import never undoes an edit made in the app; the ones whose
counts differ from the file are named. The SQL migration seeds the rows the
deployed file held, so this is for a batch of new models.

### `pipette-mgmt fix-canonical`

Re-canonicalizes the warehouse's eight opaque JSON columns and recomputes each
one's `_sha256` by the rules the ingest path applies
([storage.md § model_flags / runtime_flags](storage.md#model_flags--runtime_flags)).
Use it after a canonicalization change, or to backfill a hash column a row
predates. It shares the [`fix-*` behaviour](#pipette-mgmt-fix-model-param).

A value that does not parse as JSON is stored trimmed, never dropped:
`model_flags` and `runtime_flags` take a plain string by contract. The summary
breaks the count down by column, which says which rollout a backlog came from.

### `pipette-mgmt fix-vault-labels`

Gives each vault source in `model_descriptor` and `runtime_descriptor` that has
no `custom_label` the one its plan entry carries
([plan-ingestion.md §4](plan-ingestion.md#4-the-server-mode-plan-format-summary)).
Scoring fills the label on every row it writes, so this is for rows stored
before it did. It shares the [`fix-*` behaviour](#pipette-mgmt-fix-model-param).

A source is labelled only when every record its paths name exists, is owned by
the source's `principal_id`, is an upload rather than an import, and has the
same `name` as the others, and the stored source of the row's job (its
`rescored_from`, else its `job_id`, or the first job up its `rescored_from`
chain that has a source) names every vault resource the descriptor does; any
other source is counted under its reason. Finding those jobs takes a read-only
walk of the warehouse before the one that fills. An existing label is never
changed, `model_vault_descriptor` (imports alone) is not walked, and no
`_sha256` changes, since a descriptor's id leaves its labels out.
The summary names each resource with its label and count, then the sources left
unlabelled by reason.

### `pipette-mgmt requeue-eval`

Re-stages an `eval`, `vlm_eval` or `asr_eval` benchmark's scored jobs into `submissions/incoming/` as fresh
submissions, so the next passes score them again after a scorer fix. Jobs are
found from the warehouse rows that carry the `benchmark_id`. Each copy keeps
the original `client_id` and `requested_*_flags`, gets a fresh `job_id` and
`submitted_at = now`, names the job it copies in `rescored_from`, and keeps the
device run's time in [`run_submitted_at`](storage.md#run_submitted_at). The
original's archive and rows stay. A job whose processed body is gone, does not
parse, or is a failure is skipped with a `warn`, and the summary counts each
reason. A live run holds the [mutate lock](#concurrency).

A second run over the whole benchmark would copy the copies, so pin
`--submitted-before` to the moment the first started:

```bash
pipette-mgmt --config config.toml requeue-eval --benchmark-id eval_ifbench_2026.06.1 --submitted-before 2026-06-01T00:00:00Z --dry-run
pipette-mgmt --config config.toml requeue-eval --benchmark-id eval_ifbench_2026.06.1 --submitted-before 2026-06-01T00:00:00Z
pipette-mgmt --config config.toml process-submissions
pipette-mgmt --config config.toml score-eval
pipette-mgmt --config config.toml process-submissions
```

A campaign's evals are re-scored over HTTP instead
([operations.md §4](operations.md#4-reprocessing)).

### `pipette-mgmt clients list [--tag <tag>]...`

Lists the registered clients and their status
([authentication.md §4](authentication.md#4-access-matrix)). `--tag` is served
from the reverse tag index, not a scan.

The `Migrated` column shows when each client first signed a `v1` payload, or
`—` for one that only ever used the timestamp-only fallback. Once every client
shows a date, `accept_legacy_signatures` can be cleared
([authentication.md §2.3](authentication.md#23-timestamp-only-signatures)).
`Derived` shows the flags normalized from the device profile, `Declared` the
ones the client reports itself, and `Permissions` the stored set
([authentication.md §5](authentication.md#5-permissions)).

The capability options take the clauses of a job body
([plan-ingestion.md §5](plan-ingestion.md#5-capability-matching)), so the
listing answers which clients a job's clauses match. They test every registered
client, pending ones included; the scheduler counts only working clients
(approved and holding `perform_work`), so check `Status` and
`--columns permissions` before reading a match as who runs the job. Flags must be
canonical, and whitespace separates a list because a flag can contain a comma
(`device:iphone14,2`). A query with no `--requires` matches nobody, because
every other clause only narrows.

```bash
pipette-mgmt clients list --requires 'os:ios' --excludes 'chip:a15 chip:a16' \
    --any-of 'gpu:a17pro gpu:a18pro' --not-all-of 'runtime:llama_cpp form_factor:tablet'
```

### `pipette-mgmt clients capabilities [--status <status>]`

Reports the capability flags the fleet reports, grouped by namespace and
counted: the vocabulary to write a job's `requires` from. A flag no client
reports matches nobody. `GET /api/v1/clients/capabilities` is the same function
over HTTP ([`GET /api/v1/clients/capabilities`](api/devices/get_capability_vocabulary.md)).

The `Source` column names where a namespace comes from: `derived` (normalized
from the device profile, in a reserved namespace a client cannot report),
`declared` (reported by the client), or `assigned` (an operator's tag, shown as
`tag:<tag>`). A flag with no `:` shows `-` as its namespace.

A client record that does not read is skipped and counted. Treat a non-zero
count as an incomplete vocabulary.

### `pipette-mgmt clients tag add|remove|list`

Tags are assigned here, never by a client
([authentication.md §7](authentication.md#7-client-tags)). A job matches one as
a `tag:<tag>` flag, so `add` and `remove` flag the client for an eligible-index
rebuild, as [`flag-reindex`](#pipette-mgmt-flag-reindex) does: it takes no work
until the next `queue-maintenance` run and drops any job it holds. Tag a client
while it is idle.

### `pipette-mgmt clients permission add|remove|set|list`

A permission names one operation at one scope, and a client holds a set of them
([authentication.md §5](authentication.md#5-permissions)). An operator assigns
them here, or an account holding `manage_clients` does over HTTP
([Device administration: Device permissions](api/devices.md#device-permissions)). A client
never sets its own.

The names are in [authentication.md §5.2](authentication.md#52-the-vocabulary).
Each belongs to one principal kind, and a client holds only the client names
(`submit_results`, `perform_work`). `add`, `set` and `clients approve
--permission` refuse a name a client cannot hold, and say which. `remove` takes
one: a stored name a client cannot hold is inert on read, and removing it is how
an operator clears one. An unrecognized name aborts the command before it writes
any client, so a typo never grants part of a fleet.

`list` renders the stored set, inert names included, with nothing expanded:
the client vocabulary implies nothing, unlike the user one
([`users permission list`](#pipette-mgmt-users-permission-addremovesetlist)). It
prints `-` for a client with no permissions object and `(none)` for one whose
set an operator cleared. No default fills an absent object, so the records still
needing a grant are a listing rather than a guess
([authentication.md §5.3](authentication.md#53-defaults)).

`add --tag` grants every client that carries the tag. It names each client that
did not take the grant, carries on with the rest, and exits non-zero with a
count, so a re-run converges.

### `pipette-mgmt users create|list|show|approve|set-type|set-campaigns|disable|enable|delete|release-claim|set-org`

The operator principal, which authors work where a client does it
([storage.md §3.1](storage.md#31-the-user-principal)). Accounts are a separate
keyspace: a user never appears in `clients list`, the fleet a plan can reach.
Every subcommand names an account by **id**, except `create` and
`release-claim`, which name an **address**, so a legal address such as
`user_ops@example.com` is never misread as an id. `users list` maps one to the
other.

A service account (`--type service`, [authentication.md §5.11](authentication.md#511-users))
belongs to an organization and holds no admin name (`manage_orgs`,
`manage_clients`, any `_any_` name): `create`, `approve` and `permission add` /
`set` refuse one for it, `set-type service` refuses an account holding one, and
`set-org --none` will not leave one in no organization.

A campaign reader is a service account that reads the campaigns it names and
their plans alone, and holds `read_plans` alone
([Users: The rules that make delegation safe](api/users.md#the-rules-that-make-delegation-safe)).
`create --type service --campaign <CAMPAIGN_ID>` (repeatable) makes one, and
`set-campaigns <USER_ID> <CAMPAIGN_ID>...` makes a service account one or
replaces a reader's campaigns. Each campaign must exist and belong to the
account's organization; one handed to another organization later drops out of
its reach. Both refuse an account
holding another name, `set-type human` refuses a reader, and nothing removes a
scope: a wider account is a new account. `show` lists a reader's campaigns.

`create --permission` is where an invited account gets its set: its pre-auth
key carries no template. The grant is written after the record, so a failure
there leaves a real account holding nothing, and the error names the
`users permission set` that finishes it.

`approve` admits an account a machine asked for at `POST /api/v1/users/register`
([`POST /api/v1/users/register`](api/users/register_user_key.md)). `users list` is
where a `pending` one is noticed. `require_user_preauth_key = true` closes that
path, so every account is minted with `create`.

**Prefer `disable` to `delete`.** Disabling refuses every request and keeps the
record, so the plans it filed stay attributed to a principal that resolves.
Over HTTP it is the only offboarding. `delete` is for an account created in
error: it removes the record, the address claim and the permission set in that
order, so a failure part-way leaves state that grants nobody anything. A delete
is refused while the account owns a device (`--detach` unowns them, and
`clients owner set` reassigns one instead), a campaign (hand it over with
`campaigns owner`) or a vault resource (`vault delete` it).

`delete` is idempotent: a teardown that stopped half-way is finished by running
it again. With the record already gone it clears the permissions and points at
`release-claim`, which names the address directly. A mistyped id and a deleted
one therefore read the same, as with `clients delete`. `release-claim` refuses
an address a live account holds: freeing one is how one address ends up on two
accounts.

A `create` that fails between claiming the address and writing the record
releases the claim, unless a record already answers to that id. If the release
fails too, `create` refuses and names `--adopt-stranded-claim`, which builds the
record under the id the claim names. Adopting is never automatic, because a
stranded claim and a create in flight look the same. Adopting also clears any
permission object left at that id, so the account holds only what this
`create` grants. `delete` releases a claim only when it names the account being
deleted.

`set-org` is always an operator's assignment. The membership grants no
permission but bounds the account's bare roster grants and `manage_clients`,
and decides which organization's campaigns, vault resources and private
benchmarks it reaches. Moving an organization admin therefore makes it the new
organization's admin, and the devices the account owns move with it. See
[`pipette-mgmt orgs`](#pipette-mgmt-orgs-createlistshowupdatedisableenabledeleterelease-claimmembersclientsbenchmarks).

### `pipette-mgmt users permission add|remove|set|list`

The user half of the vocabulary: the plan, fleet and roster verbs
([authentication.md §5.2](authentication.md#52-the-vocabulary)). A client's verb
is refused at parse time, naming the kind that holds it. `add` and `remove` are
read-modify-writes, so two operators granting at once do not lose each other's
work. `list` prints the **resolved** set, implications expanded:
`read_any_clients` implies `read_clients`, so the listing is wider than what
`set` would take back.

`--org-member`, `--org-admin` and `--server-admin` are the **permission
groups**, one word for a set:

| Group | Grants |
|---|---|
| `--org-member` | `submit_plans`, `read_plans`, `cancel_plans`, `read_clients`, `edit_catalog`; it files and cancels in its own campaigns, `read_plans` also reads its organization's, `read_clients` reads the fleet, and `edit_catalog` changes the org entries of the descriptor catalog |
| `--org-admin` | everything inside its **own** organization: the member's permission names, its devices (`manage_clients`) and its roster (`manage_orgs`: the organization, its people, and what they hold) |
| `--server-admin` | the `_any_` verbs, including `edit_any_catalog`: every plan, device, roster, and org entry of the descriptor catalog on this server |

`--org-admin` carries only bare verbs, so the account's membership bounds it: it
cannot reach another organization, create or delete one, or adopt an account
that belongs to nobody, and in no organization it administers nothing. The
grant it hands out over HTTP is bounded by what it holds
([Users: Account administration](api/users.md#account-administration)), so delegation widens
nobody. No organization name is in `--org-member`: an operator who needs
read-only roster access is granted `read_orgs` by name.

A group conflicts with named permissions and with the other groups, so two
statements of one set are refused rather than one winning. `remove` takes no
group: a revocation is stated by name. A group is a **spelling of a set, not a
grant**: it expands where it is given and nothing records it, so widening a group
in a later release never widens a set already written.

So an account that got its group from an earlier release can lack a name that
the group holds today, such as `edit_catalog` or `edit_any_catalog`. Give the
name directly: `pipette-mgmt users permission add <user_id> edit_catalog` for a
member or an org admin, and `pipette-mgmt users permission add <user_id>
edit_any_catalog` for a server admin.

**Every assignment is normalized before it is stored.** A name another given
name implies is dropped, and the command says which: `set <id> submit_plans
read_plans` stores `submit_plans` alone. `add` normalizes the whole object, so
granting `submit_any_plans` to an account holding `submit_plans` leaves one
name. Normalizing never narrows a grant, since `list` puts back what it drops.
For the same reason, removing `submit_plans` from an account holding
`submit_any_plans` reaches nothing; `remove` says so and names the wider grant
to remove instead.

### `pipette-mgmt users key list|revoke`

The keys an account signs in with, one per machine. `list` prints each key's
id, `key_details`, when it was added and when it expires: `never` for a
machine's key, `expired` once a browser's key has lapsed. Oldest first, so a
machine added since sits at the bottom.

`revoke` is the CLI form of `DELETE /api/v1/users/{user_id}/keys/{key_id}`
([`DELETE /api/v1/users/{user_id}/keys/{key_id}`](api/users/revoke_user_key.md)). A key id the account
does not hold is refused, naming `users key list`; another account's key is
never touched. Revoking the last key is allowed: `users preauth create` lets the
holder back in.

This is the command for a stolen key. `disable` stops every key and `enable`
restores them all, including any the thief registered with the stolen one. So
revoke the stolen key and every key added since it, then enable.

### `pipette-mgmt users preauth create|list|revoke|prune`

Registers a machine **against an account**: the key names the account, so the
register body names none and a holder cannot redirect it. The token, read
`user_preauth_{key_id}.{secret}`, is printed once; only `sha256(secret)` is
stored. The key grants no permission and registering leaves the account's set as
it is, so reusing a `--multi-use` key cannot restore a name an operator revoked.
A key an older release minted with a permission template still registers, and
the template is ignored.

`revoke` deletes the key record; nothing holds a revoked state. `prune` deletes
every expired record. Spending a single-use key leaves a
`user-preauth/{key_id}.spent` marker, which makes the spend exactly-once against
simultaneous registrations. `prune` clears the markers whose record is gone and
keeps those whose record could still be consumed. `prune` sweeps this keyspace
only; `clients preauth prune` sweeps a client's.

An administrator of the account mints the same key over HTTP
([`POST /api/v1/users/{user_id}/preauth-keys`](api/users/mint_preauth_key.md)) and in the
console, to onboard: only for an account nobody has signed in as yet, holding
nothing the administrator does not, single-use, within 90 days, and checked
again at registration. This command is bounded by none of that, so it is also
the way back in for a holder who lost its only key.

### `pipette-mgmt clients preauth create|list|revoke|prune`

Mint and manage pre-auth registration keys
([authentication.md §3.2](authentication.md#32-pre-auth-keys)). A valid key
always approves. A key record is write-once: a spent single-use key deletes
itself on consume, `revoke` deletes on demand, and `prune` deletes expired keys
and the `client-preauth/{key_id}.spent` markers whose record is gone, as
`users preauth prune` does.

A key with no permission template registers clients at the default
(`submit_results` alone); `--no-permissions` stores an empty one, whose clients
hold nothing. `list` shows no template as `-` and an empty one as `(none)`,
because the two grant different things
([authentication.md §5.3](authentication.md#53-defaults)).

`--owner-org` or `--owner-user` is the operator vouching for a device ahead of
time, so it arrives already owned rather than unowned. The owner is resolved
when the key is minted and stored by id, so a handle that names nothing mints no
key. If the owner is deleted first, registration refuses the key with `401`
rather than registering the device outside the organization it promised.
`orgs delete` refuses while an unexpired key names the organization; `users
delete` does not count keys, so revoke one naming an account you delete.

An organization's administrator mints an owner-only key for its own
organization over HTTP
([Device administration: Client pre-auth keys](api/devices.md#client-pre-auth-keys)); `list`,
`revoke` and `prune` here cover those keys too. Every verb has an HTTP
counterpart gated on `manage_clients`
([Device administration](api/devices.md)), through the same
operations.

### `pipette-mgmt clients approve <client_id>`

Approves a pending client. `--permission` is additive, so re-running `approve`
revokes nothing. There is no `disapprove`: revoke with `clients permission
remove` or `set`.

### `pipette-mgmt clients reject <client_id>`

Deletes a registration that is still `pending`, with its ownership and tags, as
`clients delete` does.

### `pipette-mgmt clients delete <client_id>`

Deletes a client record, pending or approved, then best effort its queue
markers, ownership and tag markers; `queue-maintenance` and
[`reindex`](#pipette-mgmt-reindex) collect any it leaves, and a re-run finishes
the cleanup. An absent record is not an error, so the command reports whether it
found one. Submissions and warehouse rows naming the client stay.

### `pipette-mgmt clients list-suspended`

Lists the suspended clients, with `suspended_at` and the `conflicting_job_id`
that triggered each suspension: a target for an alerting script.

### `pipette-mgmt clients unsuspend <client_id>`

Clears a client's suspension so it may claim again. A client is suspended when
it holds more than one unexpired lease, a sign of a reboot or
crash loop ([queue.md](queue.md#claiming-a-job)). Unsuspending changes nothing
else about the client, and is a no-op for a client that is not suspended.

### `pipette-mgmt clients update <client_id> [flags]`

`--organization` is **free text the device sent about itself**, with no record
behind it. To put a device in an organization, use `clients owner set`. The two
stay apart because merging them would read every stored label as a membership
nobody approved.

### `pipette-mgmt clients owner show|set|clear`

Who a device belongs to: an organization directly, or a user account whose own
membership then says which organization
([storage.md §3.2.4](storage.md#324-ownership-client-ownersclient_idjson)). The
two kinds exist because a rack machine and somebody's laptop differ: collapsing
a shared device onto whoever racked it makes their offboarding delete it.

A user-owned device rolls up to whichever organization its owner belongs to
*now*, so `users set-org` moves an account's devices with it. `show` prints the
owner and the organization it resolves to; an owner in no organization rolls up
to none, which is a state, not an error. `clear` leaves the device unowned,
which is also where a fresh self-registration starts: an operator vouches for a
device, the device does not name itself. Both targets are resolved before the
write, so an ownership only ever names a record that exists.

### `pipette-mgmt orgs create|list|show|update|disable|enable|delete|release-claim|members|clients|benchmarks`

The organization roster and the two relations hung off it
([storage.md §3.2](storage.md#32-organizations)). **Explicitly provisional**: a
holding position until a longer-term tenancy model lands.

An organization holds no permission, but its relations decide access over HTTP
([topology.md §7](topology.md#7-decisions-per-object)): a membership bounds an
account's bare roster grants and `manage_clients`, and an ownership decides
which organization administers a device and what private content it is served.
So `users set-org`, `clients owner` and `--detach` change what accounts and
devices may do.

`<org>` is **either spelling**: an `org_`-prefixed value is read as an id,
anything else as a handle. A handle may therefore not begin `org_`. `create`
claims the handle the way `users create` claims an address, and
`--adopt-stranded-claim` and `release-claim` work as they do there.

`delete --detach` clears memberships and direct ownerships only. A private
benchmark, a campaign and a vault resource are the organization's own data, so
each goes through `orgs benchmarks delete`, `campaigns owner` or `campaigns
delete`, or `vault delete`, and a pre-auth key through `clients preauth revoke`.
The refusal names each by id before anything is detached.
An organization with a live descriptor catalog entry also refuses deletion;
first run `catalog tombstone NAME --org <org>` for each entry. A successful
organization delete removes its tombstoned descriptor catalog entries.

`members` and `clients` are scans of `user-orgs/` and `client-owners/`, with no
reverse index ([storage.md §3.2](storage.md#32-organizations) says why). Over
HTTP the device inventory also needs a fleet grant, because its rows are whole
client records ([Organizations](api/orgs.md)).

`benchmarks` curates an organization's private benchmarks
([benchmarks.md §1.1](benchmarks.md#11-private-benchmarks)), the same operations
the `/api/v1/orgs/{org_id}/benchmarks` routes offer
([Organizations: Private benchmarks](api/orgs.md#private-benchmarks)), through one
implementation. A write is served on the organization's next
`GET /benchmarks`: the keyspace is read per request. `list` names why an entry
does not read: such an entry is served to nobody, but still holds its id and
refuses `orgs delete`. `delete` finds an entry by its key, so it removes an
unreadable one too.

### `pipette-mgmt unverified`

Operator commands for the held-submission archive
(`submissions/unverified/{client_id}/`;
[storage.md §4.1](storage.md#41-unverified-submissions)). The tree is disjoint
from the warehouse and the mutate lock, so they run safely beside `serve` and
`process-submissions`.

#### `unverified promote --client-id <id>`

Re-stages a client's held submissions after it is approved, through the routing
the live submission path takes. A success and a terminal (`retriable: false`)
failure move to `incoming/`. A **retriable** failure is archived to
`processed/` unrecorded: the live path never records one either
([httpapi.md §2.7.2.1](httpapi.md#2721-request-body)), and once the job is
resolved, recording it would give a terminal failure to a cell another client
may have measured. A held body's `job_id` was never checked against a lease,
so one whose job already exists (a record in `incoming/`, the score queue or
`processed/`, a job source, or an entry in `todo/avail/` or `todo/leased/`) is
refused and stays held for `unverified delete`. The command reports the three
counts separately, and `--dry-run` reports how many would be refused. Each
other object leaves the unverified tree only after its re-stage write
succeeds, and its runtime output moves to the job's runtime-output object
([storage.md §4.2](storage.md#42-runtime-output)).

A held body whose day partition is past the warehouse lock window is archived
unrecorded by the scorer too, so a client approved long after its submissions
were held gets none of its failures into the warehouse
([storage.md §5](storage.md#which-failures-reach-the-warehouse)).

#### `unverified delete --client-id <id>`

Deletes every held submission for one client, after rejecting it.

#### `unverified prune --older-than <duration>`

Deletes held objects across all clients by their backend modification time, to
bound the archive.

### `pipette-mgmt runtime-output <job_id>`

Prints what the runtime wrote to stdout and stderr for a job, from every client
that reported on it ([storage.md §4.2](storage.md#42-runtime-output)): a
retriable failure records no result but keeps its output. Each stream prints
under a `== <client_id> <stream> ==` header, clients in id order. A stream not
captured prints nothing; one captured empty prints its header alone. A job with
no output is an error, so a mistyped id does not read as a silent run.

### `pipette-mgmt dataset-cache probe|build|pins`

Keeps upstream dataset corpora servable through `GET /datasets/{benchmark_id}`
([design/vl-spec-decode-dataset-delivery.md](design/vl-spec-decode-dataset-delivery.md)).
The upstream owns the data; mgmt serves its cached repack only while the latest
probe found the upstream publishing the pinned content. The pins are every
catalog entry's, each organization's private entries included. A git upstream
needs `git` on `PATH`; an evals-server upstream (a `vlm_eval`'s or an
`asr_eval`'s samples) is the configured `evals_server_url`, which `probe` asks
for `/samples/digest` alone and `build` streams `/samples` from once, parsing it
as it arrives. `probe` and `build` each hold their own lock
(`dataset-cache-probe`, `dataset-cache-build`), and their schedule is
[operations.md §3.0](operations.md#30-dataset-cache).

`pins` fetches and packs one benchmark's upstream without a store and prints the
hashes its catalog entry pins:

```toml
[dataset]
digest = "1862eedc832710f6c4776472d00d5ee4aa0a16d85d6a9c433398109f90239810"
bytes  = 15955456

[dataset.upstream]
identity = "2923603292c7dd463ad01a119f57965817778fc3"
digest   = "2bd3c7b41707c10d91726e90545b9ec5d741ac947637c90e2f9d5fe6321fa8f7"

# 100 samples, 100 turns; subsets: GQA
```

The trailing comment lists the subsets the corpus holds. Run `pins` on an entry
with no `subset` to see every subset, then author one entry per subset; the
`[dataset.upstream]` pins are the same for all of them.

For a `vlm_eval` entry, write the `[dataset]` table with `format`, `adapter` and
the upstream's `source`, `eval_id`, `dataset_version` and `prompt_profile`
(placeholders for the hashes), and `pins` prints the rest; there is no tree
digest, since the identity is already a digest over every sample:

```toml
[dataset]
digest = "…"
bytes  = …

[dataset.upstream]
identity = "115955fff4f95d524c81964dbe712b97ff3e8443ff180df38e4db9531aa3da72"

# 1500 samples, 1430 media files; replaces a 60123456-byte /samples body
```

An `asr_eval` entry's pin is written the same way, with `format =
"asr-eval-corpus/1"` and the entry's `prompt_profile`, which an asr_eval's
upstream always names; `pins` reads its task, version and profile and prints
the rest.

Each artifact is handled on its own, four at a time, under one
`dataset_probe_timeout_secs` / `dataset_build_timeout_secs` deadline: an error
on one is recorded for it and the run goes on. asr-eval corpora are the
exception for `build`: gigabytes each, they build one at a time after the
others, each under `dataset_asr_build_timeout_secs` (default 7200), which also
bounds `pins` for such an entry. The build lock's lease is the sum of the
artifacts' deadlines. A benchmark that pins a digest
another pins with different fields is skipped and reported.

`probe` exits non-zero while any upstream is not `ok` (`unavailable`, `gone`,
`moved`) or could not be probed, and `build` when any build failed, so a failed
run pages until the state clears. Each state change is logged once with a stable
`event` field for alarms: `dataset_upstream_unavailable`,
`dataset_upstream_gone`, `dataset_upstream_moved`, `dataset_upstream_recovered`,
`dataset_build_failed`, `dataset_probe_failed`, `dataset_pin_conflict`. `serve`
logs `dataset_cache_missing` and `dataset_probe_stale` on the refusals those
name, and `dataset_pin_conflict` when the artifact under a pinned digest was
built from another pin.

### `pipette-mgmt unlock`

Inspects the [storage mutate lock](#concurrency), and on `s3` clears a stale
lease. Use it only to recover from a crashed mutating command. On `s3` it prints
the holder, host, pid and lease window, deletes the lease once it has expired,
and with `--force` while it is still active. On `local_fs` the lock is an
`flock(2)` that dies with its process, so `unlock` only reports whether a
command holds it. With no lock held it exits 0.

### `pipette-mgmt reindex`

Reconciles the reverse tag index (`tags-index/by-tag/`) against the forward tree
(`tags-index/by-client/`): it creates missing reverse markers, drops orphan
ones, and prunes the markers of deleted clients
([authentication.md §7](authentication.md#7-client-tags)). Idempotent, and a
no-op when the trees agree.

It refuses to run when no client record reads back but forward markers exist:
with the records and the markers in separate sections, that is a misconfigured
`[auth_storage]` more often than a fleet that is gone, and proceeding would drop
every tag. The `todo/eligible/` index is [`flag-reindex`](#pipette-mgmt-flag-reindex)'s.

### `pipette-mgmt flag-reindex`

Writes a pending-reindex flag for one client, or every client with `--all`. The
next [`queue-maintenance`](#pipette-mgmt-queue-maintenance) run rebuilds each
flagged client's `todo/eligible/` markers from scratch against its current
record, and clears the flag ([queue.md](queue.md#the-reindex-gate)).

The server writes the same flag itself on every device-profile and capability
change, so use the command only when that path did not converge:

- A `PATCH /clients/me` returned `500` **and** a maintenance run consumed its
  first flag. The handler writes one flag before it stores the new record and
  one after; a failure on the second leaves the record stored, and the client's
  retry sees nothing to report. Stale markers survive only when a rebuild
  consumed the first flag inside the window the post-rebuild record check does
  not cover.
- You rolled forward past a build that read the eligibility clauses
  differently. The index records the jobs it indexed and skips them, so only
  `--all` re-matches them
  ([plan-ingestion.md §5](plan-ingestion.md#5-capability-matching)).
- Markers look wrong for any other reason: a partial restore, a manual edit, or
  a suspected bug.

> **A flagged client is work-gated.** The server refuses its claim, reclaim and
> submission until the next `queue-maintenance` run clears the flag, and it
> forfeits its in-flight leases. `--all` pauses the whole fleet for up to one
> maintenance interval. Name a single client where you can.

The command checks a named client against the auth store and refuses an unknown
id. It writes only `todo/` and takes no mutate lock.

### `pipette-mgmt plans list|status|results|cancel`

Read and cancel plans. Filing one is
[`campaigns`](#pipette-mgmt-campaigns-createingestvariantsrevisererunrevertlistshowrenameownerdelete);
[plan-ingestion.md](plan-ingestion.md) covers the ingestion flow (§8) and the
plan lifecycle (§9). Only `results` leaves storage. The commands write only
`plans/` and `todo/`, so they take no mutate lock.

Identify a plan by its `plan_id`.

**`list`** renders one row per plan. The columns that need saying:

- **Submitter** is the user account a plan is attributed to (`user_…`), the one
  that signed the filing request. A plan no user filed, such as one the
  `campaigns` commands filed, shows `operator`, and no `--submitter` selects it.
- **Jobs Expire** is the earliest and latest job deadline, frozen at ingestion:
  one value when they agree, two lines otherwise, `never` for an unbounded job,
  and `-` on a manifest that never recorded it.
- **Starved** is the outstanding jobs that match no working client: `-` when no
  `queue-maintenance` pass has computed a snapshot, `0` when every one matched,
  and `4 in 1 group(s)` when work is stranded, the pointer to `plans status
  --explain`.
- **Finished**, **Running**, **Available**, **Failed**, **Cancelled** and
  **Expired** read the snapshot, `-` without one. **Snapshot** is when it was
  computed.
- **Warnings** counts the jobs that matched no approved client at ingestion, in
  the form of **Starved**. It is frozen, so `0` means the plan drew no warning.
- **Status** carries `(cancel pending)` between a cancel and the run that acts
  on it.

**`status`** prints the manifest: the submitter, the lifecycle status, any
pending cancellation, the ingestion warnings, and the progress snapshot. The
snapshot is what `queue-maintenance` cached, never recomputed here, so a plan
before its first pass says it has none rather than reading zeroes. The warnings
are frozen at ingestion and can disagree with the live starvation once clients
register.

**`status --leases`** names the clients that hold the plan's jobs, by
`client_id`, in two lists, each skipped when empty:

- **Running now**: the holder still heartbeats.
- **Stranded**: the lease is past its deadline and the job sits outside
  `avail/`, where no claim reaches it, until `queue-maintenance` recycles it.

Each row gives the lease deadline and its distance from the read: forward for a
live lease, backward for a lapsed one. A short strand is the normal window; one
older than a maintenance interval points at a stalled cron. A `leased/` key that
does not parse names no job, so it joins neither list; the report counts such
keys last, as the caveat on both lists, and narrows its empty answer to what it
could read. The lists are a **live** reading and the **Progress** block the
cached count, so a job claimed since the last pass appears in one and not the
other.

**`status --explain`** answers why the plan's jobs are not scheduled. Use it
when **Finished** does not move or **Starved** shows a group.
[`GET /api/v1/plans/{plan_id}/explain`](api/plans/explain_plan.md) is the same
report over HTTP. It has four parts: the **snapshot** and its age (every cached
figure dates from it, so read it first); the **verdict**; the plan's **jobs**
across the queue and the **fleet** as the claim path sees it; and one block per
**starved group**.

The verdict walks the gates a job clears to reach a client, and names the first
one that blocks every outstanding job:

| Gate | Meaning |
|---|---|
| still `creating` | The jobs sit in `todo/tmp/`. No client can see them. |
| cancel pending | Teardown owns the plan. `queue-maintenance` retires the jobs. |
| leases lapsed | Every outstanding job sits behind a lapsed lease. The holders stopped heartbeating, and a claim cannot reach a job outside `avail/`. |
| nothing outstanding | No job is in `avail/`. Each one is held under a live lease, finished, or retired. |
| expired | Every available job is past its deadline. The claim path refuses it. |
| not indexed | No claimable job is in `todo/.eligible-indexed`, so no client carries an `eligible/` marker. |
| starved | Every claimable job matches no working client. |
| clients blocked | No claimable job has a client that can take it: no approved client holds `perform_work`, or every working client that matches a job is suspended, waits on a rebuild, or denied that job. |
| schedulable | At least one claimable job matches an unblocked client that holds no `denied/` marker for it. |

One further answer is not a gate: **queue moved** means every claimable job left
`avail/` while the report read it and none turned up under a live lease, so the
report asks for a re-run.

**not indexed** and **leases lapsed** have no other surface, and a stalled
`queue-maintenance` causes both. Ingestion writes no `eligible/` marker, so
between an ingest and the first pass nothing is claimable, while the plan reads
`active` with an empty starved list. A lapsed lease waits in `leased/` until a
pass recycles it. A plan holding a live lease beside a stranded one reads
**nothing outstanding**, with the strand counted in the headline.

```console
$ pipette-mgmt --config config.toml plans status plan-018fce2a-… --explain
Why these jobs are not scheduled
  Snapshot  2026-08-19T17:55:00+00:00 (2h 14m old)
  Verdict   all 4 claimable job(s) match no working client — read the starved groups below
  Jobs      4 in the plan: 4 available, 0 leased, 0 finished or retired
            4 claimable (0 past their deadline); 4 indexed; 0 denied
  Fleet     41 registered, 37 approved, 37 holding `perform_work`, 0 suspended, 0 pending an eligible-index rebuild, 37 unblocked

Starved groups — clause sets from the snapshot, client counts read live:
  4 job(s) requiring os:ios excluding flags chip:a15 with any-of [gpu:a17pro, gpu:a18pro]
      working clients                     37
      requires os:ios                     12
      excludes chip:a15                    9
      any_of [gpu:a17pro, gpu:a18pro]      0  <- no client survives this clause
```

Each starved group renders a **clause ladder**: the surviving working clients
after each clause, applied cumulatively in the order `exclude_clients`,
`requires`, `excludes`, `any_of`, `not_all_of`, so the clause that drops the
count to zero is the one to fix. The marker names the rung that empties a pool
an earlier rung left open; over an empty working fleet there is no marker, and
the note points at an approval or a `perform_work` grant instead.

The clause sets come from the snapshot. Without one, a **starved** verdict rests
on the warnings ingestion froze, said in the past tense, since an approval since
can have cleared it. A job that starves after the last pass belongs to no cached
group: the report reads the job bodies against the live fleet and answers
**starved** on that evidence, naming the stale snapshot.

A group can carry three notes: a rule proving no client can ever match (a
contradiction or a malformed clause; fix the body, there is no ladder); the
roster standing for a job naming clients by id (registered, approved, holding
`perform_work`, admitted by the exclusions; the first count that drops names
the fix); and a live match, which means a client registered after the last pass
and the next run clears the group.

`--explain` reads the fleet and the queue listings, so its cost grows with the
fleet; it never reads the `eligible/` marker tree. `--explain --leases` costs no
more than `--explain`, and both blocks date from one listing.

**`results`** prints one row per metric of each job's latest scoring run,
carried-over jobs included, with how many of the plan's jobs reported. A failed
row's value names the pass that declared it when the warehouse records one, as
`failed (lease_lost): <reason>`. `--json` prints each row as
[`GET /api/v1/plans/{plan_id}/results.csv`](api/plans/get_plan_results_csv.md#result-rows) describes it.

**`cancel`** records a cancellation and returns; `queue-maintenance` retires the
jobs and settles the plan
([plan-ingestion.md §9](plan-ingestion.md#cancellation-and-teardown)). Until that
run the jobs stay claimable and `status` reads as before, with the pending
cancel shown. A plan that stays `cancelling` across runs means teardown keeps
failing, and each run exits non-zero. Cancelling is refused for a plan with no
manifest and for a `complete` one; a plan already `cancelling` or `cancelled`
reports a no-op.

### `pipette-mgmt campaigns create|ingest|variants|revise|rerun|revert|list|show|rename|owner|delete`

Administer campaigns: [storage.md §3.3](storage.md#33-campaigns) is the record,
[Campaigns](api/campaigns.md) the same operations over HTTP. Each
filing command is a route's twin, with the route's rules:

| Command | Route |
|---|---|
| `create` | [`POST /api/v1/campaigns`](api/campaigns/create_campaign.md) |
| `ingest` | [`POST /api/v1/campaigns/{campaign_id}/plans`](api/campaigns/file_plan.md) |
| `variants` | [`POST /api/v1/campaigns/{campaign_id}/variants`](api/campaigns/edit_variants.md) |
| `revise` | [`PUT /api/v1/campaigns/{campaign_id}/document`](api/campaigns/revise_document.md) |
| `rerun` | [`POST /api/v1/campaigns/{campaign_id}/rerun`](api/campaigns/rerun_jobs.md) |
| `revert` | [`POST /api/v1/campaigns/{campaign_id}/revert`](api/campaigns/revert_campaign.md) |

Filing a document, the refusals, the merge and the carry-over are
[Campaigns: Filing a plan document](api/campaigns.md#filing-a-plan-document) and
[plan-ingestion.md §6 to §9](plan-ingestion.md#62-ingestion-time). There is no
`retarget`: only those commands move a campaign, onto a plan it just minted, so
a plan never has two campaigns. A campaign holds no jobs: `delete` removes the
pointer, and refuses while one of its plans runs with no cancel requested.

What differs from HTTP:

- **No permission check.** `create` and `owner` take exactly one of `--user` /
  `--org`, resolved against the roster before anything is written, and can hand
  a campaign anywhere; the bound in
  [`PUT /api/v1/campaigns/{campaign_id}/owner`](api/campaigns/set_campaign_owner.md) is on the
  signed routes. A job may name any organization's private benchmark.
- **`--strict-models`.** HTTP always warns on a model with no model-params row,
  and treats an unreadable table as empty, so a table the operator has not
  caught up with never costs a tenant its plan. The flag makes the CLI refuse
  instead, at the first such job:

  ```console
  $ pipette-mgmt --config config.toml campaigns ingest campaign_01a0b60e-… --plan nightly.toml --strict-models
  Error: rejected job "0" (index 0): `spec.model` matches no entry in the model
  parameter table: {"org":"someorg","repo_name":"K2-Horizon-3.7B-GGUF",...}. Add a
  model-params row for it, or drop --strict-models to ingest anyway and repair the
  rows later with `fix-model-param`
  ```

  An empty table matches everything, so a deployment that ships none sees
  neither the warning nor the refusal. `models resolve` asks the same question
  without filing, which suits CI.
- **The report is wider.** On success the command prints the ingest report as
  JSON on stdout: the `plan_id`, the `job_ids` by expansion index, the warnings
  (`plan_warnings` among them), and the merges. Over HTTP the body carries
  `merged_job_count` and omits `merges`; here `merges` names each merged cell
  with the `job_id` it shares and its `kind`. The `kind` is one of these:

  - `within_plan`: a job of this plan.
  - `across_plans`: a job of an earlier plan.
  - `carried`: a job of the plan that the revision replaced.
  - `satisfied`: an earlier finished run that answers the cell.

  It is the operator's view of the whole queue, so worth saving. Do not add
  `job_count` and `merged_job_count`: a cell that joins an earlier plan's job
  counts in both.
- **`--dry-run` prints JSON** and files nothing: `bodies` in index order,
  `plan_warnings`, and `imports`, with the fields of the HTTP dry run
  ([Campaigns: Response `201 Created`](api/campaigns.md#response-201-created)). It runs every
  check an ingest runs before a download, resolving gated models at
  `hf_import_endpoint`. `variants`, `revise` and `revert` add the revised
  document and the carry-over counts (`carried_job_count`, `new_job_count`,
  `requirements_rerun_job_count`, `satisfied_job_count`, `restored_job_count`,
  `inherited_job_count`, `dropped_job_ids`, `held_jobs`, `repeated_variants`).
  The counts include the cells that an earlier finished run answers
  ([plan-ingestion.md §7](plan-ingestion.md#satisfying-a-cell-from-an-earlier-run)). A `revise` of the
  unchanged document files nothing, and its dry run runs no import, so `imports`
  is empty. A `revert` to a plan with the document of the head also files
  nothing.
- **Recovery.** If `create` fails to write the campaign after filing its plan,
  it cancels the plan, or names it to cancel. A `variants`, `revise`, `rerun` or
  `revert` that fails after writing the new plan leaves both running and names the one
  to `plans cancel`; one that loses a race to another edit cancels its own.
  `ingest` writes its plan before advancing the campaign, so a re-run repairs a
  failure between the two.
- **Re-runnable.** A write asking for the state the record is already in prints
  `no change` and writes nothing.

`campaigns revert --to <PLAN_ID> --base <PLAN_ID>` files a new plan with a copy
of the document of an earlier plan. It also cancels the plan that the campaign
runs. The new plan always takes the finished runs of the campaign's earlier
plans. The results of the copied plan then come back with no new runs. `--to`
must name a plan of the campaign's history, at any depth. `--base` must name
the plan that the campaign runs. The command imports nothing, because a stored document names
each imported model by its vault arm.

```console
$ pipette-mgmt --config config.toml campaigns revert campaign_01a0b60e-… --to plan-01a0b61c-… --base plan-01a0b7f2-…
Campaign campaign_01a0b60e-… is now running plan-01a0b812-….
It replaces plan-01a0b7f2-…: 3 job(s) carried over, 0 dropped.
```

`campaigns variants --restore <PLAN_ID>:<INDEX>` appends one variant of an
earlier plan to the plan that the campaign runs. The plan can be a plan of any
campaign. The index counts the variants of that plan from 0, as
[`GET /api/v1/plans/{plan_id}/variants`](api/plans/get_plan_variants.md) lists
them. The finished runs of that plan answer the cells of the variant, so the
restore does not run that work again. The revision records each restored
variant. Repeat `--restore` for more variants. To replace a variant of the
current plan, add `--remove` with its number:

```console
$ pipette-mgmt --config config.toml campaigns variants campaign_01a0b60e-… --restore plan-01a0b61c-…:1 --remove 0 --base plan-01a0b7f2-…
```

`campaigns show` walks the chain back from the head, one read per revision, and
stops after 50. The id is the handle: there is no slug index, and a value
without the `campaign_` prefix is refused at the argument boundary, so a plan
id passed here is an error rather than a lookup that finds nothing.

### `pipette-mgmt catalog`

The catalog exists only with `[database]`. Host access can change global and
organization entries. The HTTP API and the console do not yet write it. Names
keep the creator's case, while lookup, uniqueness and importer-reserved prefixes ignore
case within each scope. The llama.cpp importer reserves the names `llamacpp-v*`
and `llamacpp-daily-*` in the global scope only.

Every command that names an entry accepts `--org <org>` to select an
organization's namespace; `<org>` is its slug or `org_` ID. Without it,
commands use the global namespace. An org
entry is always hand-made, and the CLI records its creator as `operator`.

`catalog import [--since YYYY-MM-DD]` reads GitHub releases for
`ggml-org/llama.cpp`. It creates one stable entry for each published `vX.Y.Z`
release. It reads `nightly-tag.txt` to find the `b<N>` build. It also creates
one daily entry for each finished UTC day with a complete build. A complete
build has all known assets for its tag. The build must also be at least three
hours old by default.

Set `GITHUB_TOKEN` in the command environment. The command needs `[database]`.
Set `descriptor_catalog.llamacpp_min_build_age_secs` to change the minimum age.

`--since` sets the first UTC day for daily entries. It does not limit stable
releases. Without `--since`, the command starts two days before the newest
imported daily entry. If there are no daily entries, it checks the last 14
finished UTC days. The command never changes the build of an existing entry.
It checks active imported entries again and adds new flavors at the end.

`catalog refresh NAME` checks one imported entry, even if it is hidden or
pinned. It adds new flavors from the entry's current build. It does not change
the entry's build. Refresh is global only and refuses `--org`. Both commands
record a run in SQL. The command adds no member for an unknown asset. It
records the asset and exits with an
error. The command keeps the valid entries and members that it already added.
If one stable release or one imported entry fails, the command records the
error and continues with the next one. The command then exits with an error.

`catalog import` also applies automatic expiry to the global llama.cpp entries.
Expiry moves an active entry to hidden by the release time of the entry:

- The `stable_keep_newest` active or pinned stable entries with the newest
  release times do not expire. Another stable entry stays active for
  `stable_max_age_days` after its release.
- A daily entry stays active for `daily_max_age_days` after its release.
- Expiry never changes these entries:
  - pinned;
  - hidden;
  - tombstoned;
  - hand-made.

The keys are in `[descriptor_catalog.expiry]`. The defaults are 2, 60 and 14.

The command also applies expiry to each entry that it creates, so an entry with
an old release starts hidden. A hidden entry still resolves and still names old
results. Expiry runs after an import error too. `catalog refresh` does not apply
expiry.

The command prints the number of entries that it hid, and the run record keeps
it as `entries_hidden`.
State commands wait while an import creates stable entries or applies expiry.

`catalog create FILE [--dry-run]` creates a hand-made entry from TOML. The
optional `--org <org>` selects an existing organization. The file has a
`runtimes` list or a `models` list, not both. `release_time` is a quoted
RFC 3339 string or a native TOML datetime, and it cannot be in the future. If
the file omits `release_time`, the command uses the creation time.

The `runtimes` list is in order of precedence. Each runtime uses the inline
runtime table from a plan:

```toml
name = "Fork-2026-10"
release_time = "2026-10-20T00:00:00Z"
runtimes = [
  { type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/example/llama.cpp", version = "b11700", flavor = "macos-arm64" },
]
```

Each item of the `models` list has a `quantization` label and a `model`, which
is the inline model table from a plan. The label matches `[a-z0-9_]+`. The
model type sets the format, for example `gguf` for `gguf_text`. An entry has at
most one member for each format and quantization:

```toml
name = "AcmeLM-VL-3B"
models = [
  { quantization = "q4_0", model = { type = "gguf_text", source = "huggingface", org = "acme", repo_name = "AcmeLM-VL-3B-GGUF", path = "AcmeLM-VL-3B-Q4_0.gguf" } },
  { quantization = "4bit", model = { type = "mlx", source = "huggingface", org = "acme", repo_name = "AcmeLM-VL-3B-MLX-4bit" } },
]
```

A model member must come from a public, ungated Hugging Face repository. The
command asks the Hugging Face API at `hf_import_endpoint` about each
repository, and it sends no token. It refuses these sources:

- a private repository
- a gated repository
- a repository that the API does not report as public and ungated
- a URL, a local file, a local directory, or the vault
- a model table that has an `auth_token`

Each model member pins one commit. The command resolves the `revision` of each
repository to its current commit, and stores the commit. If the table has no
`revision`, the command resolves `main`. The command prints each commit, also
in a dry run. Each file that the table names must exist at that commit, as a
plan import needs it. A changed model needs a new entry under a new name.

`catalog add-member NAME FILE [--dry-run]` appends one member. The optional
`--org <org>` selects an existing organization. A runtime file has one
`runtime = { ... }` inline table. The new runtime must differ from the existing
members only in flavor. An entry cannot mix builds. A model file has one
`model = { ... }` inline table and one `quantization` label:

```toml
quantization = "q8_0"
model = { type = "gguf_text", source = "huggingface", org = "acme", repo_name = "AcmeLM-VL-3B-GGUF", path = "AcmeLM-VL-3B-Q8_0.gguf" }
```

The command applies the same checks to a model member as `create`. No check
can show that two repositories hold the same model, so the operator makes that
claim. For a model member, the command prints the repository and commit of
each existing member and of the new member.

The CLI cannot remove or reorder members. The command refuses vault sources.
It accepts a member that another live entry also holds, and it prints the name
of that entry. A dry run checks the file and prints the proposed entry. It
writes nothing and makes no audit event.

`catalog pin NAME` and `catalog hide NAME` require an active entry.
`catalog unpin NAME` requires a pinned entry. `catalog unhide NAME` requires a
hidden entry. A hidden entry still shows and resolves.

`catalog tombstone NAME` permanently removes a name from reads and resolution.
The name stays unavailable for reuse. The command asks for the exact stored
name, and the prompt needs a terminal. In a script, pass `--yes` to skip the
prompt. A mismatched confirmation changes nothing.

`catalog list` shows active, pinned and hidden entries. It shows the newest
release first. Repeat `--kind`, `--state` or `--runtime-type` to filter.
`--org <org>` lists only that organization's entries; without it, the list is
global only. `--prefix` filters names and ignores case. The default for `--limit` is 50.
Set `--cursor` to the printed `next_cursor` value to continue. Each row prints
these fields:

- the scope (`global` or an organization id)
- the name
- the kind
- the state
- the runtime type or the model type
- the release time
- the member keys: a runtime member's flavor, or a model member's
  `format::quantization`

`catalog show NAME` prints the full entry and its members in order as JSON.
The JSON includes its scope. `--org <org>` selects an organization entry.
`catalog list` and `catalog show` do not return tombstoned entries. `catalog list`
refuses `--state tombstoned`.

### `pipette-mgmt vault list|show|delete|prune`

Administer the **vault**, the private artifacts a plan can name
([design/vault-resources.md](design/vault-resources.md),
[storage.md §3.4](storage.md#34-the-vault),
[Vault](api/vault.md)). **Nothing here creates a resource**:
files are uploaded and described over the API, from the machine that holds
them. **A record is immutable**: a plan carries its id in a URL, so different
bytes or a different owner are a new upload under a new id.

`delete` removes a record, the rest of its upload, and each file no other
resource names. `prune` reclaims a file no record names, such as one a failed
delete left. Nothing schedules it. It prints each file with its size and write
time, keeps a file younger than `--older-than` (an upload stores its file before
it describes it), judges only `{principal_id}/blobs/{sha256}` keys, and deletes
nothing while a record is unreadable, since that record may name one.

`show` prints the record, the variant and file it was uploaded as, its origin
(the public source it copies, or `none`), and the plan entry it belongs to as
the TOML inline table a plan's `models` or `runtimes` lists. It also reports
whether the file is still stored, which a record cannot answer about itself:

```text
Object state present
Object state present, but 8 bytes where the record says 15. Remove the object from the store, then upload the file again: an upload keeps an object already there.
Object state MISSING - clients get 404
```

No mutate lock is taken: `vault/` is a keyspace of its own. A resource id is the
handle, and a value without the `vault_` prefix is refused.

### `pipette-mgmt audit list|prune`

`list` prints events as JSON, one event on each line, newest first. With
`--org`, it prints only the events in the log of that organization. The
filters work as the filters of the HTTP listing do
([Organizations: The audit log](api/orgs.md#the-audit-log)):

- `--outcome` keeps the outcomes in its comma-separated list.
- `--actor` keeps the actor kinds in its comma-separated list.
- `--target` keeps the events that name the record with this id.
- `--limit` sets the maximum number of events to print (50 by default).

`prune` deletes the events that are older than `--older-than-days`. The default
is 730 days, so the audit log keeps two years. No other command deletes an
event, so schedule `prune`
([operations.md §3.4](operations.md#34-audit-log-retention)). `--dry-run`
counts the events that `prune` would delete. Both subcommands need
`[database]`, because no object store keeps the audit log.

### `pipette-mgmt db migrate [--to <version>] [--dry-run]` and `db check`

`db migrate` applies each migration this binary has and `[database]` lacks. Run
it before you deploy a binary that needs a newer schema, from that binary: its
output names the binary's latest migration, so a stale binary shows itself.

```
the schema stays at migration 4 (this binary's latest migration: 4)
moved the schema from migration 4 to 3: revert [4] (this binary's latest migration: 4)
```

`--to` moves the schema up or down, undoing each applied migration above the
target newest first with its down script; without it the command only applies,
so a stale binary never moves the schema down. To move down, roll the service
back first (an older binary runs on a newer schema), then run the newer binary,
which alone carries the down scripts. Undoing `0003` drops the account type, so
every account reads `human`. Undoing `0004` drops the outcome index; the older
binary writes records but no rows, so a row that stays can hold an old outcome
([storage.md §4.3](storage.md#43-the-outcome-index)). The command undoes nothing,
and exits non-zero, while a migration above the target is missing from this
binary, has no down script (`0001`, `0002`), or changed after it was applied.

Undoing `0007` drops the job-cell index and the run flags of the outcome index.
Undoing `0008` drops the note and the revision change of each plan.
Undoing `0011` drops the campaign readers' scopes, and is refused while any
account has one: disable the readers before the rollback, and retire them.

`db check` exits non-zero unless the database answers, sorts in byte order, and
holds every migration this binary has, unchanged. `serve` runs it before it
binds; a newer schema passes, so an older binary runs after a rollback.

### `pipette-mgmt db import [--dry-run]`

Copies every record from the object-store sections into `[database]`: clients,
users, organizations, tags, the key, address and slug claims, the
organizations' benchmarks, plans with their sources and markers, campaigns,
model params and vault records. Blobs, `todo/`, submissions, the warehouse, the
public benchmark catalog and the dataset cache stay in the object stores.
The descriptor catalog and the audit log exist only in `[database]`. `db import`
and `db export` do not copy them. The copy goes
through the store traits, so the server reads each copied record as any other.
It requires `[database]` and runs `db check` first.

A record the database holds unchanged counts as unchanged, so a second run
writes nothing. A claim or write-once record the database holds differently is
a conflict: reported, and the copy goes on. The command exits non-zero on a
conflict or an unreadable source record. The copy omits:

- A pre-auth key with a spent marker: no store method writes a marker, so the
  key is skipped and the marker reported.
- A plan document that names `auth_token`, even in a comment: no store keeps a
  token, so the plan keeps its manifest and has no document on the target.
- A claim no record names: the claim keyspaces have no listing.
- `vault-imports/`, which lets a repeat import reuse its records and the
  archive records of its sources. The next import of a model writes the entries
  again.
- The GC age of the pending-source and `job-plans/` markers, which restarts.

It takes no lock, because no command that writes records takes one: stop `serve`
and those commands first. The report has one row per keyspace, counting Read,
Written (would write, on a dry run), Unchanged, Skipped, Unreadable and
Conflicts, then each skip reason, unreadable record and conflict by id.

### `pipette-mgmt db export [--dry-run]`

Copies the records of `[database]` to the object-store sections, the keyspaces
`db import` copies, the other way. It requires `[database]` and runs `db check`
first. It makes the target equal to the database: it writes each record that
differs, gives each claim to the holder the database names, replaces a differing
plan source or content key, and deletes each record and marker the database
does not hold, only after a read confirms it gone (one the database cannot read
is reported and kept). It deletes a pre-auth key spent or absent in the
database. Plan manifests, job sources, job-plan links and signature migrations
have no delete method, so a target's extras of those stay.

Conflicts and unreadable records are reported, and the command exits non-zero.
A second run writes and deletes nothing. It takes no lock: stop `serve` and the
commands that write records, and the copy is exact. The table adds a Deleted
column to `db import`'s.

### `pipette-mgmt db backfill-job-cells [--dry-run]`

The command fills the job-cell index for jobs that ingestion minted before the
index existed ([storage.md §4.4](storage.md#44-the-job-cell-index)). It requires
`[database]` and runs `db check` first. The command makes two passes:

1. It writes a `job_cells` row for each job source that has none. The row holds
   the cell digest of the source and no scope, so its run answers no later
   plan.
2. It reads the submission record of each job whose outcome row is absent, or
   is `finished` with no run flags. It writes the outcome and the run flags of
   that record. If a submission write puts a row there first, the command keeps
   that row.

Run the command at these times:

- Once after migration `0007`.
- Again after `db import`.
- Again after ingestion logs the warning `failed to index the cell of a minted
  job`.

Run the command only after every binary that writes records runs migration
`0007` or later. An older binary can rewrite an outcome row and keep the run
flags of the earlier run, and the backfill does not repair that row.

A second run writes nothing. The command reads every finished record that has
no run flags. A record can be tens of megabytes (MB).

```
wrote 1824 job cell(s) and 1690 submission outcome row(s)
41 job(s) have no submission record
0 job source(s) and 0 record(s) do not parse
```

A job with no submission record is unfinished, or its record was pruned. The
command skips a job source or a record that does not parse. It logs a warning
with the job id of the skipped item, and counts the item. If a read fails for a
different cause, the command stops and exits non-zero. It takes no lock.

### `pipette-mgmt openapi [--site <dir>]`

Prints the OpenAPI document `GET /api/v1/openapi.json` serves
([HTTP API: The OpenAPI document](httpapi.md#15-the-openapi-document)) to
stdout as one line of JSON. The document is built into the binary, so the
command reads no config file.

`--site <dir>` writes the API reference instead: `index.html`, one static page
of every route and model with no script, and its `style.css`. A link between
routes or tags points at their section of the page; every other link goes to
GitHub, where only the repository's members can open it. mgmt serves the same
page at `/docs/` ([HTTP API](httpapi.md#15-the-openapi-document)).

## Concurrency

`process-submissions` / `score` and the `fix-*` commands each do
read-modify-write on the warehouse Parquet partitions or submission bodies.
Running two of them at once interleaves one command's read with another's write
and silently drops rows.

To prevent that, every mutating run holds an advisory **storage mutate lock**
at `<storage-root>/locks/mutate.lock` for its whole duration. A second mutating
command finds the lock held and fails fast with a message naming the current
holder, rather than corrupting data. The mutate lock covers
`process-submissions` (`score` alias), `fix-model-param`, `fix-canonical`,
`fix-vault-labels`, and `requeue-eval`. `--dry-run` invocations are read-only
and skip the lock.
`serve` is **not** covered, and neither are the auth-store commands
(`clients …`, `users …`, `reindex`, `flag-reindex`), which write
`[auth_storage]`, the two relation keyspaces `user-orgs/` and `client-owners/`,
and `tags-index/`. The `models` commands are not covered either: the audits are
read-only, so a scoring pass can run alongside them.

`score-eval` uses a separate `locks/score-eval.lock`: it prevents overlapping
eval scorer runs without blocking mutate-locked warehouse writers. `unlock`
clears only the mutate lock.

The mechanism is backend-specific, because each backend has a different correct
primitive:

- **`local_fs`**: an exclusive `flock(2)` on the lock file. The kernel
  releases it when the holding process exits or dies (including a crash or
  `kill -9`), so a stale lock is impossible and there is no takeover step.
- **`s3`**: a lease object whose body records an `expires_at`,
  `mutate_lock_ttl_secs` into the future. A crashed command leaves it behind;
  the next mutating command past `expires_at` takes it over with a
  compare-and-swap, so two processes racing one stale lease cannot both win.
  The `score-eval` lock uses the same TTL, and a run that outlives its lease can
  lose the lock mid-write. [`unlock`](#pipette-mgmt-unlock) clears a stale
  mutate lease by hand.
