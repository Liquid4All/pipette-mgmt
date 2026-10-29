import { describe, expect, test } from "vitest";
import {
  COLUMNS, byJob, cellText, cellsLink, changeOf, columnsOf, listedText, deviceTypeNames, deviceTypes, drawnFilters, drawnLink, earlierRows, filtering, issuesOf, jobChoices, jobFilters, keeps, linesCsv,
  columnNameOf, linesOf, modelNameOf, outcomeOf, runtimeNameOf, sortLines, sortOf, spreadOf, swapped, tierOf, toggled, SWAP_COLUMNS, type Line,
} from "../src/lib/jobs";
import { bytesText } from "../src/lib/plans";
import { canonical } from "../src/lib/report";
import { choiceMatches, drawnColumns, picksOf } from "../src/lib/table";
import type { PlanJobWithResult } from "../src/lib/api";
import type { JobResult, JobState, Model, PlanCell, ResultRow, Runtime } from "../src/lib/api-types";

const MODEL: Model = {
  type: "gguf_text", source: "huggingface", org: "LiquidAI", repo_name: "LFM2.5-1.2B-GGUF", path: "LFM2.5-1.2B-Q4_K_M.gguf",
};
const DESCRIPTOR: Model = { org: "LiquidAI", path: "LFM2.5-1.2B-Q4_K_M.gguf", repo_name: "LFM2.5-1.2B-GGUF",
                     source: "huggingface", type: "gguf_text" };
const FOUNDATION = { type: "apple_foundation" } as Runtime;
const PLANNED: PlanCell = {
  benchmark: "decode_throughput_512_100", benchmark_type: "decode_throughput", model: MODEL,
  runtime: { type: "apple_foundation", private_thermal: true }, model_flags: null, runtime_flags: null,
  benchmark_flags: null,
};

/** A result row: `metric` measured at `value`; `over` for the rest. */
const row = (metric: string, value: number | null, over: Partial<ResultRow> = {}): ResultRow => ({
  job_id: "job", benchmark_id: PLANNED.benchmark, benchmark_type: "decode_throughput", metric, value,
  value_stddev: null, unit: "tokens/sec", failed: false, failed_reason: null, failure_source: null, model_descriptor: DESCRIPTOR,
  model_params_total_millions: 1000, model_params_active_millions: 1000,
  runtime_descriptor: null, runtime_descriptor_sha256: null, model_descriptor_sha256: null, model_flags: null, runtime_flags: null, benchmark_flags: null, requested_model_flags: null,
  requested_runtime_flags: null, requested_benchmark_flags: null, model_vault_descriptor: null, eval_metadata: null,
  client_extras: null, parameters: {}, observations: {}, client_id: null,
  device_name: null, device: {}, client_version: null, submitted_at: null, scored_at: null, device_time_ms: null, rescored_from: null, run_submitted_at: null,
  score_runtime_version: null, ...over,
});
const scored = (...rows: ResultRow[]): JobResult => ({ status: "scored", rows, rescored_from: null, earlier_runs: [], recalculable: false });
/** A job queued until `expires_at`, held by `client` where there is one. */
const queued = (client: string | null, expires_at: string | null = null) =>
  ({ expires_at, lease: client ? { expires_at: "2999-01-01T00:00:00Z", client_id: client } : null });

function job(job_id: string, state: JobState, over: Partial<PlanJobWithResult> = {}) {
  return { job_id, state, result: { status: "not_run" } as JobResult, planned: PLANNED, ...over };
}

const NONE = jobFilters(new URLSearchParams());
const ctx = { names: { client_1: "bench-b", client_2: "bench-a" }, types: {} };

