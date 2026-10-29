# `GET /api/v1/orgs/{org_id}/benchmarks/{benchmark_id}`

One of the benchmarks the organization keeps to itself
([private benchmarks](../orgs.md#private-benchmarks)), as JSON or the catalog
file. It takes membership, or `read_any_orgs`.

The route answers the catalog file instead of JSON,
as `Content-Type: application/toml`, when the `Accept` header ranks
`application/toml` above `application/json`: what
[`PUT`](put_org_benchmark.md) takes as TOML, without
the id or the organization, which the path carries. Each type takes the `q` of
the most specific range naming it (itself, `application/*`, then `*/*`), and
`q` is read in any case. No `Accept`, `*/*`, a tie and `application/json`,
which the client CLIs send, keep JSON. Either answer carries `Vary: Accept`,
and a refusal is the JSON envelope whatever was asked.

## Statuses

| Status | When |
|--------|------|
| 404 | The organization is outside the caller's reach or does not exist: not the caller's own, for a read. Also an id the organization keeps nothing under |
