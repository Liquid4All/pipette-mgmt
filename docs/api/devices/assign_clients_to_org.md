# `PUT /api/v1/orgs/{org_id}/clients`

Takes `AssignClientsRequest` and answers `ClientsAssigned`
([the OpenAPI document](../../httpapi.md#15-the-openapi-document)). Hands a set of devices to one organization, within the caller's
[scope](../devices.md#the-scope): the shape onboarding a rack wants,
where [`PUT /api/v1/clients/{client_id}/owner`](set_client_owner.md) is one device at a time. Both write the
same ownership record.

**Every device is resolved and scope-checked before any is written**, so a body
naming one unknown id, or one device this caller may not touch, fails the whole
call and leaves the rack alone. Past that point
it is **not atomic**: each device is its own write, so a failure partway leaves
the ones already assigned assigned. The per-device `changed` flags and the fact
that re-assigning is a no-op are what make a retry converge.

An empty `client_ids` is a `400` rather than a successful no-op, so a caller
that built its list wrong hears about it. An id named twice is assigned once, in
the order it first appears, so each device gets one row and one truthful
`changed`.

An organization outside the caller's scope answers the same `404` as one that
does not exist, as on the [`/api/v1/orgs` routes](../orgs.md), so a `403`
would tell a scoped caller which ids exist. `{org_id}` is the id only
([either spelling](../orgs.md#either-spelling-of-org_id) is for reads).

## Statuses

| Status | When |
|--------|------|
| 400 | `client_ids` is empty, or `org_id` is not an organization id |
| 404 | The organization is outside the caller's scope or does not exist |