describe("the cells' lines", () => {
  const jobs = [
    job("job-a1", "finished", { result: scored(row("decode", 40, { value_stddev: 0.2, client_id: "client_2" }),
                                               row("ttft", 120, { client_id: "client_2" })) }),
    job("job-b2", "failed", { result: scored(row("decode", null, { failed: true, failure_source: "client",
                                                                   client_id: "client_1" })) }),
    job("job-c3", "available", { planned: { ...PLANNED, benchmark: "prefill_throughput_512" } }),
    job("job-d4", "finished", { result: scored(row("decode", 10, {
      value_stddev: 0.8, model_descriptor: { type: "unknown", value: { org: "other", repo_name: "Big-9B" } },
    })), planned: null }),
  ];
  const lines = linesOf(jobs);
  const kept = (query: string) =>
    lines.filter((line) => keeps(line, jobFilters(new URLSearchParams(query)), ctx))
      .map((line) => `${line.job.job_id}${line.row ? `:${line.row.metric}` : ""}`);

  test("are a job's result rows, or the job alone where it has none, grouped back by job", () => {
    expect(lines.map((line) => [line.job.job_id, line.row?.metric ?? null])).toEqual([
      ["job-a1", "decode"], ["job-a1", "ttft"], ["job-b2", "decode"], ["job-c3", null], ["job-d4", "decode"],
    ]);
    expect(byJob(lines).map((group) => [group.job.job_id, group.lines.length]))
      .toEqual([["job-a1", 2], ["job-b2", 1], ["job-c3", 1], ["job-d4", 1]]);
    expect(lines.map(outcomeOf)).toEqual(["measured", "measured", "failed", "none", "measured"]);
  });

  test("filters read from the address, each empty where unset, a choice per repeated key", () => {
    expect(jobFilters(new URLSearchParams("q=%20a1%20&state=failed&state=running&state=failed&state=&spread=fine")))
      .toEqual({ ...NONE, q: "a1", state: ["failed", "running"], spread: ["fine"] });
    expect([filtering(NONE), filtering({ ...NONE, issue: ["swap"] }), filtering({ ...NONE, q: "x" })])
      .toEqual([false, true, true]);
  });

  test("the search keeps a job whose id, model, runtime or runtime flags hold its text, case aside", () => {
    expect(kept("q=lfm")).toEqual(["job-a1:decode", "job-a1:ttft", "job-b2:decode", "job-c3"]);
    expect(kept("q=big-9b")).toEqual(["job-d4:decode"]);
    expect(kept(`q=${encodeURIComponent(canonical(MODEL))}`)).toHaveLength(4);
    expect(kept("q=private_thermal")).toHaveLength(4);
    expect(kept("q=B2")).toEqual(["job-b2:decode"]);
    // A job filed before plan cells were kept still matches its reported descriptor.
    const unplanned = linesOf([job("job-e5", "finished", { planned: null, result: scored(row("decode", 5)) })]);
    expect(unplanned.filter((line) => keeps(line, jobFilters(new URLSearchParams({ q: canonical(MODEL) })), ctx)))
      .toHaveLength(1);
    // Runtime flags as planned, as asked for and as the run reported them.
    const flagged = linesOf([
      job("job-f1", "available", { planned: { ...PLANNED, runtime_flags: { threads: 8 } as never } }),
      job("job-f2", "finished", { result: scored(row("decode", 5, { requested_runtime_flags: { n_gpu_layers: 99 } as never })) }),
      job("job-f3", "finished", { result: scored(row("decode", 5, { runtime_flags: { ctx_size: 4096 } as never })) }),
    ]);
    const search = (q: string) =>
      flagged.filter((line) => keeps(line, { ...NONE, q }, ctx)).map((line) => line.job.job_id);
    expect(["threads", "n_gpu_layers", "ctx_size", "nowhere"].map(search))
      .toEqual([["job-f1"], ["job-f2"], ["job-f3"], []]);
  });

  test("a vault model is found, and listed, by its resource's name", () => {
    const vault = { type: "gguf_text", source: "vault_url", url: "/vault/resources/vault_1/content",
                    principal_id: "org_acme" } as Model;
    const named = { ...ctx, names: { vault_1: "acme ACMELM q4" } };
    const stored = linesOf([
      job("job-v1", "finished", { planned: { ...PLANNED, model: vault },
                                  result: scored(row("decode", 5, { model_descriptor: vault })) }),
      job("job-h2", "finished", { result: scored(row("decode", 6)) }),
    ]);
    const found = (read: { names: Record<string, string>; types: Record<string, string> }) =>
      stored.filter((line) => keeps(line, { ...NONE, q: "acmelm" }, read)).map((line) => line.job.job_id);
    expect(found(named)).toEqual(["job-v1"]);
    // Unnamed, as for a reader outside the vault's reach, only the descriptor's own text matches.
    expect(found(ctx)).toEqual([]);
    expect(jobChoices(stored, named, {}).model_descriptor.map((c) => [c.label, c.value])).toEqual([
      [canonical(DESCRIPTOR), canonical(DESCRIPTOR)], [`acme ACMELM q4 ${canonical(vault)}`, canonical(vault)],
    ]);
  });

  test("a vault model that carries its label is found by it, with no name from the read", () => {
    // The device of an older build reports the model without the label, so only the planned one carries it.
    const reported = { type: "gguf_text", source: "vault_url", url: "/vault/resources/vault_1/content",
                       principal_id: "org_acme" } as Model;
    const vault = { ...reported, custom_label: "acme ACMELM q4" } as Model;
    const stored = linesOf([
      job("job-v1", "finished", { planned: { ...PLANNED, model: vault },
                                  result: scored(row("decode", 5, { model_descriptor: reported })) }),
      job("job-h2", "finished", { result: scored(row("decode", 6)) }),
    ]);
    expect(stored.filter((line) => keeps(line, { ...NONE, q: "acmelm" }, ctx)).map((line) => line.job.job_id))
      .toEqual(["job-v1"]);
  });

  test("a select keeps any of its choices, a row's select the rows, and every filter narrows", () => {
    expect(kept("state=failed&state=available")).toEqual(["job-b2:decode", "job-c3"]);
    expect(kept("metric=ttft")).toEqual(["job-a1:ttft"]);
    expect(kept("result=none")).toEqual(["job-c3"]);
    expect(kept("device=client_1")).toEqual(["job-b2:decode"]);
    expect(kept("spread=significant")).toEqual(["job-d4:decode"]);
    expect(kept("benchmark_type=prefill_throughput")).toEqual([]);
    expect(kept("benchmark_type=decode_throughput")).toHaveLength(5);
    expect(kept("issue=failed&issue=spread")).toEqual(["job-b2:decode", "job-d4:decode"]);
    expect(kept("issue=spread&state=failed")).toEqual([]);
  });

  test("offer what the plan has, counting jobs on a job's select and rows on a row's", () => {
    const choices = jobChoices(lines, ctx, {});
    expect(choices.state.map((c) => [c.label, c.count])).toEqual([["finished", 2], ["failed", 1], ["queued", 1]]);
    expect(choices.metric.map((c) => [c.value, c.count])).toEqual([["decode", 3], ["ttft", 1]]);
    expect(choices.spread.map((c) => [c.label, c.count])).toEqual([["≤1%", 1], ["5–10%", 1]]);
    expect(choices.issue.map((c) => [c.value, c.count])).toEqual([["failed", 1], ["spread", 1]]);
    expect(choices.device.map((c) => [c.value, c.label])).toEqual([["client_2", "bench-a"], ["client_1", "bench-b"]]);
    expect(choices.benchmark_type.map((c) => [c.value, c.count])).toEqual([["decode_throughput", 4]]);
    // A job not run yet, or a failed row that reports none, is chosen by the model and runtime it was planned with.
    expect(choices.model_descriptor.map((c) => [c.label, c.count]))
      .toEqual([[canonical(DESCRIPTOR), 3], [canonical({ org: "other", repo_name: "Big-9B" }), 1]]);
    expect(kept(`runtime_descriptor=${encodeURIComponent(canonical(PLANNED.runtime))}`))
      .toEqual(["job-b2:decode", "job-c3"]);
  });

  test("offer a choice the address holds that no line has under its label, so it can be unchecked", () => {
    const choices = jobChoices(lines.filter((line) => line.job.state !== "available"), ctx,
                               { state: ["available", "bogus"], result: ["none"] });
    expect(choices.state.map((c) => [c.value, c.label, c.count]))
      .toEqual([["finished", "finished", 2], ["failed", "failed", 1], ["available", "queued", 0], ["bogus", "bogus", 0]]);
    expect(choices.result.at(-1)).toEqual({ value: "none", label: "no result", count: 0 });
  });

  test("offer a select's choices by label, numbers by size, the fixed ones in their own order", () => {
    const at = (id: string, benchmark: string, metric: string, state: JobState = "finished") =>
      job(id, state, { planned: { ...PLANNED, benchmark }, result: scored(row(metric, 1)) });
    const choices = jobChoices(linesOf([at("j1", "decode_1024", "ttft", "failed"), at("j2", "decode_256", "decode"),
                                        at("j3", "decode_4096", "e2e_ms")]), ctx, {});
    expect(choices.benchmark.map((c) => c.value)).toEqual(["decode_256", "decode_1024", "decode_4096"]);
    expect(choices.metric.map((c) => c.value)).toEqual(["decode", "e2e_ms", "ttft"]);
    expect(choices.state.map((c) => c.value)).toEqual(["finished", "failed"]);
  });

  test("the device select is by type: devices of one type are one choice, a job without a row takes its device's", () => {
    // A finished job's device ran its rows; a running one's holds its lease.
    const mac = (id: string, state: JobState, client: string, value?: number) => job(id, state,
      value === undefined ? { queue: queued(client) }
        : { result: scored(row("decode", value, { device_name: "MacBook Pro", client_id: client })) });
    const fleet = linesOf([mac("job-1", "finished", "mbp-1", 40), mac("job-2", "finished", "mbp-2", 41),
                           mac("job-3", "running", "mbp-1"), job("job-4", "running", { queue: queued("box-1") })]);
    const typed = { ...ctx, names: { "mbp-1": "a@mbp", "mbp-2": "b@mbp", "box-1": "c@box" }, types: deviceTypes(fleet) };
    expect(typed.types).toEqual({ "mbp-1": "MacBook Pro", "mbp-2": "MacBook Pro" });
    expect(jobChoices(fleet, typed, {}).device.map((c) => [c.value, c.label, c.count]))
      .toEqual([["box-1", "c@box", 1], ["MacBook Pro", "MacBook Pro", 3]]);
    expect(fleet.filter((line) => keeps(line, { ...NONE, device: ["MacBook Pro"] }, typed)).map((line) => line.job.job_id))
      .toEqual(["job-1", "job-2", "job-3"]);
  });

  test("devices alike in make are one type whatever each is named, and apart where their make differs", () => {
    const phone = (id: string, client: string, name: string, device: ResultRow["device"]) => job(id, "finished",
      { result: scored(row("decode", 40, { device_name: name, client_id: client, device })) });
    const a19 = { os_name: "iOS", os_version: "27.0.1", chip_model: "Apple A19 Pro", ram_bytes: 12291735552 };
    const fleet = linesOf([
      // A run's state, such as its battery, is no part of the type.
      phone("job-1", "p-1", "iPhone 17 Pro", a19), phone("job-2", "p-2", "bos-17pro-1", { ...a19, battery_level: 40 }),
      phone("job-3", "p-3", "bos-17pro-1", { ...a19, os_version: "27.1" }),
      job("job-4", "running", { queue: queued("p-3") }),
    ]);
    const typed = { ...ctx, names: {}, types: deviceTypes(fleet), typeNames: deviceTypeNames(fleet) };
    const [now, later] = ["27.0.1", "27.1"].map((os) => `Apple A19 Pro · 11.4 GB RAM · iOS ${os}`);
    expect(jobChoices(fleet, typed, {}).device.map((c) => [c.value, c.label, c.count])).toEqual([[now, now, 2], [later, later, 2]]);
    // One type alone is named by its chip and RAM, and chosen by its whole make.
    const lone = fleet.slice(0, 2);
    expect(jobChoices(lone, { ...typed, typeNames: deviceTypeNames(lone) }, {}).device.map((c) => [c.value, c.label]))
      .toEqual([[now, "Apple A19 Pro · 11.4 GB RAM"]]);
    expect(fleet.filter((line) => keeps(line, { ...NONE, device: [later!] }, typed)).map((line) => line.job.job_id))
      .toEqual(["job-3", "job-4"]);
  });

  test("a chart's link to Jobs carries the filters that keep the rows it draws, a narrowing one only", () => {
    const at = (id: string, model: string, device_name: string, benchmark_id = "decode_256") =>
      row("decode", 40, { job_id: id, benchmark_id, device_name, model_descriptor: { ...DESCRIPTOR, path: model } as Model });
    const rows = [at("job-1", "a.gguf", "Phone"), at("job-2", "b.gguf", "Phone"), at("job-3", "a.gguf", "Box"),
                  at("job-4", "a.gguf", "Phone", "decode_512"), row("ttft", 5, { job_id: "job-1", benchmark_id: "decode_256" })];
    const text = (path: string) => canonical({ ...DESCRIPTOR, path });
    // Every row of the benchmark and metric: nothing but those and the outcome to say.
    expect(drawnFilters(new Set(rows.slice(0, 3)), rows)).toEqual({
      benchmark: ["decode_256"], metric: ["decode"], result: ["measured"], device: [], model_descriptor: [], runtime_descriptor: [],
    });
    // One device type's, one model's.
    expect(drawnFilters(new Set([rows[0]!]), rows)).toMatchObject({ device: ["Phone"], model_descriptor: [text("a.gguf")] });
    const link = drawnLink({ plan_id: "plan-1", campaign_id: null }, new Set([rows[0]!]), rows);
    expect(jobFilters(new URL(link, "http://x.test").searchParams)).toMatchObject({
      benchmark: ["decode_256"], metric: ["decode"], result: ["measured"], device: ["Phone"], model_descriptor: [text("a.gguf")],
    });
    // Models that would run the address too long are left out: the link then lists more than the chart draws.
    const many = Array.from({ length: 40 }, (_, k) => at(`job-m${k}`, `${"m".repeat(200)}-${k}.gguf`, "Phone"));
    const long = drawnLink({ plan_id: "plan-1", campaign_id: null }, new Set(many.slice(1)), many);
    expect([long.length < 6000, long.includes("model_descriptor")]).toEqual([true, false]);
  });

  test("a choice toggles in and out; links start from the first job, keeping the page size", () => {
    const one = toggled(NONE, "state", "failed", true);
    expect(toggled(toggled(one, "state", "running", true), "state", "failed", false).state).toEqual(["running"]);
    expect(cellsLink({ plan_id: "plan-1", campaign_id: "cmp_1" }, { ...NONE, state: ["failed", "running"], q: "lfm" },
                     "20"))
      .toBe("/app/campaigns/cmp_1/plans/plan-1/cells?q=lfm&state=failed&state=running&limit=20");
    expect(cellsLink({ plan_id: "plan-1", campaign_id: null }, NONE, "", 100)).toBe("/app/plans/plan-1/cells?offset=100");
  });
});

