# `PUT /api/v1/vault/blobs/{principal_id}/{sha256}`

Uploads one file, the request body, for `{principal_id}` (`org_…` or `user_…`)
under `{sha256}`, 64 lowercase hex characters. The body is streamed, not
collected, so its ceiling is `vault_blob_limit_bytes` (128 GiB by default)
rather than the [request size limits](../../httpapi.md#12-request-size-limits):
a body declaring more is `413` before it is read, and one that runs past it is
`413` with nothing kept. Signed with RFC 9421, the `Content-Digest` is
`{sha256}` base64-encoded, so the server streams the file and hashes it once.

- **Kept only as its digest says.** The file is kept only if its bytes hash to
  `{sha256}`; otherwise `400` and nothing is kept. An upload cut off midway is
  `400`, or `408` once it sends nothing for `read_timeout_secs`, and keeps
  nothing either: retry from the start.
- **Once.** A file already uploaded for that principal answers `200` and is kept
  as is; the body is not read.
  [`HEAD` on the same path](head_blob.md) answers `200` or `404`, so a client
  can skip it.
- **A few at a time.** mgmt streams a bounded number of uploads at once, since
  each holds multipart parts in memory; one past that is `429`, and retrying
  shortly is the answer.
- **Scope before anything is looked up.** The principal must be the caller, an
  organization it administers or a member of one, or anybody for
  `manage_any_orgs`, the owner rule of
  [`POST /api/v1/vault/resources`](upload_resources.md). Anyone else is `403`
  whether or not the file exists: a digest is guessable, and whose bytes are
  whose is not an answer to give.

Responds `201 Created`, or `200` when the file was already there, with
`{"sha256": …, "bytes": …}`.

## Statuses

| Status | When |
|--------|------|
| 200 | The file was already uploaded for that principal; the body was not read |
| 201 | The file is kept |
| 400 | `{principal_id}` is not an organization or account id, `{sha256}` is not 64 lowercase hex characters, the bytes hash to something else, the principal does not exist, or the body stopped before its end, as when the client closed its connection |
| 403 | The principal is outside the caller's scope |
| 408 | The body sent nothing for `read_timeout_secs` |
| 413 | The file is over `vault_blob_limit_bytes` |
| 429 | Too many uploads are in progress; retry shortly |
