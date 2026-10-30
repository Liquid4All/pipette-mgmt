# `PUT /api/v1/campaigns/{campaign_id}/document`

```text
PUT /api/v1/campaigns/campaign_0195c3e2-…/document?base_plan_id=plan-0195c400-…&note=nightly-3
Content-Type: application/toml

benchmarks = ["decode_throughput_512_100"]

[[variants]]
…
```

Revises the campaign by its **whole document**: the body takes the place of the
document of the plan the campaign runs, which
[`GET` on the same path](read_document.md) reads. This is the revision for what a
[variant edit](edit_variants.md) keeps as it is, such as
`benchmarks`, `expires_at`, `merge_duplicates`, or one field of a variant. It
runs the steps of a variant edit, with the body as the revised document:

1. Files the body as a revision, as [`POST …/plans`](file_plan.md)
   files a document. The revision **carries over** every job of the replaced
   plan that it still describes, finished ones included, so it runs nothing
   again. Only the cells it adds are new jobs
   ([carrying jobs into a revision](../campaigns.md#carrying-jobs-into-a-revision)).
2. Moves the campaign onto the revision.
3. Cancels the replaced plan, whose teardown retires the jobs the new document
   no longer describes.

The body is stored as it is sent, comments and layout included. A model in the
document can carry an `auth_token`. The route then imports the model for the
campaign's owner, as [`POST …/plans`](file_plan.md) does, and
stores the document with the vault arm in place of the model
([the request body of a filing](../campaigns.md#request-body)). The server does not store such a document as
the caller sent it. The server keeps the digest of the sent text instead, so
the rules below match such a document by that digest.
[`GET /api/v1/campaigns/{campaign_id}/document`](read_document.md)
reads the document to start from.

**A document identical to the one the plan was filed from is a success that
files nothing**, unless `carry_earlier` or `inherit_from` is set. The route answers `200` with the
campaign record and `"changed": false`, as a rename to the current name does
([`PATCH /api/v1/campaigns/{campaign_id}`](update_campaign.md)). The campaign stays on its plan, and
the plan is not cancelled. To run jobs again, use a [re-run](rerun_jobs.md).

**A retry is safe.** If the campaign has moved off `base_plan_id` onto the
revision of that plan filed from this very document, the request is a retry
whose first answer went missing. It answers `200` with `"changed": false` as
well, and files nothing. Any other document off a plan the campaign no longer
runs is `409`. A dry run off such a plan is `409` too.

| Query parameter | Required | Description |
|---|---|---|
| `base_plan_id` | yes | The plan the caller read the document off. If the campaign runs a different plan, the revision is refused with `409` and nothing is filed, unless it is a retry that landed (above). Required, so a retried revision is never filed over the revision the first attempt filed |
| `note` | no | The note for the revision, as [the request body of a filing](../campaigns.md#request-body) describes. The server records the change that the revision makes in `change` |
| `dry_run` | no | `true` expands the document, and files nothing |
| `carry_earlier`, `inherit_from` | no | As on a [variant edit](edit_variants.md). With either, the plan's own document files a revision rather than nothing |

Responds `201 Created` with the body of a [variant edit](edit_variants.md):
what became of the replaced
plan's jobs, and the report on the revision. A dry run answers `200` with the
dry-run body of a variant edit: the jobs, the `document`, and which of the
replaced plan's jobs it would keep and stop. It files nothing, and neither
moves the campaign nor cancels its plan. To check a document as it is edited,
without a base plan, use [`POST /api/v1/plans/validate`](../plans/validate_plan.md).

The [console app](../../../ui/README.md#pages) draws the same revision as a
form.

The `403` and `404` come before the body is read; the `409` for a moved
campaign comes after, since only the body tells a retry apart. The writes, their order, and
two revisions at once behave as they do for a variant edit.

With `Accept: application/x-ndjson`, the route answers with the
[progress stream](../campaigns.md#response-201-created), whose `result` line carries the `201`
body, a dry run's `200` body, or the `200` campaign record of a document that
files nothing. The `409` for a campaign that moved off `base_plan_id` is a line
of the stream.

The route also answers the errors every filing answers
([filing errors](../campaigns.md#errors)).

## Statuses

| Status | When |
|--------|------|
| 200 | A dry run; the campaign record with `"changed": false` for a document that files nothing or a retry that landed; or the progress stream for `Accept: application/x-ndjson` |
| 201 | The revision is filed, the campaign moved onto it, and the replaced plan cancelled |
| 400 | The document is refused for a reason [the filing errors](../campaigns.md#errors) list. The query string omits `base_plan_id` or gives an invalid plan id. This route rejects `add_from`, `restore`, and other unsupported parameters. Use a variant edit for `add_from` or `restore`. Or `inherit_from` names more than 32 plans. Or a job body holds an `auth_token` that the import does not remove, or HuggingFace refuses the import of a job's model, for a reason that [the filing errors](../campaigns.md#errors) list |
| 401 | Missing or invalid auth headers, or a device signed ([filing errors](../campaigns.md#errors)) |
| 403 | As for a [variant edit](edit_variants.md) |
| 404 | No such campaign, also when a delete removed it during an import. Or an `inherit_from` plan is not there, or is not one the caller can read; the message names it. Nothing is filed |
| 409 | The campaign is [archived](update_campaign.md): unarchive it first; nothing is filed, dry runs included. Or the campaign runs a plan other than `base_plan_id`, and not as a retry that landed, runs no plan, or runs one with no stored document: nothing is filed. Or another revision moved the campaign, or a delete removed it, while this one was filed: this revision does not become the head, and the server cancels it. Or the campaign changed its owner or its current plan during the import: nothing is filed |
| 413 | The body is over `plan_body_limit_bytes` ([request size limits](../../httpapi.md#12-request-size-limits)) |
| 415 | The `Content-Type` is not `application/toml` |
| 429 | Every import permit is held, or HuggingFace limits the requests of the token, as [the filing errors](../campaigns.md#errors) describe |
| 502 | HuggingFace failed during an import, as [the filing errors](../campaigns.md#errors) describe |