describe("the cells' sort", () => {
  const jobs = [
    job("job-b10", "finished", { result: scored(row("decode", 40), row("ttft", 120)) }),
    job("job-b9", "available"),
    job("job-a1", "finished", { result: scored(row("decode", 5), row("ttft", 300)) }),
  ];
  const lines = linesOf(jobs);
  const order = (sort: string) =>
    sortLines(lines, sort, ctx).map((line) => `${line.job.job_id}${line.row ? `:${line.row.metric}` : ""}`);

  test("is read from the address only for a column the table has", () => {
    expect([jobFilters(new URLSearchParams("sort=-value")).sort, jobFilters(new URLSearchParams("sort=nope")).sort])
      .toEqual(["-value", ""]);
    expect(["sort=swap", "sort=-swap", "sort=nope", ""].map((query) => sortOf(new URLSearchParams(query))))
      .toEqual(["swap", "-swap", "", ""]);
    expect(cellsLink({ plan_id: "plan-1", campaign_id: null }, { ...NONE, sort: "-value" }, ""))
      .toBe("/app/plans/plan-1/cells?sort=-value");
  });

  test("orders numbers by size and text with its numbers by size, none last either way, ties as they came", () => {
    expect(order("")).toEqual(["job-b10:decode", "job-b10:ttft", "job-b9", "job-a1:decode", "job-a1:ttft"]);
    expect(order("job")).toEqual(["job-a1:decode", "job-a1:ttft", "job-b9", "job-b10:decode", "job-b10:ttft"]);
    expect(order("-value")).toEqual(["job-a1:ttft", "job-b10:ttft", "job-b10:decode", "job-a1:decode", "job-b9"]);
    expect(order("value")).toEqual(["job-a1:decode", "job-b10:decode", "job-b10:ttft", "job-a1:ttft", "job-b9"]);
  });

  test("parts a job's lines into groups when a row's column orders them", () => {
    expect(byJob(sortLines(lines, "-value", ctx)).map((group) => [group.job.job_id, group.lines.length]))
      .toEqual([["job-a1", 1], ["job-b10", 2], ["job-a1", 1], ["job-b9", 1]]);
  });
});

