# GPQA Diamond 2026.06.1

Methodology for the `eval_gpqa_diamond_2026.06.1` benchmark. See
[`benchmarks.md` § Supported evals](../benchmarks.md#supported-evals) for the
catalog entry and [`scoring-service.md`](../scoring-service.md) for the scoring
contract. mgmt owns only the catalog definition and the proxy/storage path; the
scoring itself runs in `pipette-scores` and generation runs in `pipette-clients`.

## Catalog definition

| Field | Value |
|---|---|
| `benchmark_type` | `eval` |
| `parameter_eval_id` | `gpqa_diamond` |
| `parameter_dataset_name` | `2026.06.1` |
| `parameter_max_tokens` | `8192` |

198 questions, each completed 5 times → 990 completions per model.

## What it measures

Graduate-level, "Google-proof" science reasoning across biology, physics, and
chemistry — the **Diamond** subset of `Idavidrein/gpqa` (the highest-quality
tier). Each question is a 4-option multiple choice item (A–D); the correct
answer plus three distractors are shuffled deterministically per question
(`md5(question)` seed) and baked into the served prompt.

The set is **gated** (CC BY 4.0, "do not reveal examples in plain text online"),
so it is not committed to `pipette-scores` or its image — it is materialized
from the pinned upstream revision and mounted at deploy.

## How it runs

- **Temperature 0.6.** The catalog has no temperature field; the client assigns
  `0.6` from the eval id (`eval_temperature()` in `pipette-clients`). No fixed
  seed, so repeated attempts are independent draws.
- **5 repeats, served as `#k` ids.** `metadata.repeats = 5`; the scoring service
  expands each base id `<id>` into `<id>#0 … <id>#4`, so the 198 questions are
  served as 990 attempt ids. mgmt stores one warehouse row per id.
- **Generative, not constrained.** The model writes a free response ending in
  `Answer: A/B/C/D`; it does **not** use `parameter_mcq_choices` / constrained
  decoding (that is the MMLU-style path). GPQA sets no `parameter_mcq_choices`.

## Scoring

Run in `pipette-scores`:

- Reasoning blocks (`<think>` / `[THINK]`) are stripped before scoring.
- The chosen letter is extracted with the Artificial Analysis MCQ extractor
  (`score_mcq(..., valid_options="ABCD")` — `Answer: X` plus markdown-tolerant
  fallbacks, last match wins) and compared to the correct option. Extraction is
  restricted to A–D, so an unparseable response counts as incorrect.

## Reported metric

mgmt derives `accuracy = correct / total` over the 990 attempt rows. Because each
attempt is its own row, this **equals pass@1** — the mean correctness over the 5
attempts. The scorer also emits `gpqa_diamond_pass_at_1` (plus `_stderr`,
`_per_sample`, `gpqa_diamond_unparsed_rate`, and a `gpqa_diamond_choice/*`
distribution) in the response `context`; mgmt persists `context` opaquely.
