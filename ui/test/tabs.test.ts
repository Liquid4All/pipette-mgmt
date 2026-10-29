import { describe, expect, test } from "vitest";
import type { BarTab } from "../src/lib/bar";
import { cellsWith } from "../src/lib/jobs";
import { chartRefs, index, pageControls, reportFilters, type Family } from "../src/lib/report";
import { match, screenOf } from "../src/lib/routes";
import {
  baseOf, deviceBar, legacyReports, legacyResults, pickedDevice, progressBar, reportOf,
  tabLink, tabOf, topBar, type PlanTabRoute, type TabBase,
} from "../src/lib/tabs";
import type { Model, ResultRow, Runtime } from "../src/lib/api-types";

const PLAN: TabBase = { plan_id: "plan-1", campaign_id: null };
const CAMPAIGN = { plan_id: "plan-1", campaign_id: "cmp_1" };

/** A bar as its labels, `|` where a group opens and `*` on the open tab. */
const drawn = (items: BarTab[]) =>
  items.map(({ start, label, current }, i) => `${start && i ? "| " : ""}${label}${current ? "*" : ""}`).join(" ");

let next = 0;
function row(over: Partial<ResultRow> = {}): ResultRow {
  next++;
  return {
    job_id: `job-${next}`, benchmark_id: "bench", benchmark_type: "prefill_throughput", metric: "prefill_throughput",
    value: 100, value_stddev: null, unit: "tokens/sec", failed: false, failed_reason: null, failure_source: null,
    model_params_total_millions: 1000, model_params_active_millions: 1000,
    model_descriptor: { org: "acme", path: "Tiny-1B-Q4_K_M.gguf", repo_name: "Tiny-1B-GGUF", type: "gguf_text" } as Model,
    runtime_descriptor: { flavor: "macos-arm64", repository_version: "b100", type: "llamacpp_cli_stock_tools" } as Runtime,
    runtime_descriptor_sha256: null, model_descriptor_sha256: null, model_flags: null, runtime_flags: null, benchmark_flags: null,
    model_vault_descriptor: null, requested_model_flags: null, requested_runtime_flags: null, requested_benchmark_flags: null,
    eval_metadata: null, client_extras: null,
    parameters: { prefill_tokens: 256 }, observations: {}, client_id: "client-a", device_name: "Bench A",
    device: {}, client_version: null, submitted_at: null,
    scored_at: null, device_time_ms: null, rescored_from: null, run_submitted_at: null,
    score_runtime_version: null, ...over,
  };
}
const failed = (over: Partial<ResultRow> = {}) =>
  row({ value: null, failed: true, failure_source: "expired", client_id: "system", device_name: null, ...over });

