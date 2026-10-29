# `PATCH /api/v1/orgs/{org_id}`

Renames an organization. Changing its status takes `manage_any_orgs`, so an
organization cannot undo its disabling; the status it already has is accepted.

The body is `UpdateOrgRequest`, with at least
one field, or the response is a `400` rather than a silent no-op write. A body
whose every field names the value already stored is the no-op
([write answers](../../httpapi.md#16-write-answers)).

**The handle is not here.** Its whole purpose is to be stable: an organization
that needs a different handle is a new organization and a reassignment.

`disabled` marks a wound-down organization so a listing shows it as such. It
takes no new member: a create or a move into it is a `409`, and a Clerk sign-in
that would provision an account into it is refused
([signing in through Clerk](../users.md#signing-in-through-clerk)).
Members keep what their membership reaches. Stopping the accounts is
`users disable`. Only `manage_any_orgs` sets the status.

## Statuses

| Status | When |
|--------|------|
| 200 | The body is the object [`GET /api/v1/orgs/{org_id}`](get_org.md) returns, plus `changed` ([write answers](../../httpapi.md#16-write-answers)) |
| 400 | The body sets no field, or `org_id` is not an organization id |
| 403 | The caller does not hold `manage_orgs`, or changes the status without `manage_any_orgs` |
