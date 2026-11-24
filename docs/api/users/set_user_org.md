# `PUT /api/v1/users/{user_id}/org`

Moves an account to another organization. This route takes `{"org_id": …}`.
Nothing clears a membership: an account always belongs to an organization, and
disabling it ([`PATCH /api/v1/users/{user_id}`](update_user.md)) takes it out
of service. It resolves the organization against the roster before writing,
and is a roster write under `manage_orgs`
([memberships and ownerships](../orgs.md#memberships-and-ownerships)).

It responds with the updated record plus `changed`
([write answers](../../httpapi.md#16-write-answers)). Naming the organization
the account already belongs to is the no-op.

## Statuses

| Status | When |
|--------|------|
| 200 | The membership is written, or already held; the body is the updated record plus `changed` |
| 400 | The body is not JSON, omits `org_id`, holds a `null` one, or holds a key other than `org_id`. Nothing is written |
| 403 | The caller does not hold `manage_orgs`, or the move leaves its scope; or the account holds a server-wide name a scoped caller lacks ([the rules that make delegation safe](../users.md#the-rules-that-make-delegation-safe)) |
| 404 | The account named in the path does not exist, or is outside a scoped caller's organization; or the organization named in the *body* does not exist, or is outside a scoped caller's own |
| 409 | The organization is disabled: it keeps its members and takes no new one |
