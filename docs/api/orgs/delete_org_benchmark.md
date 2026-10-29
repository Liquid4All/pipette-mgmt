# `DELETE /api/v1/orgs/{org_id}/benchmarks/{benchmark_id}`

Deletes one of the benchmarks the organization keeps to itself
([private benchmarks](../orgs.md#private-benchmarks)). It takes `manage_orgs`
**and** membership, or `manage_any_orgs`. It finds the entry by its key, so an
unreadable one ([`GET /api/v1/orgs/{org_id}/benchmarks`](list_org_benchmarks.md))
is removed too.

## Statuses

| Status | When |
|--------|------|
| 204 | The entry is deleted |
| 400 | `org_id` is not an organization id |
| 403 | The caller does not hold `manage_orgs` |
| 404 | The organization is outside the caller's reach or does not exist: not the caller's own or unaffiliated, for a write. Also an id the organization keeps nothing under |
