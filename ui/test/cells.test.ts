import { describe, expect, test } from "vitest";
import {
  Cells, flagEntries, flagKinds, grouped, idLabels, idsUnder, jobParts, knobLabels, sharedKnob, type GroupedRow,
} from "../src/lib/cells";
import { modelUri, runtimeUri } from "../src/lib/artifact-uri";
import { canonical } from "../src/lib/report";
import type {
  BenchmarkFlags, FailureSource, JobResult, JobState, LlamacppFlashAttention, Model, PlanCell, ResultRow, Runtime, RuntimeFlags,
} from "../src/lib/api-types";

const gguf = (name: string, quant = "Q4_K_M"): Model => ({
  type: "gguf_text", source: "huggingface", org: "acme", repo_name: `${name}-GGUF`, path: `${name}-${quant}.gguf`,
  revision: "a".repeat(40),
});
const vision = (repo: string): Model => ({
  type: "gguf_vision", source: "huggingface", org: "example-org", repo_name: `${repo}-GGUF`, model: `${repo}-F16.gguf`,
  mmproj: "mmproj-VLM-3B-F16.gguf",
});
const drafted = (repo: string): Model => ({
  type: "gguf_vision_speculative", source: "huggingface", org: "example-org", repo_name: `${repo}-GGUF`,
  model: `${repo}-F16.gguf`, mmproj: "mmproj-VLM-3B-F16.gguf",
});
const release = (flavor = "macos-arm64", version = "b100"): Runtime => ({
  type: "llamacpp_cli_stock_tools", source: "github_release", repository_url: "github.com/acme/llama",
  repository_version: version, flavor,
});
const archive = (url: string): Runtime =>
  ({ type: "llamacpp_cli_stock_tools", source: "remote_archive", url, flavor: "macos-arm64" });
const mlx = (version: string, contents = `mlx-lm==${version}\n`): Runtime => ({
  type: "mlx_macos_pipette", version, flavor: "macos-arm64", source: { type: "pip_requirements_text", contents },
});
const MAC = release();

/** llama.cpp's flags on a GGUF text model, as a plan's entry for `benchmark_type` writes them. */
const llama = (
  benchmark_type: "prefill_throughput" | "decode_throughput",
  knobs: { flash_attention?: LlamacppFlashAttention; threads?: number; mmap?: boolean },
): RuntimeFlags => ({ benchmark_type, runtime_type: "llamacpp_cli_stock_tools", model_type: "gguf_text", ...knobs });

function cell(over: Partial<PlanCell> = {}): PlanCell {
  return {
    benchmark: "decode_throughput_512_100", model: gguf("Tiny-1B"), runtime: MAC, benchmark_type: "decode_throughput",
    model_flags: null, runtime_flags: null, benchmark_flags: null,
    ...over,
  };
}

/** A cell for each id of `benchmarks`, alike but for the id, as the server lists one cell per spec. */
const each = (benchmarks: string[], over: Partial<PlanCell>) => benchmarks.map((benchmark) => cell({ ...over, benchmark }));

const prefill: Partial<PlanCell> = { benchmark_type: "prefill_throughput", benchmark: "prefill_throughput_512" };

/** What the table draws: each grouping cell as `values/span` (merged values joined by ` + `), then type and id count. */
function shape(drawn: GroupedRow[]) {
  // A descriptor spelt as its URI, which reads shorter here than its JSON.
  const text = (cell: { value: (string | [string, string][])[]; span: number } | null,
                uri: (d: never) => string = () => "") => {
    if (!cell) return null;
    const values = cell.value.map((v) =>
      (Array.isArray(v) ? v.map(([k, x]) => `${k}=${x}`).join(" ") || "-" : uri(JSON.parse(v) as never)));
    return `${values.join(" + ")}/${cell.span}`;
  };
  return drawn.map((r) => [text(r.model, modelUri), text(r.runtime, runtimeUri), text(r.flags.runtime), text(r.flags.model),
                           text(r.flags.benchmark), r.benchmark_type, r.benchmark_ids.length]);
}

