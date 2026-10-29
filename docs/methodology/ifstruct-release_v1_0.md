# IFStruct release_v1_0

Methodology for the `eval_ifstruct_release_v1_0` benchmark, defined in
[`examples/benchmarks/eval_ifstruct_release_v1_0.toml`](../../examples/benchmarks/eval_ifstruct_release_v1_0.toml). The
sampling and seed rules shared by every eval, repeats included, are in
[`benchmarks.md` §2.5](../benchmarks.md#25-eval-accuracy) § Execution, and the
scoring contract is [`scoring-service.md`](../scoring-service.md). mgmt owns the
catalog definition and the proxy/storage path; scoring runs in `pipette-scores`
and generation in `pipette-clients`.

## What this is

The IFStruct v1.0 release set: the full 2000-task IFStruct benchmark, served as
the `release_v1_0` dataset and the single canonical IFStruct benchmark in the
catalog. Each task gives an explicit output schema and
structural constraints; the model must return JSON or YAML that satisfies them.

Prompts are **humanised**: each is phrased as a natural request that reads like
a person thinking out loud, and deliberately includes **distractors**: fields the
requester mentions wanting and then explicitly rejects ("I keep wanting to call
it `review_title`, but no, keep it `audit_focus`"). A model passes only by
following the *final, settled* instruction rather than a mentioned-then-discarded
one.

## What it measures

Structured-output generation: producing JSON or YAML that satisfies an explicit
schema and structural constraints (required fields, types, enums, item counts,
wrapper keys, code fences, no stray commentary) from realistic, human-style
prompts with embedded misdirection.

## How it runs

The full 2000-prompt set, each completed once (`metadata.repeats = 1`, so no
`#k` expansion): 2000 completions per model, one warehouse row each.

The entry pins `parameter_temperature = 0.6`, the same value `eval_temperature()`
(mgmt's `crates/pipette-plan-types/src/benchmark/mod.rs`) falls back to for this
eval id when an entry pins none. No fixed seed is sent.

## Scoring

Run in `pipette-scores` via the `structured_format` validator, deterministically
and in a fixed order: strip reasoning, then check code block when required, parse
(JSON/YAML), no commentary outside the structured value, top-level structure and
wrapper key, per-field schema (type, required presence, enum, numeric bounds),
and item count. Reasoning is removed before validation (`<think>…</think>`,
`[THINK]…[/THINK]`, orphan opening tags, and gpt-oss Harmony channel preambles),
so a model's visible scratch-work in those forms does not by itself fail the
no-commentary check. A sample passes only with zero validation errors.

## Reported metric

mgmt derives `accuracy = correct / total` over the 2000 rows: the plain
per-prompt pass rate (not a pass@1). The scorer also emits slice aggregates
(`by_format`, `by_top_level_structure`, `by_entity_type`, `common_errors`) in the
response `context`; mgmt persists `context` opaquely.
