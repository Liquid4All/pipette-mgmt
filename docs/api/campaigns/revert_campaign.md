# `POST /api/v1/campaigns/{campaign_id}/revert`

```text
POST /api/v1/campaigns/campaign_0195c3e2-…/revert?base_plan_id=plan-0195c4a0-…
Content-Type: application/json

{ "plan_id": "plan-0195c400-…", "note": "back to the April baseline" }
```

The route moves the campaign back to the state of an **earlier plan** of its
history. The history does not change. The route files a new plan, and the new
plan supersedes the plan that the campaign runs. The route runs these steps:

1. Takes the stored document of the named plan, as it is.
2. Files the document as a revision, as a [variant edit](edit_variants.md) files
   its result. The revision carries over each job of the replaced plan that the
   document describes. This includes finished jobs.
3. Offers the finished runs of every earlier plan to each cell that the
   replaced plan does not answer. A [variant edit](edit_variants.md) with
   `carry_earlier` makes the same offer. The results of the named plan then
   come back, and the revision does not run them again. A revert always makes
   this offer.
4. Moves the campaign onto the revision.
5. Cancels the replaced plan. Its teardown retires the jobs that the document
   does not describe.

The named plan must be in the campaign's history: the plan that the campaign
runs, or a plan that it superseded, at any depth. To copy a plan of another
campaign, use a [document revision](revise_document.md) with `inherit_from`.

**A revert to the document of the plan that the campaign runs is a success
that files nothing.** The route answers `200` with the campaign record and
`"changed": false`, as a [document revision](revise_document.md) of the same
document does. This applies if the named plan is the plan that the campaign
runs. It also applies if the document of the named plan equals the document of
the plan that the campaign runs.

The stored document names each imported model by its vault arm
([the request body of a filing](../campaigns.md#request-body)), so a revert
imports nothing.

| Field | Required | Description |
|---|---|---|
| `plan_id` | yes | The earlier plan, as [`GET /api/v1/campaigns/{campaign_id}/overview`](get_campaign_overview.md) lists it in `history` |
| `note` | no | The note for the revision, as [the request body of a filing](../campaigns.md#request-body) describes. The server records the change that the revision makes in `change`, as `{"type": "revert", "to_plan_id": …}` |

| Query parameter | Required | Description |
|---|---|---|
| `base_plan_id` | yes | The plan that the campaign runs, as the caller read it. `409` if the campaign runs another plan. The route checks this before it reads the body. If the first attempt succeeded, a retry gets `409` and files nothing |
| `dry_run` | no | `true` runs every check below and expands the document, and files nothing |

The route answers `201 Created` with the body of a
[variant edit](edit_variants.md). A dry run answers `200` as a variant edit's
does. Its `restored_job_count` counts the finished runs that the revert brings
back. A dry run of a revert that files nothing answers `200` with
`"unchanged": true`.

The [console app](../../../ui/README.md#pages) offers the revert on each
earlier row of a campaign's History tab, and on the page of an earlier plan of
the campaign.

The route answers these errors **before it reads the body**, as a
[variant edit](edit_variants.md) does: `403`, `404` for the campaign, and `409`
for a moved `base_plan_id`. The writes, the order of the writes, and the result
of two revisions at the same time are the same as for a variant edit.

The route also answers the errors every filing answers
([filing errors](../campaigns.md#errors)).

## Statuses

| Status | When |
|--------|------|
| 200 | A dry run, or a revert to the document of the plan that the campaign runs, which files nothing |
| 201 | The revision is filed, the campaign moved onto it, and the replaced plan cancelled |
| 400 | Any one of these conditions causes it. The body is not JSON. The body holds a field that this route does not list. The `plan_id` is not a plan id. The query string has no `base_plan_id`, or holds a parameter that this route does not list. The document does not expand, for a reason that [the filing errors](../campaigns.md#errors) list. In each case, nothing is filed |
| 401 | Missing or invalid auth headers, or a device signed ([filing errors](../campaigns.md#errors)) |
| 403 | As on a [variant edit](edit_variants.md). Any one of these conditions causes it. The campaign is outside the caller's scope. The caller lacks `submit_plans`. The caller may not read and cancel the plan that the campaign runs. The caller holds neither `read_clients` nor `read_any_clients` |
| 404 | No such campaign. Or the campaign's history does not hold `plan_id` |
| 409 | The campaign is [archived](update_campaign.md): unarchive it first; nothing is filed, dry runs included. Or the campaign runs no plan, or runs another plan than `base_plan_id`. Or the named plan has no stored document: it was filed as job bodies, or its manifest is gone. Nothing is filed |
| 409 | Another revision moved the campaign while the route filed this revision. Or a campaign delete removed the campaign while the route filed this revision. The revision does not become the head, and the server cancels it |