describe("grouping the cells", () => {
  test("models whose rows below are alike share a model cell", () => {
    const [one, two] = [gguf("One-1B"), gguf("Two-1B")];
    const cells = [cell({ model: one }), cell({ model: one, ...prefill }), cell({ model: two }), cell({ model: two, ...prefill })];
    const mac = runtimeUri(MAC);
    expect(shape(grouped(cells))).toEqual([
      [`${modelUri(one)} + ${modelUri(two)}/2`, `${mac}/2`, "-/2", "-/2", "-/2", "decode_throughput", 1],
      [null, null, null, null, null, "prefill_throughput", 1],
    ]);
  });

  test("a model spans its runtimes, a runtime its flag sets, and the types under one set of knobs share it", () => {
    const [small, big, mid] = [gguf("Sweep-1B", "Q4_0"), gguf("Sweep-1B", "Q8_0"), gguf("Sweep-1B", "Q4_K_M")];
    const phone = release("android-arm64");
    const fast = { flash_attention: "on", threads: 8 } as const;
    const latency: RuntimeFlags = {
      benchmark_type: "end_to_end_latency", runtime_type: "llamacpp_cli_stock_tools", model_type: "gguf_text",
      ctx_size: 4096,
    };
    const at = (model: Model, runtime_flags: RuntimeFlags, runtime = MAC) => cell({
      model, runtime, benchmark_type: runtime_flags.benchmark_type, benchmark: `${runtime_flags.benchmark_type}_0`,
      runtime_flags,
    });
    // As the server lists them: variant, then benchmark, then model; only the smallest also runs on the phone.
    const cells = [
      ...[small, big, mid].map((m) => at(m, llama("prefill_throughput", fast))),
      ...[small, big, mid].map((m) => at(m, llama("decode_throughput", fast))),
      ...[small, big, mid].map((m) => at(m, latency)),
      at(small, llama("prefill_throughput", { threads: 4 }), phone),
      at(small, llama("decode_throughput", { threads: 4 }), phone),
    ];
    const mac = runtimeUri(MAC);
    const android = runtimeUri(phone);
    const drawn = grouped(cells);
    // Each flag set's entry names its benchmark type, and the cell draws its knobs alone.
    expect(shape(drawn)).toEqual([
      [`${modelUri(small)}/5`, `${mac}/3`, "flash_attention=on threads=8/2", "-/2", "-/2", "prefill_throughput", 1],
      [null, null, null, null, null, "decode_throughput", 1],
      [null, null, "ctx_size=4096/1", "-/1", "-/1", "end_to_end_latency", 1],
      [null, `${android}/2`, "threads=4/2", "-/2", "-/2", "prefill_throughput", 1],
      [null, null, null, null, null, "decode_throughput", 1],
      [`${modelUri(big)} + ${modelUri(mid)}/3`, `${mac}/3`, "flash_attention=on threads=8/2", "-/2", "-/2",
       "prefill_throughput", 1],
      [null, null, null, null, null, "decode_throughput", 1],
      [null, null, "ctx_size=4096/1", "-/1", "-/1", "end_to_end_latency", 1],
    ]);
    // A line marks where each group ends: the stronger one under a model, none under the table.
    expect(drawn.map((r) => r.end)).toEqual([null, null, null, null, "model", null, null, "table"]);
    expect([drawn[0]!.model!.end, drawn[3]!.runtime!.end, drawn[5]!.model!.end]).toEqual(["model", "model", "table"]);
    expect(drawn[0]!.flags.runtime!.end).toBeNull();
  });

  test("runtimes, and flag sets, whose rows below are alike share a cell", () => {
    const fast = release("macos-arm64", "b200");
    const cells = [
      cell(), cell({ runtime: fast }),
      cell({ runtime: release("android-arm64"), runtime_flags: llama("decode_throughput", { threads: 4 }) }),
      cell({ runtime: release("android-arm64"), runtime_flags: llama("decode_throughput", { threads: 8 }) }),
    ];
    expect(shape(grouped(cells))).toEqual([
      [`${modelUri(gguf("Tiny-1B"))}/2`, `${runtimeUri(MAC)} + ${runtimeUri(fast)}/1`,
       "-/1", "-/1", "-/1", "decode_throughput", 1],
      [null, `${runtimeUri(release("android-arm64"))}/1`, "threads=4 + threads=8/1", "-/1", "-/1",
       "decode_throughput", 1],
    ]);
  });

  test("values listed in another order still share a cell", () => {
    const [a, b] = [gguf("A-1B"), gguf("B-1B")];
    const [one, two] = [MAC, release("macos-arm64", "b200")];
    // `b`'s variant lists the runtimes, and its benchmarks, the other way round.
    const cells = [
      cell({ model: a, runtime: one }), cell({ model: a, runtime: one, ...prefill }),
      cell({ model: a, runtime: two }), cell({ model: a, runtime: two, ...prefill }),
      cell({ model: b, runtime: two, ...prefill }), cell({ model: b, runtime: two }),
      cell({ model: b, runtime: one, ...prefill }), cell({ model: b, runtime: one }),
    ];
    const drawn = grouped(cells);
    expect(drawn).toHaveLength(2);
    expect([drawn[0]!.model!.value.length, drawn[0]!.runtime!.value.length]).toEqual([2, 2]);
  });

  test("ids a variant edit lists in another order still share a cell, in the order the cells first list them", () => {
    const [a, b] = [gguf("A-1B"), gguf("B-1B")];
    const ids = ["decode_throughput_256_100", "decode_throughput_512_100", "decode_throughput_1024_100"];
    // `b`'s ids came back one variant at a time, the last first.
    const cells = [...each(ids, { model: a }), ...each([ids[2]!, ids[0]!, ids[1]!], { model: b })];
    const drawn = grouped(cells);
    expect(drawn.map((r) => [r.model?.value.length, r.benchmark_ids])).toEqual([[2, ids]]);
  });

  test("the vl spec-decode example: a row a model, its 18 ids under its own flags", () => {
    const vl = ["k8", "k7", "k9"].flatMap((k) => ["gqa", "textvqa", "charxiv", "mmmu_pro", "coco", "multi_turn"]
      .map((subset) => `vl_spec_decode_mmspec_test_${subset}_${k}`));
    const runtime = archive("https://files.example/llamacpp/llama-fork-bin-macos-arm64.tar.gz");
    const axes = { benchmark_type: "vl_spec_decode", runtime_type: "llamacpp_cli_stock_tools" } as const;
    const common = { ctx_size: 8192, flash_attention: "on" } as const;
    const bench = (model_type: "gguf_vision" | "gguf_vision_speculative"): BenchmarkFlags =>
      ({ ...axes, model_type, http_timeout_seconds: 600 });
    // As the server lists them: a cell per id, the plain arm's first.
    const cells = [
      ...each(vl, { model: vision("VLM-3B"), runtime, benchmark_type: "vl_spec_decode",
                    runtime_flags: { ...axes, model_type: "gguf_vision", ...common, speculative_enabled: false },
                    benchmark_flags: bench("gguf_vision") }),
      ...each(vl, { model: drafted("VLM-3B-Draft"), runtime, benchmark_type: "vl_spec_decode",
                    benchmark_flags: bench("gguf_vision_speculative"),
                    runtime_flags: { ...axes, model_type: "gguf_vision_speculative", ...common,
                                     envs: ["DRAFT_FUSED", "GGML_DRAFT_DELTA=16"], speculative_enabled: true } }),
    ];
    expect(cells).toHaveLength(36);
    const engine = runtimeUri(runtime);
    expect(shape(grouped(cells))).toEqual([
      [`${modelUri(vision("VLM-3B"))}/1`, `${engine}/1`, "ctx_size=8192 flash_attention=on speculative_enabled=false/1",
       "-/1", "http_timeout_seconds=600/1", "vl_spec_decode", 18],
      [`${modelUri(drafted("VLM-3B-Draft"))}/1`, `${engine}/1`,
       "ctx_size=8192 flash_attention=on envs=DRAFT_FUSED\nGGML_DRAFT_DELTA=16 speculative_enabled=true/1", "-/1",
       "http_timeout_seconds=600/1", "vl_spec_decode", 18],
    ]);
    expect(flagKinds(cells)).toEqual(["runtime", "benchmark"]);
    expect(distinct(cells)).toEqual([2, 1, 1, 18]);
  });

  test("the afm-mlx example: each model on its own runtime", () => {
    const afm: Model = { type: "apple_foundation_text" };
    const mlxModel: Model = { type: "mlx", source: "huggingface", org: "LiquidAI", repo_name: "LFM2.5-350M-MLX-4bit" };
    const cells = [
      cell({ model: afm, runtime: { type: "apple_foundation", private_thermal: true } }),
      cell({ model: mlxModel, runtime: mlx("0.31.3") }),
      cell({ model: mlxModel, runtime: mlx("0.31.3"), benchmark_type: "end_to_end_latency",
             benchmark: "end_to_end_latency_512_256" }),
    ];
    expect(shape(grouped(cells))).toEqual([
      ["apple-foundation-text:///1", "apple-foundation://private_thermal=true/1", "-/1", "-/1", "-/1",
       "decode_throughput", 1],
      ["mlx://repo=LiquidAI/LFM2.5-350M-MLX-4bit/2", "mlx-macos-pipette://version=0.31.3&flavor=macos-arm64/2", "-/2",
       "-/2", "-/2", "decode_throughput", 1],
      [null, null, null, null, null, "end_to_end_latency", 1],
    ]);
    expect(flagKinds(cells)).toEqual([]);
    expect(distinct(cells)).toEqual([2, 2, 2, 2]);
  });

  test("each level keeps first-seen order, and equal knobs share a cell whatever their key order", () => {
    const [late, early] = [gguf("Late-1B"), gguf("Early-1B")];
    const cells = [
      cell({ model: late, runtime_flags: llama("decode_throughput", { threads: 4, mmap: true }) }), cell({ model: early }),
      cell({ model: late, runtime_flags: llama("prefill_throughput", { mmap: true, threads: 4 }), ...prefill }),
    ];
    expect(shape(grouped(cells)).map((r) => [r[0], r[2], r[5]])).toEqual([
      [`${modelUri(late)}/2`, "threads=4 mmap=true/2", "decode_throughput"], [null, null, "prefill_throughput"],
      [`${modelUri(early)}/1`, "-/1", "decode_throughput"],
    ]);
  });

  test("an id the planner does not type is a row of its own, with no flags", () => {
    const cells = [cell(), cell({ benchmark_type: null, benchmark: "custom_probe" })];
    expect(grouped(cells).map((r) => [r.benchmark_type, r.benchmark_ids])).toEqual(
      [["decode_throughput", ["decode_throughput_512_100"]], [null, ["custom_probe"]]]);
    expect(flagKinds(cells)).toEqual([]);
  });

  test("the cells of one type and flags draw one row, their ids in document order", () => {
    const threads = llama("decode_throughput", { threads: 4 });
    const cells = [
      cell(), cell(prefill), cell({ benchmark: "decode_throughput_1024_100" }),
      cell({ benchmark: "decode_throughput_2048_100", runtime_flags: threads }),
      cell({ benchmark: "decode_throughput_256_100" }),
    ];
    expect(grouped(cells).map((r) => [r.benchmark_type, r.flags.runtime?.value ?? null, r.benchmark_ids])).toEqual([
      ["decode_throughput", [[]], ["decode_throughput_512_100", "decode_throughput_1024_100", "decode_throughput_256_100"]],
      ["prefill_throughput", null, ["prefill_throughput_512"]],
      ["decode_throughput", [[["threads", "4"]]], ["decode_throughput_2048_100"]],
    ]);
    expect(distinct(cells)).toEqual([1, 1, 2, 5]);
  });

  test("an entry setting no knob draws as no entry does, each id once", () => {
    const axesOnly = llama("decode_throughput", {});
    const cells = [cell({ runtime_flags: axesOnly }), cell(), cell({ benchmark: "decode_throughput_1024_100" })];
    expect(grouped(cells).map((r) => [r.flags.runtime?.value ?? null, r.benchmark_ids])).toEqual([
      [[[]], ["decode_throughput_512_100", "decode_throughput_1024_100"]],
    ]);
    expect(flagKinds(cells)).toEqual([]);
  });
});

