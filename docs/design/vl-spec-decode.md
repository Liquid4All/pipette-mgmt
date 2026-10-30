# Design notes: `vl_spec_decode`

Why the benchmark is shaped the way it is. The contract is
[benchmarks.md §2.8](../benchmarks.md#28-vl-speculative-decoding) and the
pairing rule is [methodology/vl-spec-decode.md](../methodology/vl-spec-decode.md);
both win over this file. Nothing factual is repeated here.

Derived from the DSpark VL reproduction procedure: LFM2.5-VL-3B with a 4-layer
drafter at draft length 8, against the same model with speculation off, over
MMSpec `test`. That procedure is a hand-driven shell harness on one laptop;
this is what it takes to make the fleet run it unattended.

## Decisions

**The upstream owns the corpus; mgmt serves a gated cache of it.** A corpus of
real images cannot be generated from parameters. mgmt repacks the pinned
upstream into one format so a client needs no per-corpus code, keeps that repack
cached, and serves it only while a minute-old probe finds the upstream still
publishing the pin. The details are in
[vl-spec-decode-dataset-delivery.md](vl-spec-decode-dataset-delivery.md).

**One run, one row; the pair is analysis.** Each mode is its own run under the
same benchmark id, and the server scores each row alone. Both arms in one row
would file the baseline under the speculative model's identity, re-measure it
for every drafter over the same target, make a merged-gguf baseline look the
same as a regular model's, and put the comparison inside the client. What a
pair in one job would give for free, back-to-back runs in one thermal state, is
left to the per-sample readiness gate.

**The server computes nothing from two rows.** Which baseline a drafter is
compared with (a regular model, or the same artifact with speculation off) is a
choice, and the model pairing is not on the wire. A stored speedup would fix
one answer to both at write time. The pairing rule lives with the analysis
instead, in the methodology's last section.

**The mode is a runtime flag, read back from the row.** `speculative_enabled`
on the runtime flags, resolved by the client against the model, and submitted
resolved. Not a benchmark parameter: the two rows of a pair must share the
benchmark id, so the corpus and draft length match by construction. Not
inferred from the counters either: the drafter counters are validated against
the mode, which only works if the mode comes from somewhere else. It is lifted
into `runtime_speculative_enabled` because it is what a pairing splits on, and
`runtime_flags` is otherwise an opaque string.

**Batch size is a parameter, and the wire does not change for it.** Running
`N` samples at once is the same measurement under load, and what a pair must
match on, so it is in the benchmark id like the draft length rather than a
cell knob the analyst has to compare. The per-request sums keep their meaning
at any `N`; the aggregate rate is implied by them and the parameter (Little's
law, in the methodology), so a traversal wall clock was considered and left
off: it would be a second number the sums already determine, and the part it
adds (slot drain at the end, client gaps) is what the runtime clock was chosen
to exclude.

**Counters on the wire, metrics derived server-side.** `accept_length` has
several conventions in circulation and the reference figure depends on which
one produced it. Storing counters means the columns are derived now, and any
other convention the totals support stays recomputable from stored rows. The
per-turn mean needs its own sum, `turn_accept_length_sum`, because a mean of
ratios is not a ratio of sums. That sum is on the runtime's own decode
forward count with the prefill out; the pooled `accept_length` stays on the
two token counters as the legacy figure. The exact pooled figure needs two
optional fields, `decode_tokens` and `decode_forwards`.

**Only counters no other counter implies.** A field that is arithmetic on its
neighbours is two chances to disagree and one fact. The decode forward count
looked like one (`generated − accepted`) but is not: `accepted` includes
drafts discarded at the stop token. `prompt_ms` is on the wire because
something divides by it: the runtime-clock TTFT and end-to-end the
reference harness reports.

**Rejections, never repairs.** A clamp turns a misreporting runtime into a
plausible number no run produced, and hides the bug from the client author. A
refusal names the field and the bound, which is what lets every derived metric
be a single unguarded expression.

**A mismatched dataset digest is recorded, not rejected.** The run still
measured something real. The row says which corpus, so a consumer can filter;
turning the submission away would leave nothing to filter.

**Greedy identity as a digest, not a count.** Identity needs both outputs, and
no client holds both rows. A hash over the replies in order lets two rows be
compared without any text leaving the device; the per-turn hashes that locate a
divergence stay local.

**A VL benchmark, named `vl_spec_decode`.** Images plus text through a vision
encoder, over a corpus, so the `vl_` prefix alongside `vl_throughput` and
`vl_max_memory`. Named for speculation even though half its rows do not
speculate: the draft length is in every id, and the run with speculation off is
the baseline condition of the same measurement, not a separate one.

**The readiness gate runs per sample, not per run or per turn.** Heat builds
up over a traversal and late samples measure a hotter host than early ones (the
reference procedure saw late-subset TTFT drift). Gating each sample makes every
one start from a ready host. Turns within a sample are never gated: they are one
conversation, and each prompt carries the previous replies. A batched run gates
once instead: with `N` slots busy the host is never between samples, and a
gate would measure the stall it caused.

**A merged drafter is a model without a `draft` file.** DSpark ships target
and drafter as one gguf, so `gguf_vision_speculative` takes `draft` optionally
rather than adding a model type. What the fork needs to use it
(`--spec-type draft-dflash`, kernel env switches) is launch config, not model
identity, so it lives in the cell's ordinary `raw` and `envs`; `raw` may carry
no speculation flag when the mode is off.

**The catalog's `dataset` object is `pipette_plan_types::dataset`, shared
with the clients.** `pipette-core::dataset` imports it and adds what only the
server does with it: the probe verdict, the cached artifact, the blank-subset
check. The shared type tolerates unknown keys so a client can read a newer
server's catalog, so a mistyped key under `[dataset]` is dropped rather than
refused; the pinned digests and tree id catch any that change what is fetched.

## Open

- **Third-party licensing.** Serving a third-party corpus to fleet devices is a
  distribution question the synthetic benchmarks never raised. MMSpec's
  repository is Apache-2.0, but its images come from COCO, GQA, TextVQA,
  CharXiv and MMMU-Pro under their own terms; unconfirmed.
- **The model pairing.** Nothing in a row names the regular model a
  spec-decoding model should be compared with. A plan that names the pair, or a
  base-model field on the speculative model, would move it onto the wire.
- **Matched conditions.** The two rows of a pair are separate jobs, possibly
  days apart. The readiness gate holds each to a ready host; nothing checks
  that the two host states matched (the reference harness's prefill canary has
  no equivalent here).
- **Where identity broke.** The output digest says whether a subset matched,
  not how many turns diverged or which. The per-turn hashes that answer that
  stay in each device's local record and are never uploaded.
- **The client side.** This repository is the server; the download, the
  traversal, the digest checks and the refusals live in `pipette-clients`.
  `pipette-plan-types` carries the flag variants, `speculative_enabled` and the
  result variant (`BenchmarkResultData::VlSpecDecode`) the clients build their
  submission from.