describe("a plan's tabs", () => {
  test("each tab's address names it back, on the plan's pages and its campaign's", () => {
    for (const base of [PLAN, CAMPAIGN]) {
      for (const name of ["overview", "cells", "analysis", "revisions"] as const) {
        const route = match(tabLink(base, name));
        expect(tabOf(route)).toEqual({ name, view: null });
        expect(baseOf(route as PlanTabRoute)).toEqual(base);
      }
      for (const view of ["decode_throughput", "e2e_check", "swap"]) {
        expect(tabOf(match(tabLink(base, "analysis", view)))).toEqual({ name: "analysis", view });
      }
    }
    expect(tabLink(PLAN, "cells")).toBe("/app/plans/plan-1/cells");
    expect(tabLink(PLAN, "analysis", "a/b", [["model", "x"], ["quant", ""]])).toBe("/app/plans/plan-1/analysis/a%2Fb?model=x");
    expect(tabLink(CAMPAIGN, "cells", "", [["offset", "50"]])).toBe("/app/campaigns/cmp_1/plans/plan-1/cells?offset=50");
    expect(tabLink(CAMPAIGN, "revisions")).toBe("/app/campaigns/cmp_1/plans/plan-1/revisions");
    expect(tabLink(CAMPAIGN, "analysis", "e2e_check")).toBe("/app/campaigns/cmp_1/plans/plan-1/analysis/e2e_check");
    for (const base of [PLAN, CAMPAIGN]) {
      expect(tabOf(match(tabLink(base, "settings")))).toEqual({ name: "settings", view: null });
      expect(screenOf(tabLink(base, "settings"))).toBe(tabLink(base, "overview"));
    }
    expect(tabLink(PLAN, "settings")).toBe("/app/plans/plan-1/settings");
    for (const path of ["/app/campaigns/cmp_1", "/app/plans", "/app/plans/plan-1/results", "/app/plans/plan-1/jobs", "/app/x"]) {
      expect(tabOf(match(path))).toBeNull();
    }
  });

  test("the bare address is the Overview, the default tab", () => {
    expect(tabLink(PLAN, "overview")).toBe("/app/plans/plan-1");
    expect(tabLink(CAMPAIGN, "overview")).toBe("/app/campaigns/cmp_1/plans/plan-1");
    expect(tabOf(match("/app/plans/plan-1/"))).toEqual({ name: "overview", view: null });
    expect(tabOf(match("/app/campaigns/cmp_1/plans/plan-1"))).toEqual({ name: "overview", view: null });
  });

  test("every tab is a step within its plan's screen, which the router keeps", () => {
    for (const base of [PLAN, CAMPAIGN]) {
      for (const [name, view] of [["cells", ""], ["analysis", ""], ["analysis", "eval"], ["revisions", ""]] as const) {
        expect(screenOf(tabLink(base, name, view))).toBe(tabLink(base, "overview"));
      }
    }
  });
});

describe("the results page's device types", () => {
  const rows = ["Bench A", "Bench B", "Bench C"].map((device_name) => row({ device_name }));
  const bar = (search: string, of = rows) => {
    const filters = reportFilters(new URLSearchParams(search), "prefill_throughput");
    const ix = index(of, "full");
    return deviceBar(PLAN, "prefill_throughput", filters, pageControls(ix, filters).device, ix.deviceNames);
  };

  test("are a bar: all of them, open where the address names none or every one, then each type drawn", () => {
    expect(drawn(bar(""))).toBe("All devices* | Bench A Bench B Bench C");
    expect(drawn(bar("device=Bench+B"))).toBe("All devices | Bench A Bench B* Bench C");
    expect(drawn(bar("device=Bench+A&device=Bench+C"))).toBe("All devices | Bench A* Bench B Bench C*");
    expect(drawn(bar("device=Bench+A&device=Bench+B&device=Bench+C"))).toBe("All devices* | Bench A Bench B Bench C");
  });

  test("keep the address's other filters as asked, and spell every type out for all of them", () => {
    const [all, , b] = bar("device=Bench+A&metric=ttft&per=4&p.batch_size=2&status=failed");
    const rest = "metric=ttft&per=4&p.batch_size=2&status=failed";
    expect(all!.href).toBe("/app/plans/plan-1/analysis/prefill_throughput?metric=ttft&device=Bench+A&device=Bench+B&device=Bench+C&"
      + rest.slice("metric=ttft&".length));
    expect(b!.href).toBe("/app/plans/plan-1/analysis/prefill_throughput?metric=ttft&device=Bench+B&per=4&p.batch_size=2&status=failed");
  });

  test("name a type in short, its make on hover and in the address", () => {
    const mac = (ram: number) => row({ device: { chip_model: "Apple M5 Max", ram_bytes: ram * 2 ** 30, os_name: "macOS", form_factor: "laptop" } });
    const [, small, large] = bar("", [mac(48), mac(96)]);
    expect([small!.label, small!.title]).toEqual(["Apple M5 Max · 48.0 GB RAM", "Apple M5 Max · 48.0 GB RAM · macOS · laptop"]);
    expect(large!.href).toBe("/app/plans/plan-1/analysis/prefill_throughput?device=Apple+M5+Max+%C2%B7+96.0+GB+RAM+%C2%B7+macOS+%C2%B7+laptop");
  });

  test("are a bar for one type too, all devices and it, both chosen as with several", () => {
    expect(drawn(bar("", [row(), row()]))).toBe("All devices* | Bench A");
    expect(drawn(bar("device=Bench+A", [row(), row()]))).toBe("All devices | Bench A*");
  });
});

