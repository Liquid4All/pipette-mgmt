# `PUT /api/v1/orgs/{org_id}/benchmarks/{benchmark_id}`

Takes the **definition alone** and creates or replaces one of the benchmarks
the organization keeps to itself
([private benchmarks](../orgs.md#private-benchmarks)), from JSON or the catalog
file's TOML. It takes `manage_orgs` **and** membership, or `manage_any_orgs`.

## Preconditions

A precondition narrows the route to creating or replacing, judged by key,
so an entry that does not read counts as kept:

- `If-None-Match: *` creates only: an id the organization already keeps is a
  `412`, `{slug} already keeps a benchmark {benchmark_id}`.
- `If-Match: *` replaces only: an id it keeps nothing under is a `412`, so an
  entry deleted since the caller read it is not written back.

This route serves no `ETag`, so any other value, or both headers, is a `400`. The
check comes after the grant and before the body is read, and the store has no
conditional write for it, so two writes racing on one id can both pass it.

## Body

The `Content-Type` picks one of two forms of the definition. The server reads
only the essence of the type, as for a plan document
([filing a plan document](../campaigns.md#filing-a-plan-document)):

- `application/json`, or an `application/*+json` type: the shape
  [`GET /benchmarks/{benchmark_id}`](../../httpapi.md#26-get-benchmarksbenchmark_id)
  serves minus `benchmark_id`, which the path carries.
- `application/toml`: the catalog file itself, as `orgs benchmarks put` reads
  it, so a file moves between the host, the console app and this route unchanged.
- Any other type, or no `Content-Type`, gets `415`.

Either form is held to every check a catalog file is at load: an unknown or
repeated key, a missing parameter, a `benchmark_id` in the body, or a bound
the loader refuses (a negative count, a zero decode or generation length,
image size, draft length or batch size, a count above 2147483647, a corpus pin
that fails its check) is a `400`, and nothing is written. The body is read
only after the caller is found to administer the organization, so a refused
caller gets the refusal rather than a `400` about its body. A `dataset.url` is
dropped rather than refused, so a body copied from
`GET /benchmarks/{benchmark_id}` still writes: that route stamps the corpus
URL itself, and nothing else serves one.

The response is the entry as `GET /benchmarks/{benchmark_id}` will serve it to
the organization ([private benchmarks](../orgs.md#private-benchmarks)).

## Statuses

| Status | When |
|--------|------|
| 400 | The body is not a definition a catalog file could carry; or the id starts with another type's prefix (`decode_throughput…` for a `prefill_throughput` entry), which readers with only the id would type by; or an `eval`, `vlm_eval` or `asr_eval` entry's evals-server eval (or task) or dataset name is not one plain path segment (letters, digits, `.`, `_` and `-`, starting with a letter or digit), since the evals server's URLs carry it as one; or `If-Match` or `If-None-Match` is not `*`, or both are sent; or `org_id` is not an organization id |
| 403 | The caller does not hold `manage_orgs` |
| 404 | The organization is outside the caller's reach or does not exist: not the caller's own or unaffiliated, for a write |
| 409 | The public catalog already holds the id: the public entry is what would be served, so the write is refused rather than stored and never served. Or another organization keeps the id: benchmark ids are one namespace across organizations, so every id resolves to one definition. Or another organization's entry pins the definition's dataset and no public entry does: [`GET /datasets/{benchmark_id}`](../../httpapi.md#215-get-datasetsbenchmark_id) serves an artifact by its digest, so the second pin would be served the first organization's copy. Or another organization's `eval`, `vlm_eval` or `asr_eval` entry names the definition's evals-server eval (or task) and dataset name and no public entry does: the evals server serves and scores samples by name, so the second entry would read the first organization's prompts and answer key. Or a public entry pins the definition's dataset digest as another dataset, or states another `parameter_sample_count`: the build and probe of that artifact follow the public pin |
| 412 | `If-None-Match: *` and the organization keeps the id, or `If-Match: *` and it keeps nothing under it |
| 415 | The `Content-Type` is missing, or is not a JSON type or `application/toml` |
