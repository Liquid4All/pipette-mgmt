# `PATCH /api/v1/plans/{plan_id}`

Change the note of a plan. The note is the only field of a plan that a client
can change after filing. The body is `UpdatePlanRequest`. The `note`
field follows these rules:

- `note` is the new note.
- The server trims the note.
- The note has at most 4096 characters.
- The note contains no NUL character.
- A blank value clears the note.
- An absent or `null` value leaves the note unchanged.

The server writes the `change` of a revision (`RevisionChange`). The value does
not change. Only a server with `[database]` keeps the note and the change.
Without `[database]`, a plan has neither. A request that changes the note
returns `500`.

The caller must hold `submit_plans`, and one of these conditions must be true:

- The plan is in a campaign that the caller may file into.
- The plan is outside a campaign, and the caller filed it.
- The caller holds `manage_any_orgs`. This applies to each plan that the caller
  reads.

The `may_edit_note` field of [the plan page](list_plan_jobs.md#the-plan-pages-body)
gives the same answer for its reader.

## Response

The response is `200 OK`. The body is the plan, in the same form as
[`GET /api/v1/plans/{plan_id}`](get_plan.md), with `changed` added
([write answers](../../httpapi.md#16-write-answers)). If the plan already holds
the note, or the request has no `note`, the server changes nothing.

The last write wins. The server does not compare the note with an earlier
read, so a retried request replaces a note that another caller wrote after it.

## Statuses

| Status | When |
|--------|------|
| 200 | The plan, and whether its note changed |
| 400 | `plan_id` is malformed, the body carries an unknown field, the trimmed note is over 4096 characters, or the note contains a NUL character |
| 401 | Missing or invalid auth headers, or an `X-Client-Id` that is not a `user_…` |
| 403 | The caller lacks `submit_plans`, or reads the plan but may not change its note |
| 404 | No such plan, or one the caller's scope does not admit, as on [`GET /api/v1/plans/{plan_id}`](get_plan.md#one-404-for-both) |
