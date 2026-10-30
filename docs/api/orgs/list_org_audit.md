# `GET /api/v1/orgs/{org_id}/audit`

Lists the events of one organization, newest first
([the audit log](../orgs.md#the-audit-log)). It needs `manage_orgs` for the
caller's own organization, or `manage_any_orgs` for any organization. Outside
that scope, the organization reads as absent (`404`). A reader with
`manage_orgs` sees no other organization in an event. A `target` that names
another organization gives a `400`. Without these rules, the reader could find
an organization that a `404` hides.

The route refuses a service account and pages as
[pagination](../../httpapi.md#13-pagination) describes. `outcome`, `actor` and
`target` narrow what it reads ([narrowing the log](../orgs.md#narrowing-the-log)),
and `names=true` names the ids the reader may see
([names in the answer](../orgs.md#names-in-the-answer)).

## Statuses

| Status | When |
|--------|------|
| 200 | A page of the organization's events, newest first |
| 400 | An outcome or an actor kind that is not in the vocabulary, a `target` that cannot be an id, or, for a reader with `manage_orgs`, a `target` that names another organization |
| 403 | The caller does not hold `manage_orgs`, or is a service account |
| 404 | The organization is outside the caller's scope or does not exist; or the server has no `[database]`, so keeps no audit log |
