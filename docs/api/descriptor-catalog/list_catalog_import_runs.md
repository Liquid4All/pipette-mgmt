# `GET /api/v1/descriptor-catalog/import-runs`

Read the newest catalog import and refresh runs. This read needs
`read_clients`, the fleet-read permission, because an error summary can hold
internal storage and upstream details. It requires the SQL `[database]` store. `limit` defaults to the
server's `list_default_limit` (normally 50). `limit` cannot exceed the server's
`list_max_limit` (normally 200). Runs are ordered by `started_at` descending,
then `id` descending. The route returns the newest N runs in one response.

Each run in `items` has these fields:

- the ID
- the start time and the finish time
- the command: `import` or `refresh`
- the source, and an optional refresh target
- the stored result: `running`, `success`, or `error`
- the outcome: `running`, `success`, `unclassified`, or `failed`
- the counts of entries created, entries hidden, and members added
- the names of the unclassified assets
- an optional error summary

A `running` run has no finish time. The outcome `unclassified` is an `error`
run with only unclassified assets. The importer finished its catalog work but
found assets that it could not classify. The outcome `failed` is any other
`error` run, for example an upstream failure or a storage failure.

`last_successful_import_at` is the latest finish time of an `import` run with
`result=success`. That run can be older than every run in `items`. A
successful `refresh` does not change it. It is `null` when no import has
succeeded. The catalog page warns when it is more than 36 hours old.

## Statuses

| Status | When |
|--------|------|
| 200 | Recent runs and the last successful import time; `items` may be empty |
| 400 | `limit` is zero or exceeds the server maximum |
| 403 | The caller lacks `read_clients` |
| 404 | The server has no SQL `[database]` store |