describe("descriptors", () => {
  test("a model or runtime reads as its descriptor as stored, as a job row shows it", () => {
    const [drawn] = grouped([cell()]);
    expect(drawn!.model!.value).toEqual([canonical(gguf("Tiny-1B"))]);
    expect(drawn!.runtime!.value).toEqual([canonical(MAC)]);
  });

  test("distinct descriptors stay apart, first seen first, whatever a URI would leave out", () => {
    const builds = [mlx("0.31.3"), mlx("0.31.3", "mlx-lm==0.31.3\nnumpy<2\n")];
    expect(grouped(builds.map((runtime) => cell({ runtime })))[0]!.runtime!.value).toEqual(builds.map(canonical));
  });
});

describe("flags", () => {
  const vision = { benchmark_type: "vl_spec_decode", runtime_type: "llamacpp_cli_stock_tools",
                   model_type: "gguf_vision" } as const;

  test("a group reads as its knobs, key and text: a nested table dotted, a list of scalars a line each", () => {
    expect(flagEntries({
      ...vision, ctx_size: 8192, flash_attention: "on", mmap: true,
      raw: ["--spec-type", "draft-fused", "--spec-draft-n-min", "0"], envs: ["DRAFT_FUSED", "GGML_DRAFT_DELTA=16"],
    })).toEqual([
      ["ctx_size", "8192"], ["flash_attention", "on"], ["mmap", "true"],
      ["raw", "--spec-type draft-fused\n--spec-draft-n-min 0"], ["envs", "DRAFT_FUSED\nGGML_DRAFT_DELTA=16"],
    ]);
    expect(flagEntries({ ...vision, readiness: { max_wait_secs: 1800, skip_thermal: true } }))
      .toEqual([["readiness.max_wait_secs", "1800"], ["readiness.skip_thermal", "true"]]);
    expect(flagEntries({ ...vision, readiness: {} })).toEqual([["readiness", "{}"]]);
    expect(flagEntries({ ...vision, raw: ["model.gguf", "--cache-ram", "0", "-v", "-ub", "1024"] }))
      .toEqual([["raw", "model.gguf\n--cache-ram 0\n-v\n-ub 1024"]]);
  });

  test("a column counts its sets, and the knobs they all share read quieter, none where it has one set", () => {
    const base = { threads: 8, flash_attention: "on" } as const;
    const plain = cell({ runtime_flags: llama("decode_throughput", base) });
    const nocache = cell({ ...prefill, runtime_flags: { ...llama("prefill_throughput", base), raw: ["--cache-ram", "0"] } as RuntimeFlags });
    const two = new Cells({ document_state: "present", fleet_detail: "full", jobs: [job(plain), job(nocache)] });
    expect([...two.shared.runtime]).toEqual(["threads=8", "flash_attention=on"]);
    expect([two.sets.runtime, two.sets.benchmark]).toEqual([2, 1]);
    expect(sharedKnob(two.shared, "runtime", ["raw", "--cache-ram 0"])).toBe(false);
    const one = new Cells({ document_state: "present", fleet_detail: "full", jobs: [job(plain)] });
    expect([one.sets.runtime, one.shared.runtime.size]).toEqual([1, 0]);
  });

  test("a set draws a label a knob, an argv option each, those that tell rows apart before the shared", () => {
    const shared = { runtime: new Set(["threads=8", "flash_attention=on"]), model: new Set<string>(), benchmark: new Set<string>() };
    const entries: [string, string][] = [["threads", "8"], ["raw", "--spec-type draft-fused\n--spec-draft-n-min 0"],
                                         ["flash_attention", "on"], ["speculative_enabled", "true"]];
    expect(knobLabels(entries, "runtime", shared).map((l) => `${l.shared ? "~" : ""}${l.key}: ${l.value}`)).toEqual([
      "raw: --spec-type draft-fused", "raw: --spec-draft-n-min 0", "speculative_enabled: true",
      "~threads: 8", "~flash_attention: on",
    ]);
    expect(knobLabels(entries, "model", shared).every((l) => !l.shared)).toBe(true);
  });

  test("an unset group, or one of axis keys alone, has no entries and draws no column", () => {
    expect(flagEntries(null)).toEqual([]);
    expect(flagEntries({ benchmark_type: "eval", model_type: "gguf_text" })).toEqual([]);
    expect(flagKinds([cell({ benchmark_flags: { ...vision, http_timeout_seconds: 600 } }),
                      cell({ model_flags: { benchmark_type: "eval", model_type: "gguf_text" } })]))
      .toEqual(["benchmark"]);
    expect(flagKinds([cell()])).toEqual([]);
  });
});

