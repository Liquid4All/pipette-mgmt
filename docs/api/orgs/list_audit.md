# `GET /api/v1/audit`

Lists every audit event, newest first, under `manage_any_orgs`
([the audit log](../orgs.md#the-audit-log)). An event with an unverified actor
is in no organization's log, so this is the only route that lists it.

The route refuses a service account and pages as
[pagination](../../httpapi.md#13-pagination) describes. `outcome`, `actor` and
`target` narrow what it reads ([narrowing the log](../orgs.md#narrowing-the-log)),
and `names=true` names the ids the reader may see
([names in the answer](../orgs.md#names-in-the-answer)).

## Statuses

| Status | When |
|--------|------|
| 200 | A page of events, newest first |
| 400 | An outcome or an actor kind that is not in the vocabulary, or a `target` that cannot be an id |
| 403 | The caller does not hold `manage_any_orgs`, or is a service account |
| 404 | The server has no `[database]`, so keeps no audit log |
