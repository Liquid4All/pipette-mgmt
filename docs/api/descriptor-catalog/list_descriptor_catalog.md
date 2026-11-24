# `GET /api/v1/descriptor-catalog`

List entries by release time, newest first. At the same release time, global
entries come before org entries. Names break the remaining ties in ascending,
case-insensitive order. Use `limit` and `cursor` to read later pages. The
default limit is 50, and the maximum is 200.

These filters are available. `kind`, `state`, and `type` take comma-separated
values.

- `scope`: `global`, or the id or slug of the caller's organization. The
  default is every scope that the caller can read.
- `kind`: the entry's release kind, `hand-made`, `stable`, or `daily`.
- `state`: the entry's state. The default states are pinned, active, and
  hidden. `state=tombstoned` is invalid.
- `descriptor_kind`: `runtime` or `model`.
- `type`: runtime types or model types.
- `prefix`: a case-insensitive name prefix.

Every row includes `name`, `scope`, `display_name`, `kind`, `state`,
`release_time`, `created_at`, `created_by`, `descriptor_kind`,
`descriptor_types`, `member_count`, and `member_summaries`.

`created_at` is the time that the entry was made. `created_by` is the creator
of the entry:

- `operator`: an operator made a global entry with the host command-line
  interface (CLI), or moved an organization entry to global scope.
- `importer:llamacpp`: the llama.cpp importer made a global entry.
- `account:<user_id>`: an account made an organization entry.

`descriptor_types` lists the types of the entry's members.
A runtime entry has one type, and a model entry has one type for each format.
A model entry row also gives its `family`. If the family has a logo, the row
gives the digest of the logo in `family_logo_sha256`. Get the logo with
[`GET /api/v1/descriptor-catalog/family-logos/{sha256}`](get_descriptor_family_logo.md).

A member summary gives the member's key and full selector. A runtime member
also gives its offer-rule coverage. A model member has no offer rules. Set
`members=true` to include each member's digest, descriptor, addition time,
and `plan_toml`. The `plan_toml` field contains an inline table for a plan's
`runtimes` or `models` array. With `members=true`, a model member also gives:

- `model_key`: the key of its row in the model parameter table. It is absent
  if the model descriptor has no name from which to make a valid key.
- `parameters`: the `total_millions` and `active_millions` of that row. It is
  absent if the table has no such row, or if the server cannot read the table.

For example, `prefix=llamacpp` can find `llamacpp-v0.7.0` and
`llamacpp-daily-2026-10-19`.

## Statuses

| Status | When |
|--------|------|
| 200 | A page of entries, with `next_cursor` if more entries remain |
| 400 | A filter, limit, or cursor is invalid |
| 404 | `scope` names an organization that the caller cannot read |