describe("a line's spread and issues", () => {
  const line = (r: ResultRow | null, state: JobState = "finished"): Line => ({ job: job("job", state), row: r });

  test("the spread is relative to the value, by pipette-datasheet's tiers, none on a ratio", () => {
    expect([1, 1.01, 3, 5, 10, 10.1].map((s) => tierOf(s)))
      .toEqual(["fine", "slight", "slight", "notable", "significant", "unreliable"]);
    expect(spreadOf(line(row("decode", 200, { value_stddev: 12 })))).toBe(6);
    const accuracy = line(row("accuracy", 0.4, { value_stddev: 0.1, unit: "ratio" }));
    expect(spreadOf(accuracy)).toBeNull();
    // Shown absolute in percentage points and relative, though never tiered.
    expect(["stddev", "spread"].map((key) => cellText(COLUMNS.find((c) => c.key === key)!, accuracy, ctx)))
      .toEqual(["±10 pp", "±25%"]);
    expect(spreadOf(line(null))).toBeNull();
  });

  test("name a failure, a wide spread, swap, heat, a corpus mismatch, a weak battery and a missing parameter count", () => {
    const issues = (r: ResultRow | null, state?: JobState) => issuesOf(line(r, state)).map((i) => i.text);
    expect(issues(row("decode", 100, { value_stddev: 7.26 }))).toEqual(["stddev ±7.3%"]);
    expect(issues(row("decode", 100, { value_stddev: 5 }))).toEqual([]);
    expect(issues(row("decode", 100, { observations: { max_swap_bytes: 1_610_612_736, max_host_bytes: 9 } })))
      .toEqual(["swap 1.5 GB"]);
    expect(issues(row("decode", 100, { observations: { max_swap_bytes: 0 } }))).toEqual([]);
    expect(issues(row("decode", 100, { device: { android_thermal_status_after: ["none", "moderate", "light"] } })))
      .toEqual(["thermal moderate"]);
    expect(issues(row("decode", 100, { device: { android_thermal_status_after: ["light"] } }))).toEqual([]);
    expect(issues(row("decode", 100, { device: { apple_thermal_state_after: ["fair", "serious"] } })))
      .toEqual(["thermal serious"]);
    expect(issues(row("eval", 1, { observations: { dataset_digest_matched: false } }))).toEqual(["corpus mismatch"]);
    // An ASR clip past the model's documented audio limit is labelled; one within it, or unsaid, is not.
    expect(issues(row("ttft", 1, { observations: { asr_beyond_model_audio_limit: true } })))
      .toEqual(["beyond the model's audio limit"]);
    expect(issues(row("ttft", 1, { observations: { asr_beyond_model_audio_limit: false } }))).toEqual([]);
    expect(issues(row("decode", 1, { device: { power_save_mode: true, battery_level: 10 } }))).toEqual(["power save"]);
    expect(issues(row("decode", 1, { device: { battery_level: 10, power_state: "not_charging" } })))
      .toEqual(["battery 10%"]);
    expect(issues(row("decode", 1, { device: { battery_level: 10, power_state: "charging" } }))).toEqual([]);
    expect(issues(row("decode", 1, { device: { battery_level: 10, power_state: "plugged_in_not_charging" } })))
      .toEqual([]);
    expect(issues(row("decode", null, { failed: true }))).toEqual(["failed"]);
    expect(issues(null, "failed")).toEqual(["failed"]);
    // A measured row the catalog could not size; a failed one has no figure to qualify.
    expect(issues(row("decode", 100, { model_params_total_millions: null }))).toEqual(["no parameter count"]);
    expect(issues(row("decode", null, { failed: true, model_params_total_millions: null }))).toEqual(["failed"]);
  });

  test("a line swapped where its run held swap, as the `swap` issue reads it", () => {
    expect(swapped(line(row("decode", 100, { observations: { max_swap_bytes: 4096 } })))).toBe(true);
    // A reading of none, no reading and no row are no swap.
    expect([row("decode", 100, { observations: { max_swap_bytes: 0 } }), row("decode", 100), null]
      .map((r) => swapped(line(r)))).toEqual([false, false, false]);
  });
});

describe("a recalculation's earlier values", () => {
  const at = (value: number, parameters = {}) => row("accuracy", value, { parameters });
  const older = { job: { job_id: "job-0", plan_id: null }, rows: [at(0.25)] };
  const parent = { job: { job_id: "job-1", plan_id: "plan-1" }, rows: [at(0.5, { shots: 5 }), at(0.4)] };
  const result: JobResult = {
    status: "scored", rows: [at(0.75), at(0.7, { shots: 5 })], rescored_from: parent.job, earlier_runs: [parent, older],
    recalculable: false,
  };
  const [plain, shot] = linesOf([job("job-2", "finished", { result })]);

  test("are each earlier run's row of the same metric and parameters, newest first, null where it has none", () => {
    expect(earlierRows(plain!).map(({ run, row }) => [run.job_id, row?.value ?? null]))
      .toEqual([["job-1", 0.4], ["job-0", 0.25]]);
    expect(earlierRows(shot!).map(({ run, row }) => [run.job_id, row?.value ?? null]))
      .toEqual([["job-1", 0.5], ["job-0", null]]);
  });

  test("are none for a job no recalculation answers", () => {
    expect(earlierRows(linesOf([job("job-3", "finished", { result: scored(at(0.1)) })])[0]!)).toEqual([]);
  });

  test("move from the newest earlier run's, by points on a ratio and percent elsewhere, toned by the unit", () => {
    const moved = (now: ResultRow, was: ResultRow | null) => changeOf(linesOf([job("job-2", "finished", {
      result: { ...result, rows: [now], earlier_runs: [{ job: parent.job, rows: was ? [was] : [] }] },
    })])[0]!);
    expect(changeOf(plain!)).toEqual({ text: "+87.5%", tone: "better" });
    expect(moved(row("accuracy", 0.75, { unit: "ratio" }), row("accuracy", 0.4, { unit: "ratio" })))
      .toEqual({ text: "+35.0 pp", tone: "better" });
    expect(moved(row("ttft", 6.58, { unit: "ms" }), row("ttft", 7.27, { unit: "ms" })))
      .toEqual({ text: "−9.5%", tone: "better" });
    expect(moved(row("decode", 110, { unit: "tokens/sec" }), row("decode", 100, { unit: "tokens/sec" })))
      .toEqual({ text: "+10.0%", tone: "better" });
    expect(moved(row("ttft", 1.32, { unit: "ms" }), row("ttft", 1.3, { unit: "ms" })))
      .toEqual({ text: "+1.5%", tone: "flat" });
    // No earlier row, a zero to divide by, or a failed run: no change.
    expect([moved(row("ttft", 1, { unit: "ms" }), null), moved(row("ttft", 1, { unit: "ms" }), row("ttft", 0, { unit: "ms" })),
            moved(row("ttft", 1, { unit: "ms" }), row("ttft", null, { unit: "ms", failed: true }))]).toEqual([null, null, null]);
  });
});

