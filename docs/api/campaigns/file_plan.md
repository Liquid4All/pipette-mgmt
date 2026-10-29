# `POST /api/v1/campaigns/{campaign_id}/plans`

```text
POST /api/v1/campaigns/campaign_0195c3e2-…/plans?note=nightly-2026.09.18
Content-Type: application/toml

<the plan document>
```

Files the next plan of the campaign, with `submit_plans`, into your own
campaign or your organization's ([who may do what](../campaigns.md#who-may-do-what)).
The body is a scheduler plan document,
with `note` and `dry_run` in the query string
([the request body of a filing](../campaigns.md#request-body)), under the plan body ceiling. A JSON body gets
`415`. An unknown parameter in the query string gets `400`. A dry run answers
`200` with the [dry-run body](../campaigns.md#response-201-created), stages
nothing, and leaves the campaign where it is. What the campaign adds is the
chain: the plan this mints records `campaign_id` and `previous_plan_id`, and the
campaign advances onto it. **This, a [variant edit](edit_variants.md), a
[document revision](revise_document.md), a [re-run](rerun_jobs.md) and a
[recalculation](recalculate_jobs.md) are the only ways a
campaign's pointer moves.**

Responds `201 Created` with the [report](../campaigns.md#response-201-created).

With `Accept: application/x-ndjson`, the route answers with the
[progress stream](../campaigns.md#response-201-created).

Attribution is the **caller**, not the campaign's owner: a team campaign records
which colleague filed each revision, which is the question the plan listing
answers and the one the owner cannot. The request cannot name a third principal.
The attribution grants nothing: the revision
is read and cancelled as every plan of the campaign is
([who may do what](../campaigns.md#who-may-do-what)).

Superseding is **not cancelling**. The plan the campaign leaves behind keeps its
jobs, its lifecycle and its claimability; stopping it is
[`POST /api/v1/plans/{plan_id}/cancel`](../plans/cancel_plan.md), a separate decision under a separate grant.
Nor does this route carry the old plan's finished jobs over: a cell the old
plan finished runs again. To revise the plan the campaign runs, use a
[variant edit](edit_variants.md) or, to edit the whole
document, [`PUT /api/v1/campaigns/{campaign_id}/document`](revise_document.md).
Those routes and a [re-run](rerun_jobs.md) do both, because each knows which
of the old plan's jobs the new one keeps.

The `403` and `404` come **before the body is read**, as on
[`POST /api/v1/campaigns`](create_campaign.md).

**A gated model is imported for the campaign's owner.** The import of
[filing a plan document](../campaigns.md#filing-a-plan-document) runs here, and the campaign's owner owns the
records. The import checks no grant of its own. The route's campaign check
above decides who may file. The import runs before the two writes below. A
caller that disconnects during an import stops it, and the route files no
plan. After the import, the route reads the campaign again. If its owner or
its current plan changed, the route answers `409`. A retry by an unchanged
owner reuses the imported records.

**The two writes, and their order.** The plan is written first and the campaign
advanced second. A failure between them leaves a plan that names this campaign
and supersedes its head without becoming the head: the revision was filed and
never took effect. The reverse order would point a campaign at a plan that may
not exist. The `500` says so, and the repair is to file again. Once the plan
write begins, both writes finish even if the client disconnects.

The campaign is **re-read between them**, because an ingest takes as long as it
takes to stage every job and the record is written whole: advancing the copy read
before it would discard a rename or a hand-off that landed in that window. Two
revisions filed into one campaign at the same moment still race, and the loser's
plan is staged but never becomes the head.

The route also answers the errors every filing answers
([filing errors](../campaigns.md#errors)).

## Statuses

| Status | When |
|--------|------|
| 200 | A dry run, or the progress stream for `Accept: application/x-ndjson` |
| 201 | The plan is filed and the campaign advanced onto it |
| 400 | A plan document or a job is refused, for a reason that [the filing errors](../campaigns.md#errors) list; nothing is staged: a plan is ingested as a unit. Or the query string carries a parameter this route does not know, such as `submitter` or a misspelt `dry_run`; nothing is staged. Or HuggingFace refuses the import of a job's model, for a reason that [the filing errors](../campaigns.md#errors) list |
| 401 | Missing or invalid auth headers, or a device signed ([filing errors](../campaigns.md#errors)) |
| 403 | The campaign is outside the caller's scope, or the caller lacks `submit_plans`. Checked again after an import, so a member who leaves the organization during it files nothing |
| 404 | No such campaign. Checked before anything is ingested, and again after an import: a campaign that a delete removed during the import gets `404`, and nothing is staged |
| 409 | The campaign is [archived](update_campaign.md): unarchive it first; nothing is filed, dry runs included. Or a campaign delete removed the campaign while this plan was filed: the plan does not become the head, and the server cancels it. Or the campaign changed its owner or its current plan during the import: nothing is staged; file the plan again |
| 413 | The body is over `plan_body_limit_bytes` ([request size limits](../../httpapi.md#12-request-size-limits)). Nothing is staged |
| 415 | The `Content-Type` is missing, or is not `application/toml`: `send the plan as an application/toml plan document` |
| 429 | Every import permit is held, or HuggingFace limits the requests of the token, as [the filing errors](../campaigns.md#errors) describe |
| 502 | HuggingFace failed during an import, as [the filing errors](../campaigns.md#errors) describe |
