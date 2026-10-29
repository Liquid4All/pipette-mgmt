# IFBench 2026.06.1

Methodology for the `eval_ifbench_2026.06.1` benchmark, defined in
[`examples/benchmarks/eval_ifbench_2026.06.1.toml`](../../examples/benchmarks/eval_ifbench_2026.06.1.toml). The
sampling and seed rules shared by every eval, repeats included, are in
[`benchmarks.md` §2.5](../benchmarks.md#25-eval-accuracy) § Execution, and the
scoring contract is [`scoring-service.md`](../scoring-service.md). mgmt owns the
catalog definition and the proxy/storage path; scoring runs in `pipette-scores`
and generation in `pipette-clients`.

## What it measures

Precise single-turn instruction following on out-of-distribution constraints
(counting, formatting, casing, sentence structure, …). Each prompt carries one
or more verifiable constraints checked by deterministic Python checkers: no
judge model. It is the upstream `allenai/IFBench` test set, run verbatim with no
down-selection.

## How it runs

300 prompts, each completed 5 times and served by the scoring service as `<id>#0
… <id>#4`: 1500 completions per model, one warehouse row each.

The entry pins `parameter_temperature = 0.6`, the same value
`eval_temperature()` (mgmt's `crates/pipette-plan-types/src/benchmark/mod.rs`)
falls back to for this eval id when an entry pins none. No fixed seed is sent,
so repeated attempts are independent draws.

## Scoring

Run in `pipette-scores`:

- Reasoning blocks (`<think>` / `[THINK]`) are stripped before scoring.
- Degenerate output (empty, all-punctuation, repetition-collapse) is rejected so
  a non-answer cannot satisfy a constraint vacuously.
- Each attempt is scored **loose, prompt-level**: a prompt is correct when every
  constraint passes under any of the 8 upstream loose transformation variants.

## Reported metric

mgmt derives `accuracy = correct / total` over the 1500 attempt rows. Because
each attempt is its own row, this **equals pass@1**: the mean correctness over
the 5 attempts. The scorer also emits `ifbench_pass_at_1` (plus `_stderr` and
`_per_sample`) in the response `context`; mgmt persists `context` opaquely.