describe("the totals", () => {
  test("counts distinct models, runtimes, types and benchmarks, an untyped id under no type", () => {
    expect(distinct([cell()])).toEqual([1, 1, 1, 1]);
    const untyped = cell({ benchmark_type: null, benchmark: "probe" });
    expect(distinct([cell(), cell({ model: gguf("Other-2B") }), untyped])).toEqual([2, 1, 1, 2]);
  });

  test("sums every row's jobs and results", () => {
    const cells = [cell({ model: gguf("One-1B") }), cell({ model: gguf("Two-1B"), ...prefill })];
    const jobs = [job(cells[0]!, "finished", scored()), job(cells[0]!, "running"), job(cells[1]!, "failed", scored("expired"))];
    const read = new Cells({ document_state: "present", fleet_detail: "full", results_read: "ok", jobs });
    expect(read.rows?.map((row) => [row.jobs.total, row.results?.measured, row.jobs.failed ?? 0])).toEqual([[2, 1, 0], [1, 0, 1]]);
    expect([read.totals.jobs, read.totals.results]).toEqual([
      { total: 3, finished: 1, running: 1, failed: 1 },
      { measured: 1, causes: [["expired", 1]], pending: 0 },
    ]);
  });

  test("cells with no requests draw no table, and count the jobs they cannot place", () => {
    const none = new Cells({ document_state: "none", fleet_detail: "full", jobs: [job(null, "cancelled")] });
    expect([none.rows, none.unplaced]).toEqual([null, 1]);
    const read = new Cells({
      document_state: "present", fleet_detail: "full",
      jobs: [job(cell({ runtime_flags: llama("decode_throughput", { threads: 4 }) })), job(null)],
    });
    expect([read.rows?.length, read.kinds, read.totals.jobs, read.unplaced])
      .toEqual([1, ["runtime"], { total: 1, finished: 1 }, 1]);
  });
});

