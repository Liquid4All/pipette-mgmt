# `POST /api/v1/campaigns`

```text
POST /api/v1/campaigns?name=Nightly%20LFM2&note=nightly-2026.09.25
Content-Type: application/toml

<the plan document>
```

Creates a campaign **with its first plan**. The body is a scheduler plan
document, as [the request body of a filing](../campaigns.md#request-body) describes, and the rest of the
request is the query string, which the signature covers:

| Query parameter | Required | Description |
|---|---|---|
| `name` | yes | What to print for a human. Trimmed, non-empty, at most 4096 characters. Nothing keys on it, and two campaigns may share one |
| `owner` | no | A `user_…` or `org_…` id. Defaults to the caller. Bounded by the same rule [the hand-off](set_campaign_owner.md) applies |
| `note` | no | The first plan's note, as [the request body of a filing](../campaigns.md#request-body) describes |
| `dry_run` | no | `true` expands and validates the document, and creates nothing |

There is **no empty create** and no JSON form: a campaign is the stable name for
work, so it is created with the work. It cannot be created around a plan that
already exists, because a plan belongs to the campaign that minted it.

The owner follows the hand-off rule, so a member of an organization may create
that organization's campaign without `manage_orgs`. Every member then files
into it, the creator included, and only the organization's administrators
rename it, hand it over or delete it.

Responds `201 Created` with `CampaignCreated` ([the OpenAPI document](../../httpapi.md#15-the-openapi-document)):
the record, and under `plan` the [report](../campaigns.md#response-201-created)
for the first plan.

The plan records the campaign and supersedes nothing. It is attributed to the
caller, as a [revision](file_plan.md) is. A dry run
answers `200` with the [dry-run body](../campaigns.md#response-201-created) and
writes nothing. It passes the same owner and `submit_plans` checks as a real
create. To check a document before the campaign has a name or an owner, use
[`POST /api/v1/plans/validate`](../plans/validate_plan.md).

The server checks the caller and the owner **before it reads the body**, so a
refused caller gets its `403` and never has a plan-sized body read on its behalf.

The owner is checked **before anything is written**, both that the caller may
put a campaign down there and that the record it names exists: a campaign owned
by a nonexistent principal is one nobody could ever repoint.

**A gated model is imported for the new campaign's owner.** A model in the
document can carry an `auth_token`, as on [filing a plan document](../campaigns.md#filing-a-plan-document). The import
runs before the two writes below, and the new campaign's owner owns the
records. A caller that disconnects during an import stops it, and the route
creates nothing.

With `Accept: application/x-ndjson`, the route answers with the
[progress stream](../campaigns.md#response-201-created), whose `result` line carries the `201`
body above or a dry run's `200` body.

**The two writes, and their order.** The plan is written first and the campaign
second. The reverse order would create a campaign naming a plan that may not
exist. If the campaign write fails, the server cancels the plan it just filed,
because no record holds the campaign id the plan names. The `500`
[error body](../../httpapi.md#11-error-responses) carries no detail. The
server log names both ids and whether the cancel took. If the cancel failed too, an operator cancels the plan.
Either way, the create can be sent again. Once the plan write begins, both
writes finish even if the client disconnects.

The [console app](../../../ui/README.md#pages) draws the same create as a form.

The route also answers the errors every filing answers
([filing errors](../campaigns.md#errors)).

## Statuses

| Status | When |
|--------|------|
| 200 | A dry run, or the progress stream for `Accept: application/x-ndjson` |
| 201 | The campaign is created, with the report on its first plan |
| 400 | `name` is missing, empty or too long, `owner` is not a principal id or names an account or organization that does not exist or an account that holds no `submit_plans`, the query string carries a parameter this route does not list, or the document is refused for a reason [the filing errors](../campaigns.md#errors) list. Or HuggingFace refuses the import of a job's model, for a reason that [the filing errors](../campaigns.md#errors) list |
| 401 | Missing or invalid auth headers, or a device signed ([filing errors](../campaigns.md#errors)) |
| 403 | The owner is outside the caller's scope, or the caller lacks `submit_plans` |
| 413 | The body is over `plan_body_limit_bytes` ([request size limits](../../httpapi.md#12-request-size-limits)). Nothing is written |
| 415 | The `Content-Type` is not `application/toml` |
| 429 | Every import permit is held, or HuggingFace limits the requests of the token, as [the filing errors](../campaigns.md#errors) describe |
| 502 | HuggingFace failed during an import, as [the filing errors](../campaigns.md#errors) describe |
