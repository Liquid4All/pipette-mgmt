# `GET /api/v1/plans/{plan_id}/results.csv`

The plan's results as one comma-separated values (CSV) file: one row per metric
of each job's latest scoring run, carried-over jobs included. Scoped and tiered
exactly as [`GET /api/v1/plans/{plan_id}/jobs`](list_plan_jobs.md) is.

**Read from pipette-duckdb, not the store.** It republishes on its refresh
tick (5 minutes by default), so a result or a plan filed since is missing
until the next one. Without a `[duckdb]`
[configuration section](../../cli.md#configuration-file) the route answers
`404`.

The jobs read with `results=true` ([the plan page's body](list_plan_jobs.md#the-plan-pages-body)) reads
the same rows for the jobs on its page, and puts them on each job, as the
[console app](../../../ui/README.md) shows them; it is the JSON read of a
plan's results. It names those jobs by id rather than through `plan_jobs`, so a plan filed since the last
refresh still shows the results its carried-over jobs hold.

## Result rows

What a row carries, as `jobs[].result.rows[]` of
[the plan page's body](list_plan_jobs.md#the-plan-pages-body) and the columns of
[the file](#the-file) hold it: `ResultRow`. The descriptors are pipette-plan-types'
`Model` and `Runtime`, and the flag groups are in the flat form a plan's flags
take, axis keys included
([the plan format](../campaigns.md#the-plan-document)).
`device` holds the row's non-null `device_*` columns without the prefix
([reading a plan's results](../plans.md#reading-a-plans-results)). A scored
`eval` or `vlm_eval` row carries `score_runtime_version`: the version of the
scoring service that graded it. Every other row, and a row whose scoring
failed, carries `null` in that field.

`runtime_descriptor_sha256` is the digest of `runtime_descriptor` that the
scorer stored. The digest leaves out a vault `custom_label`
([storage.md](../../storage.md#model_descriptor--runtime_descriptor)). It is
`null` on a row that the warehouse stored without a digest. The file contains
`runtime_catalog_name` instead of the digest. This column contains the
runtime's catalog name from `descriptor_names` in
[the plan page's body](list_plan_jobs.md#the-plan-pages-body). The cell is
empty for a runtime with no catalog name.

`model_descriptor_sha256` is the digest of `model_descriptor` that the scorer
stored. For a vault run, `model_descriptor` is the Hugging Face alias of the
vault model, so the digest is the alias's
([storage.md](../../storage.md#model_descriptor--runtime_descriptor)). The file
contains `model_catalog_name` instead of the digest, for example
`AcmeLM-VL-3B::gguf::q4_0`. The cell is empty for a model with no catalog
name.

**The withheld tier.** `client_id`, `device_name`, `client_version`,
`client_extras` and `failed_reason` are `null` and `device` is empty: free text
can list client ids, as an all-denied or `lease_lost` failure does. Of
`failure_source`, only `expired` and `lease_lost`, which describe the job
itself, are kept. Any other reads `null`, `client` included, so that a `null`
does not single out an `all_clients_denied` or `unmatchable` failure, which
tells a closed roster or a bad clause of the document that tier withholds
([fleet disclosure](../devices.md#fleet-disclosure)).

"Unknown" is the **unknown wrapper** `{"type": "unknown", "value": ...}`, for
a stored value that does not decode (not JSON, or a shape or variant this build
does not know): `value` holds the stored JSON, or the stored text as a string
where it is not JSON. A re-scored job keeps its newest run only, as
`GET /jobs/{job_id}` does.

## The file

The file holds every result row of the plan, read in one query with no row
ceiling. Its checks run in this order: `read_plans` (`403`), the scope
(`404`), a configured `[duckdb]` (`404`) and pipette-duckdb's answer (`502`),
each refusal in the JSON envelope. The columns and the cell rules are the
description of the `200` answer.

The `Content-Disposition` of the answer names the file:

- A plan in a campaign is named `{campaign}-plan-{n}.csv`.
  - `{campaign}` is the campaign's name if the caller may read the campaign.
    If not, it is the campaign's id.
  - The server cuts a name longer than 200 bytes of UTF-8 between two
    characters. The file name then stays below the 255-byte limit of common
    file systems.
  - `{n}` is the place of the plan in the campaign's history. The campaign's
    first plan is 1, and each revision, re-run, or revert adds 1. The number
    of a plan does not change when later plans are filed.
  - If an earlier plan's record is gone, the name is `{campaign}.csv`.
- A plan outside every campaign is named `{plan_id}-results.csv`.

The server replaces control characters, invisible format characters, and each
of `/ \ : * ? " < > |` with `_`. `filename*` carries the name as UTF-8, as
RFC 5987 defines. `filename` carries the name in ASCII, with `_` in place of
each non-ASCII character.

## Statuses

| Status | When |
|--------|------|
| 200 | The file, named in its `Content-Disposition` as [The file](#the-file) gives |
| 400 | `plan_id` in the path contains characters outside `[A-Za-z0-9-]` |
| 401 | Missing or invalid auth headers, or an `X-Client-Id` that is not a `user_…` |
| 403 | Caller holds neither `read_plans` nor `read_any_plans` |
| 404 | No such plan, **or** a plan the caller's scope does not admit ([one `404` for both](get_plan.md#one-404-for-both)); or no `[duckdb]` is configured |
| 502 | pipette-duckdb is unreachable, refused the query, or took longer than `[duckdb] timeout_secs`; or it lacks a table or column [it must serve](../plans.md#reading-a-plans-results) |