/** A job planned as `planned`, standing in `state`, with `result` where its results were read. */
const job = (planned: PlanCell | null, state: JobState = "finished", result?: JobResult) =>
  ({ planned, state, ...(result ? { result } : {}) });

/** A scored run of one row: measured at `value`, or failed for `cause`. */
function scored(cause: FailureSource | null = null, value: number | null = 12.5): JobResult {
  const row = {
    job_id: "job-1", metric: "decode_throughput", unit: "tok/s", value: cause ? null : value, failed: !!cause,
    failure_source: cause,
  } as unknown as ResultRow;
  return { status: "scored", rows: [row], rescored_from: null, earlier_runs: [], recalculable: false };
}

/** The header's distinct counts: models, runtimes, benchmark types and ids. */
function distinct(cells: PlanCell[]): number[] {
  const { models, runtimes, types, benchmarks } =
    new Cells({ document_state: "present", fleet_detail: "full", jobs: cells.map((c) => job(c)) }).totals;
  return [models, runtimes, types, benchmarks];
}

describe("a row's jobs", () => {
  test("a row counts the jobs whose request it draws, merged values' too, and no other row's", () => {
    const [one, two] = [gguf("One-1B"), gguf("Two-1B")];
    const cells = [cell({ model: one }), cell({ model: one, ...prefill }), cell({ model: two }), cell({ model: two, ...prefill })];
    const [oneDecode, onePrefill, twoDecode, twoPrefill] = cells;
    const jobs = [job(oneDecode!), job(oneDecode!, "failed"), job(twoDecode!, "running"), job(onePrefill!),
                   job(twoPrefill!, "available"), job(twoPrefill!, "cancelled")];
    expect(grouped(cells, jobs).map((row) => row.jobs)).toEqual([
      { total: 3, finished: 1, failed: 1, running: 1 },
      { total: 3, finished: 1, available: 1, cancelled: 1 },
    ]);
  });

  test("ids of one row each bring their jobs, and cells read without jobs count none", () => {
    const cells = each(["decode_throughput_512_100", "decode_throughput_1024_100"], {});
    const [short, long] = cells;
    expect(grouped(cells, [job(short!), job(long!), job(long!, "failed")]).map((row) => row.jobs))
      .toEqual([{ total: 3, finished: 2, failed: 1 }]);
    expect(grouped(cells).map((row) => row.jobs)).toEqual([{ total: 0 }]);
  });

  test("reads as done of all, a failed job among them, then each other state that has any, in the order the bar fills", () => {
    const line = (counts: Parameters<typeof jobParts>[0]) =>
      jobParts(counts).map(({ state, n, text }) => `${state}:${n}:${text}`).join(" | ");
    expect(line({ total: 4, finished: 1, failed: 1, available: 2 }))
      .toBe("finished:1:2/4 done | failed:1:1 failed | available:2:2 queued");
    expect(line({ total: 3, finished: 1, unknown: 1, running: 1 }))
      .toBe("finished:1:1/3 done | running:1:1 running | unknown:1:1 unknown");
    expect(line({ total: 2, cancelled: 2 })).toBe("finished:0:0/2 done | cancelled:2:2 cancelled");
    expect(line({ total: 0 })).toBe("finished:0:0/0 done");
  });

});