describe("the descriptor selects", () => {
  const RT_A = { type: "apple_foundation" } as Runtime;
  const RT_B = { build: "cu130", type: "uv_vllm" } as Runtime;
  const OTHER = { ...MODEL, repo_name: "Other-GGUF" };
  const ran = (id: string, runtime: Runtime, model: Model) =>
    job(id, "finished", { result: scored(row("decode", 1, { runtime_descriptor: runtime, model_descriptor: model }),
                                         row("ttft", 2, { runtime_descriptor: runtime, model_descriptor: model })) });
  const lines = linesOf([ran("j1", RT_A, MODEL), ran("j2", RT_B, MODEL), ran("j3", RT_A, OTHER), job("j4", "available")]);
  const kept = (query: string) => [...new Set(lines.filter((line) => keeps(line, jobFilters(new URLSearchParams(query)), ctx))
    .map((line) => line.job.job_id))];

  test("offer each descriptor the runs reported, or a job not run was planned with, as stored, counting jobs", () => {
    const choices = jobChoices(lines, ctx, {});
    expect(choices.runtime_descriptor.map((c) => [c.value, c.count]))
      .toEqual([[canonical(RT_B), 1], [canonical(PLANNED.runtime), 1], [canonical(RT_A), 2]]);
    expect(choices.model_descriptor.map((c) => [c.value, c.count]))
      .toEqual([[canonical(MODEL), 3], [canonical(OTHER), 1]]);
  });

  test("offer each run's flags as stored, and `null` for a run that reported none", () => {
    const flagged = linesOf([job("f1", "finished", { result: scored(row("decode", 1, {
      model_flags: { enable_thinking: true } as never, runtime_flags: { type: "unknown", value: "-ngl 99" } as never })) }),
                             job("f2", "finished", { result: scored(row("decode", 1)) }), job("f3", "available")]);
    const choices = jobChoices(flagged, ctx, {});
    expect(choices.model_flags.map((c) => [c.value, c.count])).toEqual([['{"enable_thinking":true}', 1], ["null", 1]]);
    expect(choices.benchmark_flags.map((c) => [c.value, c.count])).toEqual([["null", 2]]);
    expect(flagged.filter((l) => keeps(l, jobFilters(new URLSearchParams("model_flags=null")), ctx))
      .map((l) => l.job.job_id)).toEqual(["f2"]);
  });

  test("offer runtime flags as each job asked for them, its knobs alone, a job not run included", () => {
    const asked = (id: string, state: JobState, flags: object | null, over: Partial<PlanJobWithResult> = {}) =>
      job(id, state, { planned: { ...PLANNED, runtime_flags: flags as never }, ...over });
    const flagged = linesOf([
      asked("r1", "finished", { runtime_type: "llamacpp_cli", threads: 8 }, {
        result: scored(row("decode", 1, { runtime_flags: { threads: 8, ctx_size: 4096 } as never })) }),
      asked("r2", "available", { threads: 8 }), asked("r3", "available", { threads: 4 }), asked("r4", "available", null),
      // A job filed before plan cells were kept goes by what its row says it asked for.
      job("r5", "finished", { planned: null, result: scored(row("decode", 1, { requested_runtime_flags: { threads: 4 } as never })) }),
    ]);
    expect(jobChoices(flagged, ctx, {}).runtime_flags.map((c) => [c.value, c.count]))
      .toEqual([['{"threads":4}', 2], ['{"threads":8}', 2], ["null", 1]]);
    const kept = (query: string) =>
      flagged.filter((l) => keeps(l, jobFilters(new URLSearchParams(query)), ctx)).map((l) => l.job.job_id);
    expect(kept(`runtime_flags=${encodeURIComponent('{"threads":8}')}&runtime_flags=null`)).toEqual(["r1", "r2", "r4"]);
  });

  test("keep the jobs that ran, or were planned with, any descriptor chosen, every select narrowing", () => {
    const q = (key: string, ...values: unknown[]) => values.map((v) => `${key}=${encodeURIComponent(canonical(v))}`).join("&");
    expect(kept(q("runtime_descriptor", RT_A))).toEqual(["j1", "j3"]);
    expect(kept(q("model_descriptor", MODEL, OTHER))).toEqual(["j1", "j2", "j3", "j4"]);
    expect(kept(`${q("runtime_descriptor", RT_A)}&${q("model_descriptor", MODEL)}`)).toEqual(["j1"]);
  });
});

