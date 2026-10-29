# `PUT /api/v1/users/{user_id}/org`

Sets or clears which organization an account belongs to. This route takes
`{"org_id": …}`; a `null` value clears the membership. It resolves the
organization against the roster before writing, and is a roster write under
`manage_orgs` ([memberships and ownerships](../orgs.md#memberships-and-ownerships)).

It responds with the updated record plus `changed`
([write answers](../../httpapi.md#16-write-answers)). Naming the organization
the account already belongs to (`null` for none) is the no-op.

## Statuses

| Status | When |
|--------|------|
| 200 | The membership is written, or already held; the body is the updated record plus `changed` |
| 400 | The body is not JSON, omits `org_id`, or holds a key other than `org_id`. Nothing is written: only an explicit `null` clears. Also a `null` for a service account, which belongs to an organization |
| 403 | The caller does not hold `manage_orgs`, or the move leaves its scope; or the account holds a server-wide name a scoped caller lacks ([the rules that make delegation safe](../users.md#the-rules-that-make-delegation-safe)) |
| 404 | The account named in the path does not exist, or is outside a scoped caller's organization; or the organization named in the *body* does not exist, or is outside a scoped caller's own |
| 409 | The organization is disabled: it keeps its members and takes no new one |