describe("a row's results", () => {
  test("count measured jobs, every failure by cause whatever failed it, and finished ones with no rows yet", () => {
    const jobs = [job(cell(), "finished", scored()), job(cell(), "failed", scored("expired")),
                  job(cell(), "failed", scored("lease_lost")), job(cell(), "failed", scored("client")),
                  job(cell(), "finished", { status: "not_run" }), job(cell(), "available", { status: "not_run" }),
                  job(cell(), "finished", scored(null, null))];
    expect(grouped([cell()], jobs, "full").map((row) => row.results)).toEqual([{
      measured: 1, causes: [["expired", 1], ["retired for losing its leases", 1], ["reported by its client", 1]],
      pending: 1,
    }]);
  });

  test("count a cause only for a failed job, not for one the queue still holds with an earlier run's rows", () => {
    const jobs = [job(cell(), "failed", scored("client")), job(cell(), "running", scored("lease_lost")),
                  job(cell(), "available", scored("expired"))];
    expect(grouped([cell()], jobs, "full").map((row) => [row.jobs.failed, row.results?.causes]))
      .toEqual([[1, [["reported by its client", 1]]]]);
  });

  test("are not counted where the results were not read", () => {
    expect(grouped([cell()], [job(cell(), "finished", scored())]).map((row) => row.results)).toEqual([null]);
    const unread = new Cells({ document_state: "present", fleet_detail: "full", results_read: "timed_out", jobs: [job(cell())] });
    expect([unread.rows?.[0]?.results, unread.totals.results]).toEqual([null, null]);
  });

});

