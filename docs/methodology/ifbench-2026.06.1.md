# IFBench 2026.06.1

Methodology for the `eval_ifbench_2026.06.1` benchmark. See
[`benchmarks.md` § Supported evals](../benchmarks.md#supported-evals) for the
catalog entry and [`scoring-service.md`](../scoring-service.md) for the scoring
contract. mgmt owns only the catalog definition and the proxy/storage path; the
scoring itself runs in `pipette-scores` and generation runs in `pipette-clients`.

## Catalog definition

| Field | Value |
|---|---|
| `benchmark_type` | `eval` |
| `parameter_eval_id` | `ifbench` |
| `parameter_dataset_name` | `2026.06.1` |
| `parameter_max_tokens` | `8192` |

300 prompts, each completed 5 times → 1500 completions per model.

## What it measures

Precise single-turn instruction following on out-of-distribution constraints
(counting, formatting, casing, sentence structure, …). Each prompt carries one
or more verifiable constraints checked by deterministic Python checkers — no
judge model. It is the upstream `allenai/IFBench` test set, run verbatim with no
down-selection.

## How it runs

- **Temperature 0.6.** The catalog has no temperature field; the client assigns
  `0.6` from the eval id (`eval_temperature()` in `pipette-clients`). No fixed
  seed is sent, so repeated attempts are independent draws.
- **5 repeats, served as `#k` ids.** The `2026.06.1` dataset sets
  `metadata.repeats = 5`. The scoring service expands each base sample id
  `<id>` into `<id>#0 … <id>#4` in the samples response. The client completes
  each `#k` as an ordinary sample and submits one completion per id. mgmt does
  not expose or handle repeats — it stores one warehouse row per id.

## Scoring

Run in `pipette-scores`:

- Reasoning blocks (`<think>` / `[THINK]`) are stripped before scoring.
- Degenerate output (empty, all-punctuation, repetition-collapse) is rejected so
  a non-answer cannot satisfy a constraint vacuously.
- Each attempt is scored **loose, prompt-level**: a prompt is correct when every
  constraint passes under any of the 8 upstream loose transformation variants.

## Reported metric

mgmt derives `accuracy = correct / total` over the 1500 attempt rows. Because
each attempt is its own row, this **equals pass@1** — the mean correctness over
the 5 attempts. The scorer also emits `ifbench_pass_at_1` (plus `_stderr` and
`_per_sample`) in the response `context`; mgmt persists `context` opaquely.
