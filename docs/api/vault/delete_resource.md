# `DELETE /api/v1/vault/resources/{resource_id}`

Responds `204 No Content`. **Deletes the record and every other record of its
upload**, since the plan entry they make up needs every file, and then **each
of its files no other record names**. What it revokes is every device's access
to those files, which is the point. A file another record still names stays;
describing a deleted file again means uploading it again. This is also the
only way to change anything about a resource: delete it, and upload again
under new ids.

It takes the owner, or `manage_orgs` (and membership) for an organization's
([who may do what](../vault.md#who-may-do-what)).

## Statuses

| Status | When |
|--------|------|
| 204 | The record, the rest of its upload, and each file no other record names are deleted |
| 403 | A member deleting its organization's resource is told it needs `manage_orgs`; any other resource outside the caller's scope is refused as belonging to another owner |
| 404 | No such resource |