describe("the Overview's device types over Progress", () => {
  test("are a bar: all devices, then each type by its short name, linked by the `device` Results reads", () => {
    const names = new Map([["Apple M5 Max · 48.0 GB RAM · macOS", "Apple M5 Max"]]);
    const bar = progressBar(PLAN, ["Apple M5 Max · 48.0 GB RAM · macOS"], names, "Apple M5 Max · 48.0 GB RAM · macOS");
    expect(bar.map((tab) => [tab.label, tab.href, tab.current])).toEqual([
      ["All devices", "/app/plans/plan-1", false],
      ["Apple M5 Max", "/app/plans/plan-1?device=Apple+M5+Max+%C2%B7+48.0+GB+RAM+%C2%B7+macOS", true],
    ]);
    expect(progressBar(PLAN, [], names, "")).toEqual([]);
  });

  test("pick the one type the address names, none where it names several or one the results lack", () => {
    const picked = (query: string) => pickedDevice(new URLSearchParams(query), ["Bench A", "Bench B"]);
    expect(["device=Bench+A", "device=Bench+A&device=Bench+B", "device=Bench+C", ""].map(picked))
      .toEqual(["Bench A", "", "", ""]);
  });
});

describe("the top bar", () => {
  const query = new URLSearchParams("metric=ttft&p.batch_size=2&device=Bench+A&device=Bench+B&model=tiny&status=failed&offset=50");
  const overview = { name: "overview", view: null } as const;
  const cells = { name: "cells", view: null } as const;
  const analysis = { name: "analysis", view: "eval" } as const;
  const revisions = { name: "revisions", view: null } as const;
  const settings = { name: "settings", view: null } as const;
  const all = { analysis: true, revisions: true, settings: true };
  const none = { analysis: false, revisions: false, settings: false };

  test("opens on the Overview, then Results where results are served, Jobs and History where readable", () => {
    expect(drawn(topBar(CAMPAIGN, overview, all, query))).toBe("Overview* Results Jobs History | Settings");
    expect(drawn(topBar(CAMPAIGN, cells, { ...all, settings: false }, query))).toBe("Overview Results Jobs* History");
    expect(drawn(topBar(PLAN, analysis, { ...none, analysis: true }, query))).toBe("Overview Results* Jobs");
    expect(drawn(topBar(PLAN, overview, none, query))).toBe("Overview* Jobs");
    expect(drawn(topBar(PLAN, cells, { ...none, revisions: true }, query))).toBe("Overview Jobs* History");
  });

  test("sets Settings apart, the reads before it", () => {
    expect(drawn(topBar(CAMPAIGN, settings, all, query))).toBe("Overview Results Jobs History | Settings*");
    expect(topBar(CAMPAIGN, overview, all, query).at(-1)?.href).toBe("/app/campaigns/cmp_1/plans/plan-1/settings");
  });

  test("keeps the open tab listed, so the bar always says where the reader is", () => {
    expect(drawn(topBar(PLAN, analysis, none, query))).toBe("Overview Results* Jobs");
    expect(drawn(topBar(PLAN, revisions, { ...none, analysis: true }, query)))
      .toBe("Overview Results Jobs History*");
    expect(drawn(topBar(CAMPAIGN, settings, none, query))).toBe("Overview Jobs | Settings*");
    expect(drawn(topBar(PLAN, overview, { ...none, settings: true }, query))).toBe("Overview* Jobs | Settings");
  });

  test("carries the shared filters to Results alone, never a metric, pin or offset", () => {
    expect(Object.fromEntries(topBar(CAMPAIGN, analysis, all, query).map((t) => [t.key, t.href]))).toEqual({
      overview: "/app/campaigns/cmp_1/plans/plan-1",
      cells: "/app/campaigns/cmp_1/plans/plan-1/cells",
      analysis: "/app/campaigns/cmp_1/plans/plan-1/analysis?device=Bench+A&device=Bench+B&model=tiny&status=failed",
      revisions: "/app/campaigns/cmp_1/plans/plan-1/revisions",
      settings: "/app/campaigns/cmp_1/plans/plan-1/settings",
    });
  });
});

