# `POST /api/v1/campaigns/{campaign_id}/variants`

```text
POST /api/v1/campaigns/campaign_0195c3e2-…/variants?base_plan_id=plan-0195c400-…&note=nightly-2
Content-Type: application/toml

remove = [0]

[[variants]]
requires = ["os:macos"]
models   = [{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "LFM2-700M-MLX-4bit" }]
runtimes = [{ type = "mlx_macos_pipette", version = "0.31.3", flavor = "macos-arm64", source = { type = "pip_requirements_text", contents = "mlx-lm==0.31.3\n" } }]
```

Revises the campaign by its **variants**, in four steps:

1. Takes the document of the plan the campaign runs, drops the `[[variants]]`
   that `remove` names, and appends the ones the body carries, then those of
   each `add_from` plan, then each `restore` variant.
2. Files the result as a revision, as [`POST …/plans`](file_plan.md)
   files a document. The revision **carries over** every job of the replaced
   plan that it still describes, finished ones included, so it runs nothing
   again. Only the cells it adds are new jobs
   ([carrying jobs into a revision](../campaigns.md#carrying-jobs-into-a-revision)).
3. Moves the campaign onto the revision.
4. Cancels the replaced plan. The next `queue-maintenance` teardown releases
   every job the revision carried, because the revision holds it, and retires
   the jobs the edit dropped. A dropped job that another plan also holds is
   released too, and runs for that plan.

The body is a TOML **variant edit**, and it has two keys:

| Key | Description |
|---|---|
| `remove` | Indices of the variants to drop, numbered from 0 in the order of the current document, as the planner's messages number them |
| `[[variants]]` | Blocks to append after the variants that stay, written as in a plan document |

The rest of the plan document (`benchmarks`, `expires_at`, `merge_duplicates`)
is kept as it is; a [document revision](revise_document.md) changes it. The revised
document is re-serialized from its TOML values, so it keeps the data of the old
one and not its comments or layout. A first
line names the plan it was derived from. An edit that gives a model a token is
the exception: its stored document starts with the vault-import header of
[the request body of a filing](../campaigns.md#request-body) instead. The plan manifest's `previous_plan_id`
names the plan in both cases. [`GET /api/v1/plans/{plan_id}/document`](../plans/get_plan_document.md) reads the current
document, which is where the indices come from.

An added variant can give a model an `auth_token`. The route imports the model
for the campaign's owner, as [`POST …/plans`](file_plan.md) does.
The import runs before the writes. The route stores the revision with the vault
arm in place of the model ([the request body of a filing](../campaigns.md#request-body)). A model that
the head already imported is its vault arm in the stored document, so an edit
that keeps it imports nothing.

With `Accept: application/x-ndjson`, the route answers with the
[progress stream](../campaigns.md#response-201-created), whose `result` line carries the `201`
body below or a dry run's `200` body. A variant edit that is not valid is a
line of the stream.

| Query parameter | Required | Description |
|---|---|---|
| `base_plan_id` | yes | The plan the caller read the indices off. If the campaign runs a different plan, the edit is refused with `409` and nothing is filed. Required, so a retried edit is refused rather than applied to the revision the first attempt filed |
| `note` | no | The note for the revision, as [the request body of a filing](../campaigns.md#request-body) describes. The server records the change that the revision makes in `change` |
| `dry_run` | no | `true` applies and expands the edit, and files nothing |
| `carry_earlier` | no | `true` lets a cell the replaced plan has not answered take a finished run of the campaign's earlier plans, under the rules that carry the replaced plan's ([carrying jobs into a revision](../campaigns.md#carrying-jobs-into-a-revision)). A queued job of the replaced plan gives way to such a run, and stops with that plan |
| `inherit_from` | no | Plan ids of any campaign, comma-separated, whose finished runs such a cell may take as it takes the earlier plans'. At most 32, the plans of `add_from` and `restore` included. Each must be a plan the caller can read at [`GET /api/v1/plans/{plan_id}`](../plans/get_plan.md); one it cannot, or one that is not there, is `404` and nothing is filed. A named plan's job still queued or running is not inherited: the duplicate merge joins it when the revision is filed, as it joins any live run of the cell |
| `add_from` | no | Plan ids of any campaign, comma-separated, whose variants to append after the body's, in order. A copied variant that names no `benchmarks` takes its plan's list, so it expands to the cells that plan ran. Each plan is also offered as an `inherit_from` plan, and read under the same rule. With it, the body may be empty |
| `restore` | no | Set `restore` to a comma-separated list of `<plan_id>:<index>` values. The server appends these variants after the `add_from` variants. The index counts the variants of the plan from 0, as [`GET /api/v1/plans/{plan_id}/variants`](../plans/get_plan_variants.md) lists them. A restored variant takes its plan's `benchmarks` as an `add_from` variant does. The server offers each source plan through `inherit_from` and checks read access. Its finished runs answer the restored cells. The revision records each variant in `change.restored`. With `remove`, the restore replaces a variant of the current plan. With it, the body may be empty |

Responds `201 Created` with `CampaignRevised` ([the OpenAPI document](../../httpapi.md#15-the-openapi-document)):
the replaced plan as `previous_plan_id`, what became of its jobs, and under
`plan` the [report](../campaigns.md#response-201-created) for the revision. `cancel_requested` is
`false` when the replaced plan had already ended, or was already being
cancelled.

A dry run answers `200` with `RevisionDryRun`: the
[dry-run body](../campaigns.md#response-201-created), plus the revised `document` and what
filing it would do to the plan it replaces. It files nothing, and neither moves
the campaign nor cancels its plan.

The console app makes the same edit on its plan document page instead
([document revision](revise_document.md)).

The `403` and `404` come **before the body is read**, as on
[`POST /api/v1/campaigns`](create_campaign.md).
The edit reads the plan it replaces and cancels it, so it takes the grants
either of those takes on its own, on top of filing into the campaign.

**The writes, and their order.** The revision is written first, the campaign
advanced second, and the replaced plan cancelled last. A failure after the
revision is written leaves both plans running: nothing the edit kept is
stopped. The `500` names which step failed in the server log. Once the first
write begins, the rest finish even if the client disconnects.

**Two edits at once.** The campaign is re-read before it advances, and it
advances only if it still runs the plan the edit revised. Otherwise the edit
lost a race: its revision is cancelled and the answer is `409`, so the loser
lists the variants again and edits the plan that won. The re-read and the write
are not atomic, so two edits that land in that gap both advance, and the last
one wins. An edit that arrives after
another has moved the campaign names a stale `base_plan_id`, and is refused
rather than applied to a plan its caller did not read.

The route also answers the errors every filing answers
([filing errors](../campaigns.md#errors)).

## Statuses

| Status | When |
|--------|------|
| 200 | A dry run, or the progress stream for `Accept: application/x-ndjson` |
| 201 | The revision is filed, the campaign moved onto it, and the replaced plan cancelled |
| 400 | The edit is not valid TOML, holds a key other than `remove` and `variants`, adds and removes nothing, names a variant that does not exist or one twice, or removes every variant. The revised document is refused for a reason [the filing errors](../campaigns.md#errors) list. The query string omits `base_plan_id`, carries one that is not a plan id, carries a parameter this route does not list, or names more than 32 plans in `inherit_from`, `add_from` and `restore`. The server returns 400 if a `restore` entry has another format or names an index that the plan does not have. Or a job body holds an `auth_token` that the import does not remove, or HuggingFace refuses the import of a job's model, for a reason that [the filing errors](../campaigns.md#errors) list |
| 401 | Missing or invalid auth headers, or a device signed ([filing errors](../campaigns.md#errors)) |
| 403 | The campaign is outside the caller's scope, or the caller lacks `submit_plans`. Or the caller may not read and cancel the plan the campaign runs, as [`GET /api/v1/plans/{plan_id}/document`](../plans/get_plan_document.md) and [`POST /api/v1/plans/{plan_id}/cancel`](../plans/cancel_plan.md) decide: `read_plans` and `cancel_plans` for a plan the campaign filed, whoever filed it, and `read_any_plans` and `cancel_any_plans` for any other. Or the caller holds neither `read_clients` nor `read_any_clients`: the revision reads the plan document, which names client ids and requirement sets ([fleet disclosure](../devices.md#fleet-disclosure)). This rule applies to the account that filed the plan too. The scope is checked again after an import, so a member who leaves the organization during it files nothing |
| 404 | No such campaign, also when a delete removed it during an import. The server returns 404 if a source plan does not exist or the caller cannot read it. The message names the plan. Nothing is filed |
| 409 | The campaign is [archived](update_campaign.md): unarchive it first; nothing is filed, dry runs included. Or the campaign runs a plan other than `base_plan_id`, runs no plan, or runs one with no stored document (a plan filed as job bodies, or one that an older build deleted), or an `add_from` or `restore` plan has no stored document: nothing is filed. Or another edit moved the campaign, or a campaign delete removed it, while this revision was filed: the revision does not become the head, and the server cancels it. Or the campaign changed its owner or its current plan during the import: nothing is filed |
| 413 | The body is over `plan_body_limit_bytes` ([request size limits](../../httpapi.md#12-request-size-limits)) |
| 415 | The `Content-Type` is not `application/toml` |
| 429 | Every import permit is held, or HuggingFace limits the requests of the token, as [the filing errors](../campaigns.md#errors) describe |
| 502 | HuggingFace failed during an import, as [the filing errors](../campaigns.md#errors) describe |
