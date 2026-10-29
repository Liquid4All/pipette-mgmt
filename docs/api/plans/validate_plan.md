# `POST /api/v1/plans/validate`

Check a plan document as filing checks it, and file nothing. Every outcome of
the check answers `200` with a verdict rather than a refusal. A caller can check
a document before it has a campaign, a name, an owner or a base plan. The
console app's plan editors call this route as a document is typed.

Only a user account that holds `submit_plans`
([status, then permission](../users.md#status-then-permission)) may check a
document, and a device gets `403`. The server settles the caller from the
request headers, before it reads the body. The route takes no query parameters.
The body is a scheduler plan document as `application/toml`
([request body](../campaigns.md#request-body)), up to `plan_body_limit_bytes`, the ceiling of
the other plan-document routes ([request size limits](../../httpapi.md#12-request-size-limits)).

The server parses and expands the document as a filing does, then validates the
job bodies as a dry run does. The benchmark catalog and the vault are read in
the caller's reach, as for filing, so another organization's private benchmark
or vault resource reads as one that does not exist. The owner a document is
filed for plays no part. A document that does not expand costs no read beyond
the gate's.

## Response

The `200 OK` body is `PlanValidation`: the jobs the document expands to after
equal bodies merge, the distinct models that carry an `auth_token` (filing
imports them and checks the token's access; this check does neither), and the
`issues` found. An `error` issue is a fault filing refuses, a `warning` one it
reports and files; its message is a filing's `400` without the TOML excerpt or
the job index.

The document is valid when no issue is an error. Errors come before warnings,
and the warnings are the planner's, in plan order. A check stops at its first
error today, as filing does, so a verdict holds at most one; a client should not
rely on that. An error is in the verdict exactly when the expansion and job
checks refuse the same text. Every dry run and every filing runs those checks
before any import, and the error's message is part of their refusal's message.

```json
{"job_count": 8, "gated_model_count": 0, "issues": [{"severity": "error", "message": "spec.benchmark decode_throughput_512_10 not in the benchmark catalog; did you mean decode_throughput_512_100?", "line": 1, "column": null}]}
```

**Where a position comes from.** A TOML fault is placed at the span the parser
reports, except a fault of the whole document, such as a missing `variants`,
which has no position. A fault of one variant, and each warning, is placed at
the line where the variant starts: its `[[variants]]` header, or the opening
brace of its inline table. An unknown benchmark is placed at the first string
that names it in a `benchmarks` list, the top-level list first. Any other fault
has no position.

**What a verdict does not cover.** A dry run or a filing can still refuse a
document with no error:

- The check imports no gated model, so it does not check a token's access or
  the import's other refusals. `gated_model_count` says when that applies.
- It runs no fleet match, so it gives none of the `201` report's `warnings`.
- It reads no model parameter table, so it does not warn about an uncatalogued model.
- It checks no campaign name, owner, authority or base plan, and counts no
  revision's carried jobs. The dry runs of
  [`POST /api/v1/campaigns`](../campaigns/create_campaign.md) and
  [`PUT /api/v1/campaigns/{campaign_id}/document`](../campaigns/revise_document.md) do.
- The catalogs, the vault and a campaign's plan can change between a check and
  a filing.

The verdict holds no job body; a dry run returns them.

## What a check costs

The expansion is the cost of a check, and `plan_max_jobs` bounds it: a document
that does not parse costs the gate's reads alone, and one at the default ceiling
of 5000 jobs about a second of server time. A caller that checks as a user
types should wait for a pause, keep one request in flight, and send no
unchanged text. The server logs the counts of jobs, errors and warnings at
debug level, never a message, because a message can quote the document.

A method other than `POST` answers `405`. A store failure while the check read
the benchmark catalog or the vault answers `500`, its message redacted.

## Statuses

| Status | When |
|--------|------|
| 200 | The verdict, whatever the check found |
| 400 | The body is not valid UTF-8 |
| 401 | Missing or invalid auth headers |
| 403 | `X-Client-Id` names a device, the user is pending or disabled, or the user holds no `submit_plans`. Before the body is read |
| 413 | The body is over `plan_body_limit_bytes`. The body is never parsed |
| 415 | The `Content-Type` is not `application/toml` |