describe("the columns and the CSV", () => {
  // The columns drawn where the reader picked exactly `keys`.
  const exactly = (keys: string[], columns = COLUMNS) => drawnColumns(columns, picksOf(columns, keys));
  const jobs = [
    job("job-a", "finished", {
      result: scored(row("decode", 4385.673, { value_stddev: 7.84, device: { battery_level: 98 }, client_id: "client_1" })),
    }),
    job("job-b", "failed", {
      result: scored(row("ttft", null, { failed: true, failure_source: "client", failed_reason: 'said "no", then quit',
                                         unit: "ms" })),
    }),
  ];
  const lines = linesOf(jobs);

  test("show the default ones until picked, then the picked ones in the table's order", () => {
    expect(drawnColumns(COLUMNS, null).map((c) => c.key)).toEqual(COLUMNS.filter((c) => c.shown).map((c) => c.key));
    expect(exactly(["value", "job", "swap"]).map((c) => c.key)).toEqual(["job", "value", "swap"]);
  });

  test("offer every field the rows report, the device's shown with the device columns, parameters as JSON", () => {
    const at = (tokens: number, over: Partial<ResultRow> = {}) => row("decode", 1, {
      parameters: { prefill_tokens: tokens, batch_size: 1 },
      device: { battery_level: 90, gpu_model: "RTX 4090", gpu_vram_bytes: 24e9 },
      observations: { max_swap_bytes: 0, dataset_digest_matched: true }, ...over,
    });
    const rows = linesOf([job("job-p", "finished", { result: scored(at(256), at(512, { client_extras: { app: "x" } as never })) })]);
    const columns = columnsOf(rows);
    const keys = columns.map((c) => c.key);
    // A column per device field, its make first, then the state the run found it in.
    expect(keys.slice(keys.indexOf("device_name"), keys.indexOf("device_name") + 5))
      .toEqual(["device_name", "device.gpu_model", "device.gpu_vram_bytes", "device.battery_level", "device_time"]);
    expect(keys.at(-1)).toBe("observations.dataset_digest_matched");
    // An observation a column reads already is left out.
    expect(keys.filter((key) => key.includes("swap"))).toEqual(["swap"]);
    expect(drawnColumns(columns, null).map((c) => c.key)).toEqual([
      "job", "state", "model_descriptor", "runtime_descriptor", "benchmark_type", "benchmark", "metric", "parameters",
      "value", "unit", "spread", "issues", "failure", "failed_reason", "failure_source", "device", "device_type",
      "device_name", "device.gpu_model", "device.gpu_vram_bytes", "device.battery_level", "device_time", "swap", "runtime_flags", "model_flags", "benchmark_flags",
      "row_requested_runtime_flags", "row_requested_model_flags", "row_requested_benchmark_flags", "row_runtime_flags",
      "row_model_flags", "row_benchmark_flags",
    ]);
    const column = (key: string) => columns.find((c) => c.key === key)!;
    expect([column("model_descriptor").label, column("runtime_descriptor").label, column("model").label])
      .toEqual(["model", "runtime", "planned model"]);
    expect([column("device.gpu_model").num, column("device.battery_level").num, column("parameters").json])
      .toEqual([false, true, true]);
    const text = (key: string) => cellText(column(key), rows[0]!, ctx);
    expect(["device.gpu_model", "device.battery_level", "device.gpu_vram_bytes", "parameters"].map(text))
      .toEqual(["RTX 4090", "90", bytesText(24e9), '{"batch_size":1,"prefill_tokens":256}']);
    expect(linesCsv(rows.slice(0, 1), [column("device.gpu_vram_bytes")], ctx).split("\r\n")[1]).toBe("24000000000");
    // The address sorts by a column of the rows' own, which the table finds among them.
    expect(sortOf(new URLSearchParams("sort=-device.gpu_model"))).toBe("-device.gpu_model");
    expect(sortLines(rows, "-parameters", ctx, columns).map((line) => line.row?.parameters.prefill_tokens))
      .toEqual([512, 256]);
  });

  test("Swap opens on the run's memory first and the URIs last, every other column left to pick", () => {
    expect(drawnColumns(SWAP_COLUMNS, null).map((c) => c.key)).toEqual([
      "job", "state", "swap", "host_memory", "benchmark", "metric", "value", "unit", "failure", "device",
      "runtime_descriptor", "model_descriptor",
    ]);
    expect(SWAP_COLUMNS.map((c) => c.key).sort()).toEqual(COLUMNS.map((c) => c.key).sort());
    expect(exactly(["issues", "swap", "job"], SWAP_COLUMNS).map((c) => c.key)).toEqual(["job", "swap", "issues"]);
  });

  test("draw a value, its spread and unit apart, as pipette-datasheet prints them", () => {
    const text = (key: string, line = lines[0]!) => cellText(COLUMNS.find((c) => c.key === key)!, line, ctx);
    expect([text("value"), text("unit"), text("stddev"), text("spread"), text("model")])
      .toEqual(["4,385.67", "tokens/sec", "±7.84 tokens/sec", "±0.2%", canonical(MODEL)]);
    expect([text("value", lines[1]), text("unit", lines[1]), text("failure", lines[1])])
      .toEqual(["", "secs", "failed: reported by its client"]);
  });

  test("count the jobs listed, of all where filters narrow them, and their rows", () => {
    expect([listedText(1661, 2072, null), listedText(40, 40, 1661), listedText(1, 1, null), listedText(1, 2, 1)])
      .toEqual(["1,661 jobs (2,072 rows)", "40 of 1,661 jobs (40 rows)", "1 job (1 row)", "1 of 1 job (2 rows)"]);
  });

  test("write the picked columns' raw values, a line per row, fields quoted where needed", () => {
    const csv = linesCsv(lines, exactly(["job", "device", "metric", "value", "stddev", "spread", "failed_reason"]), ctx);
    expect(csv.split("\r\n")).toEqual([
      "job,metric,value,stddev,stddev %,failure reason,device",
      "job-a,decode,4385.673,7.84,0.18,,bench-b",
      'job-b,ttft,,,,"said ""no"", then quit",',
      "",
    ]);
  });

  test("never write a client's text as a formula, numbers as they are", () => {
    const formula = linesOf([job("job-f", "failed", {
      result: scored(row("decode", -3, { failed_reason: "=HYPERLINK(\"x\")", device_name: "@phone" })),
    })]);
    expect(linesCsv(formula, exactly(["value", "failed_reason", "device"]), ctx).split("\r\n")[1])
      .toBe(`-3,"'=HYPERLINK(""x"")",'@phone`);
  });

  test("print a device time as a duration in the table, the milliseconds in the CSV", () => {
    const timed = linesOf([job("job-t", "finished", { result: scored(row("decode", 1, { device_time_ms: 184_250 })) })]);
    const picked = exactly(["device_time"]);
    expect([timed[0]!, lines[1]!].map((line) => cellText(picked[0]!, line, ctx))).toEqual(["3m 04s", ""]);
    expect(linesCsv([timed[0]!, lines[1]!], picked, ctx).split("\r\n")).toEqual(["device time", "184250", "", ""]);
  });

  test("print a recalculation's run time beside the time it was submitted", () => {
    const recalculated = linesOf([job("job-r", "finished", { result: scored(row("decode", 1, {
      submitted_at: "2026-10-14T09:00:00Z", run_submitted_at: "2026-09-01T08:00:00Z", rescored_from: "job-0",
    })) })]);
    const picked = exactly(["submitted_at", "run_submitted_at"]);
    expect(picked.map((column) => cellText(column, recalculated[0]!, ctx)))
      .toEqual(["2026-10-14T09:00:00Z", "2026-09-01T08:00:00Z"]);
    expect(linesCsv([recalculated[0]!, lines[1]!], picked, ctx).split("\r\n"))
      .toEqual(["submitted,run submitted", "2026-10-14T09:00:00Z,2026-09-01T08:00:00Z", ",", ""]);
  });

  test("print the scorer version on an eval row, hidden until picked", () => {
    const evals = linesOf([job("job-e", "finished", { result: scored(row("accuracy", 0.7, {
      benchmark_type: "eval", score_runtime_version: "1.2.3",
    })) })]);
    expect(COLUMNS.find((column) => column.key === "score_runtime_version")?.shown).toBe(false);
    const picked = exactly(["score_runtime_version"]);
    expect([evals[0]!, lines[1]!].map((line) => cellText(picked[0]!, line, ctx))).toEqual(["1.2.3", ""]);
    expect(linesCsv([evals[0]!, lines[1]!], picked, ctx).split("\r\n")).toEqual(["scorer version", "1.2.3", "", ""]);
  });

  test("print a lease and a lapse in the table, the stored timestamps in the CSV", () => {
    const leased = linesOf([job("job-l", "running", {
      queue: { expires_at: "2026-10-07T09:00:00Z", lease: { expires_at: "2026-10-07T10:00:00.123Z", client_id: null } },
      plan: { lapsed: { expires_at: "2026-10-07T08:00:00Z", recorded_at: "2026-10-07T11:00:00.5Z" }, rerun_of: null, rerun_history: [] },
    }), job("job-n", "finished", { queue: null, plan: { lapsed: null, rerun_of: null, rerun_history: [] } })]);
    const picked = exactly(["expires", "lease", "lapsed_for_plan"]);
    expect(picked.map((column) => cellText(column, leased[0]!, ctx))).toEqual([
      "2026-10-07 09:00:00Z (expired)", "until 2026-10-07 10:00:00Z (lapsed)",
      "expired 2026-10-07 08:00:00Z, recorded 2026-10-07 11:00:00Z",
    ]);
    expect(linesCsv(leased, picked, ctx).split("\r\n").slice(1, 3)).toEqual([
      "2026-10-07T09:00:00Z (expired),until 2026-10-07T10:00:00.123Z (lapsed),"
        + "\"expired 2026-10-07T08:00:00Z, recorded 2026-10-07T11:00:00.5Z\"",
      ",,",
    ]);
  });

  test("read reported descriptors first and use planned descriptors when a failed row omits them", () => {
    const ran = row("decode", 1, { benchmark_id: "decode_throughput_128", runtime_descriptor: FOUNDATION,
                                   client_id: "client_1" });
    const reportedModel: Model = { ...MODEL, repo_name: "Big-9B" };
    const [planned, unplanned, running, bare, failed, failedModel, failedRuntime] = linesOf([
      job("job-p", "finished", { result: scored(ran) }), job("job-u", "finished", { planned: null, result: scored(ran) }),
      job("job-r", "running"), job("job-e", "finished", { result: scored(row("decode", 1, { model_descriptor: null })) }),
      job("job-f", "failed", { result: scored(row("decode", null, { failed: true, model_descriptor: null })) }),
      job("job-fm", "failed", { result: scored(row("decode", null, {
        failed: true, model_descriptor: reportedModel, runtime_descriptor: null,
      })) }),
      job("job-fr", "failed", { result: scored(row("decode", null, {
        failed: true, model_descriptor: null, runtime_descriptor: FOUNDATION,
      })) }),
    ]);
    const text = (key: string, line: Line) => cellText(COLUMNS.find((c) => c.key === key)!, line, ctx);
    expect(["benchmark", "model", "device", "client_id", "runtime_descriptor"].map((key) => text(key, planned!)))
      .toEqual([PLANNED.benchmark, canonical(MODEL), "bench-b", "client_1", '{"type":"apple_foundation"}']);
    expect(["benchmark", "model", "runtime", "runtime_descriptor"].map((key) => text(key, unplanned!)))
      .toEqual(["decode_throughput_128", "", "", '{"type":"apple_foundation"}']);
    expect(["model_descriptor", "runtime_descriptor"].map((key) => text(key, running!)))
      .toEqual([canonical(MODEL), canonical(PLANNED.runtime)]);
    expect(["model_descriptor", "runtime_descriptor"].map((key) => text(key, bare!))).toEqual(["", ""]);
    // A client that fails a job often files no descriptors, so its row shows the ones the job asked for.
    expect(["model_descriptor", "runtime_descriptor"].map((key) => text(key, failed!)))
      .toEqual([canonical(MODEL), canonical(PLANNED.runtime)]);
    expect(["model_descriptor", "runtime_descriptor"].map((key) => text(key, failedModel!)))
      .toEqual([canonical(reportedModel), canonical(PLANNED.runtime)]);
    expect(["model_descriptor", "runtime_descriptor"].map((key) => text(key, failedRuntime!)))
      .toEqual([canonical(MODEL), canonical(FOUNDATION)]);
    const csvDescriptor = (descriptor: Model | Runtime) => `"${canonical(descriptor).replaceAll('"', '""')}"`;
    expect(linesCsv([failed!], exactly(["model_descriptor", "runtime_descriptor"]), ctx).split("\r\n"))
      .toEqual(["model,runtime", `${csvDescriptor(MODEL)},${csvDescriptor(PLANNED.runtime)}`, ""]);
    // Each result line draws its own descriptors, including the fallback for a failed row.
    expect(COLUMNS.filter((c) => c.key.endsWith("_descriptor")).map((c) => c.job)).toEqual([false, false, false]);
  });
});

