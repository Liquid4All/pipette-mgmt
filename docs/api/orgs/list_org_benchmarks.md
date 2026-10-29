# `GET /api/v1/orgs/{org_id}/benchmarks`

The benchmarks the organization keeps to itself
([private benchmarks](../orgs.md#private-benchmarks)). It takes membership, or
`read_any_orgs`.

The route answers `{"org_id": …, "benchmarks": [ … ],
"unreadable_benchmark_ids": [ … ]}`, each sorted by id. `unreadable_benchmark_ids`
names the ids kept under a key whose entry does not read as a definition: they
are served to nobody, and each still holds its id and refuses the
organization's delete (the CLI's `orgs delete`), so the
listing names them rather than hiding them. Why one does not read stays in the server log. A read that fails, rather
than an entry that does not parse, is a `500`: the fault is the server's, and
the entry may be sound.

## Statuses

| Status | When |
|--------|------|
| 404 | The organization is outside the caller's reach or does not exist: not the caller's own, for a read |
