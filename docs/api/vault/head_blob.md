# `HEAD /api/v1/vault/blobs/{principal_id}/{sha256}`

Whether that file is already uploaded for `{principal_id}`, so a client can
skip [the upload](put_blob.md). It takes the upload's scope, checked the same
way before anything is looked up: a principal outside it is `403` whether or
not the file exists.

## Statuses

| Status | When |
|--------|------|
| 200 | The file is uploaded for that principal |
| 400 | `{principal_id}` is not an organization or account id, `{sha256}` is not 64 lowercase hex characters, or the principal does not exist |
| 403 | The principal is outside the caller's scope |
| 404 | The file is not uploaded for that principal |