describe("the Results tab", () => {
  const short = (key: string, label: string, type = key, metrics: string[] = []) =>
    ({ key, short: label, types: [type], metrics: metrics.map((m) => ({ key: m, label: m })), metric: metrics[0] ?? null,
       disabled: null }) as unknown as Family;
  const benchmarks = [short("prefill_throughput", "Prefill"), short("end_to_end_latency", "E2E latency")];
  const check = short("e2e_check", "E2E latency vs prefill + decode", "prefill_throughput", ["excess"]);

  test("a chart per metric of each benchmark type in plain words, the type named where two share a name or for an eval", () => {
    const types = [short("prefill_throughput", "Prefill", "prefill_throughput", ["prefill_throughput", "ttft"]),
                   short("vl_throughput", "VL throughput", "vl_throughput", ["ttft", "accept_length"]),
                   short("eval-math", "math", "eval", ["accuracy"]), short("decode_throughput", "Decode")];
    const charts = chartRefs({ benchmarks: types, families: [...types, check] });
    expect(charts.map((c) => c.name)).toEqual([
      "Prefill throughput", "Prefill · time to first token", "VL throughput · time to first token", "Accept length",
      "math · accuracy", "Decode", "E2E latency vs prefill + decode",
    ]);
    // A type that measured nothing is one chart with no metric; the check comes last, and not where it is not offered.
    expect(charts.map((c) => [c.family.key, c.metric?.key ?? null]).slice(-2)).toEqual([["decode_throughput", null], ["e2e_check", "excess"]]);
    expect(chartRefs({ benchmarks: types, families: [...types, { ...check, disabled: "needs device names" }] })).toHaveLength(6);
  });

  test("an address draws the charts open on a family, on none at the bare one, Swap, or a note for one it lacks", () => {
    const plan = { benchmarks, families: [...benchmarks, check], swapped: false, missing: null };
    expect(reportOf(null, plan)).toEqual({ kind: "charts", family: "" });
    expect(reportOf("end_to_end_latency", plan)).toEqual({ kind: "charts", family: "end_to_end_latency" });
    expect(reportOf("e2e_check", plan)).toEqual({ kind: "charts", family: "e2e_check" });
    expect(reportOf("e2e_check", { ...plan, families: [...benchmarks, { ...check, disabled: "no decode throughput results" }] }))
      .toEqual({ kind: "unknown", view: "e2e_check" });
    expect(reportOf("decode_throughput", plan)).toEqual({ kind: "unknown", view: "decode_throughput" });
    expect(reportOf("swap", { ...plan, swapped: true })).toEqual({ kind: "swap" });
    expect(reportOf("swap", plan)).toEqual({ kind: "unknown", view: "swap" });
    // The swap chart is a section of the charts where the plan lists it; the table of swapped jobs keeps `swap`.
    const swapping = { ...plan, families: [...plan.families, short("max_swap", "Swap", "prefill_throughput", ["max_swap"])],
                       swapped: true };
    expect(reportOf("max_swap", swapping)).toEqual({ kind: "charts", family: "max_swap" });
    expect(reportOf("max_swap", plan)).toEqual({ kind: "unknown", view: "max_swap" });
    // A run that swapped where the chart lists none, as a VL type's, lists its jobs alone.
    expect(reportOf("max_swap", { ...plan, swapped: true })).toEqual({ kind: "unknown", view: "max_swap" });
    // The VL check and VL swap are sections where the plan lists them.
    const vl = [short("vl_e2e_check", "VL E2E latency vs prompt + decode", "vl_throughput", ["excess"]),
                short("vl_max_swap", "VL swap", "vl_throughput", ["max_swap"])];
    for (const view of ["vl_e2e_check", "vl_max_swap"]) {
      expect(reportOf(view, { ...plan, families: vl, swapped: true })).toEqual({ kind: "charts", family: view });
      expect(reportOf(view, swapping)).toEqual({ kind: "unknown", view });
    }
    // Missing data is a page where a measured row has no parameter count.
    const missing = index([row({ model_params_total_millions: null, model_params_active_millions: null })], "full").missing!;
    expect(reportOf("missing", { ...plan, missing })).toEqual({ kind: "missing", missing });
    expect(reportOf("missing", plan)).toEqual({ kind: "unknown", view: "missing" });
    // An eval is a family per eval_id; the type's own address opens the first.
    const types = [short("eval-math", "math", "eval"), short("eval-quiz", "quiz", "eval")];
    const evals = { benchmarks: types, families: types, swapped: false, missing: null };
    expect(reportOf("eval", evals)).toEqual({ kind: "charts", family: "eval-math" });
  });
});

