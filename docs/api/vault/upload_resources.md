# `POST /api/v1/vault/resources`

Names the plan variant uploaded files make up, and gets its plan entry. The
body is `UploadVaultResourcesRequest` in the
[OpenAPI document](../../httpapi.md#15-the-openapi-document): the variant's own
plan `type` beside a `name` and an optional `owner`, and each of the variant's
files as `{filename, sha256}`:

```json
{"name": "acme LFM2-VL-3B q4", "type": "gguf_vision",
 "model":  {"filename": "LFM2-VL-3B-Q4_K_M.gguf",     "sha256": "9f86…"},
 "mmproj": {"filename": "mmproj-LFM2-VL-3B-F16.gguf", "sha256": "2c26…"}}
```

`name` is what a plan entry carries as its `custom_label`; nothing keys on it.
`owner`, the organization the files were uploaded for, defaults to the
caller's organization; a caller in none gets a `400` asking it to name one. A
member may name its organization, as for a campaign
([`POST /api/v1/campaigns`](../campaigns/create_campaign.md));
`manage_any_orgs` may name any organization
([who may do what](../vault.md#who-may-do-what)). Scope is checked before
existence, so a caller that may not upload there never learns whether the
organization exists.

Which files each `type` takes, and in what format, is
[`GET /api/v1/vault/types`](types.md). A `filename` is one path segment
of `[A-Za-z0-9._-]`, starting with a letter or digit, at most 255 characters,
and a refusal quotes it. A missing, extra or misnamed file is a `400`. A
directory model is **packed**: one tar, tar.gz or zip whose root is the
directory itself (the model's files at the top, not inside a folder), because
a device unpacks it as is. A directory is never uploaded file by file. A
prebuilt runtime is uploaded as the upstream release archive for its `flavor`,
not repacked.

Responds `201 Created` with `VaultResourcesUploaded`: the variant's plan
entry in its vault arm, as JSON and as the TOML inline table a plan lists, each
vault source carrying the upload's `name` as `custom_label`
([the server-mode plan format](../campaigns.md#the-plan-document)),
and one resource per file, in the variant's file order.

Each file must already be uploaded for the owner, and its first bytes must be
what its field needs: a gguf under `model`, `mmproj` and `draft`, a tar, tar.gz
or zip under every `*archive`. What an archive holds is taken on trust. The
upload of a file does not say which field it is for, so a wrong file is refused
only here, after its bytes were sent: pack a directory before uploading it.

A refusal writes no record.

## Statuses

| Status | When |
|--------|------|
| 201 | The resources are recorded |
| 400 | The body does not parse; a file is not uploaded for the owner; a file is not what its field needs; `owner` names an organization that does not exist; or `owner` is omitted by a caller in no organization |
| 403 | The owner is outside the caller's scope |