describe("a row's ids", () => {
  test("drop their type's prefix where all share it, else read in full", () => {
    expect(idsUnder("prefill_throughput", ["prefill_throughput_256", "prefill_throughput_512"])).toEqual(["256", "512"]);
    expect(idsUnder("decode_throughput", ["decode_throughput_512_100", "custom_decode"]))
      .toEqual(["decode_throughput_512_100", "custom_decode"]);
    expect(idsUnder("eval", ["eval_"])).toEqual(["eval_"]);
    expect(idsUnder(null, ["probe"])).toEqual(["probe"]);
  });

  test("a row's ids drop, beside its type's, the longest `_` prefix every one of several shares", () => {
    const vl = ["gqa_k8", "textvqa_k8", "gqa_k7"].map((id) => `vl_spec_decode_mmspec_test_${id}`);
    expect(idLabels("vl_spec_decode", vl)).toEqual({ shared: "mmspec_test_", labels: ["gqa_k8", "textvqa_k8", "gqa_k7"] });
    expect(idLabels("eval", ["ifbench_2026.06.1", "ifstruct_release_v1_0", "gpqa_diamond_2026.06.1"]))
      .toEqual({ shared: "", labels: ["ifbench_2026.06.1", "ifstruct_release_v1_0", "gpqa_diamond_2026.06.1"] });
    expect(idLabels("decode_throughput", ["decode_throughput_512_100", "decode_throughput_1024_100"]))
      .toEqual({ shared: "", labels: ["512_100", "1024_100"] });
    expect(idLabels(null, ["a_b", "a_b_c"])).toEqual({ shared: "a_", labels: ["b", "b_c"] });
    // A shared prefix that is a whole id is kept, so no label reads empty.
    expect(idLabels(null, ["x_", "x_y"])).toEqual({ shared: "", labels: ["x_", "x_y"] });
    expect(idLabels(null, ["x_one_1", "x_one_2", "x_two_1"])).toEqual({ shared: "x_", labels: ["one_1", "one_2", "two_1"] });
    expect(idLabels("prefill_throughput", ["prefill_throughput_256"])).toEqual({ shared: "", labels: ["256"] });
  });
});

