# `POST /api/v1/orgs/{org_id}/descriptor-catalog`

Create a hand-made runtime entry or model entry in one organization. The path
takes the organization identifier (ID), not its slug. The caller needs
`edit_catalog` and membership in that organization, or `edit_any_catalog`.

The JavaScript Object Notation (JSON) body has a `name` and exactly one of these
nonempty ordered arrays:

- `runtimes`: each item is a runtime descriptor, as in a `catalog create` file.
- `models`: each item is `{"quantization": "q4_0", "model": { ... }}`. The
  `model` is the model table of a plan. The `quantization` label matches
  `[a-z0-9_]+`. The server derives the format from the model type.

The optional `release_time` follows Request for Comments (RFC) 3339. A runtime
member cannot use a vault source. An org model member can use vault resources
owned by that organization. The server checks each resource's owner, record,
and pinned digest. A verified Hugging Face origin also lets the member name
result rows that use the model's public alias.

A Hugging Face model member must name only public, ungated repositories. The
server asks Hugging Face about each repository without a token and pins each
revision to its commit. A branch such as `main` becomes the commit that it
names at that time. The API refuses a model with an `auth_token`.

```json
{
  "name": "AcmeLM-VL-3B",
  "models": [{
    "quantization": "q4_0",
    "model": {
      "type": "gguf_text", "source": "huggingface", "org": "acme",
      "repo_name": "AcmeLM-VL-3B-GGUF", "path": "AcmeLM-Q4_0.gguf"
    }
  }]
}
```

Use a name that no entry in this organization holds. Tombstoned entries also
reserve their names. Names in another organization or the global catalog do
not prevent creation. The response is the catalog entry view with its complete
`members` list and an `account:<user_id>` creator.

## Statuses

| Status | When |
|--------|------|
| 201 | The API created the entry |
| 400 | The body or a member fails catalog validation, or Hugging Face refuses a repository, or a repository is private or gated |
| 403 | The caller lacks catalog edit permission |
| 404 | The organization is unreachable, or the server has no database catalog |
| 409 | An entry in this organization already holds the name |
| 502 | Hugging Face failed or limited the rate of the requests |
