# `GET /api/v1/clients`

List the fleet. The HTTP form of `pipette-mgmt clients list`, and the
counterpart of [`GET /clients/me`](../../httpapi.md#23-get-clientsme): that route is a *device*
reading its own record, and this is an operator reading the fleet, so it takes a
**user** and gates on `read_clients` / `read_any_clients`
([fleet disclosure](../devices.md#fleet-disclosure)).

`GET /api/v1/clients/capabilities` groups these flags by namespace and reports their
client counts across the fleet
([`GET /api/v1/clients/capabilities`](get_capability_vocabulary.md)). That route takes the same
fleet grant and the same `status` filter. Use `GET /api/v1/clients/capabilities` to
prepare a job's `requires` set. Use the client routes to identify clients with
a specific flag.

**Both grants read the whole fleet today**: `read_clients` means the clients
the caller's plans can reach, and no per-submitter device restriction can be
recorded yet.

## Query parameters

The OpenAPI document ([the OpenAPI document](../../httpapi.md#15-the-openapi-document)) lists the parameters,
the repeatable capability and tag filters among them, with the paging of
[pagination](../../httpapi.md#13-pagination). An absent `status` means **every** status on this
route. `owned`, `owner` and `org_id` are one choice: any two together are a
`400`.

**`org_id` is roster data, so it is gated, not only a filter.** Which devices
roll up to an organization says who belongs to it, so the filter takes
`read_orgs` over that organization, as [`GET /api/v1/orgs/{org_id}/clients`](list_org_clients.md) does: `403`
without the grant, `404` for an organization outside its scope or one that does
not exist. `org_id` on the owned listings
([`GET /api/v1/campaigns`](../campaigns/list_campaigns.md)) only narrows, and answers an
empty page outside the caller's reach.

**The app's fleet pages default to `approved` where this route defaults to
neither.** A fleet is read to see what can take work, so that is the page a
reader wants; narrowing a published JSON listing by default would quietly drop
rows from a program that was reading them all. The two are told apart by whether
`status` is present: absent takes the surface's default, and an explicitly empty
`status=` is a reader clearing the filter and means every status on both.

A selective filter can make the server read many more records than it
returns ([pagination](../../httpapi.md#13-pagination)). The cursor identifies the last key examined,
including one whose record did not match, so the next page resumes without
rescanning it.

Whitespace separates the flags in one occurrence. A comma cannot separate them:
a canonical flag can contain one, such as `device:iphone14,2`. For example,
`?any_of=gpu:a17pro+gpu:a18pro&any_of=runtime:llama_cpp` specifies two groups.
A flag must be canonical. The route rejects a flag that is not canonical with
`400`. A byte-exact capability comparison does not find such a flag.

**A capability query without `requires` matches nobody.** Every other clause
only narrows. The scheduler applies the same rule, so this route and
`clients list` agree. For example, `?any_of=gpu:a17pro` returns `200` with empty
`items`.

**Ordered ascending by `client_id`.** A client id is a `sha256` digest, so the
order carries no meaning of its own. What it has is stability: a row's page
never shifts because a machine elsewhere re-registered. Ordering by
registration time would read every record on every request.

## Response `200 OK`

The [pagination](../../httpapi.md#13-pagination) envelope, whose `items` are `ClientListRow`s in
the OpenAPI document ([the OpenAPI document](../../httpapi.md#15-the-openapi-document)): the stored client
record, plus what the server reads beside it (derived capabilities, tags,
permissions, reindex and suspension flags, `last_seen_at` and `backlog`; see
[what a device row carries](../devices.md#what-a-device-row-carries)).
`contact_email` is there only for a caller that administers the device:
`manage_clients` over its organization's devices, or `manage_any_clients`
([visibility](../devices.md#what-a-device-row-carries)).

Unlike `GET /clients/me`, an unset `device_*` field is **omitted** rather than
emitted as `null`. That route is a published contract the `pipette-clients` crate
deserializes; this one is read by a human, for whom a field a machine never set
is noise.

## Errors

A `400` names one of these conditions:

- `status` is invalid, or `owned` is not `true` or `false`.
- `owner` is not `mine`, or `org_id` is not an `org_…` id.
- Two of `owned`, `owner` and `org_id` are present.
- `limit` is zero or above the configured maximum.
- `cursor` is malformed.
- A tag is invalid.
- A capability flag is not canonical.
- A flag list is empty.

## Statuses

| Status | When |
|--------|------|
| 200 | The [pagination](../../httpapi.md#13-pagination) envelope of `ClientListRow`s |
| 400 | One of the query parameters is invalid (see [Errors](#errors)) |
| 401 | Missing or invalid auth headers, or an `X-Client-Id` that is not a `user_…` |
| 403 | Caller holds neither `read_clients` nor `read_any_clients`, or names an `org_id` without `read_orgs` |
| 404 | `org_id` names an organization outside the caller's roster read, or none |