describe("the results page's old address", () => {
  const moved = (search: string, hash = "") => legacyResults(PLAN, new URLSearchParams(search), hash);

  test("lands on Analysis, a graph on its report, every other key kept", () => {
    expect(moved("")).toBe("/app/plans/plan-1/analysis");
    expect(moved("column=all&status=failed", "#elsewhere")).toBe("/app/plans/plan-1/analysis?column=all&status=failed");
    expect(moved("graph=decode_throughput&metric=ttft&p.batch_size=2&model=x"))
      .toBe("/app/plans/plan-1/analysis/decode_throughput?metric=ttft&p.batch_size=2&model=x");
    expect(moved("graph=e2e_check&per=4")).toBe("/app/plans/plan-1/analysis/e2e_check?per=4");
    expect(moved("graph=&column=all")).toBe("/app/plans/plan-1/analysis?column=all");
  });

  test("its table lands on Cells, which lists the rows, keeping its outcome filter alone", () => {
    expect(moved("graph=decode_throughput&rows=all&status=failed&limit=5")).toBe("/app/plans/plan-1/cells?result=failed");
    expect(moved("graph=decode_throughput&column=all&status=measured", "#rows"))
      .toBe("/app/plans/plan-1/cells?result=measured");
    expect(moved("graph=e2e_check&offset=50", "#rows")).toBe("/app/plans/plan-1/cells");
    expect(moved("", "#rows")).toBe("/app/plans/plan-1/cells");
  });
});

describe("the Reports tab's old address", () => {
  test("lands on the same report under Analysis, its query kept; All rows on Cells", () => {
    const query = new URLSearchParams("metric=ttft&model=tiny");
    expect(legacyReports(PLAN, "", query)).toBe("/app/plans/plan-1/analysis?metric=ttft&model=tiny");
    expect(legacyReports(CAMPAIGN, "eval", query))
      .toBe("/app/campaigns/cmp_1/plans/plan-1/analysis/eval?metric=ttft&model=tiny");
    expect(legacyReports(PLAN, "rows", new URLSearchParams("benchmark=eval&status=failed")))
      .toBe("/app/plans/plan-1/cells?result=failed");
    expect(legacyReports(CAMPAIGN, "rows", new URLSearchParams("status=maybe"))).toBe("/app/campaigns/cmp_1/plans/plan-1/cells");
  });
});

describe("a link into Cells", () => {
  test("narrows it to the choices given alone, a type by every benchmark id it holds", () => {
    expect(cellsWith(CAMPAIGN, { benchmark: ["decode_512", "decode_1024"], result: ["failed"] }))
      .toBe("/app/campaigns/cmp_1/plans/plan-1/cells?result=failed&benchmark=decode_512&benchmark=decode_1024");
    expect(cellsWith(PLAN, { result: ["failed"] })).toBe("/app/plans/plan-1/cells?result=failed");
    expect(cellsWith(PLAN, {})).toBe("/app/plans/plan-1/cells");
    expect(cellsWith(PLAN, { q: "gguf-text://repo=a/b&path=b-Q4_0.gguf", device: ["client-a"], issue: ["swap"] }))
      .toBe("/app/plans/plan-1/cells?q=gguf-text%3A%2F%2Frepo%3Da%2Fb%26path%3Db-Q4_0.gguf&device=client-a&issue=swap");
  });
});
