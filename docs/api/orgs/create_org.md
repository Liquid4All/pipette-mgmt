# `POST /api/v1/orgs`

Creates an organization under a handle. The body is `CreateOrgRequest`.
The display name defaults to the handle, which is truthful and overwritable
rather than blank. The handle claim is exclusive, so two operators creating one
organization produce one.

## Statuses

| Status | When |
|--------|------|
| 201 | The organization is created; the body is the organization and its (zero) counts |
| 400 | `slug` is empty, too long, outside the charset, or starts with `org_`, which spells an id |
| 403 | The caller does not hold `manage_any_orgs` |
| 409 | The handle is already held, and the body names which organization holds it |
