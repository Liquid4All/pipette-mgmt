# `POST /api/v1/orgs/{org_id}/descriptor-catalog/{name}/members`

Append one member to a hand-made entry in the named organization. The path
takes the organization identifier (ID) and an unqualified catalog name. The
caller needs `edit_catalog` and membership, or `edit_any_catalog`.

The JavaScript Object Notation (JSON) body is one of these:

- `{"runtime": { ... }}` for a runtime entry. Use the same runtime descriptor
  as `catalog add-member`, and the same build as the entry's current members.
- `{"model": { ... }, "quantization": "q4_0"}` for a model entry. The model
  follows the rules of
  [`create_org_descriptor_entry`](create_org_descriptor_entry.md): public,
  ungated Hugging Face repositories pinned to commits, or vault resources
  owned by the organization.

Use a new member key and digest. Runtime members cannot use vault sources.

The response is the updated entry view with its complete `members` list.

## Statuses

| Status | When |
|--------|------|
| 200 | The API appended the member |
| 400 | The body or member fails catalog validation, the member's kind differs from the entry's, or Hugging Face refuses a repository |
| 403 | The caller lacks catalog edit permission |
| 404 | The organization is unreachable, the entry is absent, or the server has no database catalog |
| 409 | The entry is tombstoned or the member duplicates a current member |
| 502 | Hugging Face failed or limited the rate of the requests |
