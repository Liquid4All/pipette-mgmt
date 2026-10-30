# `GET /api/v1/clients/{client_id}`

One client, as `ClientDetail` ([the OpenAPI document](../../httpapi.md#15-the-openapi-document)): the
row [`GET /api/v1/clients`](list_clients.md) lists, and what the caller may do to it
([The scope](../devices.md#the-scope)), so a page draws the forms the routes would take.
Same gate. Drawn, not enforced: each write decides again. `owning_org_id` is gated because
the rollup through a person is a membership, which the fleet grant does not
read; a scoped administrator learns it from its forms anyway.

`backlog_by_campaign` is the device's backlog by campaign
([what a device row carries](../devices.md#what-a-device-row-carries)). It names the campaigns that the caller
may read. The field `unreadable_job_count` sums the jobs of all other campaigns. It is
`null` before the first count, or if the server cannot read the count.

The static path [`/api/v1/clients/capabilities`](get_capability_vocabulary.md) takes precedence over
`{client_id}`.

"Out of reach" and "does not exist" are one answer.

## Statuses

| Status | When |
|--------|------|
| 400 | `client_id` contains characters outside the id charset |
| 401 | Missing or invalid auth headers, or an `X-Client-Id` that is not a `user_…` |
| 403 | Caller holds neither `read_clients` nor `read_any_clients` |
| 404 | No such client, **or** one outside the caller's reach |
