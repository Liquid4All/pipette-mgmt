# `GET /api/v1/clients/capabilities`

Report the capability flags the fleet reports. The response groups the flags by
namespace and counts the clients behind each one. Write a job's `requires` set
from this vocabulary rather than from memory. A flag that no client reports
matches nobody.

**A user route.** The route requires `read_clients` or `read_any_clients`
([fleet disclosure](../devices.md#fleet-disclosure)). The
answer covers every client record, so a caller that reads it learns which
devices exist, one flag at a time. The server rejects an `ev1_…` client
identifier during header validation, before the permission check.

The route returns one aggregate. It takes no `limit` and no `cursor`, so the
[pagination](../../httpapi.md#13-pagination) convention does not apply to it.

The server reads each client's **effective capability set**. This set combines
flags that the server derives from the device profile, flags that the client
reports, and the client's tags as `tag:<tag>` flags
([capability matching](../devices.md#capability-matching)). It is the
set a job's clauses test.

## Query parameters

| Parameter | Type | Description |
|-----------|------|-------------|
| `status` | string | Report over the clients of one status alone: `approved` or `pending`. Absent or empty means every status. An unknown value is a `400` |

## Response `200 OK`

The body is `CapabilityVocabulary`: flags grouped by namespace, the token
before a flag's first `:`, each with how many clients carry it, and the flags
with no namespace under `free_form`.

A namespace's `source` says where its flags come from. A `derived` namespace
is **reserved** ([capability matching](../devices.md#capability-matching)):
a client cannot report a flag in it, so a form shows its flags and lets nobody
create one. Nor can a client report a `tag:` flag: an operator assigns tags
([client tags](../devices.md#client-tags)).

Check `unreadable_client_count` before you conclude that a flag is absent: an unreadable
record can carry flags the response omits, so a non-zero value means the
vocabulary is incomplete. The server logs each skipped record.

## Statuses

| Status | When |
|--------|------|
| 200 | The vocabulary, `CapabilityVocabulary` |
| 400 | `status` is invalid |
| 401 | Missing or invalid auth headers, or an `X-Client-Id` that is not a `user_…` |
| 403 | Caller holds neither `read_clients` nor `read_any_clients` |
