# Campaigns

A **campaign** is the stable id for work whose plan keeps being revised. A
`plan_id` names one immutable ingestion, so revising a run mints a new one and
every reference to the old plan goes stale; a `campaign_id` never changes, and
`current_plan_id` names the plan being executed under it. A dashboard, a bookmark
or a scheduled report holds the campaign and follows the pointer. See
[the campaign record](#the-campaign-record).

**A campaign mints its plans.** [`POST /api/v1/campaigns`](campaigns/create_campaign.md)
creates a campaign with its first plan, filed from a plan document.
[`POST /api/v1/campaigns/{id}/plans`](campaigns/file_plan.md) ingests a
revision as a new plan that belongs to the campaign. The new plan supersedes
the plan that the campaign ran. The route then moves the campaign onto it.
Each of these routes also files a revision and moves the campaign onto it. The
revisions differ:

- [`POST /api/v1/campaigns/{id}/variants`](campaigns/edit_variants.md): the
  document of the plan that the campaign runs, with variants added or removed.
- [`PUT /api/v1/campaigns/{id}/document`](campaigns/revise_document.md): a
  whole new document in place of that plan's document.
- [`POST /api/v1/campaigns/{id}/rerun`](campaigns/rerun_jobs.md): that plan's
  document, filed again to run some of its jobs again.
- [`POST /api/v1/campaigns/{id}/recalculate`](campaigns/recalculate_jobs.md):
  that plan's document, filed again to score some of its evals again.
- [`POST /api/v1/campaigns/{id}/revert`](campaigns/revert_campaign.md): a copy
  of the document of an earlier plan of the campaign.

No route points a campaign at a plan that somebody chose. A plan thus never
has two campaigns:

```text
Campaign ──current──> P2 ──previous──> P1
```

The [console app](../../ui/README.md#pages) draws the same records, and
[`pipette-mgmt campaigns`](../cli.md) is the operator surface.

**A pointer, not a container.** A campaign holds no jobs and schedules nothing.
Cancelling a plan is [`POST /api/v1/plans/{plan_id}/cancel`](plans/cancel_plan.md).
The exceptions are a [variant edit](campaigns/edit_variants.md), a
[document revision](campaigns/revise_document.md), a
[re-run](campaigns/rerun_jobs.md), a
[recalculation](campaigns/recalculate_jobs.md) and a
[revert](campaigns/revert_campaign.md), which cancel the plan
they replace. A campaign delete removes the pointer. Every plan that it named
keeps its manifest, its jobs and its lifecycle. The API refuses the delete while
a plan that the campaign filed still runs. Cancel those plans first.

**Every route takes an active user.** A campaign is an operator's record, and a
device can hold no permission that reaches one
([permissions](users.md#permissions)).

## The campaign record

`Campaign` ([the OpenAPI document](../httpapi.md#15-the-openapi-document))
carries `campaign_id`, `name`, `owner`, `created_at`, `updated_at` and:

- `current_plan_id`: the plan the campaign runs. Only a filing sets it, so it
  names a plan this campaign minted, though possibly one an older build
  deleted. It is absent only on a record from before every campaign was created
  with a plan, which runs nothing. Each plan's `previous_plan_id` leads back
  through what the campaign ran
  ([`GET …/plans`](campaigns/list_campaign_plans.md)).
- `labels`: free `key: value` pairs to group and filter by, on the rules of an
  AWS resource tag: at most 50, a key of 1 to 128 characters and a value of 0
  to 256, each of letters, digits, spaces and `_ . : / = + - @`, with no space
  at either end, compared as written. Absent when nobody labelled the campaign.
  Nothing on the server reads a label.
- `state`: `"archived"` on a campaign taken out of use, which takes no new
  plan ([archiving](campaigns/update_campaign.md)). Absent while active.

A write replaces the whole record with no version check, so two administrators
changing one campaign at once is last-writer-wins. A write that asks for the
state the record already holds writes nothing and leaves `updated_at`, so it is
safe to retry. A revision moves the campaign only if it still runs the plan
the revision replaced and still has the same owner; otherwise the server
cancels the revision and answers `409`.

## Who may do what

There are **no new permission names**. Reading a campaign is a plan read and
repointing one is the authority to decide what work runs, which the vocabulary
already spells. An operator who can file plans today can run campaigns today,
with nothing re-granted.

The **ownership rule** is the whole of the write model:

- A **user-owned** campaign is its owner's. Nobody else's `submit_plans` reaches
  it, whatever organization the two share.
- Once its owner is **disabled**, a user-owned campaign is also its
  organization's administrators' to wind down, since the owner can no longer
  do it: they read the record, rename it, delete it, hand it over, and cancel
  its plans with `cancel_plans`. They do not read its plans or file into it
  until it is handed over, and they reach no account holding a server-wide
  name they lack ([the rules that make delegation safe](users.md#the-rules-that-make-delegation-safe)).
- An **org-owned** campaign is run by that organization's members, whoever
  created it: each reads it, files into it and revises it with `submit_plans`,
  and cancels its plans with `cancel_plans`. Its **record** belongs to the
  organization's administrators, the accounts holding `manage_orgs` **and**
  belonging to it: they rename it, hand it over and delete it. A vault
  resource draws the same line, where a member uploads and an administrator
  deletes ([the vault's who may do what](vault.md#who-may-do-what)).
- `manage_any_orgs` administers every campaign and reads its record, and `read_any_plans` reads every one with its plans.

**A plan has no owner; its campaign does.** The plan grants reach a plan through
the campaign that filed it, as that campaign's owner stands now
([permissions](users.md#permissions)):

- `read_plans` reads the plans of every campaign the caller reads: its own, and
  its organization's.
- `cancel_plans` cancels the plans of every campaign the caller owns, its
  organization owns, or it winds down for a disabled member of it. Its own
  counts only while it holds `submit_plans`, the grant it owns one under; its
  organization's counts on membership alone.
- `read_any_plans` and `cancel_any_plans` reach every plan.

Who filed a plan (`submitted_by`) grants nothing. So handing a campaign over
hands over every plan it filed ([`PUT …/owner`](campaigns/set_campaign_owner.md)), and
no route moves or edits a plan. `manage_any_orgs` administers every campaign
without reading or cancelling its plans, which take `read_any_plans` and
`cancel_any_plans`; revising one reads and ends the plan it runs, so it takes
both. Nobody reaches a deleted campaign, so its plans are left to
the `_any_` grants. A plan filed outside every campaign, which an older build
allowed, is its filer's.

Ownership is **not** derived through the owner's membership, where a user-owned
device rolls up to whichever organization its owner belongs to now. Moving an
account between organizations does not hand that account's personal campaigns to
a team's administrators, nor take them away.

A campaign outside the caller's scope is a **`403`**, where a plan or an
organization outside one reads as `404`. Those two are addressed by values a
caller can guess (a handle somebody typed, a plan id from a shared listing), so
confirming existence maps the deployment one probe at a time. A campaign id is a
minted UUIDv7 under a `campaign_` prefix, so a caller that can name one was given
it, and the refusal tells them the true thing: a campaign the caller owns is
refused as needing `submit_plans`, a write to the record of one its
organization owns as needing `manage_orgs`, and any other as belonging to
another owner.

## Filing a plan document

What the filing routes share: the plan document ([request body](#request-body)),
the report and the dry run ([response](#response-201-created)), and the errors
([errors](#errors)). These routes take a plan document as their body:

- [`POST /api/v1/campaigns`](campaigns/create_campaign.md) creates a campaign with
  its first plan.
- [`POST /api/v1/campaigns/{campaign_id}/plans`](campaigns/file_plan.md)
  files the next plan of a campaign.
- [`PUT /api/v1/campaigns/{campaign_id}/document`](campaigns/revise_document.md)
  replaces the document of the plan that a campaign runs.

A [variant edit](campaigns/edit_variants.md), a [re-run](campaigns/rerun_jobs.md),
a [recalculation](campaigns/recalculate_jobs.md) and a
[revert](campaigns/revert_campaign.md) also file a plan, and refer to these
subsections. [The plan document](#the-plan-document) and
[what the server checks](#what-the-server-checks) describe the format, the
expansion and the validation rules, and [plan statuses](plans.md#plan-statuses)
the plan lifecycle.
[`POST /api/v1/plans/validate`](plans/validate_plan.md) checks a
document without filing it.

The server validates the job bodies of a plan as a unit and stages them into
the queue as one plan, where [`POST /plans/claim`](../httpapi.md#29-post-plansclaim) hands
them out. It stores the plan document, with each imported model as its vault
arm, and [`GET /api/v1/plans/{plan_id}/document`](plans/get_plan_document.md)
returns it.

**The server derives attribution from the credential.** It records the user
that signed the request as the plan's `submitted_by`, and the request cannot
name another account. A plan the `pipette-mgmt campaigns` commands filed
records no submitter.

**A gated model is
[imported](../plan-ingestion.md#the-import-of-a-gated-model), and the server
stores no token.** A model in a plan document can carry an `auth_token`. The server downloads that model from
HuggingFace with the token into the vault ([the vault](vault.md)), and stages
each job body and stores the document with the vault arm in place of the model.
The campaign's owner owns the records, so only the owner's devices can fetch
the model, and a later plan of the same owner that names it downloads nothing.
The vault arm is `vault_url` for a gguf model and `vault_archive` for a
directory model, naming the owner in `principal_id`. The rewritten bodies are
checked as any vault reference is, so the filer needs the owner in its reach
([naming a resource in a plan](vault.md#who-may-do-what)): the owner, a member
of the owning organization, or a holder of `submit_any_plans`. A model with a
field its type does not know is not imported, since the rewrite would drop the
field, and its token is refused. A plan of another owner
imports the model again, under that owner. An import that completed keeps its
records when a later step fails, and a retry reuses them. A revision expands
the stored document, where each imported model is already its vault arm, so it
imports nothing and its jobs carry over.

A download can take a long time. A proxy closes an idle connection after its
timeout, for example 60 seconds on an AWS load balancer. To keep the connection
open, send `Accept: application/x-ndjson` (newline-delimited JSON). The server
then answers with a progress stream ([response](#response-201-created)).
The stream sends a line at intervals of 15 seconds or less. Without the header,
the server sends no bytes until the import completes.

A caller that closes the connection during an import stops the import, and the
server files no plan. The server keeps each gguf file that it stored
completely. A retry does not download that file again. A retry downloads a
directory model again.

A filing is **not idempotent**: the same document filed twice creates two
plans. Those two plans **share** their jobs: the server merges duplicate work
(see [shared jobs](#shared-jobs)).
So the second filing costs a plan manifest rather than a second run of every
benchmark cell.

### Request body

The body is the text of a scheduler plan document, in UTF-8, with
`Content-Type: application/toml`. The server reads only the essence of the
type, ignores its case, and ignores its parameters, such as `charset`. Any
other type, JSON included, or no `Content-Type`, gets `415`.

[The plan document](#the-plan-document)
describes the format, and the OpenAPI document's `SchedulerPlan` schema
([the OpenAPI document](../httpapi.md#15-the-openapi-document)) lists every field.
Each filing route's request example is such a document, and the repository's
[`examples/plans/scheduler/`](../../examples/plans/scheduler/) holds more.
The server parses the document, applies the capability rules, and expands each
cell into a job body
([what the server checks](#what-the-server-checks)).
It then validates and stages the job bodies, in the order of the expansion. A
`400` that names a job by index names the job at that position in the
expansion. A dry run returns the expansion in that order.

The metadata travels in the query string, which the request signature covers.
An unknown parameter gets `400`, so a misspelt `dry_run` stages nothing:

| Parameter | Required | Description |
|-----------|----------|-------------|
| `note` | no | Free text on why the plan was filed. The server trims the value; an empty or whitespace-only value counts as no note. The trimmed value must be at most 4096 characters and must not contain a NUL character. To change the note after filing, use [`PATCH /api/v1/plans/{plan_id}`](plans/update_plan.md) |
| `dry_run` | no | `true` to expand and validate the document and stage nothing. The default is `false` |

The server stores a document as submitted, and
[`GET /api/v1/plans/{plan_id}/document`](plans/get_plan_document.md) returns it to each
caller that can read the plan at the full fleet-detail tier. A document whose
models carry a token is the exception. The server stores it with each imported
model replaced by its vault arm, under a header comment, and with no token. The
stored document has a new layout and none of the original comments.

A model, or the draft of a speculative model, can carry an `auth_token`. The
server imports the model ([filing a plan document](#filing-a-plan-document)). The server refuses a
`runtime_flags` entry that sets `HF_TOKEN`, `HUGGING_FACE_HUB_TOKEN` or
`PIPETTE_HF_TOKEN` to a value with `400`. The bare name of the variable, which
inherits the value on the device, is allowed. Set `PIPETTE_HF_TOKEN` on each
device that runs a gated model.

The server does not check a document for other secrets. It stores each other
value as written, so do not put a secret of any other kind in a document.

```bash
curl -X POST "https://mgmt.example/api/v1/campaigns/campaign_0195c3e2-…/plans?note=afm-smoke" \
  -H "Content-Type: application/toml" \
  --data-binary @examples/plans/scheduler/afm-mlx.toml
```

The example leaves out the [signature headers](../httpapi.md#1-authentication).

### The plan document

The `SchedulerPlan` schema of
[`POST /api/v1/campaigns`](campaigns/create_campaign.md)'s request body lists
every field, with each model, runtime and flag shape. A document declares
which clients may run each cell, not where it runs, so it has no transports.
What the schema cannot say:

- **Expansion.** Each `[[variants]]` block is a matrix of benchmarks × models
  × runtimes, and its `benchmarks` overrides the top-level list. Each cell is
  one job body, and the expansion sorts the bodies by their content, not by
  variant. Two cells that render one body, such as two
  variants that differ only in a flag the capability rules inject, make one,
  also under `merge_duplicates = false`. So do two cells that differ only in
  a model's `auth_token`, both with one: the body keeps the first token in
  sort order. A cell with a token and one without make two bodies, and the
  one without runs from HuggingFace with the device's token.
- **Eligibility.** Every variant names `requires`, `clients`, or both. A job
  admits each client its `clients` names (a device id, `ev1_…`) and, when
  `requires` is not empty, each one whose flags meet `requires` and the
  injected `any_of` groups; a variant with no `requires` runs on its roster
  alone. `exclude_clients`, `excludes` and `not_all_of` take clients out
  ([the capability vocabulary](devices/get_capability_vocabulary.md)).
- **`client_versions`** pins a variant to app builds: one more `any_of` group
  of `client_version:<v>` flags, which a device reports for every build it
  can run. The `clients` roster ignores it, so pin a
  device by a `tag:` flag in `requires` to pin its build too. A negative
  clause reads the device's whole set: one that reports a version `excludes`
  names is not offered the job, whatever else it reports. Copy an iOS
  runtime from the build's release manifest as written: on the proxied iPhones
  a runtime without `private_thermal = true` is in no internal build, and
  `private_memory` defaults to `private_thermal`.
- **`:required`** after a `client_versions` value or a `tag:` flag (in
  `requires`, or on every member of an `any_of` group) makes a revision keep
  only runs that held the flag
  ([carrying jobs into a revision](#carrying-jobs-into-a-revision)). The job
  matches the plain flag. A negative clause or a roster refuses the suffix, as
  does an `any_of` group that marks some members and not others. On any other
  flag the suffix is part of the flag.
- **`expires_at`** is an RFC 3339 timestamp or `never`. Without one, each job
  expires 30 days after the filing. A job a plan shares keeps the deadline it
  has ([shared jobs](#shared-jobs)).
- **`merge_duplicates`** is a boolean, `true` by default. `false` gives this
  plan its own run of every cell, for a plan whose point is a second,
  independent run, such as one that measures variance. A later plan can still
  join the jobs it mints.
- **`custom_label`** on a vault source must be the `name` of the record its
  path names. It travels to the device and into a result's descriptor, and no
  part of a job's identity depends on it. The vault's answer to an upload
  carries the entry with its label; the vault arm an import writes carries
  none.

### What the server checks

Two passes, each stopping at the first failure; a refusal files nothing (only
a completed model import keeps its vault records; see
[what a failed filing leaves](#errors)). A revision checks its whole document
again, under the reviser's reach.

**The document.** It must parse as a `SchedulerPlan`: each model fits a
runtime of its variant and each runtime a model, every variant names
`requires` or `clients`, each `client_versions` value is a canonical flag
(non-empty, lowercase, no whitespace) listed once, and `expires_at` and
`merge_duplicates` are well formed. Each `runtime_flags`, `model_flags` and
`benchmark_flags` entry selects at least one cell of its variant, and no two
select the same one. An entry selects by `benchmark_type`, which is the type of
the benchmark's definition in the catalog the filer reaches. A private id does
not have to start with its type. An id outside that catalog is typed by its
prefix, and the job-body checks refuse it. Then come the `runtime_flags` token rule
([request body](#request-body)) and the `plan_max_jobs` ceiling. Last, the
capability rules inject the hardware policy for each runtime kind (for example
the supported iPhones for the Apple Foundation Model on iOS) and refuse a
variant that contradicts it, requires two flags of one reserved namespace such
as `os:`, or describes a body no client could ever match. A variant whose
`clients` roster is still live gets a `plan_warnings` entry instead, as does a
variant whose `exclude_clients` denies every listed client, or whose roster
ignores its `client_versions`.

**The job bodies.** Each must:

- Carry well-formed eligibility clauses, negatives included, with at most one
  flag of a reserved namespace in `requires` and in each `not_all_of` group.
- Expire after the time of filing. The server checks again after an import,
  which can take hours.
- Carry a `spec` with `model`, `runtime` and a `benchmark` in the catalog the
  filer reaches: the public one, plus its own organization's private
  benchmarks (every organization's with `submit_any_plans`). A private id out
  of reach is refused as an unknown one is, and an id two organizations hold
  is refused for everyone. The message suggests the nearest id.
- Be runnable where this build can parse the spec: the runtime loads the
  model, the `runtime_flags` fit the runtime and the model, and an eval
  cell's engine honors the context bound its catalog entry sets. On a
  free-text entry with no bound, a vLLM cell must not pin `max_model_len` at
  or under `parameter_max_tokens`, where every request would get vLLM's
  HTTP 400.
- On a vlm_eval benchmark whose entry has a recipe, name a model, projector
  and `enable_thinking` that the recipe covers, judged by the model's
  HuggingFace origin. No other entry for the same task and dataset version (a
  public one, or one private to the same organization) may also cover the
  model. A model or `model_flags` this build cannot read is refused.
- Name vault resources that pass the checks in [errors](#errors), and models
  whose import succeeds.

A model that has no entry in the model parameter table is never refused here:
it is an `uncatalogued_model` warning.

### Response `201 Created`

The ingest report, `PlanFiled`
([the OpenAPI document](../httpapi.md#15-the-openapi-document)), minus the per-input merge detail
`campaigns ingest` prints. Its `job_ids` maps each job's index in the expansion,
as a decimal string, to the `job_id` it landed on, in expansion order; two
indices that describe one cell map to one `job_id`. `plan_warnings` answers the
filer about its own document, so every tier reads it here;
[`GET /api/v1/plans/{plan_id}`](plans/get_plan.md) shows them at the
full tier only.

Each `warnings` entry covers one group of jobs, lists their job ids, and names
its `kind`: a `fleet` warning when no *working* client (registered, approved,
holding `perform_work`) matches the group's requirements, one per distinct
requirement set (all six eligibility clauses), and an `uncatalogued_model` one,
per model, when the model has no entry in the model parameter table
([model params](model-params.md)). A kind
this build does not know, which a later build wrote, is reported as written,
its message withheld as a `fleet` message is.

**The `fleet` warning text follows the caller's tier.** A user holding
`read_clients` or `read_any_clients` reads it whole. Every other caller reads
the withheld tier: the server rebuilds each `fleet` message from the number of
job ids (`1 job matches no working client`) and drops the requirement set. A
warning of another kind describes the caller's own jobs, so every caller reads
its message as ingestion wrote it. Read `fleet_detail` to tell the two forms
apart; [fleet disclosure](devices.md#fleet-disclosure) lists
every surface that carries the tier.

**This response reports the count, not the itemization.** `merged_job_count`
answers why the plan holds fewer jobs than it sent cells. The per-input
`merges`, which `pipette-mgmt campaigns ingest` prints on the server host, is
what the response drops.

**The narrower shape is not a redaction.** The `job_ids` map carries the shared
`job_id`. A job id is a UUIDv7, so its first 48 bits are the time the server
minted it. A submitter that parses the ids sees which indices landed on a job
minted before the request. That is the same overlap the `merges` array names. To
withhold it, the server would have to answer with per-plan alias ids, which
changes every endpoint that takes a `job_id`.

To run every cell independently, set `merge_duplicates = false` at the top level
of the plan document
(see [the plan document](#the-plan-document)).
That flag governs what this plan joins. A later plan can still join the jobs it
mints.

A warning does not fail the submission. The jobs stay in the queue. A client
that matches them can register afterward. Track the plan with
`pipette-mgmt plans status`.

**A dry run answers `200 OK`.** The server expands the document and runs
every check that a submission runs before it downloads a model. It stages
nothing: no manifest, no job and no source.

For each model that carries an `auth_token`, the server asks HuggingFace for
the commit and the file list, with the token. The dry run downloads no file and
writes no vault record. A dry run refuses a token for the same reason as a
submission. The status and the message are the same. A dry run takes no import
permit. A submission answers `429` when the service holds every import permit.
The dry run of the same document answers `200`. Each request to HuggingFace
takes a request permit, as a submission does. When HuggingFace answers `429`,
the dry run answers `429`, as a submission does.

The body is `PlanDryRun`
([the OpenAPI document](../httpapi.md#15-the-openapi-document)): the job bodies, the planner's warnings, and
what the submission does for each gated model. A gated model appears in its
HuggingFace form without the token, and two tokens for one model give one
`imports` entry. A dry run runs no fleet match and reads no model parameter table, so it carries no
`warnings`.

**A progress stream answers `200 OK`.** Send `Accept: application/x-ndjson` to
get it. The header opts in if one of its media ranges is
`application/x-ndjson`, in any letter case and with any parameters. A wildcard
range, such as `*/*`, gets the plain answer. The server reads the body before
the stream starts, so a refusal before or during that read keeps its status
([errors](#errors)).

After the read, the server sends `200` at once, with
`Content-Type: application/x-ndjson`. Each line of the body is one JSON object,
one of the OpenAPI document's `…IngestEvent` schemas
([the OpenAPI document](../httpapi.md#15-the-openapi-document)), whose `event` field names its kind. The server sends at most one `progress`
line for each file in each second, and one when the file completes; an earlier
line can drop when the caller reads slowly, but the completing one always
arrives. A `keepalive` follows 15 seconds with no other line. The last line is
one `result`, carrying the plain answer's status and body, or one `error`,
carrying its status and message.

```json
{"event":"progress","hf_ref":"LiquidAI/LFM2-700M-GGUF@3f1c…","file":"LFM2-700M-Q4_0.gguf","bytes_done":33554432,"bytes_total":469762048}
{"event":"keepalive"}
{"event":"progress","hf_ref":"LiquidAI/LFM2-700M-GGUF@3f1c…","file":"LFM2-700M-Q4_0.gguf","bytes_done":469762048,"bytes_total":469762048}
{"event":"result","status":201,"body":{"fleet_detail":"full","plan_id":"plan-018f4c2e-7a10-7c3d-9b21-4d6f9a2c5e88","job_count":1,"merged_job_count":0,"job_ids":{"0":"job-550e8400-e29b-41d4-a716-446655440000"},"warnings":[]}}
```

Ignore an `event` that you do not know. If a stream ends with no `result`
line and no `error` line, the connection closed before the end. The plan can
still exist, because the server can complete its writes after the connection
closes. On a campaign, read its revisions
([`GET …/plans`](campaigns/list_campaign_plans.md)) before you file the plan again. A
line never holds a token.

### Errors

Each route that files a plan answers these errors. Its own page lists the
errors of its campaign checks.

| Status | Condition |
|--------|-----------|
| 400 | The query string carries a parameter this route does not know, such as `submitter` or a misspelt `dry_run`. Nothing is staged |
| 400 | A plan document is not valid UTF-8, is not a valid scheduler plan, or breaks a capability rule. The message names the variant at fault |
| 400 | A plan document sets a HuggingFace token variable to a value in `runtime_flags`. The message names the variant and never the token |
| 400 | A plan document names `auth_token` outside the token of a model, for example in a comment. The server stores no token, so it refuses the document before an import. The message never quotes the token |
| 400 | A plan document expands to more cells than `plan_max_jobs` (default 5000). The server counts the cells before it expands the document. Nothing is staged |
| 400 | The trimmed `note` is over 4096 characters or contains a NUL character |
| 400 | A job failed validation ([what the server checks](#what-the-server-checks)). The plan is rejected as a unit and **nothing is staged**. Validation stops at the first offending job, so the message names that one job by its index in the expansion |
| 400 | A job names a vault resource that does not exist, or one outside the filer's reach, answered exactly as one that does not exist ([the vault's who may do what](vault.md#who-may-do-what)), or one with no `principal_id` beside it or whose owner is not that `principal_id`, or whose object has been removed, or pins a `sha256` the record does not (`sha256` beside `url`, `model_sha256` beside `model`, and so on), or carries a `vault_*` source with no mgmt-relative path, or a `custom_label` that is not the `name` of every record it names ([the vault](vault.md), [what the server checks](#what-the-server-checks)). The message names the resource |
| 400 | HuggingFace refuses the import of a job's model: the token has no access, or the repository, the revision or a file does not exist. The import also refuses a file that does not match its pin, a gguf file not in Large File Storage (LFS), a file over `vault_blob_limit_bytes`, and a file that is not what its role needs ([what the server checks](#what-the-server-checks)). The message names the job and the model, and never the token. Nothing is staged |
| 401 | Missing or invalid auth headers. A device (`ev1_…` in `X-Client-Id`) gets `401`, because only a user signs these routes |
| 403 | A user is pending an operator's approval, or is disabled |
| 403 | A user holds no `submit_plans`: `user lacks the submit_plans permission` |
| 413 | The body is over `plan_body_limit_bytes` (see [request size limits](../httpapi.md#12-request-size-limits)). Nothing is staged: the body is never parsed |
| 415 | The `Content-Type` is missing, or is not `application/toml` |
| 429 | The plan must download a gated model, and every one of the `hf_import_max_concurrent` import permits is held. Nothing is staged or downloaded. Retry later |
| 429 | HuggingFace answered `429` while it served a job's model, because it limits the requests of the token. The message names the job and the model, and gives the wait if HuggingFace sent one. It never holds the token. Nothing is staged. Retry after the wait |
| 500 | The store failed while reading the benchmark catalog, writing the manifest or staging a job. Nothing it staged is claimable: its `tmp/` files are reaped, and `queue-maintenance` [tears down](../plan-ingestion.md#8-ingestion-flow-atomic-ish) its manifest, stuck in `creating`, once that is older than `plan_creating_max_age_secs` (default 6 hours) |
| 502 | HuggingFace failed while it served a job's model: a `5xx`, a lost connection, or bytes that do not match the listing. Nothing is staged. The log names the job. Retry later |

With `Accept: application/x-ndjson`, the server sends `200` after it reads the
body ([response](#response-201-created)). A refusal decided before then
keeps its plain status: a `401`, `403`, `404`, `413` or `415`, a `400` for the
query string or while the body is read, and a revision's `409` for a campaign
that runs no plan or one with no stored document, or, on a variant edit, a plan
other than `base_plan_id`. Every other failure, of this table or a route's own, is the
last line of the stream: an `error` line with the same status and message.

The authentication, status and permission checks read the headers alone, so they
precede the body. An unauthenticated caller or a device gets `401`. A pending or
disabled user, or a user that holds no `submit_plans`, gets `403`, whatever the
body's size. The server never reads a plan-sized body for a caller that it
refuses.

A `400` names the offending job:

```json
{"error": "rejected job \"2\" (index 2): spec.benchmark decode_throughput_512_10 not in the benchmark catalog; did you mean decode_throughput_512_100?"}
```

**What a failed filing leaves.** Every check runs before the first write,
except the import's vault records, which an import writes as it completes. A caller that disconnects during an import stops
it, and nothing is filed. After the import, a route that files into an
existing campaign reads it again: `404` if a delete removed it meanwhile, `409`
if its owner or its current plan changed, `403` if the caller may no longer
file into it. From then
on the filing runs to completion, disconnect or not. The plan is written
before the campaign moves onto it, so a failure between the two leaves a plan
that names the campaign and never took effect: file again. A `500` leaves
nothing a device can claim. A staged job is claimable from the next
`queue-maintenance` run.

### Shared jobs

A cell that a live job of any plan already runs joins that job rather than
minting one: the job runs once, and each plan counts it as its own. Two bodies
share a job when they are equal after the import, with the six eligibility
clauses compared as sets, so a merge never changes who may run a job. The job
id, `expires_at`, `merge_duplicates`, a model's token and a vault source's
`custom_label` take no part. Two spellings of one number (`1` and `1.0`)
differ, so both run: a missed merge, never a wrong one.

A plan that joins accepts the job's deadline. Each plan stops waiting at its
own `expires_at`: once that passes with no result, the plan lists the job in
`expired_jobs` and counts it `failed`, and the job stays queued for the plans
still waiting on it.

A cell the fleet already measured need not run again. On a server with a
`[database]`, a cell takes the newest successful run of the same
cell whose recorded device meets its clauses, from a campaign of the same
scope: the owner's organization and its users, or, for a user in no
organization, that user's own campaigns. The plan lists that job, which
counts `finished`, and `merged_job_count` counts it. A job a later plan
re-ran answers nothing. `merge_duplicates = false` skips both this and the
merge.

### Carrying jobs into a revision

A [variant edit](campaigns/edit_variants.md), a
[document revision](campaigns/revise_document.md), a
[re-run](campaigns/rerun_jobs.md), a [recalculation](campaigns/recalculate_jobs.md)
and a [revert](campaigns/revert_campaign.md) supersede the plan that the
campaign runs. Before it looks for shared jobs, the revision takes each job of
that plan whose body it still describes:

- A **finished** job, failed or not, is listed and counted as the superseded
  plan counted it. Nothing runs it again.
- A **queued or running** job is joined, so cancelling the superseded plan
  does not stop it, unless a finished run of the same cell answers it
  ([shared jobs](#shared-jobs)): the revision then lists that run, and leaves
  the queued job to the superseded plan.
- A job past its deadline, gone with no record, or one the superseded plan
  lost to a lapse is not carried. The cell is placed afresh.

**A change of eligibility moves where a cell runs next, not a run it already
has.** A cell whose clauses changed takes the superseded plan's successful run
of the same body when that run still answers them. With the routing flags left
out (`tag:` and `client_version:` flags not marked `:required`, and the
`clients` and `exclude_clients` rosters), either the clauses are unchanged and
the old body had no roster, or the new clauses add no exclusion and the
recorded device is known to hold each flag they require. Anything else runs
again. So moving a build pin keeps the runs on other builds, and a `:required`
pin keeps only runs on the builds it names.

`carry_earlier` and `inherit_from`, on the routes that take them, let a cell
that the superseded plan has not answered take a successful run of the
campaign's earlier plans, newest first, then of the named plans in the order
named, by the same rule. A job that lapsed, or that a later plan of its
campaign re-ran, is not on offer, and a named plan's live job is joined as a
shared job instead. Carrying ignores `merge_duplicates`.

A re-run withholds the jobs it names: each cell is a fresh job that joins
nothing, and `rerun_of` maps it to the job it runs again. A recalculation
withholds them too, and scores a copy of each named job's record under a fresh
job that never queues. Both file the superseded plan's own document, so each
must place every cell again. A document that now expands differently is
refused with `400`, and nothing is filed.
