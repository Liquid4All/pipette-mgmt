# `PUT /api/v1/users/{user_id}/permissions`

Replace the whole set. The names granted are bounded by the ceiling, and a
scoped caller by the rank; the set is weighed again as it is swapped, so a
server-wide grant landing in between is kept rather than overwritten
([the rules that make delegation safe](../users.md#the-rules-that-make-delegation-safe)).
[`PATCH /api/v1/users/{user_id}/permissions`](amend_permissions.md) adds and
removes names instead.

It answers `ChangedUserView`, the account plus `changed`
([Write answers](../../httpapi.md#16-write-answers)). The no-op is a set that
normalizes ([`PATCH /api/v1/users/{user_id}/permissions`](amend_permissions.md))
to the stored one, so naming, beside a wider name, one it implies changes
nothing.

## Statuses

| Status | When |
|--------|------|
| 200 | The account, and whether it changed |
| 400 | The caller removing its own `manage_orgs` or `manage_any_orgs`; or a service account granted `manage_orgs`, `manage_clients` or an `_any_` name |
| 403 | The set names a name the caller does not hold, and the refusal names every one; or, for a scoped caller, the account holds a server-wide name it lacks; or the caller is a service account |
| 404 | The account is outside the caller's scope or does not exist |
