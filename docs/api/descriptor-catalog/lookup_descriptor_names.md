# `GET /api/v1/descriptor-catalog/names`

Give one to 100 comma-separated names in `names`. Each name has one of these
forms:

- `<name>`: the entry of the caller's organization. If that entry does not
  exist, the global entry.
- `global/<name>`: the global entry.
- `<org-slug>/<name>`: the entry of that organization, if the caller can read
  it.

The response has two fields. `entries` lists the found entries in request
order. `unknown` lists the names that do not exist, that are tombstoned, or
that the caller cannot read. Each name in `unknown` has the text that the
caller wrote. The lookup is case-insensitive. The response
lists an entry one time, even if two names select it. Each entry has its full
members, member keys, selectors, and `plan_toml` inline tables for plans. A
runtime member also has its offer-rule coverage. A model member also has its
`model_key` and `parameters`, as in
[the full list](list_descriptor_catalog.md).

Set `offer_counts=true` to add `offered_device_count` to each runtime member.
The caller needs `read_clients`; the server returns `403` without it.
It counts approved devices whose capabilities and assigned tags meet that
member's offer rule, even if precedence would select another member. The
server reads the fleet and tags once for the whole lookup. If any device
record is unreadable, or either read fails, the count is absent for every
member rather than incomplete.
Without this option, the server does not read the fleet for counts.

For example, `names=llamacpp-v0.7.0,global/my-fork` looks up a bare name
and a global name.

## Statuses

| Status | When |
|--------|------|
| 200 | Found entries and unknown names |
| 400 | Names are missing, invalid, or more than 100 |
| 404 | The server has no `[database]` catalog |
