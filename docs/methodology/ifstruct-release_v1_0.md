# IFStruct release_v1_0

Methodology for the `eval_ifstruct_release_v1_0` benchmark. See
[`benchmarks.md` § Supported evals](../benchmarks.md#supported-evals) for the
catalog entry and [`scoring-service.md`](../scoring-service.md) for the scoring
contract. mgmt owns only the catalog definition and the proxy/storage path; the
scoring itself runs in `pipette-scores` and generation runs in `pipette-clients`.

## What this is

The IFStruct v1.0 release set: the full 2000-task IFStruct benchmark, served as
the `release_v1_0` dataset and the single canonical IFStruct benchmark in the
catalog. Each task gives an explicit output schema and
structural constraints; the model must return JSON or YAML that satisfies them.

Prompts are **humanised** — each is phrased as a natural request that reads like
a person thinking out loud, and deliberately includes **distractors**: fields the
requester mentions wanting and then explicitly rejects ("I keep wanting to call
it `review_title`, but no, keep it `audit_focus`"). A model passes only by
following the *final, settled* instruction rather than a mentioned-then-discarded
one.

## Catalog definition

| Field | Value |
|---|---|
| `benchmark_type` | `eval` |
| `parameter_eval_id` | `ifstruct` |
| `parameter_dataset_name` | `release_v1_0` |
| `parameter_max_tokens` | `8192` |

The full 2000-prompt set, each completed once → 2000 completions per model.

## What it measures

Structured-output generation: producing JSON or YAML that satisfies an explicit
schema and structural constraints (required fields, types, enums, item counts,
wrapper keys, code fences, no stray commentary) from realistic, human-style
prompts with embedded misdirection.

## How it runs

- **Temperature 0.6.** The catalog has no temperature field; the client assigns
  `0.6` from the eval id (`eval_temperature()` in `pipette-clients`), no fixed
  seed.
- **Single attempt.** `metadata.repeats = 1`, so the samples endpoint serves the
  2000 ids unchanged — no `#k` expansion. One completion per prompt, one
  warehouse row per id.

## Scoring

Run in `pipette-scores` via the `structured_format` validator, deterministically
and in a fixed order: strip reasoning, then check code block when required, parse
(JSON/YAML), no commentary outside the structured value, top-level structure and
wrapper key, per-field schema (type, required presence, enum, numeric bounds),
and item count. Reasoning is removed before validation — `<think>…</think>`,
`[THINK]…[/THINK]`, orphan opening tags, and gpt-oss Harmony channel preambles —
so a model's visible scratch-work in those forms does not by itself fail the
no-commentary check. A sample passes only with zero validation errors.

## Reported metric

mgmt derives `accuracy = correct / total` over the 2000 rows — the plain
per-prompt pass rate (not a pass@1). The scorer also emits slice aggregates
(`by_format`, `by_top_level_structure`, `by_entity_type`, `common_errors`) in the
response `context`; mgmt persists `context` opaquely.
