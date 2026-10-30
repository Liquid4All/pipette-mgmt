# `GET /api/v1/plans/{plan_id}/variants`

The document's `[[variants]]` blocks alone, read as
[`GET /api/v1/plans/{plan_id}/document`](get_plan_document.md) reads the document and under its
gates and errors. Each block that names no `benchmarks` holds the document's
top-level list, the list it expanded under, so another document can append the
blocks to describe the cells this plan ran. A variant edit's `add_from`
([`POST /api/v1/campaigns/{campaign_id}/variants`](../campaigns/edit_variants.md)) appends them so,
and its `restore` appends one of them.

The `200 OK` body is TOML, with `Content-Type: application/toml; charset=utf-8`:
one `variants` array of tables and nothing else. The document's `expires_at`,
`merge_duplicates` and comments are left out, and the blocks are written anew,
keys sorted and each model and runtime as a table of its own:

```toml
[[variants]]
benchmarks = ["decode_throughput_512_100"]
requires = ["os:macos"]

[[variants.models]]
org = "LiquidAI"
repo_name = "LFM2.5-350M-MLX-4bit"
source = "huggingface"
type = "mlx"

[[variants.runtimes]]
flavor = "macos-arm64"
type = "mlx_macos_pipette"
version = "0.31.3"

[variants.runtimes.source]
contents = """
mlx-lm==0.31.3
"""
type = "pip_requirements_text"
```

With `Accept: application/json`, the server returns `PlanVariants`
([the OpenAPI document](../../httpapi.md#15-the-openapi-document)). Its `variants`
array contains one `[[variants]]` block for each variant. Each block uses
the TOML format. The position of a block in the array is the index that a variant
edit's `restore` names:

```json
{ "variants": ["[[variants]]\nbenchmarks = [\"decode_throughput_512_100\"]\nrequires = [\"os:macos\"]\n…"] }
```

An `Accept` header that names no `application/json`, or names only `*/*`,
gets the TOML answer.

## Statuses

| Status | When |
|--------|------|
| 200 | The `[[variants]]` blocks, as `application/toml; charset=utf-8`. With `Accept: application/json`, one block for each variant, as `PlanVariants` |
| 400 | `plan_id` in the path contains characters outside `[A-Za-z0-9-]` |
| 401 | Missing or invalid auth headers, or an `X-Client-Id` that is not a `user_…` |
| 403 | As for [the document](get_plan_document.md#statuses): no plan read grant, or no fleet read grant for a plan in scope that has a document |
| 404 | As for [the document](get_plan_document.md#statuses): no such plan or one outside the scope, a plan with no document, or a collected one |