describe("the catalog names of the planned runtimes and models", () => {
  const digest = (char: string) => char.repeat(64);
  const named = (display: string) => ({ display, name: display.split("::")[0]!, synonyms: [] });
  const q4 = gguf("AcmeLM-3B", "Q4_0");
  const q8 = gguf("AcmeLM-3B", "Q8_0");
  const cuda = release("linux-cuda-12-8");
  const cpu = release("linux-x64");
  test.each([
    {
      kind: "runtimes", key: "planned_runtime_sha256", names: (cells: Cells) => cells.runtimeNames,
      cells: [cell({ runtime: cuda }), cell({ runtime: cpu }), cell({ runtime: MAC })],
      texts: [canonical(cuda), canonical(cpu)],
      displays: ["llamacpp-v0.7.0::linux-cuda-12-8", "llamacpp-v0.7.0::linux-x64"],
      grouped: (row: NonNullable<Cells["rows"]>[number]) => row.runtime?.value ?? [],
    },
    {
      kind: "models", key: "planned_model_sha256", names: (cells: Cells) => cells.modelNames,
      cells: [cell({ model: q4 }), cell({ model: q8 }), cell({ model: gguf("Other-1B") })],
      texts: [canonical(q4), canonical(q8)],
      displays: ["AcmeLM-3B::gguf::q4_0", "AcmeLM-3B::gguf::q8_0"],
      grouped: (row: NonNullable<Cells["rows"]>[number]) => row.model?.value ?? [],
    },
  ])("name each planned $kind by its digest, as stored, and leave one with no name out", (
    { key, names, cells: planned, texts, displays, grouped },
  ) => {
    const cells = new Cells({
      document_state: "present", fleet_detail: "full",
      jobs: planned.map((spec, at) => ({ ...job(spec), [key]: digest("abc"[at]!) })),
      descriptor_names: { [digest("a")]: named(displays[0]!), [digest("b")]: named(displays[1]!) },
    });
    // Two members of one entry have different names.
    expect([...names(cells)].map(([text, name]) => [text, name.display]))
      .toEqual([[texts[0], displays[0]], [texts[1], displays[1]]]);
    // The progress table looks a descriptor up by the text it groups by.
    expect(cells.rows?.flatMap(grouped).map((text) => names(cells).get(text)?.display))
      .toEqual(expect.arrayContaining([...displays, undefined]));
  });

  test.each([
    {
      kind: "runtime", key: "planned_runtime_sha256", digests: (cells: Cells) => cells.runtimeDigests,
      cells: [cell({ runtime: cuda }), cell({ runtime: cpu })], text: canonical(cuda),
      grouped: (row: NonNullable<Cells["rows"]>[number]) => row.runtime?.value ?? [],
    },
    {
      kind: "model", key: "planned_model_sha256", digests: (cells: Cells) => cells.modelDigests,
      cells: [cell({ model: q4 }), cell({ model: q8 })], text: canonical(q4),
      grouped: (row: NonNullable<Cells["rows"]>[number]) => row.model?.value ?? [],
    },
  ])("key each planned $kind's digest by the $kind as stored, and leave a job without one out", (
    { key, digests, cells: planned, text, grouped },
  ) => {
    const cells = new Cells({
      document_state: "present", fleet_detail: "full",
      jobs: [{ ...job(planned[0]!), [key]: digest("a") }, { ...job(planned[1]!), [key]: null }],
    });
    // The progress table groups by the same text, so a cell finds its descriptor's digest.
    expect([...digests(cells)]).toEqual([[text, digest("a")]]);
    expect(cells.rows?.flatMap(grouped).map((shown) => digests(cells).get(shown)))
      .toEqual(expect.arrayContaining([digest("a"), undefined]));
  });
});
