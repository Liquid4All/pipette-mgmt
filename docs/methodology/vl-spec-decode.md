# VL Speculative Decoding: reading the rows

How `vl_spec_decode` rows are compared once they are stored, which
[benchmarks.md §2.8](../benchmarks.md#28-vl-speculative-decoding) leaves to the
reader. What a client counts and how it runs a traversal is the clients repo's
[methodology/vl-spec-decode.md](https://github.com/Liquid4All/pipette-clients/blob/main/docs/methodology/vl-spec-decode.md).
Derived from the DSpark VL reproduction procedure (LFM2.5-VL-3B, 4-layer
drafter, `k = 8`, over MMSpec `test`).

## 1. One run, one row

A run decodes one corpus with one model and submits one row; its mode, and what
that mode requires of the row, are in
[benchmarks.md §2.8.3](../benchmarks.md#283-execution) and
[§2.8.4](../benchmarks.md#284-submission-fields). What a drafter buys is two
rows over the same corpus, one in each mode, compared as in §2.

## 2. Analysis outside the server

The speedups and the identity check need two rows, and the server scores one at
a time, so they are computed over the warehouse by whoever reads it. That makes
this section the pairing rule: nothing else writes it down.

### Forming a pair

A pair is one row with `runtime_speculative_enabled = true` (**S**) and one with
it `false` (**B**) that agree on everything the measurement depends on but the
drafter:

- the same `benchmark_id`, so the same corpus pin, subset, draft length,
  batch size and temperature;
- the same `observation_dataset_digest`, with neither row
  `observation_dataset_digest_matched IS FALSE`;
- the same `observation_vl_spec_decode_turns`, so a ratio of per-turn means is
  a ratio of sums;
- the same device (`client_id`) and runtime (`runtime_descriptor_sha256`);
- the same launch settings apart from speculation (`ctx_size`, load mode,
  `flash_attention`, …), which `runtime_flags` records;
- neither row `failed`, and one copy per `job_id`: an append-only re-score can
  leave several.

`runtime_flags` is a string, and the two rows' strings differ by
`speculative_enabled` at least, so comparing launch settings means parsing both.

Which model plays **B** for a given **S** is the analyst's choice and has to be
supplied: a regular model, or the same spec-decoding artifact run with
`speculative_enabled = false`. Nothing in a row names it
([design/vl-spec-decode.md § Open](../design/vl-spec-decode.md#open)), so the
join has to be told (`model_pairs` below). No speedup or identity lands in the
warehouse, so two consumers can pair differently.

### The four comparisons

| Comparison | Formula over the pair's metric rows |
|---|---|
| `decode_speedup` | `S.decode_throughput / B.decode_throughput` |
| `e2e_speedup` | `B.e2e_ms / S.e2e_ms` |
| `runtime_e2e_speedup` | `B.runtime_e2e_ms / S.runtime_e2e_ms`: the reference harness's end-to-end |
| `greedy_output_identical` | `S.observation_output_digest = B.observation_output_digest` |

`decode_speedup` is a ratio of pooled rates, so it is token-normalized and
survives rows that emit different counts. `e2e_speedup` is the wait a caller
actually sees shrink, prompt processing included, which speculation does nothing
for: it is always below `decode_speedup`, widest on short-answer datasets (GQA,
TextVQA). `runtime_e2e_speedup` compares within one engine only.
`greedy_output_identical` is a **correctness gate, not a performance number**:
greedy speculative decoding is meant to be output-identical, so a pair that
differs is read with suspicion before its speedups are. A rare late divergence
is expected, since a batched forward pass is not bitwise identical to a
single-token one. The gate is for `parameter_temperature = 0` rows only: a
sampled pair (`_t<temperature>` in the id) differs by construction, and its
digests say nothing about the drafter.

For a whole split, average the six subsets as `SUM / SUM`, not a mean of
ratios: a row's per-turn mean times `observation_vl_spec_decode_turns` recovers
its sum. The pooled tok/s needs two sums the row does not carry directly:
`predicted_ms` is `(runtime_e2e_ms − runtime_ttft) × turns`, and
`generated_tokens` is `decode_throughput × predicted_ms / 1000`.

### Aggregate throughput at a batch size

A batched row (`parameter_batch_size = N`) carries per-request sums: each
request's timings are what it saw while sharing the host with `N − 1` others, so
`decode_throughput` is a per-stream rate and the latency means are latency
under load. The host's aggregate output is implied by them rather than counted: the client is a
closed loop holding exactly `N` requests in flight, so by Little's law the
request rate is `N / mean e2e`, and

```
output_throughput  = N × generated_tokens / e2e_ms_sum × 1000   tokens/sec
request_throughput = N × turns            / e2e_ms_sum × 1000   requests/sec
```

with `e2e_ms_sum = e2e_ms × turns` and `generated_tokens` recovered as above.
At `N = 1` this is the reference harness's tok/s (`537` vs `258` on the H100
run). It is the steady-state figure: a wall clock over the same run comes out a
few percent longer, because the last samples finish with slots draining and
the client's own gaps are in it. That gap is transport and corpus shape, not
the host, which is why no wall clock is on the wire; compare against a
serving benchmark's wall-clock throughput with that in mind. A pair shares its
benchmark id, so it shares `N`, and `S.output_throughput / B.output_throughput`
is `e2e_speedup` scaled by the ratio of generated tokens: the number that says
whether the drafter still pays under load.

A sketch in DuckDB, with the model pairing supplied as a table:

```sql
WITH runs AS (
  SELECT job_id, benchmark_id, client_id, runtime_descriptor_sha256,
         model_descriptor_sha256 AS model,
         runtime_speculative_enabled AS spec,
         observation_dataset_digest AS corpus,
         observation_vl_spec_decode_turns AS turns,
         observation_output_digest AS output,
         any_value(value) FILTER (WHERE metric = 'decode_throughput') AS tps,
         any_value(value) FILTER (WHERE metric = 'e2e_ms') AS e2e,
         any_value(value) FILTER (WHERE metric = 'runtime_e2e_ms') AS rt_e2e
  FROM read_parquet('warehouse/results/**/*.parquet', union_by_name = true)
  WHERE benchmark_type = 'vl_spec_decode'
    AND failed IS NOT TRUE
    AND observation_dataset_digest_matched IS NOT FALSE
  GROUP BY ALL
)
SELECT s.benchmark_id, s.client_id,
       s.tps / b.tps       AS decode_speedup,
       b.e2e / s.e2e       AS e2e_speedup,
       b.rt_e2e / s.rt_e2e AS runtime_e2e_speedup,
       s.output = b.output AS greedy_output_identical
FROM runs s
JOIN model_pairs p ON p.speculative = s.model
JOIN runs b
  ON b.model = p.baseline
 AND (b.benchmark_id, b.client_id, b.runtime_descriptor_sha256, b.corpus, b.turns)
   = (s.benchmark_id, s.client_id, s.runtime_descriptor_sha256, s.corpus, s.turns)
WHERE s.spec AND NOT b.spec;
```

## 3. Sanity checks

- With llama.cpp and the DSpark drafter, the server log lines
  `DSpark markov head enabled` and `block=… taps=[…]` confirm the drafter
  loaded. An accept length near 1.0 usually means it did not.
- tok/s scales with memory bandwidth. Reference: about 140 tok/s with
  speculation on and 62 off, on an M4 Max.
