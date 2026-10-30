# `PATCH /api/v1/users/{user_id}/permissions`

Add and remove names.

```json
{ "add": ["read_clients"], "remove": ["cancel_plans"] }
```

Goes through the store's read-modify-write, so two operators amending at once do
not lose each other's work. At least one of `add` / `remove`, or the response is
a `400`. An `add` is bounded by the ceiling, a `remove` is not, and a scoped
caller by the rank
([the rules that make delegation safe](../users.md#the-rules-that-make-delegation-safe)).

The stored set is **normalized**: a name another already implies is not written
twice. The response, `UserPermissionsAmended`, carries the resolved set and two
fields about the amend itself: `changed`, and `still_granted`, the names in
`remove` the account still holds.

A name lands in `still_granted` when a wider name the account keeps implies it:
the narrow name is never stored beside the wide one, so removing it by name
reaches nothing. Removing `manage_orgs` from an account holding
`manage_any_orgs` answers `200` with `"changed": false` and
`"still_granted": ["manage_orgs"]`; remove the wider name instead.
`users permission remove` prints the same list. A name sent in both `add` and
`remove` lands there too, since the removal is applied first and the add puts
it back.

## Statuses

| Status | When |
|--------|------|
| 200 | `UserPermissionsAmended`: the resolved set, `changed` and `still_granted` |
| 400 | Neither `add` nor `remove`; or the caller removing its own `manage_orgs` or `manage_any_orgs`; or a service account granted `manage_orgs`, `manage_clients` or an `_any_` name |
| 403 | `add` names a name the caller does not hold, and the refusal names every one; or, for a scoped caller, the account holds a server-wide name it lacks; or the caller is a service account |
| 404 | The account is outside the caller's scope or does not exist |
