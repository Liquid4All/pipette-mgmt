# GPQA Diamond 2026.06.1

Methodology for the `eval_gpqa_diamond_2026.06.1` benchmark, defined in
[`examples/benchmarks/eval_gpqa_diamond_2026.06.1.toml`](../../examples/benchmarks/eval_gpqa_diamond_2026.06.1.toml). The
sampling and seed rules shared by every eval, repeats included, are in
[`benchmarks.md` §2.5](../benchmarks.md#25-eval-accuracy) § Execution, and the
scoring contract is [`scoring-service.md`](../scoring-service.md). mgmt owns the
catalog definition and the proxy/storage path; scoring runs in `pipette-scores`
and generation in `pipette-clients`.

## What it measures

Graduate-level, "Google-proof" science reasoning across biology, physics, and
chemistry: the **Diamond** subset of `Idavidrein/gpqa` (the highest-quality
tier). Each question is a 4-option multiple choice item (A–D); the correct
answer plus three distractors are shuffled deterministically per question
(`md5(question)` seed) and baked into the served prompt.

The set is **gated** (CC BY 4.0, "do not reveal examples in plain text online"),
so it is not committed to `pipette-scores` or its image. It is materialized
from the pinned upstream revision and mounted at deploy.

## How it runs

198 questions, each completed 5 times and served by the scoring service as
`<id>#0 … <id>#4`: 990 completions per model, one warehouse row each.

The entry pins `parameter_temperature = 0.6`, the same value
`eval_temperature()` (mgmt's `crates/pipette-plan-types/src/benchmark/mod.rs`)
falls back to for this eval id when an entry pins none. No fixed seed is sent,
so repeated attempts are independent draws.

**Generative, not constrained.** The model writes a free response ending in
`Answer: A/B/C/D`; it does **not** use `parameter_mcq_choices` / constrained
decoding (that is the MMLU-style path). GPQA sets no `parameter_mcq_choices`.

## Scoring

Run in `pipette-scores`:

- Reasoning blocks (`<think>` / `[THINK]`) are stripped before scoring.
- The chosen letter is extracted with the Artificial Analysis MCQ extractor
  (`score_mcq(..., valid_options="ABCD")`, `Answer: X` plus markdown-tolerant
  fallbacks, last match wins) and compared to the correct option. Extraction is
  restricted to A–D, so an unparseable response counts as incorrect.

## Reported metric

mgmt derives `accuracy = correct / total` over the 990 attempt rows. Because each
attempt is its own row, this **equals pass@1**: the mean correctness over the 5
attempts. The scorer also emits `gpqa_diamond_pass_at_1` (plus `_stderr`,
`_per_sample`, `gpqa_diamond_unparsed_rate`, and a `gpqa_diamond_choice/*`
distribution) in the response `context`; mgmt persists `context` opaquely.