describe("the runtime's catalog name", () => {
  const release = (flavor: string) => ({
    type: "llamacpp_cli_stock_tools", source: "github_release", repository_url: "github.com/ggml-org/llama.cpp",
    repository_version: "b11669", flavor,
  }) as Runtime;
  const CPU = release("linux-x64");
  const CUDA = release("linux-cuda-12-8");
  const OTHER = release("windows-x64");
  const digest = (char: string) => char.repeat(64);
  const descriptorNames = {
    [digest("a")]: { display: "llamacpp-v0.7.0::linux-x64", name: "llamacpp-v0.7.0", synonyms: ["b11669::linux-x64"] },
    [digest("b")]: { display: "llamacpp-v0.7.0::linux-cuda-12-8", name: "llamacpp-v0.7.0",
                     synonyms: ["b11669::linux-cuda-12-8"] },
  };
  const named = { ...ctx, descriptorNames };
  const lines = linesOf([
    job("job-cpu", "finished", { result: scored(row("decode", 5, { runtime_descriptor: CPU,
                                                                    runtime_descriptor_sha256: digest("a") })) }),
    job("job-cuda", "finished", { result: scored(row("decode", 6, { runtime_descriptor: CUDA,
                                                                     runtime_descriptor_sha256: digest("b") })) }),
    job("job-other", "finished", { result: scored(row("decode", 7, { runtime_descriptor: OTHER,
                                                                      runtime_descriptor_sha256: digest("c") })) }),
    // Queued, so the line shows its planned runtime, under the planned runtime's digest.
    job("job-queued", "available", { planned: { ...PLANNED, runtime: CUDA }, planned_runtime_sha256: digest("b") }),
  ]);
  const line = (id: string) => lines.find((l) => l.job.job_id === id)!;
  const column = COLUMNS.find((c) => c.key === "runtime_catalog_name")!;

  test("is the display of the digest the line shows, so two members of one entry differ", () => {
    expect(["job-cpu", "job-cuda", "job-queued"].map((id) => runtimeNameOf(line(id), named)?.display)).toEqual([
      "llamacpp-v0.7.0::linux-x64", "llamacpp-v0.7.0::linux-cuda-12-8", "llamacpp-v0.7.0::linux-cuda-12-8",
    ]);
  });

  test("is none for a runtime with no name, which shows its descriptor as stored", () => {
    const runtime = COLUMNS.find((c) => c.key === "runtime_descriptor")!;
    expect(runtimeNameOf(line("job-other"), named)).toBeNull();
    expect(runtime.raw(line("job-other"), named)).toBe(canonical(OTHER));
    expect(runtimeNameOf(line("job-cpu"), ctx)).toBeNull();
    // A row that reports its runtime without a digest is not named by the planned runtime's.
    const undigested = linesOf([job("job-old", "finished", {
      planned_runtime_sha256: digest("b"),
      result: scored(row("decode", 5, { runtime_descriptor: CUDA, runtime_descriptor_sha256: null })),
    })]);
    expect(runtimeNameOf(undigested[0]!, named)).toBeNull();
  });

  test("the search finds a line by its runtime's name or a synonym", () => {
    const found = (q: string) => lines.filter((l) => keeps(l, { ...NONE, q }, named)).map((l) => l.job.job_id);
    expect(found("llamacpp-v0.7.0::linux-cuda")).toEqual(["job-cuda", "job-queued"]);
    expect(found("b11669::linux-x64")).toEqual(["job-cpu"]);
    expect(lines.filter((l) => keeps(l, { ...NONE, q: "llamacpp-v0.7.0" }, ctx))).toEqual([]);
  });

  test("the runtime select labels a named runtime by its name", () => {
    const labels = jobChoices(lines, named, {}).runtime_descriptor.map((c) => [c.label, c.value]);
    expect(labels).toContainEqual([`llamacpp-v0.7.0::linux-x64 ${canonical(CPU)}`, canonical(CPU)]);
    expect(labels).toContainEqual([canonical(OTHER), canonical(OTHER)]);
  });

  test("the runtime select keeps a name when a legacy row with the same descriptor follows", () => {
    const legacy = linesOf([job("job-old", "finished", {
      result: scored(row("decode", 5, { runtime_descriptor: CPU, runtime_descriptor_sha256: null })),
    })])[0]!;
    const namedLine = line("job-cpu");
    for (const ordered of [[namedLine, legacy], [legacy, namedLine]]) {
      const labels = jobChoices(ordered, named, {}).runtime_descriptor.map((c) => c.label);
      expect(labels).toContain(`llamacpp-v0.7.0::linux-x64 ${canonical(CPU)}`);
    }
  });

  test("is a column the CSV carries, hidden until picked, empty for a runtime with no name", () => {
    expect(column.shown).toBe(false);
    const csv = linesCsv(lines, [COLUMNS.find((c) => c.key === "job")!, column], named);
    expect(csv.split("\r\n")).toEqual([
      "job,runtime catalog name", "job-cpu,llamacpp-v0.7.0::linux-x64", "job-cuda,llamacpp-v0.7.0::linux-cuda-12-8",
      "job-other,", "job-queued,llamacpp-v0.7.0::linux-cuda-12-8", "",
    ]);
  });
});

describe("the model's catalog name", () => {
  const member = (type: "gguf_text" | "mlx", repo_name: string) => ({
    type, source: "huggingface", org: "acme", repo_name, revision: "0123456789abcdef0123456789abcdef01234567",
    ...(type === "gguf_text" ? { path: "model.gguf" } : {}),
  }) as Model;
  const GGUF = member("gguf_text", "AcmeLM-3B-GGUF");
  const MLX = member("mlx", "AcmeLM-3B-MLX-4bit");
  const digest = (char: string) => char.repeat(64);
  const descriptorNames = {
    [digest("a")]: { display: "AcmeLM-3B::gguf::q4_0", name: "AcmeLM-3B", synonyms: ["acme/Acme-3B::gguf::q4_0"] },
    [digest("b")]: { display: "AcmeLM-3B::mlx::4bit", name: "AcmeLM-3B", synonyms: [] },
  };
  const named = { ...ctx, descriptorNames };
  const lines = linesOf([
    job("job-gguf", "finished", { result: scored(row("decode", 5, { model_descriptor: GGUF,
                                                                     model_descriptor_sha256: digest("a") })) }),
    job("job-mlx", "finished", { result: scored(row("decode", 6, { model_descriptor: MLX,
                                                                    model_descriptor_sha256: digest("b") })) }),
    job("job-other", "finished", { result: scored(row("decode", 7, { model_descriptor_sha256: digest("c") })) }),
    // Queued, so the line shows its planned model, under the planned model's digest.
    job("job-queued", "available", { planned: { ...PLANNED, model: MLX }, planned_model_sha256: digest("b") }),
  ]);
  const line = (id: string) => lines.find((l) => l.job.job_id === id)!;
  const column = COLUMNS.find((c) => c.key === "model_catalog_name")!;

  test("is the display of the digest the line shows, so two members of one entry differ", () => {
    expect(["job-gguf", "job-mlx", "job-queued"].map((id) => modelNameOf(line(id), named)?.display)).toEqual([
      "AcmeLM-3B::gguf::q4_0", "AcmeLM-3B::mlx::4bit", "AcmeLM-3B::mlx::4bit",
    ]);
    expect(columnNameOf("model_descriptor", line("job-gguf"), named)?.display).toBe("AcmeLM-3B::gguf::q4_0");
    expect(columnNameOf("runtime_descriptor", line("job-gguf"), named)).toBeNull();
  });

  test("is none for a model with no name, which shows its descriptor as stored", () => {
    const model = COLUMNS.find((c) => c.key === "model_descriptor")!;
    expect(modelNameOf(line("job-other"), named)).toBeNull();
    expect(model.raw(line("job-other"), named)).toBe(canonical(DESCRIPTOR));
    expect(modelNameOf(line("job-gguf"), ctx)).toBeNull();
  });

  test("the search finds a line by its model's name or a synonym", () => {
    const found = (q: string) => lines.filter((l) => keeps(l, { ...NONE, q }, named)).map((l) => l.job.job_id);
    expect(found("AcmeLM-3B::mlx")).toEqual(["job-mlx", "job-queued"]);
    expect(found("acme/Acme-3B::gguf")).toEqual(["job-gguf"]);
    expect(lines.filter((l) => keeps(l, { ...NONE, q: "AcmeLM-3B::gguf" }, ctx))).toEqual([]);
  });

  test("the model select labels a named model by its name", () => {
    const labels = jobChoices(lines, named, {}).model_descriptor.map((c) => [c.label, c.value]);
    expect(labels).toContainEqual([`AcmeLM-3B::gguf::q4_0 ${canonical(GGUF)}`, canonical(GGUF)]);
    expect(labels).toContainEqual([canonical(DESCRIPTOR), canonical(DESCRIPTOR)]);
  });

  test("the model select finds a synonym without showing it in the label", () => {
    const choices = jobChoices(lines, named, {}).model_descriptor;
    const matches = choices.filter((choice) => choiceMatches(choice, "acme/Acme-3B::gguf"));
    expect(matches.map((choice) => choice.value)).toEqual([canonical(GGUF)]);
    expect(matches[0]?.label).not.toContain("acme/Acme-3B");
  });

  test("is a column the CSV carries, hidden until picked, empty for a model with no name", () => {
    expect(column.shown).toBe(false);
    const csv = linesCsv(lines, [COLUMNS.find((c) => c.key === "job")!, column], named);
    expect(csv.split("\r\n")).toEqual([
      "job,model catalog name", "job-gguf,AcmeLM-3B::gguf::q4_0", "job-mlx,AcmeLM-3B::mlx::4bit", "job-other,",
      "job-queued,AcmeLM-3B::mlx::4bit", "",
    ]);
  });
});
