import { describe, expect, test } from "vitest";
import { e2eText, pairingText } from "../src/lib/findings";
import {
  canonical, cellText, chartNames, chartRefs, descriptorText, deviceNames, deviceType, drawnRows, effortOf, estimate, familyOf, index, modelLabels, pageControls, partText, reportFilters, reportPairs, runtimeLabels, shortLabel, thinkingOf, speculativeJobs, view,
  ASR_SWAPPED, BEYOND_LIMIT, SWAPPED, toggledRuntime, VL_SWAPPED, withFilter, type BarChart, type LineChart, type ReportFilters,
} from "../src/lib/report";
import type { FleetDetail, Model, ModelFlags, ResultRow, Runtime } from "../src/lib/api-types";
import type { DescriptorNames } from "../src/lib/descriptor-names";


const gguf = (name: string, quant = "Q4_K_M", revision = "a".repeat(40)): Model => ({
  org: "acme", path: `${name}-${quant}.gguf`, repo_name: `${name}-GGUF`, revision, source: "huggingface",
  type: "gguf_text",
});
const torch = (name: string): Model =>
  ({ org: "acme", repo_name: name, revision: "c".repeat(40), source: "huggingface", type: "torch" });
const speculative = (draftRevision = "d".repeat(40), targetRevision = "e".repeat(40)): Model => ({
  draft: { org: "acme", repo_name: "Tiny-Draft", revision: draftRevision, source: "huggingface" },
  target: { org: "acme", repo_name: "Big-3B", revision: targetRevision, source: "huggingface" },
  type: "torch_speculative",
});
const release = (flavor = "macos-arm64", version = "b100"): Runtime => ({
  flavor, repository_url: "github.com/acme/llama", repository_version: version, source: "github_release",
  type: "llamacpp_cli_stock_tools",
});
const archive = (url: string): Runtime =>
  ({ flavor: "macos-arm64", source: "remote_archive", type: "llamacpp_cli_stock_tools", url });
const VLLM = { build: "cu130", server_version: "0.30.0", type: "uv_vllm" } as Runtime;
const SGLANG = { build: "cu130", server_version: "0.5.8", type: "uv_sglang" } as Runtime;
const MAC = release();
const BOX = release("windows-x64-vulkan");
const PHONE = release("android-arm64-v8a");

let next = 0;
function row(over: Partial<ResultRow> = {}): ResultRow {
  next++;
  return {
    job_id: `job-${next}`, benchmark_id: "bench", benchmark_type: "prefill_throughput", metric: "prefill_throughput",
    value: 100, value_stddev: null, unit: "tokens/sec", failed: false, failed_reason: null, failure_source: null,
    model_params_total_millions: 1000, model_params_active_millions: 1000,
    model_descriptor: gguf("Tiny-1B"), runtime_descriptor: MAC, runtime_descriptor_sha256: null, model_descriptor_sha256: null, model_flags: null,
    runtime_flags: null, benchmark_flags: null,
    model_vault_descriptor: null, requested_model_flags: null, requested_runtime_flags: null, requested_benchmark_flags: null,
    eval_metadata: null, client_extras: null, parameters: { prefill_tokens: 256 }, observations: {}, client_id: "client-a", device_name: "Bench A",
    device: {}, client_version: null, submitted_at: null,
    scored_at: null, device_time_ms: null, rescored_from: null, run_submitted_at: null,
    score_runtime_version: null, ...over,
  };
}

/** One job's rows, a metric each. */
function job(type: string, metrics: [string, number | null, string][], over: Partial<ResultRow> = {}): ResultRow[] {
  next++;
  const id = `job-${next}`;
  return metrics.map(([metric, value, unit]) => row({ job_id: id, benchmark_type: type, metric, value, unit, ...over }));
}

/** How clients#1567 produces an ASR row it sized and counted exactly, on a clip the model is documented to take. */
const ASR_EXACT = { asr_context_sizing: "exact", asr_audio_tokens_source: "calibrated",
                    asr_beyond_model_audio_limit: false };

const failedRow = (over: Partial<ResultRow> = {}) => row({
  value: null, failed: true, failure_source: "all_clients_denied", client_id: "system", device_name: null, ...over,
});

const none: ReportFilters = {
  benchmark: "", metric: "", device: [], drop: [], model: [], per: "", params: {}, status: "",
};
/** What `filters` draw, on the tab of the first measured row's type unless they name one. */
const show = (rows: ResultRow[], filters: Partial<ReportFilters> = {}, fleet: FleetDetail = "full") =>
  view(index(rows, fleet), { ...none, benchmark: familyOf(rows.find((r) => !r.failed) ?? row()), ...filters });
const lines = (charts: unknown[]) => charts as LineChart[];
const bars = (charts: unknown[]) => charts as BarChart[];
/** A panel's heading: its title, then its runtime where that has a line of its own, the `runtime = ` lead aside. */
const headed = (p: LineChart["panels"][number]) =>
  [p.title, ...p.runtime.map((line) => partText(line).replace(/^runtime = /, ""))].join(" · ");
const medians = (chart: LineChart, panel = 0) =>
  chart.panels[panel]!.series.map((s) => s.cells.map((cell) => cell?.median ?? null));

/** A runtime's label, off the text the report keys it by. */
const label = (d: Runtime) => canonical(d);
const MAC_A = `Bench A · ${label(MAC)}`;
const MAC_B = `Bench B · ${label(MAC)}`;
const [MAC_RT, BOX_RT, PHONE_RT, VLLM_RT, SGLANG_RT] = [MAC, BOX, PHONE, VLLM, SGLANG].map(label);
/** A model's label: the URI of its descriptor. */
/** A model as the filter picks it and a legend names it: its descriptor as stored. */
const D = (name: string, quant?: string) => canonical(gguf(name, quant));
const DT = (name: string) => canonical(torch(name));
const [G, T] = [D, DT];
const TINY_DESCRIPTOR = gguf("Tiny-1B");

describe("identity from descriptors", () => {
  test("an unknown descriptor keys by its stored value: an object's canonical JSON, a text as is", () => {
    const stored = { type: "catalog_defined", name: "x" };
    expect(descriptorText({ type: "unknown", value: stored })).toBe(canonical(stored));
    expect(descriptorText({ type: "unknown", value: "llama.cpp b100" })).toBe("llama.cpp b100");
    expect(descriptorText(gguf("Tiny-1B"))).toBe(canonical(gguf("Tiny-1B")));
    expect(descriptorText(null)).toBe("");
    const rows = [row({ runtime_descriptor: { type: "unknown", value: stored } }), row({ runtime_descriptor: null })];
    expect([...index(rows, "full").columns].sort()).toEqual(["Bench A · no runtime", `Bench A · ${canonical(stored)}`]);
  });

  test("a model is its descriptor as stored, which labels it too; none reads `no model`", () => {
    const first = canonical(gguf("Tiny-1B"));
    const noted = canonical({ ...gguf("Tiny-1B"), note: "x" });
    const labels = modelLabels([first, noted, first, "not json", ""]);
    const named = (id: string, label: string) => ({ id, label, parts: [label] });
    expect([...labels.values()]).toEqual([
      named(first, first), named(noted, noted), named("not json", "not json"), named("", "no model"),
    ]);
  });

  test("a vault model is labelled by its custom label, else its resource's name where the read names it, then its descriptor", () => {
    const vault = canonical({ type: "gguf_text", source: "vault_url", url: "/vault/resources/vault_1/content",
                              principal_id: "org_acme" });
    const pinned = canonical({ ...JSON.parse(vault), sha256: "ab" });
    const labels = modelLabels([vault, pinned, canonical(gguf("Tiny-1B"))], { vault_1: "acme ACMELM q4" });
    expect([...labels.values()].map((model) => model.label))
      .toEqual([`acme ACMELM q4 ${vault}`, `acme ACMELM q4 ${pinned}`, canonical(gguf("Tiny-1B"))]);
    expect(modelLabels([vault]).get(vault)!.label).toBe(vault);
    // A source's own label needs no name from the read.
    const labelled = canonical({ ...JSON.parse(vault), custom_label: "acme ACMELM q8" });
    expect(modelLabels([labelled]).get(labelled)!.label).toBe(`acme ACMELM q8 ${labelled}`);
    // Labelled or not, one file is one model: the first seen, under the label either gives.
    const both = modelLabels([vault, labelled]);
    expect(both.get(labelled)).toBe(both.get(vault));
    const named = `acme ACMELM q8 ${vault}`;
    expect(both.get(vault)).toEqual({ id: vault, label: named, parts: [named] });
  });

  test("a descriptor stored as JSON text keys as its object does, whatever its keys' order", () => {
    const reordered = JSON.stringify(Object.fromEntries(Object.entries(gguf("Tiny-1B")).reverse()));
    expect(descriptorText({ type: "unknown", value: reordered } as never)).toBe(canonical(gguf("Tiny-1B")));
    expect(descriptorText({ type: "unknown", value: "not json" } as never)).toBe("not json");
  });

  test("a runtime reads as its descriptor as stored; one with and without its URL's scheme is one", () => {
    const measured = canonical(archive("https://files.example/llama-x1.tar.gz"));
    const elsewhere = canonical(archive("https://mirror.example/llama-x1.tar.gz"));
    const labels = runtimeLabels([measured, canonical(archive("files.example/llama-x1.tar.gz")), elsewhere, "not json", ""]);
    expect([...labels.values()]).toEqual([measured, measured, elsewhere, "not json", "no runtime"]);
  });

  test("a vl_spec_decode job is speculative when any of its rows names a drafter metric, failed ones too", () => {
    const rows = [
      ...job("vl_spec_decode", [["decode_throughput", 100, "tokens/sec"], ["accept_length", 3, "tokens"]]),
      ...job("vl_spec_decode", [["decode_throughput", 90, "tokens/sec"]]),
      ...job("vl_spec_decode", [["decode_throughput", 95, "tokens/sec"]]),
    ];
    rows.push(failedRow({ job_id: rows[3]!.job_id, benchmark_type: "vl_spec_decode", metric: "draft_acceptance_rate" }));
    expect(speculativeJobs(rows)).toEqual(new Set([rows[0]!.job_id, rows[3]!.job_id]));
    const drawn = show(rows.map((r) => ({ ...r, model_descriptor: speculative(), runtime_descriptor: SGLANG })),
      { metric: "decode_throughput" });
    expect(bars(drawn.charts)[0]!.bars.map((b) => [b.label, b.cell.values])).toEqual(
      [["mode=baseline", [90]], ["mode=speculative", [95, 100]]]);
  });

  test("a column is a device and runtime, devices withheld below the full tier, and an eval's runtime alone", () => {
    const rows = [
      row({ device_name: "Bench B" }), row({ device_name: "Phone", runtime_descriptor: PHONE }), row({ runtime_descriptor: BOX }),
      failedRow(),
      row({ benchmark_type: "eval", metric: "accuracy", unit: "ratio", value: 0.5, runtime_descriptor: VLLM,
            model_descriptor: torch("Big-3B"), parameters: { eval_id: "quiz" } }),
    ];
    const [mac, box, phone, vllm] = [MAC, BOX, PHONE, VLLM].map(label);
    expect(index(rows, "full").columns).toEqual([`Phone · ${phone}`, `Bench A · ${box}`, `Bench B · ${mac}`, vllm]);
    const withheld = rows.map((r) => ({ ...r, device_name: null, client_id: null }));
    expect(index(withheld, "withheld").columns).toEqual([
      `devices withheld · ${phone}`, `devices withheld · ${mac}`, `devices withheld · ${box}`, vllm,
    ]);
  });

  test("a device column is a device type: devices alike in make chart as one, whatever each is named", () => {
    const phone = { form_factor: "phone", os_name: "iOS", os_version: "27.0.1", chip_model: "Apple A19 Pro", ram_bytes: 12291735552 };
    const rows = [
      row({ device_name: "iPhone 17 Pro", client_id: "a", device: phone, value: 100 }),
      // A run's state and its OS build are not its type.
      row({ device_name: "bos-17pro-1", client_id: "b", device: { ...phone, battery_level: 20, os_build: "24A446" }, value: 110 }),
      row({ device_name: "bos-17pro-1", client_id: "b", device: { ...phone, os_version: "27.1" }, value: 200 }),
    ];
    const mac = label(MAC);
    // A column prints the type's short name: its chip and RAM, and here the OS, which alone tells the two apart.
    const [now, later] = ["27.0.1", "27.1"].map((os) => `Apple A19 Pro · 11.4 GB RAM · iOS ${os} · ${mac}`);
    expect(index(rows, "full").columns).toEqual([now, later]);
    expect(index(rows, "full").devices).toEqual(["27.0.1", "27.1"].map((os) => `Apple A19 Pro · 11.4 GB RAM · iOS ${os} · phone`));
    const drawn = view(index(rows, "full"), { ...none, benchmark: "prefill_throughput" });
    expect(bars(drawn.charts)[0]!.bars.map((b) => [b.label, b.cell.values])).toEqual([[later, [200]], [now, [100, 110]]]);
    // Named by the device type alone: the list prints the runtime both bars share.
    expect(bars(drawn.charts)[0]!.bars.map((b) => b.name))
      .toEqual(["Apple A19 Pro · 11.4 GB RAM · iOS 27.1", "Apple A19 Pro · 11.4 GB RAM · iOS 27.0.1"]);
  });

  test("a bar names its column by what the section's columns do not share: device type, runtime name", () => {
    const rows = [row({ value: 10 }), row({ device_name: "Bench B", runtime_descriptor: BOX, value: 20 }),
                  row({ device_name: "Bench B", value: 30 })];
    const drawn = view(index(rows, "full"), { ...none, benchmark: "prefill_throughput" });
    expect(bars(drawn.charts)[0]!.bars.map((b) => [b.name, b.label])).toEqual([
      [`Bench B · ${MAC_RT}`, MAC_B], [`Bench B · ${BOX_RT}`, `Bench B · ${BOX_RT}`], [`Bench A · ${MAC_RT}`, MAC_A]]);
  });

  test("a device type's short name is its chip and RAM, and only what tells it from another of that chip and RAM", () => {
    const of = (device: ResultRow["device"], device_name: string | null = "x") => ({ device_name, device });
    const mac = { chip_model: "Apple M3 Ultra", ram_bytes: 96 * 2 ** 30, os_name: "macOS", os_version: "27.0.1", form_factor: "desktop" };
    const box = { chip_model: "Ryzen 9", ram_bytes: 64 * 2 ** 30, os_name: "Linux", form_factor: "desktop" };
    const types = [
      of(mac), of({ ...mac, ram_bytes: 48 * 2 ** 30 }), of({ ...box, gpu_model: "RTX 4090" }), of({ ...box, gpu_model: "RTX 3060" }),
      of(box), of({}, "Bench A"), of({}, null),
    ];
    expect(types.map((type) => deviceNames(types).get(deviceType(type)))).toEqual([
      "Apple M3 Ultra · 96.0 GB RAM", "Apple M3 Ultra · 48.0 GB RAM", "Ryzen 9 · 64.0 GB RAM · GPU RTX 4090",
      "Ryzen 9 · 64.0 GB RAM · GPU RTX 3060", "Ryzen 9 · 64.0 GB RAM", "Bench A", "no device",
    ]);
  });

  test("a device type names the make a row reports, and a row that reports none goes by its name", () => {
    const box = { chip_model: "Ryzen 9", ram_bytes: 64 * 2 ** 30, os_name: "Linux", form_factor: "desktop" };
    expect([
      { device_name: "Box", device: { ...box, gpu_model: "RTX 4090", gpu_vram_bytes: 24 * 2 ** 30, npu_model: "XDNA" } },
      { device_name: "Box", device: box }, { device_name: "Bench A", device: { power_state: "charging" } },
      { device_name: null, device: {} },
    ].map(deviceType)).toEqual([
      "Ryzen 9 · 64.0 GB RAM · GPU RTX 4090 24.0 GB · NPU XDNA · Linux · desktop", "Ryzen 9 · 64.0 GB RAM · Linux · desktop",
      "Bench A", "no device",
    ]);
  });
});

/** A memory job's pools, in GB, at `tokens`. */
const memory = (device: string, runtime: Runtime, pools: Record<string, number>, tokens = 256, model?: Model) => job(
  "max_memory_usage",
  Object.entries(pools).map(([pool, gb]): [string, number, string] => [`max_${pool}_usage`, gb * 1e9, "bytes"]),
  { device_name: device, runtime_descriptor: runtime, parameters: { prefill_tokens: tokens }, ...(model ? { model_descriptor: model } : {}) },
);

describe("families and metrics", () => {
  test("a chart is headed as the menu names it: the metric, the type first where two share it", () => {
    const rows = [
      ...job("prefill_throughput", [["prefill_throughput", 900, "tokens/sec"], ["ttft", 300, "ms"]]),
      ...job("vl_throughput", [["ttft", 400, "ms"], ["e2e_latency", 900, "ms"], ["end_to_end_latency", 950, "ms"]]),
      ...job("decode_throughput", [["decode_throughput", 40, "tokens/sec"]]),
      ...job("end_to_end_latency", [["end_to_end_latency", 1300, "ms"]]),
    ];
    const ix = index(rows, "full");
    // A shared name keeps no prefix where it already says its type: `E2E latency` is the E2E latency type's.
    // VL's sum and its measured span read apart.
    // VL throughput's come in its text twins' order, its sum last.
    expect([...chartNames(ix.benchmarks).values()]).toEqual(["Prefill throughput", "Prefill · time to first token",
      "Decode throughput", "E2E latency", "VL throughput · time to first token", "VL throughput · E2E latency",
      "Prefill + decode time"]);
    // The page's charts are those, then the checks, text then VL.
    expect(chartRefs(ix).map((c) => [c.name, c.family.key, c.metric?.key]).slice(-3)).toEqual([
      ["Prefill + decode time", "vl_throughput", "e2e_latency"], ["E2E latency vs prefill + decode", "e2e_check", "excess"],
      ["VL E2E latency vs prompt + decode", "vl_e2e_check", "excess"],
    ]);
  });

  test("families come in the report's order, each opening on its own metric", () => {
    const rows = [
      ...job("max_memory_usage", [["max_host_usage", 2e9, "bytes"]]),
      ...job("prefill_throughput", [["prefill_throughput", 900, "tokens/sec"], ["ttft", 300, "ms"]]),
      row({ benchmark_type: "custom_probe", metric: "beta", unit: "x" }),
      row({ benchmark_type: "custom_probe", metric: "alpha", unit: "x" }),
      row({ benchmark_type: "custom_probe", metric: "alpha", unit: "x", parameters: { prefill_tokens: 512 } }),
    ];
    const ix = index(rows, "full");
    expect(ix.families.map((f) => [f.key, f.label, f.metric])).toEqual([
      ["prefill_throughput", "prefill throughput", "prefill_throughput"],
      ["e2e_check", "E2E latency vs prefill + decode", null],
      ["max_memory_usage", "peak memory", "peak_memory"],
      ["custom_probe", "custom_probe", "alpha"],
    ]);
    // Host is every column's pool, so its own chart would repeat peak memory's.
    expect(ix.families[2]!.metrics.map((m) => [m.key, m.unit, m.lower])).toEqual([["peak_memory", "GB", true]]);
    expect(ix.families[0]!.metrics.map((m) => [m.key, m.lower])).toEqual([["prefill_throughput", false], ["ttft", true]]);
  });

  test("ASR opens like VL: throughput on E2E latency, peak memory on one chart across the pools", () => {
    const rows = [
      ...job("asr_throughput", [["end_to_end_latency", 420, "ms"], ["decode_throughput", 60, "tokens/sec"],
                                ["ttft", 300, "ms"], ["prefill_throughput", 900, "tokens/sec"]]),
      ...job("asr_max_memory", [["max_host_usage", 2e9, "bytes"], ["max_gpu_usage", 3e9, "bytes"]]),
    ];
    const ix = index(rows, "full");
    expect(ix.families.map((f) => [f.key, f.label, f.metric])).toEqual([
      ["asr_throughput", "ASR throughput", "end_to_end_latency"],
      ["asr_max_memory", "ASR peak memory", "peak_memory"],
    ]);
    expect(ix.families[0]!.metrics.map((m) => m.key)).toEqual(
      ["prefill_throughput", "ttft", "decode_throughput", "end_to_end_latency"]);
    expect(ix.families[1]!.metrics.map((m) => m.key)).toEqual(["peak_memory"]);
  });

  test("Prompt tokens charts each ASR job's audio and prompt counts, and its prefill says counts are the encoder's", () => {
    const at = (name: string, ms: number, audio: number, prompt: number) =>
      job("asr_throughput", [["prefill_throughput", 900, "tokens/sec"], ["ttft", 300, "ms"]], {
        model_descriptor: gguf(name),
        parameters: { audio_duration_ms: ms, audio_sample_rate_hz: 16000, audio_channels: 1, decode_tokens: 128 },
        observations: { ...ASR_EXACT, asr_throughput_audio_tokens: audio, asr_throughput_prefill_tokens: prompt } });
    // One clip, two encoders, each making its own count of it.
    const rows = [...at("Tiny-Audio", 2500, 32, 51), ...at("Tiny-Audio", 10000, 125, 144),
                  ...at("Wide-Audio", 2500, 125, 140), ...at("Wide-Audio", 10000, 500, 515)];
    const ix = index(rows, "full");
    expect(ix.families[0]!.metrics.map((m) => m.key)).toEqual(["prefill_throughput", "prompt_tokens", "ttft"]);
    const drawn = view(ix, { ...none, benchmark: "asr_throughput", metric: "prompt_tokens" });
    const chart = lines(drawn.charts)[0]!;
    expect([chart.x, chart.panels.map((p) => p.title)])
      .toEqual(["audio_duration_ms", ["Bench A · audio tokens", "Bench A · prompt tokens"]]);
    expect([medians(chart, 0), medians(chart, 1)]).toEqual([[[32, 125], [125, 500]], [[51, 144], [140, 515]]]);
    expect(drawnRows(drawn.charts, new Set()).size).toBe(rows.length);
    // VL's counts are not ASR's: a VL key on an ASR row charts nothing.
    const misfiled = rows.map((r) => ({ ...r, observations: { ...ASR_EXACT, vl_throughput_image_tokens: 64 } }));
    expect(index(misfiled, "full").families[0]!.metrics.map((m) => m.key)).not.toContain("prompt_tokens");
    // Its prefill rate says what it counts, and that a count is the encoder's.
    const encoder = "A clip is as many audio tokens as each model's encoder makes of it, which differs between models";
    expect(show(rows).notes.some((n) => n.includes(encoder) && n.endsWith("Prompt tokens charts each model's count."))).toBe(true);
    expect(show(misfiled).notes.some((n) => n.includes(encoder) && !n.includes("Prompt tokens charts"))).toBe(true);
    expect(show(rows, { metric: "ttft" }).notes.some((n) => n.includes(encoder))).toBe(false);
  });

  test("an ASR chart runs over the clip's length before the decode length", () => {
    const rows = [2500, 10000].flatMap((ms) => [64, 256].flatMap((decode) => job("asr_throughput",
      [["ttft", ms / 10, "ms"]], { parameters: { audio_duration_ms: ms, audio_sample_rate_hz: 16000, audio_channels: 1,
                                                 decode_tokens: decode }, observations: ASR_EXACT })));
    const drawn = show(rows, { metric: "ttft" });
    expect(lines(drawn.charts)[0]!.x).toBe("audio_duration_ms");
  });

  test("an ASR chart runs over the clip's length before the text beside it", () => {
    const rows = [2500, 5000, 15000, 29900].flatMap((ms) => [0, 1024].flatMap((text) => job("asr_throughput",
      [["ttft", ms / 10 + text / 100, "ms"]], { parameters: { audio_duration_ms: ms, audio_sample_rate_hz: 16000,
                                                             audio_channels: 1, text_tokens: text, decode_tokens: 256 },
                                               observations: ASR_EXACT })));
    const chart = lines(show(rows, { metric: "ttft" }).charts)[0]!;
    expect([chart.x, chart.rungs.map((r) => r.x)]).toEqual(["audio_duration_ms", [2500, 5000, 15000, 29900]]);
    expect(drawnRows(show(rows, { metric: "ttft" }).charts, new Set()).size).toBe(rows.length);
  });

  test("ASR throughput compares only runtime-counted jobs, never mixing regimes in a cell, and counts the rest", () => {
    const clip = { audio_duration_ms: 2500, audio_sample_rate_hz: 16000, audio_channels: 1, decode_tokens: 128 };
    // One model, one clip, four regimes at the same benchmark parameters. A pinned context is a flag the cell asked for.
    const at = (sizing: string, source: string, ttft: number, audio: number, pinned = false) =>
      job("asr_throughput", [["ttft", ttft, "ms"], ["prefill_throughput", 1000 / ttft, "tokens/sec"]], {
        parameters: clip, requested_runtime_flags: (pinned ? { ctx_size: 4096 } : null) as never,
        observations: { asr_context_sizing: sizing, asr_audio_tokens_source: source,
                        asr_throughput_audio_tokens: audio, asr_throughput_prefill_tokens: audio + 19 } });
    const rows = [...at("exact", "calibrated", 100, 32), ...at("upper_bound", "measured", 900, 36),
                  ...at("pinned", "calibrated", 200, 32, true), ...at("pinned", "measured", 800, 36, true)];
    const ttft = show(rows, { metric: "ttft" });
    // Each bar a cell: the chart, whether its column is the pinned one, and the cell's values.
    const cells = (v: ReturnType<typeof show>) => bars(v.charts).flatMap((c) => c.bars.map((b) =>
      [c.title, b.label.includes("ctx_size"), b.cell?.values]));
    // The exact and the pinned calibrated jobs, each in its own column; neither measured job reaches a cell.
    expect(cells(ttft)).toEqual([["ttft", false, [100]], ["ttft", true, [200]]]);
    expect(drawnRows(ttft.charts, new Set()).size).toBe(2);
    const note = "Only jobs whose audio tokens the runtime counted (calibrated) are compared: 2 of 4 jobs. A measured "
      + "count includes the audio markers, and unless the cell pinned its context, that context was sized from an "
      + "upper bound, so the job counted another way and may have run another workload.";
    expect(ttft.notes).toContain(note);
    // The token panels, likewise: marker-free counts only.
    const tokens = show(rows, { metric: "prompt_tokens" });
    expect(cells(tokens)).toEqual([["audio tokens", false, [32]], ["audio tokens", true, [32]],
                                   ["prompt tokens", false, [51]], ["prompt tokens", true, [51]]]);
    expect(tokens.notes).toContain(note);
    // All calibrated: nothing set aside, no note. None calibrated: the tab says why it draws nothing.
    const counted = [...rows.slice(0, 2), ...rows.slice(4, 6)];
    expect(show(counted, { metric: "ttft" }).notes.some((n) => n.startsWith("Only jobs whose audio"))).toBe(false);
    const measured = show([...rows.slice(2, 4), ...rows.slice(6)], { metric: "ttft" });
    expect([measured.charts, measured.empty]).toEqual([[], "No job's audio tokens were counted by the runtime."]);
    // A VL throughput row, without the ASR keys, is not held to an ASR rule.
    const vl = job("vl_throughput", [["ttft", 5, "ms"]], { parameters: { num_images: 1 } });
    expect(show(vl, { metric: "ttft" }).charts.length).toBe(1);
  });

  test("ASR peak memory compares only exact-sized peaks, and says how many of its jobs those are", () => {
    const at = (name: string, ms: number, gb: number, sizing: string | null) =>
      job("asr_max_memory", [["max_host_usage", gb * 1e9, "bytes"]], {
        model_descriptor: gguf(name),
        parameters: { audio_duration_ms: ms, audio_sample_rate_hz: 16000, audio_channels: 1, decode_tokens: 256 },
        observations: sizing ? { asr_context_sizing: sizing } : {} });
    const rows = [...at("Tiny-Audio", 2500, 1, "exact"), ...at("Tiny-Audio", 10000, 1.5, "exact"),
                  ...at("Wide-Audio", 2500, 3, "upper_bound"), ...at("Wide-Audio", 10000, 3, "pinned"),
                  ...at("Old-Audio", 2500, 9, null)];
    const drawn = show(rows);
    expect(lines(drawn.charts)[0]!.legend.map((s) => s.label)).toEqual([G("Tiny-Audio")]);
    expect(drawn.notes).toContain("Only peaks whose context was sized from the clip's own audio tokens (exact) are "
      + "compared: 2 of 5 jobs. An upper-bound context holds KV for audio the clip does not have, and a pinned one is "
      + "the plan's size, so neither is the clip's peak.");
    // All exact: nothing set aside, no note.
    const exact = show(rows.slice(0, 2));
    expect(exact.notes.some((n) => n.startsWith("Only peaks"))).toBe(false);
    // None exact: the tab says why it draws nothing.
    const none_ = show(rows.slice(2));
    expect([none_.charts, none_.empty]).toEqual([[], "No job's context was sized exactly."]);
    // A text peak, without the ASR keys, compares as before.
    const text = memory("Box", BOX, { host: 1 });
    expect(show(text).charts.length).toBe(1);
  });

  test("an ASR point past the model's documented audio limit stays in the comparison, marked", () => {
    const clip = (ms: number) => ({ audio_duration_ms: ms, audio_sample_rate_hz: 16000, audio_channels: 1 });
    const at = (name: string, ms: number, ttft: number, beyond: boolean) =>
      job("asr_throughput", [["ttft", ttft, "ms"]], { model_descriptor: gguf(name),
        parameters: { ...clip(ms), decode_tokens: 256 },
        observations: { ...ASR_EXACT, asr_beyond_model_audio_limit: beyond } });
    // Gemma 4's card documents 30 s, so its long clip is flagged; the other model's is within its limit.
    const rows = [...at("Gemma-4-E2B", 29900, 100, false), ...at("Gemma-4-E2B", 119900, 400, true),
                  ...at("Tiny-Audio", 29900, 120, false), ...at("Tiny-Audio", 119900, 480, false)];
    const drawn = show(rows, { metric: "ttft" });
    const chart = lines(drawn.charts)[0]!;
    expect(chart.rungs.map((r) => r.x)).toEqual([29900, 119900]);
    const marks = chart.panels[0]!.series.map((s) => [s.label.includes("Gemma") ? "Gemma" : "Tiny",
      s.cells.map((c) => [c?.median ?? null, c?.beyond === true])]);
    // Both models drawn, every value kept; only the flagged cell is marked.
    expect(marks.sort()).toEqual([["Gemma", [[100, false], [400, true]]], ["Tiny", [[120, false], [480, false]]]]);
    expect(drawnRows(drawn.charts, new Set()).size).toBe(rows.length);
    const note = (v: ReturnType<typeof show>) => v.notes.find((n) => n.includes(BEYOND_LIMIT));
    expect(note(drawn)).toMatch(/^Marked points \(a dashed ring; on a bar, a dashed outline\) are beyond the model's audio limit/);
    // Nothing flagged: nothing marked, no note.
    const within = show(rows.filter((r) => r.observations.asr_beyond_model_audio_limit === false), { metric: "ttft" });
    expect(lines(within.charts)[0]!.panels[0]!.series.flatMap((s) => s.cells).some((c) => c?.beyond)).toBe(false);
    expect(note(within)).toBeUndefined();
    // A peak drawn as bars, one clip length: the flagged bar is marked too.
    const peak = (name: string, beyond: boolean) => job("asr_max_memory", [["max_host_usage", 3e9, "bytes"]],
      { model_descriptor: gguf(name), parameters: { ...clip(119900), decode_tokens: 256 },
        observations: { asr_context_sizing: "exact", asr_beyond_model_audio_limit: beyond } });
    const peaks = show([...peak("Gemma-4-E2B", true), ...peak("Tiny-Audio", false)]);
    expect(bars(peaks.charts).flatMap((c) => c.bars.map((b) => [b.label.includes("Gemma"), b.cell.beyond === true]))
      .sort()).toEqual([[false, false], [true, true]]);
    expect(note(peaks)).toBeDefined();
  });

  test("VL throughput's metrics run as their text twins' tabs do, any other after by name; it opens on E2E latency", () => {
    const metrics: [string, number, string][] = [
      ["end_to_end_latency", 9, "ms"], ["decode_throughput", 4, "tokens/sec"], ["ttft", 3, "ms"], ["image_encode", 2, "ms"],
      ["prefill_throughput", 1, "tokens/sec"]];
    const vl = index(job("vl_throughput", metrics), "full").families[0]!;
    expect([vl.metrics.map((m) => m.key), vl.metric]).toEqual([
      ["prefill_throughput", "ttft", "decode_throughput", "end_to_end_latency", "image_encode"], "end_to_end_latency"]);
    // A client that does not time the request sends none: it opens on prefill throughput.
    expect(index(job("vl_throughput", metrics.slice(1)), "full").families[0]!.metric).toBe("prefill_throughput");
  });

  test("VL prefill throughput says what it counts, which the text type's need not", () => {
    const counts = "Counts the whole prompt, image and template tokens included, over a time to first token that holds "
      + "the vision encoding";
    const rows = job("vl_throughput", [["prefill_throughput", 900, "tokens/sec"], ["ttft", 300, "ms"]]);
    expect(show(rows).notes).toContain(`${counts}.`);
    // Where the jobs observed their counts, it points at the chart of them.
    const observed = rows.map((r) => ({ ...r, observations: { vl_throughput_prefill_tokens: 1098 } }));
    expect(show(observed).notes).toContain(`${counts}. Prompt tokens charts each model's count.`);
    expect(show(rows, { metric: "ttft" }).notes.some((n) => n.startsWith(counts))).toBe(false);
    expect(show(job("prefill_throughput", [["prefill_throughput", 900, "tokens/sec"]])).notes.some((n) => n.startsWith(counts)))
      .toBe(false);
  });

  test("Prompt tokens charts each VL job's observed counts once, a panel per count, after prefill throughput", () => {
    const at = (name: string, images: number, text: number, image: number, prompt: number) =>
      job("vl_throughput", [["prefill_throughput", 900, "tokens/sec"], ["ttft", 300, "ms"]], {
        model_descriptor: gguf(name), parameters: { num_images: images, text_tokens: text, decode_tokens: 256 },
        observations: { vl_throughput_image_tokens: image, vl_throughput_prefill_tokens: prompt } });
    const rows = [...at("Tiny-VL", 1, 0, 64, 75), ...at("Tiny-VL", 1, 1024, 64, 1098), ...at("Tiny-VL", 5, 1024, 320, 1362),
                  ...at("Tiny-VL", 16, 1024, 1024, 2088), ...at("Lean-VL", 1, 0, 32, 43), ...at("Lean-VL", 1, 1024, 32, 1066),
                  ...at("Lean-VL", 5, 1024, 96, 1138), ...at("Lean-VL", 16, 1024, 256, 1298)];
    const ix = index(rows, "full");
    expect(ix.families[0]!.metrics.map((m) => m.key)).toEqual(["prefill_throughput", "prompt_tokens", "ttft"]);
    const drawn = view(ix, { ...none, benchmark: "vl_throughput", metric: "prompt_tokens" });
    // Lines over the images at 1024 text tokens, a panel per count; the image alone after them, a bar chart per count.
    const [line, ...apart] = drawn.charts;
    expect([line!.form, line!.group, lines([line])[0]!.panels.map((p) => p.title)])
      .toEqual(["line", "text_tokens=1024", ["Bench A · image tokens", "Bench A · prompt tokens"]]);
    expect([medians(lines([line])[0]!, 0), medians(lines([line])[0]!, 1)])
      .toEqual([[[32, 96, 256], [64, 320, 1024]], [[1066, 1138, 1298], [1098, 1362, 2088]]]);
    expect(bars(apart).map((c) => [c.group, c.title, c.unit, c.bars.map((b) => b.cell.median)])).toEqual([
      ["text_tokens=0", "num_images=1 · image tokens", "tokens", [32, 64]],
      ["text_tokens=0", "num_images=1 · prompt tokens", "tokens", [43, 75]],
    ]);
    // A job's counts are one reading, though each of its rows carries them.
    expect(drawn.notes.some((n) => n.includes("hold more than one result"))).toBe(false);
    // Every row of a job is behind its one point, as Jobs lists them.
    expect(drawnRows(drawn.charts, new Set()).size).toBe(rows.length);
    // A count prints as a count.
    expect(cellText(bars(apart)[0]!.bars[0]!.cell)).toBe("32");
    // Without observed counts there is no chart of them.
    const bare = rows.map((r) => ({ ...r, observations: {} }));
    expect(index(bare, "full").families[0]!.metrics.map((m) => m.key)).not.toContain("prompt_tokens");
    // A job that observed no count draws nothing; where none observed an image's, the prompt's panel is the only one.
    const partial = [...rows.filter((r) => r.model_descriptor && JSON.stringify(r.model_descriptor).includes("Tiny"))
      .map((r) => ({ ...r, observations: { vl_throughput_prefill_tokens: r.observations.vl_throughput_prefill_tokens } })),
                     ...bare.filter((r) => JSON.stringify(r.model_descriptor).includes("Lean"))];
    const counted = view(index(partial, "full"), { ...none, benchmark: "vl_throughput", metric: "prompt_tokens" });
    expect([lines(counted.charts)[0]!.panels.map((p) => p.title), lines(counted.charts)[0]!.legend.length])
      .toEqual([["Bench A · prompt tokens"], 1]);
    // One model's image alone collapses to one chart of both counts, titled by both.
    const one = view(index(at("Tiny-VL", 1, 0, 64, 75), "full"), { ...none, benchmark: "vl_throughput", metric: "prompt_tokens" });
    expect(bars(one.charts).map((c) => [c.title, c.bars.map((b) => [b.label, b.cell.median])]))
      .toEqual([["image and prompt tokens", [["image tokens", 64], ["prompt tokens", 75]]]]);
  });

  test("a family the address does not name, or the plan lacks, is none: no fallback draws another", () => {
    const ix = index([row(), ...job("max_memory_usage", [["max_host_usage", 2e9, "bytes"]])], "full");
    for (const benchmark of ["", "decode_throughput", "e2e_check"]) {
      const drawn = view(ix, { ...none, benchmark, per: "4" });
      expect([drawn.family, drawn.metric, drawn.charts, drawn.filters]).toEqual([null, null, [], { ...none, per: "4" }]);
    }
  });

  test("a family that measured nothing draws only its failed jobs", () => {
    const rows = [row(), failedRow({ benchmark_type: "decode_throughput", metric: "decode_throughput" }),
                  failedRow({ benchmark_type: "decode_throughput", metric: "decode_throughput" })];
    const ix = index(rows, "full");
    const drawn = view(ix, { ...none, benchmark: "decode_throughput" });
    expect(drawn.unmeasured).toBe(2);
    expect(drawn.charts).toEqual([]);
    expect(ix.benchmarks.find((f) => f.key === "decode_throughput"))
      .toMatchObject({ measured: 0, failed: 2, causes: [["every client denied it", 2]], models: 0, columns: [] });
  });

  test("a series with no value in the ranked pool ranks by its own pools, not first", () => {
    const at = (device: string, runtime: Runtime, pools: Record<string, number>, name: string) =>
      [256, 512].flatMap((tokens) => memory(device, runtime, pools, tokens, gguf(name)));
    const rows = [...at("Mac", MAC, { host: 21 }, "Mid-2B"), ...at("Mac", MAC, { host: 9 }, "Low-1B"),
                  ...at("Box", BOX, { gpu: 30 }, "Gpu-3B")];
    const legend = lines(show(rows).charts)[0]!.legend.map((s) => /Low|Mid|Gpu/.exec(s.label)?.[0]);
    expect(legend).toEqual(["Low", "Mid", "Gpu"]);
  });

  test("peak memory draws a panel per device type and pool it reported, over the x it ran", () => {
    const small = gguf("Small-1B");
    const rows = [
      ...memory("Mac", MAC, { host: 21 }), ...memory("Mac", MAC, { host: 22 }, 512),
      ...memory("Mac", MAC, { host: 9 }, 256, small),
      ...memory("Box", BOX, { host: 0.39, gpu: 20.9 }), ...memory("Box", BOX, { host: 0.4, gpu: 21.5 }, 512),
      ...memory("Box", BOX, { host: 0.3, gpu: 8 }, 256, small),
      ...memory("Phone", PHONE, { host: 1, npu: 2.49 }), ...memory("Phone", PHONE, { host: 1.1, npu: 2.6 }, 512),
    ];
    const drawn = show(rows);
    expect(drawn.metric?.key).toBe("peak_memory");
    const chart = lines(drawn.charts)[0]!;
    expect(chart.x).toBe("prefill_tokens");
    // A pool its device did not report draws no panel.
    expect(chart.panels.map((p) => p.title)).toEqual([
      "Phone · host memory", "Phone · NPU memory", "Box · host memory", "Box · GPU memory", "Mac · host memory"]);
    const gb = (chart: LineChart, panel: number) =>
      medians(chart, panel).map((cells) => cells.map((v) => (v === null ? null : Number(v.toFixed(6)))));
    expect(gb(chart, 3)).toEqual([[20.9, 21.5], [8, null]]);
    expect(gb(chart, 2)).toEqual([[0.39, 0.4], [0.3, null]]);
    expect(drawnRows(drawn.charts, new Set()).size).toBe(rows.length);
    // One device type chosen draws its pools alone, its runtime under them by the name the plan gives it.
    const box = lines(show(rows, { device: ["Box"] }).charts)[0]!;
    expect(box.panels.map((p) => [p.title, p.runtime])).toEqual([
      ["Box · host memory", [`runtime = ${BOX_RT}`]], ["Box · GPU memory", [`runtime = ${BOX_RT}`]]]);
  });

  test("a memory job's pools are one chart, none a chart of its own", () => {
    const keys = (rows: ResultRow[]) => index(rows, "full").families.find((f) => f.key === "max_memory_usage")!
      .metrics.map((m) => m.key);
    expect(keys(memory("Box", BOX, { host: 0.4, gpu: 6 }))).toEqual(["peak_memory"]);
    expect(keys([...memory("Mac", MAC, { host: 5 }), ...memory("Box", BOX, { gpu: 6 })])).toEqual(["peak_memory"]);
  });

  test("units: bytes in GB, a scored ratio in %, the rest as reported", () => {
    const rows = [
      ...memory("Box", BOX, { host: 2, gpu: 6 }),
      row({ benchmark_type: "eval", metric: "accuracy", unit: "ratio", value: 0.25, runtime_descriptor: VLLM,
            parameters: { eval_id: "quiz" } }),
    ];
    // One prompt size draws bars, each named by its pool.
    const memoryChart = bars(show(rows).charts)[0]!;
    expect([memoryChart.unit, memoryChart.bars.map((b) => [b.label, Number(b.cell.median.toFixed(6))])])
      .toEqual(["GB", [["host memory", 2], ["GPU memory", 6]]]);
    // A chart keys its bars apart, as it draws them by key.
    expect(new Set(memoryChart.bars.map((b) => b.key)).size).toBe(2);
    const eval_ = show(rows, { benchmark: "eval-quiz" });
    expect(bars(eval_.charts)[0]).toMatchObject({ unit: "%", atLeast: 100 });
    expect(bars(eval_.charts)[0]!.bars[0]!.cell.median).toBe(25);
  });

  test("an ASR error rate is a % where lower is better, named by its unit", () => {
    const rows = [
      row({ benchmark_type: "asr_eval", metric: "wer", unit: "ratio", value: 0.031, runtime_descriptor: MAC,
            parameters: { asr_task: "asr_esb_librispeech_test_clean" } }),
      // A task mgmt doesn't know scores as `error_rate`.
      row({ benchmark_type: "asr_eval", metric: "error_rate", unit: "ratio", value: 0.08, runtime_descriptor: MAC,
            parameters: { asr_task: "asr_fleurs_sw_ke" } }),
      row({ benchmark_type: "asr_eval", metric: "error_rate", unit: "ratio", value: 0.09, runtime_descriptor: MAC,
            parameters: { asr_task: "asr_fleurs_sw_ke" } }),
    ];
    const asr = index(rows, "full").families.find((f) => f.key === "asr_eval")!;
    expect([asr.label, asr.short]).toEqual(["ASR error rate", "ASR eval"]);
    expect(asr.metrics.map((m) => [m.key, m.unit, m.lower])).toEqual([["error_rate", "%", true], ["wer", "%", true]]);
    // Opens on WER, whichever has more rows.
    expect(asr.metric).toBe("wer");
    expect(chartNames([asr]).get(`asr_eval\nwer`)).toBe("Word error rate");
    // The character error rate is retired: a `cer` row is no ASR error rate.
    const cer = index([row({ benchmark_type: "asr_eval", metric: "cer", unit: "ratio", value: 0.08,
                             runtime_descriptor: MAC, parameters: { asr_task: "asr_fleurs_ja_jp" } })], "full")
      .families.find((f) => f.key === "asr_eval")!;
    expect(cer.metrics.map((m) => [m.key, m.unit, m.lower])).toEqual([["cer", "ratio", false]]);
  });
});

describe("parameters, axes and series", () => {
  /** One decode_throughput row of `value` at `parameters`; `over` sets the model, device and the rest. */
  const decode = (value: number, parameters: Record<string, unknown>, over: Partial<ResultRow> = {}) =>
    row({ benchmark_type: "decode_throughput", metric: "decode_throughput", value, parameters, ...over });

  test("a parameter varies with two values, is constant with one, and is counted where some rows lack it", () => {
    const rows = [
      decode(10, { prefill_tokens: 256, decode_tokens: 100, kind: "a" }),
      decode(11, { prefill_tokens: 512, decode_tokens: 100, kind: "a" }),
      decode(20, { prefill_tokens: 256, decode_tokens: 100, kind: "b", temperature: 0.5 }),
      decode(21, { prefill_tokens: 512, decode_tokens: 100, kind: "b" }),
      decode(30, { prefill_tokens: 256, decode_tokens: 100 }),
      decode(31, { prefill_tokens: 512, decode_tokens: 100 }),
    ];
    const drawn = show(rows);
    expect(lines(drawn.charts)[0]!.x).toBe("prefill_tokens");
    expect(lines(drawn.charts)[0]!.legend.map((s) => s.label)).toEqual(["kind=unset", "kind=b", "kind=a"]);
    expect(drawn.caption).toEqual([G("Tiny-1B"), "decode_tokens=100",
                                   "temperature=0.5 (1 of 6 jobs; unset on the rest)"]);
    expect(drawn.pins.map((c) => [c.key, c.options.map((o) => o.value)]))
      .toEqual([["p.kind", ["", '"a"', '"b"', "unset"]]]);

    const pinned = show(rows, { params: { kind: '"b"' } });
    expect(lines(pinned.charts)[0]!.legend.map((s) => s.label)).toEqual([G("Tiny-1B")]);
    expect(pinned.caption).toContain("kind=b");
    expect(pinned.caption).toContain("temperature=0.5 (1 of 2 jobs; unset on the rest)");
    expect(pinned.pins[0]!.value).toBe('"b"');
    expect(lines(show(rows, { params: { kind: "unset" } }).charts)[0]!.panels[0]!.series[0]!.cells.map((c) => c?.median))
      .toEqual([30, 31]);
  });

  test("each parameter value is a choice of its own, an empty string or a literal `unset` too", () => {
    const kinds: [unknown, number][] = [["", 10], ["unset", 20], ["1", 30], [1, 40], [undefined, 50]];
    const rows = kinds.flatMap(([kind, value]) => [256, 512].map((tokens) => decode(value + tokens / 256,
      kind === undefined ? { prefill_tokens: tokens } : { prefill_tokens: tokens, kind })));
    const options = show(rows).pins[0]!.options;
    expect(options.map((o) => [o.value, o.label])).toEqual(
      [["", "all"], ["1", "1"], ['""', ""], ['"1"', "1"], ['"unset"', "unset"], ["unset", "unset"]]);
    expect(options.slice(1).map((o) => medians(lines(show(rows, { params: { kind: o.value } }).charts)[0]!)))
      .toEqual([[[41, 42]], [[11, 12]], [[31, 32]], [[21, 22]], [[51, 52]]]);
  });

  test("a parameter that moves with x rides on it: in the rung, the caption, and no select", () => {
    const vl = (value: number, text: number, decodeTokens: number) => row({
      benchmark_type: "vl_throughput", metric: "prefill_throughput", value,
      parameters: { text_tokens: text, decode_tokens: decodeTokens, image_width: 512, num_images: 1 },
    });
    const drawn = show([vl(100, 0, 256), vl(110, 0, 256), vl(200, 1024, 64)]);
    expect(lines(drawn.charts)[0]!.x).toBe("text_tokens");
    expect(lines(drawn.charts)[0]!.rungs).toEqual([
      { x: 0, text: "0 · decode_tokens=256" }, { x: 1024, text: "1024 · decode_tokens=64" }]);
    expect(drawn.caption.at(-1)).toBe("decode_tokens rides on text_tokens");
    expect(drawn.caption).toContain("image_width=512");
    expect(drawn.pins).toEqual([]);
  });

  test("x is the first varying numeric parameter in the report's order; the rest split series", () => {
    const rows = [256, 512].flatMap((tokens) => [1, 2].map((batch) =>
      decode(tokens + batch, { prefill_tokens: tokens, batch_size: batch })));
    const drawn = show(rows);
    expect(lines(drawn.charts)[0]!.x).toBe("prefill_tokens");
    expect(lines(drawn.charts)[0]!.legend.map((s) => s.label).sort()).toEqual(["batch_size=1", "batch_size=2"]);
    expect(drawn.pins.map((c) => c.key)).toEqual(["p.batch_size"]);
  });

  /** A model's VL shapes: one image alone, then 1, 5 and 16 images over 1024 text tokens; its time grows with each. */
  const shapes = (name: string, base: number) => [[1, 0], [1, 1024], [5, 1024], [16, 1024]].map(([images, text]) => row({
    benchmark_type: "vl_throughput", metric: "ttft", unit: "ms", value: base + images! * 10 + text! / 1024,
    model_descriptor: gguf(name), parameters: { num_images: images, text_tokens: text, decode_tokens: 256 },
  }));

  test("x is the candidate that leaves the fewest series a lone point, ties in the report's order", () => {
    // Over text_tokens, 5 and 16 images would each be a lone point; over num_images only the image alone is.
    const drawn = show([...shapes("Alpha-1B", 100), ...shapes("Beta-2B", 200)]);
    expect(lines(drawn.charts)[0]!.x).toBe("num_images");
    expect(lines(drawn.charts)[0]!.rungs.map((r) => r.x)).toEqual([1, 5, 16]);
    expect(drawn.pins.map((c) => [c.key, c.options.map((o) => o.value)])).toEqual([["p.text_tokens", ["", "0", "1024"]]]);
    // A full grid leaves no lone point either way, so the report's order holds even where batch sizes outnumber lengths.
    const grid = [256, 512].flatMap((tokens) => [1, 2, 4, 8].map((batch) =>
      decode(tokens + batch, { prefill_tokens: tokens, batch_size: batch })));
    expect(lines(show(grid).charts)[0]!.x).toBe("prefill_tokens");
  });

  test("a combination none of whose series reaches two rungs draws bars after the lines, x splitting them", () => {
    const rows = [...shapes("Alpha-1B", 100), ...shapes("Beta-2B", 200)];
    const drawn = show(rows);
    const [line, bar] = drawn.charts;
    expect([line!.form, line!.group, bar!.form, bar!.group]).toEqual(["line", "text_tokens=1024", "bars", "text_tokens=0"]);
    expect(lines([line]).map((c) => [c.legend.map((s) => s.label), medians(c)])).toEqual(
      [[[G("Alpha-1B"), G("Beta-2B")], [[111, 151, 261], [211, 251, 361]]]]);
    expect(bars([bar]).map((c) => [c.title, c.bars.map((b) => [b.label, b.cell.median])])).toEqual(
      [["num_images=1", [[G("Alpha-1B"), 110], [G("Beta-2B"), 210]]]]);
    expect(drawn.caption).toEqual(["decode_tokens=256"]);
    expect(drawnRows(drawn.charts, new Set()).size).toBe(rows.length);
    // Pinned, one part draws alone, ungrouped, its pin in the caption.
    const alone = show(rows, { params: { text_tokens: "0" } });
    expect(alone.charts.map((c) => [c.form, c.group, c.title])).toEqual([["bars", undefined, "num_images=1"]]);
    expect(alone.caption).toContain("text_tokens=0");
    expect(show(rows, { params: { text_tokens: "1024" } }).charts.map((c) => [c.form, c.group]))
      .toEqual([["line", undefined]]);
    // A repeated model's cells count over both parts: three on its line, one bar.
    expect(show([...rows, ...shapes("Alpha-1B", 100)]).notes[0]).toMatch(/^4 of 8 cells hold more than one result/);
  });

  test("bars apart from the lines name their column where the section has several, though they ran on one", () => {
    const on = (device: string, of: ResultRow[]) => of.map((r) => ({ ...r, device_name: device }));
    const drawn = show([...on("Bench A", shapes("Alpha-1B", 100)), ...on("Bench B", shapes("Alpha-1B", 200).slice(1))]);
    expect(bars(drawn.charts.filter((c) => c.form === "bars"))[0]!.bars.map((b) => [b.label, b.name]))
      .toEqual([[MAC_A, "Bench A"]]);
  });

  test("past 12 panels the lines narrow to one device type, as a section without a lone combination does", () => {
    const rows = [1, 2, 3, 4, 5].flatMap((device) => Array.from({ length: 9 }, (_, m) => shapes(`M${m}-1B`, m)
      .map((r) => ({ ...r, device_name: `Bench ${device}` }))).flat());
    expect(show(rows, { per: "4" }).notes).toContain("Showing Bench 1; choose device types for the other 4.");
  });

  test("each lone combination is a group of its own, headed by its values, in value order after the lines", () => {
    // A resolution sweep at one image, then frame counts at 256 px only; no decode_tokens, so VL's own sweeps draw none.
    const vl = (width: number, images: number) => row({
      benchmark_type: "vl_throughput", metric: "ttft", unit: "ms", value: width + images,
      parameters: { image_width: width, image_height: width, num_images: images, text_tokens: 0 },
    });
    const drawn = show([...[256, 512, 1024, 2048].map((w) => vl(w, 1)), ...[80, 5, 16].map((n) => vl(256, n))]);
    expect(drawn.charts.map((c) => [c.form, c.group, c.form === "line" ? c.x : c.title])).toEqual([
      ["line", "num_images=1", "image_width"], ["bars", "num_images=5", "image_width=256"],
      ["bars", "num_images=16", "image_width=256"], ["bars", "num_images=80", "image_width=256"],
    ]);
    expect(drawn.caption).toEqual([G("Tiny-1B"), "text_tokens=0", "image_height rides on image_width"]);
  });

  test("batch sizes run at one prompt length alone draw apart, each under its own heading", () => {
    const rows = [...[256, 512, 1024].map((tokens) => decode(tokens, { prefill_tokens: tokens, batch_size: 1 })),
                  ...[2, 4].map((batch) => decode(batch, { prefill_tokens: 512, batch_size: batch }))];
    const drawn = show(rows);
    expect(drawn.charts.map((c) => [c.form, c.group, c.form === "line" ? c.x : c.title])).toEqual([
      ["line", "batch_size=1", "prefill_tokens"], ["bars", "batch_size=2", "prefill_tokens=512"],
      ["bars", "batch_size=4", "prefill_tokens=512"],
    ]);
  });

  /** The desktop acmelm-vl plans' 30 vl_throughput ids, as (width, text, decode, images); the phones run no 16 images. */
  const CATALOG = [
    ...[256, 512, 768, 1024, 1280, 2048].map((w) => [w, 0, 64, 1]),
    ...[256, 512].flatMap((w) => [1, 5, 16].flatMap((n) => [0, 1024].flatMap((t) => [64, 256].map((d) => [w, t, d, n]))))
      .filter(([, t, d, n]) => !(t === 0 && d === 64 && n === 1)),
    [1280, 1024, 64, 1], [512, 0, 16, 1],
  ];
  /** Two models' end-to-end latency at each of `points`, Beta's 100 ms slower everywhere. */
  const catalog = (points: number[][]) => ["Alpha-1B", "Beta-2B"].flatMap((name, m) =>
    points.map(([width, text, decodeTokens, images]) => row({
      benchmark_type: "vl_throughput", metric: "end_to_end_latency", unit: "ms", model_descriptor: gguf(name),
      value: 100 * m + width! + images! + text! / 1024 + decodeTokens! / 1000,
      parameters: { image_width: width, image_height: width, text_tokens: text, decode_tokens: decodeTokens, num_images: images },
    })));
  const SIZE = "By image size · decode_tokens=64 · num_images=1 · text_tokens=0";
  const COUNT = "By image count · decode_tokens=64 · text_tokens=0";
  const OTHER = "Other parameters";
  const behind = (chart: LineChart) => chart.panels.flatMap((p) => p.series.flatMap((s) => s.points.flat()));

  test("VL throughput draws image size and image count apart, at no text and 64 decode tokens, then every other point", () => {
    expect(CATALOG).toHaveLength(30);
    const rows = catalog(CATALOG);
    const drawn = show(rows, { per: "all" });
    expect(drawn.charts.map((c) => [c.group, c.form, c.form === "line" ? c.x : c.title])).toEqual([
      [SIZE, "line", "image_width"], [COUNT, "line", "num_images"], [OTHER, "line", "image_width"],
      [`${OTHER} · decode_tokens=16 · num_images=1 · text_tokens=0`, "bars", "image_width=512"],
    ]);
    const [size, count] = lines(drawn.charts);
    // A line per model over every size, image_height riding on the width as before.
    expect([size!.rungs.map((r) => r.text), size!.panels.map((p) => p.title), size!.legend.map((s) => s.label),
            medians(size!)]).toEqual([
      [256, 512, 768, 1024, 1280, 2048].map((w) => `${w} · image_height=${w}`), ["Bench A"], [G("Alpha-1B"), G("Beta-2B")],
      [[257.064, 513.064, 769.064, 1025.064, 1281.064, 2049.064], [357.064, 613.064, 869.064, 1125.064, 1381.064, 2149.064]],
    ]);
    // A panel per width, a line per model over the image counts.
    expect([count!.rungs.map((r) => r.text), count!.panels.map((p) => p.title), count!.legend.map((s) => s.label),
            medians(count!, 1)]).toEqual([
      ["1", "5", "16"], ["Bench A · image_width=256", "Bench A · image_width=512"], [G("Alpha-1B"), G("Beta-2B")],
      [[513.064, 517.064, 528.064], [613.064, 617.064, 628.064]],
    ]);
    for (const [chart, n] of [[size!, 12], [count!, 12]] as const) {
      expect(behind(chart).map((r) => [r.parameters.text_tokens, r.parameters.decode_tokens]))
        .toEqual(Array(n).fill([0, 64]));
      for (const s of chart.panels.flatMap((p) => p.series)) expect(s.cells.filter(Boolean).length).toBeGreaterThan(1);
    }
    expect(behind(size!).every((r) => r.parameters.num_images === 1)).toBe(true);
    // Every point neither draws, as the page laid them out before, so every row is drawn.
    const other = drawnRows(drawn.charts.filter((c) => c.group?.startsWith(OTHER)), new Set());
    expect([...other].sort((a, b) => rows.indexOf(a) - rows.indexOf(b)))
      .toEqual(rows.filter((r) => r.parameters.text_tokens !== 0 || r.parameters.decode_tokens !== 64));
    expect(drawnRows(drawn.charts, new Set()).size).toBe(rows.length);
    // A legend hides a series in its own part alone.
    const keys = lines(drawn.charts.filter((c) => c.form === "line")).flatMap((c) => c.legend.map((s) => s.key));
    expect(new Set(keys).size).toBe(keys.length);
  });

  test("image count ranks a model over every width, not the first alone", () => {
    // Alpha leads at 256 px and trails at 512 px by more: over both, Beta leads.
    const rows = Object.entries({ "Alpha-1B": [10, 100], "Beta-2B": [20, 30] }).flatMap(([name, ms]) =>
      [256, 512].flatMap((width, w) => [1, 5, 16].map((images) => row({
        benchmark_type: "vl_throughput", metric: "end_to_end_latency", unit: "ms", model_descriptor: gguf(name),
        value: ms[w]! + images, parameters: { image_width: width, image_height: width, text_tokens: 0, decode_tokens: 64,
                                              num_images: images } }))));
    const [count] = lines(show(rows).charts.filter((c) => c.group === COUNT));
    expect(count!.legend.map((s) => s.label)).toEqual([G("Beta-2B"), G("Alpha-1B")]);
  });

  test("a plan without 16 images counts 1 and 5", () => {
    const drawn = show(catalog(CATALOG.filter(([, , , n]) => n !== 16)));
    const count = lines(drawn.charts.filter((c) => c.group === COUNT));
    expect([count.length, count[0]!.rungs.map((r) => r.x), count[0]!.panels.length]).toEqual([1, [1, 5], 2]);
  });

  test("a pin stands in for a sweep's own value; a sweep that reaches no two values is not drawn", () => {
    const rows = catalog(CATALOG);
    const parts = (params: Record<string, string>) => show(rows, { per: "all", params }).charts
      .map((c) => [c.group, c.form === "line" ? c.rungs.map((r) => r.x) : c.title]);
    // The rest holds one value the pin does not, which its heading names, as nothing else does.
    expect(parts({ text_tokens: "1024" })).toEqual([
      ["By image size · decode_tokens=64 · num_images=1 · text_tokens=1024", [256, 512, 1280]],
      ["By image count · decode_tokens=64 · text_tokens=1024", [1, 5, 16]], [`${OTHER} · decode_tokens=256`, [256, 512]],
    ]);
    expect(parts({ decode_tokens: "256" })).toEqual([
      ["By image size · decode_tokens=256 · num_images=1 · text_tokens=0", [256, 512]],
      ["By image count · decode_tokens=256 · text_tokens=0", [1, 5, 16]], [`${OTHER} · text_tokens=1024`, [256, 512]],
    ]);
    expect(parts({ num_images: "5" })).toEqual([
      ["By image size · decode_tokens=64 · num_images=5 · text_tokens=0", [256, 512]], [OTHER, [256, 512]],
    ]);
    const pinned = show(rows, { params: { num_images: "5" } });
    expect(drawnRows(pinned.charts, new Set()).size).toBe(rows.filter((r) => r.parameters.num_images === 5).length);
  });

  test("a section names a runtime alike in its list, its panels' lines and its bars' names", () => {
    const on = (runtime: Runtime, of: ResultRow[]) => of.map((r) => ({ ...r, runtime_descriptor: runtime }));
    // Two models a runtime, so its panel holds two series and is not folded into one with the other's.
    const drawn = show([...on(MAC, [...shapes("Alpha-1B", 100), ...shapes("Gamma-1B", 150)]),
                        ...on(BOX, [...shapes("Beta-2B", 200), ...shapes("Delta-2B", 250)])]);
    const [line, bar] = drawn.charts;
    expect(drawn.runtimes.options.map((variant) => drawn.runtimeLines[variant]))
      .toEqual([[`runtime = ${MAC_RT}`], [`runtime = ${BOX_RT}`]]);
    expect(lines([line])[0]!.panels.map((p) => p.runtime))
      .toEqual([[`runtime = ${MAC_RT}`], [`runtime = ${BOX_RT}`]]);
    expect(bars([bar])[0]!.bars.map((b) => b.name).sort()).toEqual([
      `${G("Alpha-1B")} · ${MAC_RT}`, `${G("Gamma-1B")} · ${MAC_RT}`, `${G("Beta-2B")} · ${BOX_RT}`,
      `${G("Delta-2B")} · ${BOX_RT}`].sort());
  });

  test("a runtime's panels and bars name it by its descriptor, whatever order the columns draw in", () => {
    // Bench A runs BOX and Bench B MAC, so the panels come BOX first, while the list sorts MAC first.
    const on = (device: string, runtime: Runtime, of: ResultRow[]) =>
      of.map((r) => ({ ...r, device_name: device, runtime_descriptor: runtime }));
    const drawn = show([...on("Bench A", BOX, [...shapes("Beta-2B", 200), ...shapes("Delta-2B", 250)]),
                        ...on("Bench B", MAC, [...shapes("Alpha-1B", 100), ...shapes("Gamma-1B", 150)])]);
    const [line, bar] = drawn.charts;
    expect(lines([line])[0]!.panels.map((p) => [p.title, p.runtime])).toEqual([
      ["Bench A", [`runtime = ${BOX_RT}`]], ["Bench B", [`runtime = ${MAC_RT}`]]]);
    expect(bars([bar])[0]!.bars.map((b) => b.name.slice(b.name.indexOf("} · ") + 4)).sort())
      .toEqual([`Bench A · ${BOX_RT}`, `Bench A · ${BOX_RT}`, `Bench B · ${MAC_RT}`, `Bench B · ${MAC_RT}`]);
  });

  test("below the full tier a panel keeps its named runtime line, as its bars name the runtime", () => {
    const on = (runtime: Runtime, of: ResultRow[]) =>
      of.map((r) => ({ ...r, runtime_descriptor: runtime, device_name: null, client_id: null }));
    const rows = [...on(MAC, [...shapes("Alpha-1B", 100), ...shapes("Gamma-1B", 150)]),
                  ...on(BOX, [...shapes("Beta-2B", 200), ...shapes("Delta-2B", 250)])];
    const drawn = view(index(rows, "withheld"), { ...none, benchmark: "vl_throughput" });
    const [line, bar] = drawn.charts;
    expect(lines([line])[0]!.panels.map((p) => [p.title, p.runtime])).toEqual([
      ["devices withheld", [`runtime = ${MAC_RT}`]], ["devices withheld", [`runtime = ${BOX_RT}`]]]);
    expect([...new Set(bars([bar])[0]!.bars.map((b) => b.name.slice(b.name.lastIndexOf("} · ") + 4)))].sort()).toEqual([MAC_RT, BOX_RT]);
  });

  test("a bar chart per layer, as a layer is a panel in line form, each ranked on its own", () => {
    const rows = [...memory("Box", BOX, { host: 0.4, gpu: 6 }), ...memory("Box", BOX, { host: 0.3, gpu: 9 }, 256, gguf("Wide-2B"))];
    expect(bars(show(rows).charts).map((c) => [c.title, c.bars.map((b) => [b.label, Number(b.cell.median.toFixed(6))])]))
      .toEqual([["host memory", [[G("Wide-2B"), 0.3], [G("Tiny-1B"), 0.4]]], ["GPU memory", [[G("Tiny-1B"), 6], [G("Wide-2B"), 9]]]]);
  });

  test("a model's form holds under the model filter: it draws where its combination does plan-wide", () => {
    const imageOnly = shapes("Lone-1B", 300).slice(0, 1);
    const oneCount = shapes("Half-1B", 400).slice(1, 2);
    const rows = [...shapes("Alpha-1B", 100), ...imageOnly, ...oneCount];
    expect(lines(show(rows).charts)[0]!.x).toBe("num_images");
    const lone = show(rows, { model: [D("Lone-1B")] });
    expect([lone.charts.map((c) => [c.form, c.group]), lone.caption]).toEqual([[["bars", undefined]],
      [G("Lone-1B"), "decode_tokens=256", "text_tokens=0"]]);
    // One image at 1024 is a lone point, but of the lines' combination, so it draws as one.
    const half = show(rows, { model: [D("Half-1B")] }).charts;
    expect([half.map((c) => c.form), lines(half)[0]!.rungs.map((r) => r.x)]).toEqual([["line"], [1]]);
  });

  test("with no series reaching two rungs the family draws bars, one chart of them all when each holds one", () => {
    const rows = [
      decode(10, { prefill_tokens: 256 }, { model_descriptor: gguf("Alpha-1B") }),
      decode(30, { prefill_tokens: 512 }, { model_descriptor: gguf("Beta-2B") }),
    ];
    const drawn = show(rows);
    expect(drawn.charts.every((c) => c.form === "bars")).toBe(true);
    expect(bars(drawn.charts).map((c) => [c.title, c.bars.map((b) => b.label)])).toEqual([
      ["decode_throughput", [`prefill_tokens=256 · ${G("Alpha-1B")}`, `prefill_tokens=512 · ${G("Beta-2B")}`]],
    ]);
    // One chart of them all: each name leads with its combination, as its label does.
    expect(bars(drawn.charts)[0]!.bars.map((b) => b.name)).toEqual(bars(drawn.charts)[0]!.bars.map((b) => b.label));
    expect(drawn.caption).toEqual([]);
    expect(drawn.pins.map((c) => c.key)).toEqual(["p.prefill_tokens"]);
    // No family of the plan draws lines, so there is nothing to split per chart.
    expect(pageControls(index(rows, "full"), none).per).toBeNull();
    const lined = [...rows, ...[256, 512].map((tokens) => row({ parameters: { prefill_tokens: tokens } }))];
    expect(pageControls(index(lined, "full"), { ...none, per: "4" }).per).toMatchObject({ value: "4", disabled: false });
  });

  test("a series label names its model whole where models differ; the caption names one they share", () => {
    const at = (model: Model) => [256, 512].map((tokens) => decode(tokens, { prefill_tokens: tokens }, { model_descriptor: model }));
    const legend = (rows: ResultRow[]) => lines(show(rows).charts)[0]!.legend.map((s) => s.label).sort();
    const quants = [...at(gguf("Tiny-1B", "Q4_K_M")), ...at(gguf("Tiny-1B", "Q8_0"))];
    expect(legend(quants)).toEqual([G("Tiny-1B"), G("Tiny-1B", "Q8_0")]);
    expect(show(quants).caption).toEqual([]);
    expect(show(at(gguf("Tiny-1B"))).caption).toEqual([G("Tiny-1B")]);
    const both = [...at(gguf("Alpha-1B", "Q4_0")), ...at(gguf("Beta-2B", "Q8_0")), ...at(torch("Gamma-3B"))];
    expect(legend(both)).toEqual([G("Alpha-1B", "Q4_0"), G("Beta-2B", "Q8_0"), T("Gamma-3B")]);
    expect(show(both).caption).toEqual([]);
  });

  test("a cell holds every value that fell on it: median, count, range and values", () => {
    const rows = [103.6, 171.1, 104.2].map((value) => decode(value, { prefill_tokens: 256 }))
      .concat(decode(90, { prefill_tokens: 512 }));
    const drawn = show(rows);
    const [cell, single] = lines(drawn.charts)[0]!.panels[0]!.series[0]!.cells;
    expect(cell).toEqual({ median: 104.2, n: 3, min: 103.6, max: 171.1, values: [103.6, 104.2, 171.1] });
    expect(cellText(cell!)).toBe("104.2 (n=3: 103.6, 104.2, 171.1)");
    expect(cellText(single!)).toBe("90.00");
    expect(cellText({ median: 5, n: 9, min: 1, max: 9, values: [1, 2, 3, 4, 5, 6, 7, 8, 9] })).toBe("5.00 (n=9: 1.00–9.00)");
    expect(cellText(null)).toBe("—");
    expect(drawn.notes).toEqual(["1 of 2 cells hold more than one result: variants the result row cannot tell apart "
      + "(runtime flags, sampling, or a re-run) and several devices of one type."]);
    const withheld = show(rows.map((r) => ({ ...r, device_name: null, client_id: null })), {}, "withheld");
    expect(withheld.notes[0]).toMatch(
      /a re-run\) and different devices on one runtime \(device names are withheld at your access\)\.$/);
  });

  test("series rank once across every column, best first, a chart holding `per` of them and the last the rest", () => {
    const rows = Array.from({ length: 10 }, (_, k) => [256, 512].flatMap((tokens) => [
      decode(k * 10 + 5, { prefill_tokens: tokens }, { model_descriptor: gguf(`Model-${k}`) }),
      decode(k * 10, { prefill_tokens: tokens }, { model_descriptor: gguf(`Model-${k}`), device_name: "Bench B" }),
    ])).flat();
    const drawn = show(rows, { per: "4" });
    const charts = lines(drawn.charts);
    expect(charts.map((c) => [c.title, c.legend.length])).toEqual(
      [["Rank 1–4 of 10", 4], ["Rank 5–8 of 10", 4], ["Rank 9–10 of 10", 2]]);
    expect(charts[0]!.legend.map((s) => [s.label, s.hue, s.shape])).toEqual([
      [G("Model-9"), 1, 0], [G("Model-8"), 2, 0], [G("Model-7"), 3, 0], [G("Model-6"), 4, 0]]);
    expect(charts[2]!.legend[0]).toMatchObject({ label: G("Model-1"), hue: 1 });
    // Every choice draws its own split, a lone last series named by its one rank, and `all` one chart untitled.
    const split = (per: string, of = rows) => lines(show(of, { per }).charts).map((c) => c.title);
    expect([split("8"), split("12"), split("all")]).toEqual([["Rank 1–8 of 10", "Rank 9–10 of 10"], [""], [""]]);
    expect(split("4", rows.filter((r) => !canonical(r.model_descriptor).includes("Model-0")))).toEqual(
      ["Rank 1–4 of 9", "Rank 5–8 of 9", "Rank 9 of 9"]);
    // The result rows drawn are those of every chart's series but one a legend hides: two rungs on two columns each.
    expect([drawnRows(charts, new Set()).size, drawnRows(charts, new Set([charts[0]!.legend[1]!.key])).size]).toEqual([40, 36]);
    expect([...drawnRows(charts.slice(2), new Set())].every((r) => /Model-[01]-/.test(canonical(r.model_descriptor)))).toBe(true);
    // Each chart draws the same members, in the same slots, in a panel per column.
    expect(charts[0]!.panels.map(headed)).toEqual([MAC_A, MAC_B]);
    expect(charts[0]!.panels.map((p) => p.series.map((s) => s.key))).toEqual([
      charts[0]!.legend.map((s) => s.key), charts[0]!.legend.map((s) => s.key)]);
    expect(medians(charts[0]!, 1)[0]).toEqual([90, 90]);
    expect(lines(show(rows).charts).map((c) => c.legend.length)).toEqual([8, 2]);
    expect(lines(show(rows, { per: "all" }).charts).map((c) => [c.title, c.legend.length])).toEqual([["", 10]]);
    // Lower is better for a time: the fastest ranks first.
    const ttft = rows.map((r) => ({ ...r, benchmark_type: "prefill_throughput", metric: "ttft", unit: "ms" }));
    expect(lines(show(ttft, { per: "all" }).charts)[0]!.legend[0]!.label).toBe(G("Model-0"));
  });

  test("a chart draws no panel for a column none of its series ran in", () => {
    const rows = Array.from({ length: 8 }, (_, k) => [256, 512].map((tokens) => decode(80 - k * 10,
      { prefill_tokens: tokens }, { model_descriptor: gguf(`Model-${k}`), device_name: k < 4 ? "Bench A" : "Bench B" }))).flat();
    const charts = lines(show(rows, { per: "4" }).charts);
    expect(charts.map((c) => [c.legend.map((s) => s.label), c.panels.map(headed)])).toEqual([
      [[G("Model-0"), G("Model-1"), G("Model-2"), G("Model-3")], [MAC_A]],
      [[G("Model-4"), G("Model-5"), G("Model-6"), G("Model-7")], [MAC_B]],
    ]);
    expect(lines(show(rows).charts)[0]!.panels.map(headed)).toEqual([MAC_A, MAC_B]);
  });

  test("past 48 series in a chart the colours and shapes repeat, and a note says so", () => {
    const rows = Array.from({ length: 49 }, (_, k) => [256, 512].map((tokens) =>
      decode(k, { prefill_tokens: tokens }, { model_descriptor: gguf(`Model-${k}`) }))).flat();
    expect(show(rows, { per: "all" }).notes).toContain(
      "A chart holds more than 48 series, so colours and shapes repeat; choose fewer per chart.");
    expect(show(rows).notes).toEqual([]);
  });

  test("columns that each hold one series fold into one panel, a series per column", () => {
    const at = (model: Model, device: string) =>
      [256, 512].map((tokens) => decode(tokens, { prefill_tokens: tokens }, { model_descriptor: model, device_name: device }));
    const apart = show([...at(gguf("Alpha-1B"), "Bench A"), ...at(gguf("Beta-2B"), "Bench B")]);
    const chart = lines(apart.charts)[0]!;
    expect(chart.panels.map((p) => p.title)).toEqual(["decode_throughput"]);
    // A folded series names its column by device type and runtime name, as a bar does.
    expect(chart.legend.map((s) => s.label).sort()).toEqual([`${MAC_A} · ${G("Alpha-1B")}`, `${MAC_B} · ${G("Beta-2B")}`]);
    const same = show([...at(gguf("Alpha-1B"), "Bench A"), ...at(gguf("Alpha-1B"), "Bench B")]);
    expect(lines(same.charts)[0]!.legend.map((s) => s.label).sort()).toEqual([MAC_A, MAC_B]);
    expect(same.caption).toEqual([G("Alpha-1B")]);
  });

  test("bars: a chart per combination of the varying parameters, best first, the column named when several", () => {
    // An eval with no eval_id, whose column is its runtime alone.
    const score = (model: string, kind: string, value: number, runtime = VLLM) => row({
      benchmark_type: "eval", metric: "score", unit: "points", value, model_descriptor: torch(model), runtime_descriptor: runtime,
      parameters: { case: kind },
    });
    const rows = [score("Alpha", "quiz", 50), score("Beta", "quiz", 70), score("Alpha", "math", 90),
                  score("Beta", "math", 20), score("Beta", "math", 40)];
    const drawn = show(rows);
    expect(bars(drawn.charts).map((c) => [c.title, c.bars.map((b) => [b.label, b.cell.median, b.cell.n])])).toEqual([
      ["case=math", [[T("Alpha"), 90, 1], [T("Beta"), 30, 2]]],
      ["case=quiz", [[T("Beta"), 70, 1], [T("Alpha"), 50, 1]]],
    ]);
    expect(drawn.notes[0]).toMatch(/^1 of 4 cells hold more than one result/);
    const pinned = show(rows, { params: { case: '"quiz"' } });
    expect(bars(pinned.charts).map((c) => c.title)).toEqual(["score"]);
    expect(pinned.caption).toContain("case=quiz");

    const twoRuntimes = show([...rows, score("Alpha", "quiz", 60, { ...VLLM, server_version: "0.31.0" } as Runtime)]);
    expect(bars(twoRuntimes.charts)[1]!.bars.map((b) => b.label)).toEqual(
      [`${T("Beta")} · ${VLLM_RT}`, `${T("Alpha")} · ${label({ ...VLLM, server_version: "0.31.0" } as Runtime)}`,
       `${T("Alpha")} · ${VLLM_RT}`]);

    const one = show([score("Alpha", "quiz", 50), score("Alpha", "math", 90), score("Alpha", "code", 10)]);
    expect(bars(one.charts).map((c) => [c.title, c.bars.map((b) => b.label)])).toEqual(
      [["score", ["case=code", "case=math", "case=quiz"]]]);
    // Collapsed, the bars run in combination order, not by rank, so `per` leaves them one chart.
    const many = show(Array.from({ length: 9 }, (_, k) => score("Alpha", `case-${k}`, k * 10)), { per: "4" });
    expect(bars(many.charts).map((c) => [c.title, c.bars.length])).toEqual([["score", 9]]);
  });

  const accuracy = (model: string, evalId: string, value: number, over: Partial<ResultRow> = {}) => row({
    benchmark_type: "eval", metric: "accuracy", unit: "ratio", value, model_descriptor: torch(model), runtime_descriptor: VLLM,
    parameters: { eval_id: evalId }, ...over,
  });

  test("each eval is a tab of its own, after the benchmark types, its bars best first", () => {
    const rows = [accuracy("Alpha", "quiz", 0.5), accuracy("Beta", "quiz", 0.7), accuracy("Alpha", "math", 0.9),
                  accuracy("Beta", "math", 0.2), accuracy("Beta", "math", 0.4), row()];
    const ix = index(rows, "full");
    expect(ix.benchmarks.map((f) => [f.key, f.short, f.label, f.types])).toEqual([
      ["prefill_throughput", "Prefill", "prefill throughput", ["prefill_throughput"]],
      ["eval-math", "math", "eval · math", ["eval"]], ["eval-quiz", "quiz", "eval · quiz", ["eval"]],
    ]);
    const math = view(ix, { ...none, benchmark: "eval-math" });
    expect(math.pins).toEqual([]);
    expect(bars(math.charts).map((c) => [c.title, c.bars.map((b) => [b.label, b.cell.median, b.cell.n])])).toEqual([
      ["accuracy", [[T("Alpha"), 90, 1], [T("Beta"), 30, 2]]],
    ]);
    // An eval's column is its runtime alone, which the chart's list of runtimes names.
    expect(math.caption).toEqual(["eval_id=math"]);
    expect(math.runtimes.options).toEqual([VLLM_RT]);
    // An eval run before eval_id was recorded keeps the one tab.
    expect(familyOf(accuracy("Alpha", "", 0.5, { parameters: {} }))).toBe("eval");
  });

  test("an eval's bars rank in charts of `per`, as lines do, the select offered with no line drawn", () => {
    const rows = Array.from({ length: 9 }, (_, k) => accuracy(`Model-${k}`, "quiz", k / 10));
    const ix = index(rows, "full");
    const split = (per: string) => bars(view(ix, { ...none, benchmark: "eval-quiz", per }).charts)
      .map((c) => [c.title, c.bars.length]);
    expect(split("4")).toEqual([["accuracy · Rank 1–4 of 9", 4], ["accuracy · Rank 5–8 of 9", 4],
                                ["accuracy · Rank 9 of 9", 1]]);
    expect([split("8"), split("all")]).toEqual([[["accuracy · Rank 1–8 of 9", 8], ["accuracy · Rank 9 of 9", 1]],
                                                [["accuracy", 9]]]);
    expect(bars(view(ix, { ...none, benchmark: "eval-quiz", per: "4" }).charts)[0]!.bars[0]!.label).toBe(T("Model-8"));
    expect(pageControls(ix, none).per).toMatchObject({ value: "8" });
  });

  test("an eval run on several runtime types draws a group of charts per type, each naming its own runtimes", () => {
    const vllm = { ...VLLM, server_version: "0.31.0" } as Runtime;
    const rows = [accuracy("Alpha", "quiz", 0.5), accuracy("Beta", "quiz", 0.7, { runtime_descriptor: vllm }),
                  accuracy("Alpha", "quiz", 0.6, { runtime_descriptor: SGLANG })];
    const drawn = show(rows);
    expect(bars(drawn.charts).map((c) => [c.group, c.title, c.bars.map((b) => b.label)])).toEqual([
      ["runtime type = uv_sglang", "accuracy", [T("Alpha")]],
      ["runtime type = uv_vllm", "accuracy", [`${T("Beta")} · ${label(vllm)}`, `${T("Alpha")} · ${VLLM_RT}`]],
    ]);
    // The section's caption and runtimes are those of every group.
    expect(drawn.caption).toEqual(["eval_id=quiz"]);
    expect(drawn.runtimes.options).toEqual([SGLANG_RT, VLLM_RT, label(vllm)].sort());
    // One runtime type left draws no group.
    expect(bars(show(rows, { drop: [SGLANG_RT] }).charts).map((c) => c.group)).toEqual([undefined]);
  });

  test("a group of an eval's charts names what the section's caption does not", () => {
    const vllm = { ...VLLM, server_version: "0.31.0" } as Runtime;
    // Beta alone on SGLang, Alpha alone on both vLLMs: each group holds one model, which the caption cannot name.
    const rows = [accuracy("Alpha", "quiz", 0.5), accuracy("Alpha", "quiz", 0.6, { runtime_descriptor: vllm }),
                  accuracy("Beta", "quiz", 0.7, { runtime_descriptor: SGLANG })];
    const drawn = show(rows);
    expect(bars(drawn.charts).map((c) => [c.group, c.bars.map((b) => b.label)])).toEqual([
      ["runtime type = uv_sglang", [T("Beta")]],
      ["runtime type = uv_vllm", [`${T("Alpha")} · ${label(vllm)}`, `${T("Alpha")} · ${VLLM_RT}`]],
    ]);
    expect(drawn.caption).toEqual(["eval_id=quiz"]);
  });

  test("a series hidden in one group of an eval's line charts stays drawn in the others", () => {
    const rows = [1, 2].flatMap((n_shots) => ["Alpha", "Beta"].flatMap((model) => [VLLM, SGLANG].map((runtime) =>
      accuracy(model, "quiz", 0.5, { runtime_descriptor: runtime, parameters: { eval_id: "quiz", n_shots } }))));
    const charts = lines(show(rows).charts);
    expect(charts.map((c) => [c.group, c.legend.map((s) => s.label)])).toEqual([
      ["runtime type = uv_sglang", [T("Alpha"), T("Beta")]], ["runtime type = uv_vllm", [T("Alpha"), T("Beta")]],
    ]);
    const [sglang, vllm] = charts.map((c) => c.legend.map((s) => s.key));
    expect(sglang!.filter((key) => vllm!.includes(key))).toEqual([]);
    const alpha = new Set([sglang![0]!]);
    expect(drawnRows(charts, alpha).size).toBe(drawnRows(charts, new Set()).size - 2);
  });

  test("an eval's line panels name no device, so each is headed by its metric over its runtime", () => {
    const vllm = { ...VLLM, server_version: "0.31.0" } as Runtime;
    const rows = [VLLM, vllm].flatMap((runtime) => ["Alpha", "Beta"].flatMap((model) => [1, 2].map((n_shots) =>
      accuracy(model, "quiz", 0.5 + n_shots / 10, { runtime_descriptor: runtime, parameters: { eval_id: "quiz", n_shots } }))));
    expect(lines(show(rows).charts)[0]!.panels.map((p) => [p.title, p.runtime]))
      .toEqual([["accuracy", [`runtime = ${VLLM_RT}`]], ["accuracy", [`runtime = ${label(vllm)}`]]]);
  });

  test("a runtime type drawing only a lone combination is headed by its values as the others are", () => {
    const at = (n_shots: number, temp: number, runtime: Runtime) =>
      accuracy("Alpha", "quiz", 0.5 + n_shots / 10, { runtime_descriptor: runtime, parameters: { eval_id: "quiz", n_shots, temp } });
    const drawn = show([at(1, 0, VLLM), at(2, 0, VLLM), at(1, 1, VLLM), at(1, 1, SGLANG)]);
    expect(drawn.charts.map((c) => [c.form, c.group])).toEqual([
      ["bars", "runtime type = uv_sglang · temp=1"], ["line", "runtime type = uv_vllm · temp=0"],
      ["bars", "runtime type = uv_vllm · temp=1"]]);
  });

  test("thinking and non-thinking runs of a model are two rows, where its flags say which", () => {
    const flags = (enable_thinking: boolean): ModelFlags => ({ benchmark_type: "eval", model_type: "torch", enable_thinking });
    // The legacy text a client reported, which the server hands over as unknown.
    const legacy = (value: string) => ({ type: "unknown" as const, value });
    const rows = [accuracy("Alpha", "quiz", 0.8, { model_flags: flags(true) }),
                  accuracy("Alpha", "quiz", 0.6, { model_flags: legacy("enable_thinking=false") }),
                  accuracy("Beta", "quiz", 0.7)];
    const drawn = show(rows);
    expect(bars(drawn.charts)[0]!.bars.map((b) => [b.label, b.cell.median, b.cell.n])).toEqual([
      [`${T("Alpha")} · thinking`, 80, 1], [`${T("Beta")}`, 70, 1], [`${T("Alpha")} · non-thinking`, 60, 1],
    ]);
    expect(drawn.notes).toEqual([]);
    // The pick goes by the model's descriptor, so it keeps both modes.
    expect(bars(show(rows, { model: [DT("Alpha")] }).charts)[0]!.bars.map((b) => b.label))
      .toEqual([`${T("Alpha")} · thinking`, `${T("Alpha")} · non-thinking`]);
    expect([thinkingOf(flags(true)), thinkingOf(flags(false)), thinkingOf(legacy("enable_thinking=true")),
            thinkingOf(legacy("x=1")), thinkingOf(legacy({ enable_thinking: false } as never)), thinkingOf(null)])
      .toEqual([true, false, true, null, false, null]);
  });

  test("runs of a model at two reasoning efforts are two rows, where its flags say which", () => {
    const flags = (reasoning_effort: "low" | "high"): ModelFlags =>
      ({ benchmark_type: "eval", model_type: "torch", reasoning_effort });
    const legacy = (value: string) => ({ type: "unknown" as const, value });
    const rows = [accuracy("Alpha", "quiz", 0.8, { model_flags: flags("high") }),
                  accuracy("Alpha", "quiz", 0.6, { model_flags: legacy("enable_thinking=true,reasoning_effort=low") }),
                  accuracy("Alpha", "quiz", 0.7)];
    expect(bars(show(rows).charts)[0]!.bars.map((b) => [b.label, b.cell.median, b.cell.n])).toEqual([
      [`${T("Alpha")} · effort high`, 80, 1], [`${T("Alpha")}`, 70, 1], [`${T("Alpha")} · thinking · effort low`, 60, 1],
    ]);
    // A name reads as a legend's: the model, then its mode and effort.
    expect(bars(show(rows).charts)[0]!.bars.map((b) => b.name))
      .toEqual([`${T("Alpha")} · effort high`, T("Alpha"), `${T("Alpha")} · thinking · effort low`]);
    // A lone bar, its model the caption's, is named whole too.
    expect(bars(show([rows[1]!]).charts)[0]!.bars.map((b) => b.name)).toEqual([`${T("Alpha")} · thinking · effort low`]);
    expect([effortOf(flags("low")), effortOf(legacy("enable_thinking=false, reasoning_effort=medium")),
            effortOf(legacy("reasoning_effort=none")), effortOf(legacy({ reasoning_effort: "high" } as never)), effortOf(null)])
      .toEqual(["low", "medium", null, "high", null]);
  });

  test("a bar's name is its model as a legend names it, then its runtime by name; its label keeps them whole", () => {
    const flags = (enable_thinking: boolean): ModelFlags => ({ benchmark_type: "eval", model_type: "torch", enable_thinking });
    const vllm = { ...VLLM, server_version: "0.31.0" } as Runtime;
    const rows = [accuracy("Alpha", "quiz", 0.8, { model_flags: flags(true) }),
                  accuracy("Alpha", "quiz", 0.6, { model_flags: flags(false) }),
                  accuracy("Beta", "quiz", 0.7, { runtime_descriptor: vllm })];
    const drawn = show(rows);
    expect(bars(drawn.charts)[0]!.bars.map((b) => [b.name, b.label])).toEqual([
      [`${T("Alpha")} · thinking · ${VLLM_RT}`, `${T("Alpha")} · thinking · ${VLLM_RT}`],
      [`${T("Beta")} · ${label(vllm)}`, `${T("Beta")} · ${label(vllm)}`],
      [`${T("Alpha")} · non-thinking · ${VLLM_RT}`, `${T("Alpha")} · non-thinking · ${VLLM_RT}`],
    ]);
    expect(drawn.runtimes.options.map((variant) => drawn.runtimeLines[variant]))
      .toEqual([[`runtime = ${VLLM_RT}`], [`runtime = ${label(vllm)}`]]);
    // The one runtime drawn needs no name in a bar's.
    const one = show(rows, { drop: [label(vllm)] });
    expect([bars(one.charts)[0]!.bars.map((b) => b.name), one.runtimeLines[VLLM_RT]])
      .toEqual([[`${T("Alpha")} · thinking`, `${T("Alpha")} · non-thinking`], [`runtime = ${VLLM_RT}`]]);
  });

  test("below the full tier an eval's note blames no device: its column never names one", () => {
    const score = (value: number) => row({
      benchmark_type: "eval", metric: "accuracy", unit: "ratio", value, model_descriptor: torch("Alpha"), runtime_descriptor: VLLM,
      parameters: { eval_id: "quiz" }, client_id: null, device_name: null,
    });
    const notes = show([score(0.5), score(0.7)], {}, "withheld").notes;
    expect(notes).toEqual(["1 of 1 cells hold more than one result: variants the result row cannot tell apart "
      + "(runtime flags, sampling, or a re-run)."]);
  });

  test("past 12 panels at every device type the report opens on the first, and says so", () => {
    const rows = Array.from({ length: 5 }, (_, d) => Array.from({ length: 9 }, (_, m) => [256, 512].map((tokens) =>
      decode(m + d, { prefill_tokens: tokens }, { model_descriptor: gguf(`Model-${m}`), device_name: `Bench ${d + 1}` })))).flat(2);
    const types = [1, 2, 3, 4, 5].map((d) => `Bench ${d}`);
    const drawn = show(rows, { per: "4" });
    expect(drawn.notes).toContain("Showing Bench 1; choose device types for the other 4.");
    expect(lines(drawn.charts).map((c) => c.panels.length)).toEqual([1, 1, 1]);
    expect([drawn.filters.device, drawn.devices]).toEqual([[], ["Bench 1"]]);
    const all = show(rows, { per: "4", device: types });
    expect(all.devices).toEqual(types);
    expect(lines(all.charts).reduce((n, c) => n + c.panels.length, 0)).toBe(15);
    expect(all.notes.some((n) => n.startsWith("Showing"))).toBe(false);
    const two = show(rows, { per: "4", device: ["Bench 2", "Bench 4"] });
    expect(lines(two.charts)[0]!.panels.map((p) => [p.title, p.runtime])).toEqual([["Bench 2", [`runtime = ${MAC_RT}`]], ["Bench 4", [`runtime = ${MAC_RT}`]]]);
    expect(show(rows).devices).toEqual([]);
  });
});

describe("E2E vs prefill + decode", () => {
  test("the estimate is prefill time plus the e2e rows' decode tokens at the decode rate", () => {
    expect(estimate(1000, 200, null, 50, 100, 2420)).toEqual([220, 10]);
    expect(estimate(1000, null, 5000, 50, 100, 2420)).toEqual([220, 10]);
    expect(estimate(1000, 200, 1, 50, 100, 2420)).toEqual([220, 10]);
  });

  test.each([
    ["no prefill", [1000, null, null, 50, 100, 2420]],
    ["no prefill rate", [1000, null, 0, 50, 100, 2420]],
    ["a non-finite ttft and no rate", [1000, Number.NaN, null, 50, 100, 2420]],
    ["no decode", [1000, 200, null, null, 100, 2420]],
    ["a zero decode rate", [1000, 200, null, 0, 100, 2420]],
    ["no decode length", [1000, 200, null, 50, null, 2420]],
    ["no e2e", [1000, 200, null, 50, 100, null]],
    ["a zero e2e", [1000, 200, null, 50, 100, 0]],
    ["an infinite e2e", [1000, 200, null, 50, 100, Infinity]],
  ] as [string, Parameters<typeof estimate>][])("%s is a gap", (_, inputs) => {
    expect(estimate(...inputs)).toBeNull();
  });

  type Metric = [string, number, string];
  const prefill = (tokens: number, tps: number, ttft: number | null, over: Partial<ResultRow> = {}) =>
    job("prefill_throughput",
        [["prefill_throughput", tps, "tokens/sec"], ...(ttft === null ? [] : [["ttft", ttft, "ms"] as Metric])],
        { parameters: { prefill_tokens: tokens }, ...over });
  const decodeJob = (tokens: number, tps: number, over: Partial<ResultRow> = {}) => job("decode_throughput",
    [["decode_throughput", tps, "tokens/sec"]], { parameters: { prefill_tokens: tokens, decode_tokens: 100 }, ...over });
  const e2e = (tokens: number, ms: number, over: Partial<ResultRow> = {}) => job("end_to_end_latency",
    [["end_to_end_latency", ms, "ms"]], { parameters: { prefill_tokens: tokens, decode_tokens: 256 }, ...over });
  const other = { model_descriptor: gguf("Other-2B") };
  const plan = [
    ...prefill(256, 1000, 250), ...decodeJob(256, 50), ...e2e(256, 5500),
    ...prefill(512, 1024, null), ...decodeJob(512, 64), ...e2e(512, 4950),
    ...e2e(1024, 9000),
    ...prefill(2048, 900, 2000), ...decodeJob(2048, 40),
    ...prefill(256, 1000, 250, other), ...decodeJob(256, 50, other), ...e2e(256, 5370, other),
  ];

  test("the check lists a runtime alone, as it pools the runtime's flag sets, under its vault name", () => {
    const set = (envs: string[] | null) => ({ requested_runtime_flags: (envs && { envs }) as never });
    const rows = [null, ["LLAMA_ARG_CTX_SIZE=4096"]].flatMap((envs) =>
      [...prefill(256, 2560, 100, set(envs)), ...decodeJob(256, 50, set(envs)), ...e2e(256, 100 + 256 * 20, set(envs))]);
    const lines = (v: ReturnType<typeof show>) => v.runtimes.options.map((variant) => v.runtimeLines[variant]);
    expect(lines(show(rows, { benchmark: "e2e_check" }))).toEqual([[`runtime = ${MAC_RT}`]]);
    expect(lines(show(rows))).toHaveLength(2);
    // A runtime from the vault reads under its label, as a model does.
    const twin = (id: string) => ({ custom_label: "acme build", flavor: "macos-arm64", source: "vault_remote_archive",
      type: "llamacpp_cli_stock_tools", url: `/vault/resources/${id}/content` }) as Runtime;
    const twins = ["vault_1", "vault_2"].flatMap((id) => [...prefill(256, 2560, 100, { runtime_descriptor: twin(id) }),
      ...decodeJob(256, 50, { runtime_descriptor: twin(id) }), ...e2e(256, 100 + 256 * 20, { runtime_descriptor: twin(id) })]);
    expect(lines(show(twins, { benchmark: "e2e_check" })))
      .toEqual(["vault_1", "vault_2"].map((id) => ["acme build", `runtime = ${canonical(twin(id))}`]));
    expect(bars(show(twins, { benchmark: "decode_throughput" }).charts)[0]!.bars.map((b) => b.name))
      .toEqual(["vault_1", "vault_2"].map((id) => `acme build ${canonical(twin(id))}`));
  });

  test("each rung charts measured minus expected as % of the expected total, ranked by it ascending", () => {
    const drawn = show(plan, { benchmark: "e2e_check" });
    expect(drawn.family?.key).toBe("e2e_check");
    const chart = lines(drawn.charts)[0]!;
    expect(chart.panels.map((p) => [p.title, p.unit, p.band, p.reference])).toEqual([
      ["Bench A · Extra E2E time as % of expected total (shaded: within ±5 %)", "%", 5, 0],
    ]);
    // The device bar names the one device type, so the caption does not.
    expect(drawn.caption).not.toContain("Bench A");
    expect(chart.rungs.map((r) => r.x)).toEqual([256, 512]);
    expect(chart.legend.map((s) => s.label)).toEqual([G("Other-2B"), G("Tiny-1B")]);
    expect(medians(chart, 0)[1]!.map((v) => Number(v!.toFixed(2)))).toEqual([2.42, 10]);
    expect(drawn.notes[0]).toBe(e2eText(index(plan, "full").e2e!));
    expect(drawn.notes).toContain("Runtime flags and client builds are not on a result row, so the page cannot tell "
      + "whether the three measurements ran alike.");
  });

  const e2eOf = (rows: ResultRow[]) => index(rows, "full").e2e;
  const NONE = { e2e: 0, decode: 0, prefill: 0 };

  test("the Overview's finding counts what agreed, what strayed which way, and what could not be checked", () => {
    expect(e2eOf(plan)).toEqual({
      kind: "checked", vl: false, total: 5, complete: 3, unestimated: 0, missing: { e2e: 1, decode: 1, prefill: 1 },
      batched: false, within: 2, slower: { n: 1, max: 10 }, faster: null,
    });
    expect(index([...memory("Mac", MAC, { host: 1 })], "full").e2e).toBeNull();
    const lacking = index(plan.filter((r) => r.benchmark_type !== "decode_throughput"), "full");
    expect(lacking.e2e).toEqual({ kind: "lacking", types: ["decode throughput"] });
  });

  test("each combination is classed and its spread taken from one rounding, the band's edge within", () => {
    // Prefill takes 1000 ms and 256 tokens decode in 1000 ms, so E2E is `pct` % over 2000 ms.
    const agree = (pcts: number[]) => pcts.flatMap((pct, i) => {
      const tokens = 256 * 2 ** i;
      return [...prefill(tokens, 1000, 1000), ...decodeJob(tokens, 256), ...e2e(tokens, 2000 * (1 + pct / 100))];
    });
    const classed = (pcts: number[]) => {
      const f = e2eOf(agree(pcts));
      return f?.kind === "checked" ? [f.within, f.slower, f.faster] : f;
    };
    expect(classed([0, 4.96, -5])).toEqual([3, null, null]);
    expect(classed([5.04, 0])).toEqual([2, null, null]);
    expect(classed([0, 6.4, 5.2, -13.2])).toEqual([1, { n: 2, max: 6.4 }, { n: 1, max: 13.2 }]);
    expect(classed([10, -20])).toEqual([0, { n: 1, max: 10 }, { n: 1, max: 20 }]);
    expect(e2eOf([...agree([0, 1]), ...e2e(4096, 9000)]))
      .toMatchObject({ total: 3, complete: 2, unestimated: 0, missing: { e2e: 0, decode: 1, prefill: 1 }, within: 2 });
    const stalled = (tokens: number) => [...prefill(tokens, 1000, 1000), ...decodeJob(tokens, 0), ...e2e(tokens, 2000)];
    expect(e2eOf([...agree([0, 1]), ...stalled(4096), ...stalled(8192)]))
      .toMatchObject({ total: 4, complete: 4, unestimated: 2, missing: NONE, within: 2 });
  });

  test("a result without a prefill length joins no combination", () => {
    const unnamed = { parameters: {} };
    const typed = [...prefill(256, 1000, 250, unnamed), ...decodeJob(256, 50, unnamed)];
    expect(e2eOf([...typed, ...e2e(256, 5500, unnamed)])).toMatchObject({ total: 0, complete: 0 });
    expect(e2eOf([...typed, ...e2e(256, 5500)]))
      .toMatchObject({ total: 1, complete: 0, missing: { e2e: 0, decode: 1, prefill: 1 } });
  });

  test("with no combination holding all three, it draws nothing and says so", () => {
    const apart = [...prefill(256, 1000, 250), ...decodeJob(512, 50), ...e2e(1024, 9000)];
    expect(show(apart, { benchmark: "e2e_check" }))
      .toMatchObject({ charts: [], empty: "No combination has all three measurements." });
    expect(e2eOf(apart)).toMatchObject({ total: 3, complete: 0, missing: { e2e: 2, decode: 2, prefill: 2 } });
  });

  test("a combination with all three that cannot be estimated is counted apart, and drawn as nothing", () => {
    const stalled = [256, 512].flatMap((tokens) => [...prefill(tokens, 1000, 100), ...decodeJob(tokens, 0),
                                                    ...e2e(tokens, 7000)]);
    expect(e2eOf(stalled)).toMatchObject({ total: 2, complete: 2, unestimated: 2, within: 0 });
    expect(show(stalled, { benchmark: "e2e_check" })).toMatchObject(
      { charts: [], empty: "No combination could be estimated: a rate or time ≤ 0, or no decode_tokens." });
    const lengthless = e2e(512, 7000, { parameters: { prefill_tokens: 512 } });
    const some = [...prefill(256, 1000, 250), ...decodeJob(256, 50), ...e2e(256, 5500),
                  ...prefill(512, 1000, 250), ...decodeJob(512, 50), ...lengthless];
    expect(e2eOf(some)).toMatchObject({ total: 2, complete: 2, unestimated: 1, within: 1 });
    expect(lines(show(some, { benchmark: "e2e_check" }).charts)[0]!.rungs.map((r) => r.x)).toEqual([256]);
  });

  test("a batch size is a rung of its own, so each is checked against its own prefill and decode", () => {
    // Each batch size is exactly prefill + 256 tokens at its own decode rate.
    const at = (batch: number, ttft: number, tps: number) => {
      const over = (params: Record<string, unknown>) => ({ parameters: { ...params, prefill_tokens: 256, batch_size: batch } });
      return [...prefill(256, 2560, ttft, over({})), ...decodeJob(256, tps, over({ decode_tokens: 100 })),
              ...e2e(256, ttft + (256 * 1000) / tps, over({ decode_tokens: 256 }))];
    };
    const rows = [...at(1, 100, 50), ...at(8, 400, 20)];
    expect(e2eOf(rows)).toMatchObject({ total: 2, batched: true, within: 2 });
    expect(e2eOf(at(1, 100, 50))).toMatchObject({ total: 1, batched: false, within: 1 });
    const drawn = show(rows, { benchmark: "e2e_check" });
    const chart = lines(drawn.charts)[0]!;
    expect(chart.legend.map((s) => s.label).sort()).toEqual(["batch_size=1", "batch_size=8"]);
    expect(medians(chart, 0)).toEqual([[0], [0]]);
    expect(drawn.pins.map((c) => c.key)).toEqual(["p.batch_size"]);
    // One batch size is the caption's; rows without the column read as 1 and say nothing.
    expect(show(at(1, 100, 50), { benchmark: "e2e_check" }).caption).toContain("batch_size=1");
    expect(show(plan, { benchmark: "e2e_check" }).caption.some((c) => partText(c).startsWith("batch_size"))).toBe(false);
  });

  test("a batch size checked at one prefill length alone keeps line form, so its points keep the band", () => {
    const at = (batch: number, tokens: number) => {
      const over = (params: Record<string, unknown>) => ({ parameters: { ...params, prefill_tokens: tokens, batch_size: batch } });
      return [...prefill(tokens, 2560, 100, over({})), ...decodeJob(tokens, 50, over({ decode_tokens: 100 })),
              ...e2e(tokens, 100 + (256 * 1000) / 50, over({ decode_tokens: 256 }))];
    };
    const drawn = show([...at(1, 256), ...at(1, 512), ...at(8, 256)], { benchmark: "e2e_check" });
    expect(drawn.charts.map((c) => [c.form, c.group])).toEqual([["line", "batch_size=1"], ["line", "batch_size=8"]]);
    const [, lone] = lines(drawn.charts);
    expect([lone!.rungs.map((r) => r.x), lone!.panels.map((p) => [p.band, p.reference])]).toEqual([[256], [[5, 0]]]);
  });

  test("a VL row's end-to-end latency is no rung of the check, whatever its parameters", () => {
    const vl = job("vl_throughput", [["end_to_end_latency", 900, "ms"]],
                   { parameters: { prefill_tokens: 256, decode_tokens: 256 } });
    expect(e2eOf([...plan, ...vl])).toEqual(e2eOf(plan));
    // Nor does a text plan list VL's check.
    expect(index(plan, "full").vlE2e).toBeNull();
  });

  test("it is listed but not offered without one of its inputs, or without device names", () => {
    const partial = plan.filter((r) => r.benchmark_type !== "end_to_end_latency");
    const ix = index(partial, "full");
    const e2eFamily = ix.families.find((f) => f.key === "e2e_check")!;
    expect(e2eFamily.disabled).toBe("no end-to-end latency results");
    // Without end-to-end latency there is nothing to check, so the Overview says nothing.
    expect(ix.e2e).toBeNull();
    expect(index(plan.filter((r) => r.benchmark_type !== "decode_throughput"), "full").e2e)
      .toEqual({ kind: "lacking", types: ["decode throughput"] });
    const drawn = view(ix, { ...none, benchmark: "e2e_check" });
    expect([drawn.family, drawn.filters.benchmark, drawn.empty]).toEqual([null, "", null]);
    expect(ix.benchmarks.map((f) => f.key)).toEqual(["prefill_throughput", "decode_throughput"]);
    const withheld = index(plan.map((r) => ({ ...r, device_name: null, client_id: null })), "withheld");
    expect(withheld.families.find((f) => f.key === "e2e_check")!.disabled).toBe("needs device names");
    expect(withheld.e2e).toEqual({ kind: "withheld" });
    expect(index([row()], "full").families.find((f) => f.key === "e2e_check")!.disabled)
      .toBe("no decode throughput or end-to-end latency results");
    expect(index([...memory("Mac", MAC, { host: 1 })], "full").families.map((f) => f.key)).toEqual(["max_memory_usage"]);
  });
});

describe("VL E2E latency vs prompt + decode", () => {
  type Metric = [string, number, string];
  /** One vl_throughput job at `images` images; a null metric is one its client did not send. */
  const vl = (images: number, ttft: number | null, decode: number | null, e2e: number | null,
              over: Partial<ResultRow> = {}, text = 1024, decodeTokens = 256) => job("vl_throughput", [
    ["prefill_throughput", 500, "tokens/sec"],
    ...(ttft === null ? [] : [["ttft", ttft, "ms"] as Metric]),
    ...(decode === null ? [] : [["decode_throughput", decode, "tokens/sec"] as Metric]),
    ...(e2e === null ? [] : [["end_to_end_latency", e2e, "ms"] as Metric]),
  ], { parameters: { image_width: 256, image_height: 256, text_tokens: text, decode_tokens: decodeTokens, num_images: images },
       ...over });
  // 256 tokens at 256 tok/s decode in 1000 ms, so a job expects its prompt time + 1000 ms; 512 tokens in 2000.
  const plan = [
    ...vl(1, 1000, 256, 2000), ...vl(5, 1000, 256, 2200), ...vl(16, 1000, 256, null), ...vl(16, null, 256, 1500),
    ...vl(16, 1000, 256, 3000, {}, 1024, 512),
  ];

  test("checks each vl_throughput job's span against its own time to first token plus decode time", () => {
    const ix = index(plan, "full");
    expect(ix.e2e).toBeNull();
    expect(ix.vlE2e).toEqual({
      kind: "checked", vl: true, total: 5, complete: 3, unestimated: 0, missing: { e2e: 1, decode: 0, prefill: 1 },
      batched: false, within: 2, slower: { n: 1, max: 10 }, faster: null,
    });
    expect(e2eText(ix.vlE2e!)).toBe("Measured end-to-end latency is within ±5 % of prompt + decode time for 2 of 3 VL "
      + "throughput jobs; 1 ran slower, by 10.0 %. 2 more lack a measurement: end-to-end latency in 1, time to first "
      + "token in 1.");
    const drawn = view(ix, { ...none, benchmark: "vl_e2e_check" });
    const chart = lines(drawn.charts)[0]!;
    // Over the x VL throughput's own charts draw, each job's own decode_tokens timing its decode.
    expect([chart.x, chart.rungs.map((r) => r.text), medians(chart)])
      .toEqual(["num_images", ["1 · decode_tokens=256", "5 · decode_tokens=256", "16 · decode_tokens=512"], [[0, 10, 0]]]);
    expect(chart.panels[0]!.title).toBe("Bench A · Extra E2E time as % of expected total (shaded: within ±5 %)");
    expect(drawn.notes[0]).toBe(e2eText(ix.vlE2e!));
    expect(drawn.notes).toContain("One job measures all three, so each point checks a run against its own time to "
      + "first token, image encoding included, and decode time.");
    expect(chartRefs(ix).at(-1)).toMatchObject({ name: "VL E2E latency vs prompt + decode", key: "vl_e2e_check\nexcess" });
  });

  test("lists the models it checked, which the model filter draws alone", () => {
    // A client that does not time the request sends no end-to-end latency: its model is not checked.
    const untimed = vl(1, 1000, 256, null, { model_descriptor: gguf("Untimed-2B") });
    const ix = index([...plan, ...vl(1, 1000, 256, 2000, { model_descriptor: gguf("Other-2B") }), ...untimed], "full");
    expect(ix.families.find((f) => f.key === "vl_e2e_check")!.models).toBe(2);
    // Picked from another tab, it filters nothing here, as a model this family did not measure.
    const picked = view(ix, { ...none, benchmark: "vl_e2e_check", model: [D("Untimed-2B")] });
    expect([picked.empty, lines(picked.charts)[0]!.legend.length]).toEqual([null, 2]);
    const drawn = view(ix, { ...none, benchmark: "vl_e2e_check", model: [D("Other-2B")] });
    expect([drawn.filters.model, lines(drawn.charts)[0]!.legend.length]).toEqual([[D("Other-2B")], 1]);
  });

  test("needs no device names, and is not listed where no job measured end-to-end latency", () => {
    const withheld = index(plan.map((r) => ({ ...r, device_name: null, client_id: null })), "withheld");
    expect(withheld.vlE2e).toMatchObject({ kind: "checked", complete: 3, within: 2 });
    expect(view(withheld, { ...none, benchmark: "vl_e2e_check" }).charts).toHaveLength(1);
    const spanless = index(plan.filter((r) => r.metric !== "end_to_end_latency"), "full");
    expect([spanless.vlE2e, spanless.families.map((f) => f.key)]).toEqual([null, ["vl_throughput"]]);
    const stalled = index(vl(1, 1000, 0, 2000), "full");
    expect(stalled.vlE2e).toMatchObject({ complete: 1, unestimated: 1 });
    expect(view(stalled, { ...none, benchmark: "vl_e2e_check" }).empty)
      .toBe("No job could be estimated: a rate or time ≤ 0, or no decode_tokens.");
    expect(view(index(vl(1, 1000, null, 2000), "full"), { ...none, benchmark: "vl_e2e_check" }).empty)
      .toBe("No job has all three measurements.");
  });

  test("an image-only job keeps line form after the lines, its one rung inside the band", () => {
    const drawn = view(index([...plan, ...vl(1, 500, 256, 1500, {}, 0)], "full"), { ...none, benchmark: "vl_e2e_check" });
    expect(drawn.charts.map((c) => [c.form, c.group])).toEqual([["line", "text_tokens=1024"], ["line", "text_tokens=0"]]);
    const [, alone] = lines(drawn.charts);
    expect([alone!.rungs.map((r) => r.x), alone!.panels.map((p) => [p.band, p.reference]), medians(alone!)])
      .toEqual([[1], [[5, 0]], [[0]]]);
  });

  test("with no line to draw it keeps line form at the first rung every job names, so the band stays", () => {
    // One shape alone, and two models each on a shape of its own: no series reaches two rungs either way.
    const other = { model_descriptor: gguf("Other-2B") };
    for (const rows of [[...vl(1, 1000, 256, 2020), ...vl(1, 1000, 256, 1970, other)],
                        [...vl(1, 1000, 256, 2020, {}, 0), ...vl(5, 1000, 256, 1970, other)]]) {
      const drawn = view(index(rows, "full"), { ...none, benchmark: "vl_e2e_check" });
      expect(drawn.charts.map((c) => [c.form, c.form === "line" ? c.panels.map((p) => [p.band, p.reference]) : []]))
        .toEqual(drawn.charts.map(() => ["line", [[5, 0]]]));
    }
  });

  test("a model run with two flag sets is two points on two columns, each its own job's", () => {
    const flags = (threads: number) => ({ requested_runtime_flags: { threads } as never });
    const drawn = view(index([...vl(1, 1000, 256, 2000, flags(4)), ...vl(1, 1000, 256, 2420, flags(8)),
                              ...vl(5, 1000, 256, 2000, flags(4)), ...vl(5, 1000, 256, 2420, flags(8))], "full"),
                       { ...none, benchmark: "vl_e2e_check" });
    // A column each, folded into one panel as each holds one series.
    const chart = lines(drawn.charts)[0]!;
    expect([chart.legend.map((s) => /"threads":(\d)/.exec(s.label)?.[1]), medians(chart)]).toEqual([["4", "8"], [[0, 0], [21, 21]]]);
  });

  test("a plan of text and VL results checks each over its own types, a section each after the benchmarks", () => {
    const text = [...job("prefill_throughput", [["prefill_throughput", 1000, "tokens/sec"], ["ttft", 1000, "ms"]]),
                  ...job("decode_throughput", [["decode_throughput", 256, "tokens/sec"]],
                         { parameters: { prefill_tokens: 256, decode_tokens: 100 } }),
                  ...job("end_to_end_latency", [["end_to_end_latency", 2000, "ms"]],
                         { parameters: { prefill_tokens: 256, decode_tokens: 256 } })];
    const ix = index([...text, ...plan], "full");
    expect([ix.e2e, ix.vlE2e]).toEqual([index(text, "full").e2e, index(plan, "full").vlE2e]);
    for (const [benchmark, alone] of [["e2e_check", text], ["vl_e2e_check", plan]] as const) {
      const drawn = (rows: ResultRow[]) => view(index(rows, "full"), { ...none, benchmark });
      expect([drawn([...text, ...plan]).charts, drawn([...text, ...plan]).caption])
        .toEqual([drawn(alone).charts, drawn(alone).caption]);
    }
    expect(chartRefs(ix).map((c) => c.name)).toEqual([
      "Prefill throughput", "Prefill · time to first token", "Decode throughput", "E2E latency",
      "VL throughput · prefill throughput", "VL throughput · time to first token", "VL throughput · decode throughput",
      "VL throughput · E2E latency", "E2E latency vs prefill + decode", "VL E2E latency vs prompt + decode",
    ]);
  });
});

describe("speculative vs baseline", () => {
  const params = (batch: number) => ({ batch_size: batch, dataset_subset: "set-a", draft_length: 8 });
  const spec = (mode: "S" | "B", benchmark: string, client: string, batch: number, dt: number | null, e2e: number | null,
                device = "Bench A") => job("vl_spec_decode", [
    ...(dt === null ? [] : [["decode_throughput", dt, "tokens/sec"] as [string, number, string]]),
    ...(e2e === null ? [] : [["e2e_ms", e2e, "ms"] as [string, number, string]]),
    ...(mode === "S" ? [["accept_length", 3, "tokens"] as [string, number, string]] : []),
  ], { benchmark_id: benchmark, client_id: client, device_name: device, model_descriptor: speculative(), runtime_descriptor: SGLANG,
       parameters: params(batch) });
  const plan = [
    ...spec("S", "bench-1", "client-a", 1, 150, 1000), ...spec("B", "bench-1", "client-a", 1, 100, 1500),
    ...spec("S", "bench-2", "client-a", 4, 150, 1000), ...spec("B", "bench-2", "client-b", 4, 100, 1500),
    ...spec("S", "bench-3", "client-a", 8, 150, 1000),
    ...spec("S", "bench-4", "client-a", 16, 120, null), ...spec("B", "bench-4", "client-a", 16, 0, 900),
    ...spec("S", "bench-5", "client-a", 2, 200, 800), ...spec("S", "bench-5", "client-a", 2, 220, 800),
    ...spec("B", "bench-5", "client-a", 2, 100, 1200),
  ];
  const SGL = `Bench A · ${SGLANG_RT}`;

  test("the pairs list a runtime alone, as they pool its flag sets", () => {
    const newer = { ...SGLANG, server_version: "0.5.9" } as Runtime;
    const run = (runtime: Runtime, mode: "S" | "B") =>
      spec(mode, `bench-${String(runtime.type === "uv_sglang" && runtime.server_version)}`, "client-a", 1, 150, 1000)
        .map((r) => ({ ...r, runtime_descriptor: runtime, requested_runtime_flags: { speculative_enabled: mode === "S" } as never }));
    const ix = index([SGLANG, newer].flatMap((runtime) => [...run(runtime, "S"), ...run(runtime, "B")]), "full");
    const listed = (metric: string) => {
      const v = view(ix, { ...none, benchmark: "vl_spec_decode", metric });
      return v.runtimes.options;
    };
    const runtimes = [SGLANG_RT, label(newer)].sort();
    expect(listed("decode_speedup")).toEqual(runtimes);
    expect(listed("decode_throughput")).toEqual(runtimes.flatMap((rt) =>
      [false, true].map((on) => `${rt} + {"speculative_enabled":${on}}`)).sort());
  });

  test("a pair is one benchmark's two modes on one client, its speedups the ratio of the sides' medians", () => {
    const drawn = show(plan);
    expect(drawn.metric?.key).toBe("decode_speedup");
    const chart = lines(drawn.charts)[0]!;
    expect(chart.x).toBe("batch_size");
    expect(chart.rungs.map((r) => r.x)).toEqual([1, 2]);
    // The one column in view titles its panel, as each of several would.
    expect(headed(chart.panels[0]!)).toBe(SGL);
    expect(drawn.caption).not.toContain("Bench A");
    expect(chart.panels[0]!.reference).toBe(1);
    expect(medians(chart)).toEqual([[1.5, 2.1]]);
    expect(drawn.notes).toContain("Pairs a speculative and a baseline job of one benchmark on one device; other launch "
      + "settings are not on a result row.");
    expect(drawn.caption).toContain("dataset_subset=set-a");
    expect(drawn.caption).not.toContain("mode=speculative");
    const e2e = lines(show(plan, { metric: "e2e_speedup" }).charts)[0]!;
    expect(e2e.rungs.map((r) => r.x)).toEqual([1, 2]);
    expect(medians(e2e)).toEqual([[1.5, 1.5]]);
  });

  test("its metrics come derived first, the rest by name: VL throughput's order is its own", () => {
    expect(index(plan, "full").families[0]!.metrics.map((m) => m.key))
      .toEqual(["decode_speedup", "e2e_speedup", "accept_length", "decode_throughput", "e2e_ms"]);
  });

  test("a subset paired at one batch size alone keeps line form, so its speedup keeps the reference line", () => {
    const subset = (name: string, batch: number) => ({ parameters: { ...params(batch), dataset_subset: name } });
    const at = (name: string, batch: number) => [...job("vl_spec_decode", [["decode_throughput", 150, "tokens/sec"],
      ["accept_length", 3, "tokens"]], { benchmark_id: `${name}-${batch}`, runtime_descriptor: SGLANG, ...subset(name, batch) }),
      ...job("vl_spec_decode", [["decode_throughput", 100, "tokens/sec"]],
             { benchmark_id: `${name}-${batch}`, runtime_descriptor: SGLANG, ...subset(name, batch) })];
    const drawn = show([...at("set-a", 1), ...at("set-a", 2), ...at("set-b", 1)]);
    expect(drawn.charts.map((c) => [c.form, c.group, c.form === "line" ? c.panels.map((p) => p.reference) : []]))
      .toEqual([["line", "dataset_subset=set-a", [1]], ["line", "dataset_subset=set-b", [1]]]);
    // With no line to draw, the speedup keeps line form at the first rung every pair names.
    const alone = lines(show(at("set-a", 1)).charts);
    expect(alone.map((c) => [c.x, c.panels.map((p) => p.reference)])).toEqual([["batch_size", [1]]]);
  });

  test("the tab counts the benchmarks paired on one device, on different devices only, and in one mode", () => {
    const ix = index(plan, "full");
    expect(ix.benchmarks.map((f) => [f.key, f.measured, f.failed])).toEqual([["vl_spec_decode", 10, 0]]);
    expect(ix.pairing).toEqual({ kind: "paired", benchmarks: 5, paired: 3, split: 1, one: 1 });
    expect(show(plan, { benchmark: "vl_spec_decode" }).notes[0]).toBe(pairingText(ix.pairing!));
    const of = (...ids: string[]) => index(plan.filter((r) => ids.includes(r.benchmark_id)), "full").pairing;
    expect(of("bench-1", "bench-5")).toEqual({ kind: "paired", benchmarks: 2, paired: 2, split: 0, one: 0 });
    expect(of("bench-2", "bench-3")).toEqual({ kind: "paired", benchmarks: 2, paired: 0, split: 1, one: 1 });
    expect(index([row()], "full").pairing).toBeNull();
  });

  test("below the full tier nothing pairs, and the raw metrics split by mode", () => {
    const withheld = plan.map((r) => ({ ...r, device_name: null, client_id: null }));
    const ix = index(withheld, "withheld");
    expect(ix.families[0]!.metrics.map((m) => m.key)).not.toContain("decode_speedup");
    expect(ix.pairing).toEqual({ kind: "withheld" });
    // With no speculative job measured there is nothing to pair, at either tier.
    const lost = withheld.map((r) => ({ ...r, failed: true, value: null }));
    expect(index(lost, "withheld").pairing).toBeNull();
    const drawn = view(ix, { ...none, benchmark: "vl_spec_decode" });
    expect(drawn.metric?.key).toBe("decode_throughput");
    expect(drawn.pins.map((c) => c.key)).toContain("p.mode");
    expect(lines(drawn.charts)[0]!.legend.map((s) => s.label).sort()).toEqual(["mode=baseline", "mode=speculative"]);
  });
});

describe("the address", () => {
  const plan = [
    ...[256, 512].flatMap((tokens) => [1, 2].flatMap((batch) => [
      row({ parameters: { prefill_tokens: tokens, batch_size: batch } }),
      row({ parameters: { prefill_tokens: tokens, batch_size: batch }, model_descriptor: gguf("Tiny-1B", "Q8_0"),
            device_name: "Bench B" }),
      row({ benchmark_type: "decode_throughput", metric: "decode_throughput",
            parameters: { prefill_tokens: tokens, batch_size: batch }, model_descriptor: gguf("Other-2B", "Q8_0") }),
    ])),
    ...job("prefill_throughput", [["ttft", 40, "ms"]]),
  ];
  const ix = index(plan, "full");

  test("filters survive a round trip through the address, a chart tab's benchmark in its path and Rows' in its query", () => {
    const filters: ReportFilters = {
      benchmark: "decode_throughput", metric: "decode_throughput", device: ["Bench B", "Bench A"],
      drop: [`${MAC_RT} + {"threads":8}`], model: [D("Other-2B"), D("Tiny-1B")],
      per: "4", params: { batch_size: "2" }, status: "failed",
    };
    const pairs = reportPairs(filters);
    expect(pairs).toEqual([
      ["metric", "decode_throughput"], ["device", "Bench B"], ["device", "Bench A"],
      ["drop", `${MAC_RT} + {"threads":8}`], ["model", D("Other-2B")],
      ["model", D("Tiny-1B")], ["per", "4"],
      ["p.batch_size", "2"], ["status", "failed"],
    ]);
    expect(reportFilters(new URLSearchParams(pairs), "decode_throughput")).toEqual(filters);
    const device = { ...none, device: ["Bench B"] };
    expect(reportFilters(new URLSearchParams(reportPairs(device)), "")).toEqual(device);
  });

  test("unknown and stale values read as the default", () => {
    const stale = new URLSearchParams([
      ["metric", "accuracy"], ["device", "Nowhere"], ["model", "nowhere"], ["per", "7"],
      ["p.nope", "1"], ["p.batch_size", "99"], ["p.prefill_tokens", "256"], ["status", "maybe"],
    ]);
    expect(view(ix, reportFilters(stale, "nope")).filters).toEqual(none);
    // A family drops the metric another has; a retired `column` or `quant` key reads as nothing.
    expect(view(ix, reportFilters(new URLSearchParams("column=" + encodeURIComponent(MAC_B)
      + "&quant=Q4_K_M&metric=ttft"), "decode_throughput")).filters).toEqual({ ...none, benchmark: "decode_throughput" });
  });

  test("a control's change sets one filter; an empty parameter unpins it; another benchmark opens on its own metric", () => {
    const pinned = withFilter({ ...none, metric: "ttft", model: [D("Tiny-1B")] }, "p.batch_size", "2");
    expect(pinned.params).toEqual({ batch_size: "2" });
    expect(withFilter(pinned, "p.batch_size", "").params).toEqual({});
    expect(withFilter(pinned, "per", "4")).toEqual({ ...pinned, per: "4" });
    // The model pick is a list no select writes, and an unknown key is none of the filters.
    expect([withFilter(pinned, "model", "x"), withFilter(pinned, "nope", "x")]).toEqual([pinned, pinned]);
    expect(withFilter(pinned, "benchmark", "decode_throughput"))
      .toEqual({ ...pinned, benchmark: "decode_throughput", metric: "", params: {} });
  });

  test("the chosen model descriptors narrow the charts; none on the device type drawn draws nothing, saying so", () => {
    const at = (model: Model) => [256, 512].map((tokens) => row({ parameters: { prefill_tokens: tokens }, model_descriptor: model }));
    const rows = [...at(gguf("Tiny-1B", "Q4_K_M")), ...at(gguf("Tiny-1B", "Q8_0")), ...at(gguf("Other-2B", "Q8_0"))];
    const legend = (filters: Partial<ReportFilters>) => lines(show(rows, filters).charts)[0]!.legend.map((s) => s.label);
    expect(legend({ model: [D("Tiny-1B", "Q8_0"), D("Other-2B", "Q8_0")] }).sort())
      .toEqual([G("Other-2B", "Q8_0"), G("Tiny-1B", "Q8_0")]);
    expect(legend({ model: [D("Tiny-1B")] })).toEqual([G("Tiny-1B")]);
    const elsewhere = view(ix, { ...none, benchmark: "prefill_throughput", device: ["Bench B"], model: [D("Tiny-1B")] });
    expect([elsewhere.charts, elsewhere.empty]).toEqual([[], "No results match these filters."]);
  });

  test("a family draws every model where it measured none chosen, the address keeping them for the others", () => {
    const drawn = view(ix, { ...none, benchmark: "decode_throughput", model: [D("Tiny-1B")] });
    expect(drawn.filters.model).toEqual([D("Tiny-1B")]);
    expect(drawn.empty).toBeNull();
    expect(drawn.charts).not.toEqual([]);
  });

  test("a chart has a line per runtime, each with the flags its jobs asked for, drawn until dropped", () => {
    const run = (threads: number | null, value: number, runtime_descriptor: Runtime = MAC) => [256, 512].map((tokens) => row({
      value, parameters: { prefill_tokens: tokens }, runtime_descriptor,
      // The keys that name the cell are no knob, and what the client reported (`ctx_size`) is not what was asked.
      requested_runtime_flags: threads === null ? null
        : { benchmark_type: "prefill_throughput", runtime_type: "llamacpp_cli_stock_tools", threads } as never,
      runtime_flags: { ctx_size: tokens } as never,
    }));
    const rows = [...run(8, 100), ...run(4, 60), ...run(null, 30, BOX)];
    const [eight, four] = [8, 4].map((threads) => `${MAC_RT} + {"threads":${threads}}`);
    const all = [BOX_RT, four!, eight!].sort();
    const drawn = show(rows);
    expect(drawn.runtimes).toEqual({ options: all, dropped: [] });
    // Each reads as its runtime, then the flags its jobs asked for where they asked any.
    expect([drawn.runtimeLines[eight!], drawn.runtimeLines[BOX_RT]])
      .toEqual([[`runtime = ${MAC_RT}`, `runtime flags = {"threads":8}`], [`runtime = ${BOX_RT}`]]);
    // Each flag set charts apart, as another runtime does, never pooled into a median neither ran.
    const chart = lines(drawn.charts)[0]!;
    expect(medians(chart)).toEqual([[100, 100], [60, 60], [30, 30]]);
    // Each point keeps the rows behind it, which a click on it lists by job.
    expect(chart.panels[0]!.series.map((s) => s.points.map((at) => at.map((r) => r.value))))
      .toEqual([[[100], [100]], [[60], [60]], [[30], [30]]]);
    // One model on three columns folds into one panel, a line each, named by its runtime and flags.
    expect(chart.legend.map((s) => s.label))
      .toEqual([eight, four, BOX_RT].map((v) => `Bench A · ${v}`));
    expect(drawn.notes).toEqual([]);
    const dropped = show(rows, { drop: [four!, "nowhere"] });
    expect([dropped.runtimes, dropped.filters.drop]).toEqual([{ options: all, dropped: [four] }, [four]]);
    expect(medians(lines(dropped.charts)[0]!)).toEqual([[100, 100], [30, 30]]);
    // A click drops a drawn runtime and draws a dropped one again.
    const ix = index(rows, "full");
    const prefill = { ...none, benchmark: "prefill_throughput" };
    expect(toggledRuntime(ix, prefill, four!)).toEqual([four]);
    expect(toggledRuntime(ix, { ...prefill, drop: [four!] }, eight!)).toEqual([four, eight]);
    expect(toggledRuntime(ix, { ...prefill, drop: [four!] }, four!)).toEqual([]);
    // The lines are the runtimes the other filters leave: another model's own, here.
    expect(show([...rows, row({ model_descriptor: gguf("Other-2B"), runtime_descriptor: VLLM })], { model: [D("Other-2B")] })
      .runtimes.options).toEqual([canonical(VLLM)]);
    // A flag set another chart ran drops this one's flag sets of that runtime, and a click here lets go of it.
    const two = `${MAC_RT} + {"threads":2}`;
    const other = index([...rows, ...job("decode_throughput", [["decode_throughput", 5, "tokens/sec"]],
      { requested_runtime_flags: { threads: 2 } as never }),
      ...job("decode_throughput", [["decode_throughput", 5, "tokens/sec"]], { runtime_descriptor: VLLM })], "full");
    const elsewhere = view(other, { ...prefill, drop: [two] });
    expect([elsewhere.runtimes.dropped, elsewhere.filters.drop]).toEqual([[four, eight].sort(), [two]]);
    expect(toggledRuntime(other, { ...prefill, drop: [two] }, four!)).toEqual([]);
    // A runtime a chart did not run at all drops nothing there, so a drop leaves another chart's own runtimes be.
    expect(view(other, { ...prefill, drop: [canonical(VLLM)] }).runtimes).toEqual({ options: all, dropped: [] });
    expect(view(other, { ...none, benchmark: "decode_throughput", drop: [four!] }).runtimes)
      .toEqual({ options: [two, canonical(VLLM)].sort(), dropped: [two] });
  });

  test("the E2E check, drawn across flag sets, takes a dropped one for its runtime", () => {
    const at = (type: string, metric: string, value: number, flags: object, runtime_descriptor: Runtime) =>
      job(type, [[metric, value, type === "end_to_end_latency" ? "ms" : "tokens/sec"]], {
        runtime_descriptor, requested_runtime_flags: flags as never,
        parameters: { prefill_tokens: 256, ...(type === "end_to_end_latency" ? { decode_tokens: 64 } : {}) },
      });
    const on = (runtime: Runtime) => [
      ...at("prefill_throughput", "prefill_throughput", 256, { threads: 8 }, runtime),
      ...at("decode_throughput", "decode_throughput", 64, { threads: 8 }, runtime),
      ...at("end_to_end_latency", "end_to_end_latency", 2000, { threads: 8, no_cache: true }, runtime),
    ];
    const ix = index([...on(MAC), ...on(BOX)], "full");
    const check = (drop: string[]) => view(ix, { ...none, benchmark: "e2e_check", drop }).runtimes;
    expect(check([])).toEqual({ options: [BOX_RT, MAC_RT].sort(), dropped: [] });
    expect(check([`${MAC_RT} + {"threads":8}`]).dropped).toEqual([MAC_RT]);
    // Its own line, a runtime alone that no job ran without flags, drops that runtime in every chart.
    expect(toggledRuntime(ix, { ...none, benchmark: "e2e_check" }, MAC_RT)).toEqual([MAC_RT]);
    expect(check([MAC_RT]).dropped).toEqual([MAC_RT]);
    expect(view(ix, { ...none, benchmark: "prefill_throughput", drop: [MAC_RT] }).runtimes.dropped)
      .toEqual([`${MAC_RT} + {"threads":8}`]);
  });

  test("the page offers its device types, one included, and a chart's size where the plan has a choice", () => {
    const page = pageControls(ix, { ...none, device: ["Bench B", "Nowhere"] });
    expect(page.device).toEqual({ options: ["Bench A", "Bench B"], chosen: ["Bench B"] });
    expect(page.per).toMatchObject({ value: "8", disabled: false });
    // One device type leaves nothing to pick; an all-failed plan offers no choice at all.
    // One type is a bar too, so every plan's page reads alike.
    expect(pageControls(index([row()], "full"), none).device).toEqual({ options: ["Bench A"], chosen: [] });
    expect(pageControls(index([failedRow(), failedRow()], "full"), none)).toEqual({ device: null, per: null });
    expect(view(index([failedRow(), failedRow()], "full"), { ...none, benchmark: "prefill_throughput" }).pins).toEqual([]);
  });
});

describe("the index's families", () => {
  const plan = [
    ...job("prefill_throughput", [["prefill_throughput", 900, "tokens/sec"], ["ttft", 300, "ms"]]),
    ...job("prefill_throughput", [["prefill_throughput", 800, "tokens/sec"], ["ttft", 320, "ms"]],
           { device_name: "Box", runtime_descriptor: BOX, model_descriptor: gguf("Other-2B") }),
    ...job("decode_throughput", [["decode_throughput", 40, "tokens/sec"]]),
    failedRow({ runtime_descriptor: MAC }),
    failedRow({ runtime_descriptor: BOX }),
  ];
  const BOX_A = `Box · ${BOX_RT}`;

  test("names each type by its short label, an unknown one by its key", () => {
    expect(["prefill_throughput", "vl_max_memory", "asr_throughput", "asr_max_memory", "custom_probe"].map(shortLabel))
      .toEqual(["Prefill", "VL memory", "ASR throughput", "ASR memory", "custom_probe"]);
  });

  test("the Overview counts measured and failed jobs by cause, columns and models", () => {
    const failures = [failedRow(), failedRow({ failure_source: "expired" }), failedRow({ failure_source: null })];
    expect(index([...plan, ...failures], "full").benchmarks[0]).toMatchObject({
      key: "prefill_throughput", short: "Prefill", measured: 2, failed: 5, models: 2, columns: [MAC_A, BOX_A],
      causes: [["every client denied it", 3], ["expired", 1], ["no cause given", 1]],
    });
    expect(index([row(), failedRow({ failure_source: null, client_id: null })], "withheld").benchmarks[0]!.causes)
      .toEqual([["cause withheld", 1]]);
  });
});

describe("missing data", () => {
  test("lists each measured row without a model parameter count, in the plan's order", () => {
    const none = { model_params_total_millions: null, model_params_active_millions: null };
    const big = gguf("Big-8B");
    const rows = [
      row(), row({ ...none, model_descriptor: big }), row({ ...none }), row({ ...none, model_descriptor: big }),
      failedRow({ ...none, model_descriptor: gguf("Gone-1B") }),
    ];
    const missing = index(rows, "full").missing!;
    expect(missing.params.map((m) => m.row.job_id)).toEqual([rows[1], rows[2], rows[3]].map((r) => r!.job_id));
    expect(missing.params[0]!.descriptor).toBe(canonical(big));
    expect(missing.measured).toBe(4);
    expect(index([row()], "full").missing).toBeNull();
  });
});

describe("swap", () => {
  const swapped = (...observations: Record<string, unknown>[]) =>
    index(observations.map((o) => row({ observations: o })), "full").swapped;

  test("is offered where a run held swap, an eval's included, as Cells' `swap` issue reads it", () => {
    expect(swapped({ max_swap_bytes: 4096 })).toBe(true);
    expect(swapped({ max_swap_bytes: 0 }, { max_host_bytes: 9e9 }, {})).toBe(false);
    expect(index([row({ benchmark_type: "eval", metric: "accuracy", unit: "ratio", value: 0.5,
                        observations: { max_swap_bytes: 1e9 } })], "full").swapped).toBe(true);
  });

  test("charts each job's swap once, a panel per benchmark type, for the models that swapped, most first", () => {
    const at = (type: string, tokens: number, gb: number, model = "Tiny-1B") =>
      job(type, type === "prefill_throughput" ? [[type, 900, "tokens/sec"], ["ttft", 300, "ms"]] : [[type, 40, "tokens/sec"]],
          { parameters: { prefill_tokens: tokens }, observations: { max_swap_bytes: gb * 1e9 }, model_descriptor: gguf(model) });
    const rows = [
      ...at("prefill_throughput", 256, 0), ...at("prefill_throughput", 512, 2),
      ...at("decode_throughput", 256, 0.5), ...at("decode_throughput", 512, 0),
      ...at("prefill_throughput", 256, 3, "Big-7B"), ...at("prefill_throughput", 512, 0, "Big-7B"),
      // Never swapped: left out of the chart, counted in the note.
      ...at("prefill_throughput", 256, 0, "Calm-1B"), ...at("decode_throughput", 256, 0, "Calm-1B"),
    ];
    const ix = index(rows, "full");
    expect(ix.families.at(-1)).toMatchObject({ key: SWAPPED, types: ["prefill_throughput", "decode_throughput"] });
    expect(chartRefs(ix).at(-1)!.name).toBe("Swap");
    const drawn = view(ix, { ...none, benchmark: SWAPPED });
    const chart = lines(drawn.charts)[0]!;
    expect(chart.panels.map((p) => p.title)).toEqual(["Bench A · prefill throughput", "Bench A · decode throughput"]);
    expect(chart.legend.map((s) => s.label)).toEqual([G("Big-7B"), G("Tiny-1B")]);
    // A prefill job's two rows are one swap reading.
    expect(medians(chart, 0)).toEqual([[3, 0], [0, 2]]);
    expect(medians(chart, 1)).toEqual([[null, null], [0.5, 0]]);
    expect(drawn.notes).toContain(
      "Only models that swapped on a device type and runtime are drawn: 2 of 3. A run's swap is read once per job, 0 where it held none.");
    expect(index(rows.map((r) => ({ ...r, observations: { max_swap_bytes: 0 } })), "full").families.map((f) => f.key))
      .not.toContain(SWAPPED);
  });

  test("charts the VL types' swap apart, over their own prompt's parameters, a text plan's Swap as it was", () => {
    const prompt = (images: number, text: number) => ({ image_width: 256, image_height: 256, text_tokens: text, num_images: images });
    const at = (type: string, images: number, gb: number, text = 1024) => job(type,
      type === "vl_throughput" ? [["decode_throughput", 40, "tokens/sec"], ["ttft", 300, "ms"]] : [["max_host_usage", 3e9, "bytes"]],
      { parameters: type === "vl_throughput" ? { ...prompt(images, text), decode_tokens: 256 } : prompt(images, text),
        observations: { max_swap_bytes: gb * 1e9 } });
    const vl = [...at("vl_throughput", 1, 0), ...at("vl_throughput", 5, 1), ...at("vl_throughput", 16, 2),
                ...at("vl_max_memory", 1, 0), ...at("vl_max_memory", 16, 3)];
    // Never swapped: counted in the note, not drawn.
    const calm = [1, 5].flatMap((images) => at("vl_throughput", images, 0).map((r) => ({ ...r, model_descriptor: gguf("Calm-1B") })));
    // Two batch sizes and a type that names none: the text Swap keeps batch_size, which VL's prompt does not name.
    const text = [256, 512].flatMap((tokens) => [1, 2].flatMap((batch) => job("prefill_throughput",
      [["prefill_throughput", 900, "tokens/sec"]], { parameters: { prefill_tokens: tokens, batch_size: batch },
                                                    observations: { max_swap_bytes: batch * 1e9 } })))
      .concat(memory("Bench A", MAC, { host: 1 }).map((r) => ({ ...r, observations: { max_swap_bytes: 1e9 } })));
    const ix = index([...text, ...vl, ...calm], "full");
    expect(ix.families.slice(-2).map((f) => [f.key, f.types])).toEqual([
      [SWAPPED, ["prefill_throughput", "max_memory_usage"]], [VL_SWAPPED, ["vl_throughput", "vl_max_memory"]]]);
    expect(chartRefs(ix).slice(-2).map((c) => c.name)).toEqual(["Swap", "VL swap"]);
    const drawn = view(ix, { ...none, benchmark: VL_SWAPPED });
    const chart = lines(drawn.charts)[0]!;
    expect([chart.x, chart.rungs.map((r) => r.x)]).toEqual(["num_images", [1, 5, 16]]);
    expect(chart.panels.map((p) => p.title)).toEqual(["Bench A · VL throughput", "Bench A · VL peak memory"]);
    expect([medians(chart, 0), medians(chart, 1)]).toEqual([[[0, 1, 2]], [[0, null, 3]]]);
    // A point lists its own type's job alone, not the other type's at that x.
    expect(chart.panels.map((p) => [...new Set(p.series[0]!.points[0]!.map((r) => r.benchmark_type))]))
      .toEqual([["vl_throughput"], ["vl_max_memory"]]);
    // The prompt's parameters alone, which both types name: no decode_tokens.
    expect(drawn.caption).toEqual([G("Tiny-1B"), "image_height=256", "image_width=256", "text_tokens=1024"]);
    expect(drawn.notes).toContain("Only models that swapped on a device type and runtime are drawn: 1 of 2. A run's swap "
      + "is read once per job, 0 where it held none.");
    const textSwap = view(ix, { ...none, benchmark: SWAPPED });
    expect(lines(textSwap.charts)[0]!.legend.map((s) => s.label)).toEqual(["batch_size=2", "batch_size=1"]);
    expect(view(index(text, "full"), { ...none, benchmark: SWAPPED })).toMatchObject({ charts: textSwap.charts });
    // An image alone draws after the lines as bars, a type each: one bar apiece, so one chart of both.
    const alone = view(index([...vl, ...at("vl_throughput", 1, 0.5, 0), ...at("vl_max_memory", 1, 0.25, 0)], "full"),
                       { ...none, benchmark: VL_SWAPPED });
    expect(alone.charts.map((c) => [c.form, c.group])).toEqual([["line", "text_tokens=1024"], ["bars", "text_tokens=0"]]);
    const imageAlone = bars(alone.charts.slice(1))[0]!.bars;
    expect([imageAlone.map((b) => [b.label, b.name, b.cell.median]), new Set(imageAlone.map((b) => b.key)).size]).toEqual([
      [["num_images=1 · VL throughput", "num_images=1 · VL throughput", 0.5],
       ["num_images=1 · VL peak memory", "num_images=1 · VL peak memory", 0.25]], 2]);
    expect(bars(alone.charts.slice(1))[0]!.title).toBe("max swap");
    // Two models' image alone: a chart per type, the metric after it.
    const big = [...at("vl_throughput", 1, 1, 0), ...at("vl_max_memory", 1, 2, 0)]
      .map((r) => ({ ...r, model_descriptor: gguf("Big-7B") }));
    const two = view(index([...vl, ...at("vl_throughput", 1, 0.5, 0), ...at("vl_max_memory", 1, 0.25, 0), ...big],
                           "full"), { ...none, benchmark: VL_SWAPPED });
    expect(bars(two.charts.slice(1)).map((c) => [c.title, c.bars.length])).toEqual([
      ["num_images=1 · VL throughput · max swap", 2], ["num_images=1 · VL peak memory · max swap", 2]]);
  });

  test("charts the ASR types' swap apart, over their clip's parameters", () => {
    const clip = (ms: number) => ({ audio_duration_ms: ms, audio_sample_rate_hz: 16000, audio_channels: 1 });
    const at = (type: string, ms: number, gb: number, sizing = "exact") => job(type,
      type === "asr_throughput" ? [["decode_throughput", 40, "tokens/sec"], ["ttft", 300, "ms"]] : [["max_host_usage", 3e9, "bytes"]],
      { parameters: { ...clip(ms), decode_tokens: 256 },
        observations: { max_swap_bytes: gb * 1e9, asr_context_sizing: sizing, ...(type === "asr_throughput"
          && { asr_audio_tokens_source: sizing === "exact" ? "calibrated" : "measured" }) } });
    const asr = [...at("asr_throughput", 2500, 0), ...at("asr_throughput", 10000, 1), ...at("asr_throughput", 29900, 2),
                 ...at("asr_max_memory", 2500, 0), ...at("asr_max_memory", 29900, 3)];
    // At the same parameters, an upper-bound context's swap, which would move every median it joined.
    const unsized = [...at("asr_throughput", 2500, 8, "upper_bound"), ...at("asr_max_memory", 29900, 9, "upper_bound")];
    const vl = job("vl_throughput", [["decode_throughput", 40, "tokens/sec"]],
      { parameters: { num_images: 1, text_tokens: 0, decode_tokens: 256 }, observations: { max_swap_bytes: 1e9 } });
    const ix = index([...asr, ...vl, row({ observations: { max_swap_bytes: 1e9 } })], "full");
    expect(ix.families.slice(-3).map((f) => [f.key, f.types])).toEqual([
      [SWAPPED, ["prefill_throughput"]], [VL_SWAPPED, ["vl_throughput"]], [ASR_SWAPPED, ["asr_throughput", "asr_max_memory"]]]);
    expect(chartRefs(ix).slice(-1).map((c) => c.name)).toEqual(["ASR swap"]);
    const drawn = view(ix, { ...none, benchmark: ASR_SWAPPED });
    const chart = lines(drawn.charts)[0]!;
    expect([chart.x, chart.rungs.map((r) => r.x)]).toEqual(["audio_duration_ms", [2500, 10000, 29900]]);
    expect(chart.panels.map((p) => p.title)).toEqual(["Bench A · ASR throughput", "Bench A · ASR peak memory"]);
    expect([medians(chart, 0), medians(chart, 1)]).toEqual([[[0, 1, 2]], [[0, null, 3]]]);
    expect(drawn.notes.some((n) => n.startsWith("Only ASR jobs"))).toBe(false);
    // The upper-bound jobs join no cell, and the note counts them out.
    const mixed = view(index([...asr, ...unsized, ...vl], "full"), { ...none, benchmark: ASR_SWAPPED });
    const both = lines(mixed.charts)[0]!;
    expect([medians(both, 0), medians(both, 1)]).toEqual([[[0, 1, 2]], [[0, null, 3]]]);
    expect(mixed.notes).toContain("Only ASR jobs whose context was sized from the clip's own audio tokens (exact) are "
      + "compared: 5 of 7 jobs. An upper-bound or pinned context sizes the KV cache, and with it the swap, to more than "
      + "the clip.");
    // A type with no exact read left still keeps the other type's clips apart.
    const memoryUnsized = [...asr.filter((r) => r.benchmark_type === "asr_throughput"),
                           ...at("asr_max_memory", 2500, 0, "pinned")];
    const throughputOnly = lines(view(index(memoryUnsized, "full"), { ...none, benchmark: ASR_SWAPPED }).charts)[0]!;
    expect([throughputOnly.rungs.map((r) => r.x), medians(throughputOnly, 0)]).toEqual([[2500, 10000, 29900], [[0, 1, 2]]]);
    // None exact, or only a job the rule sets aside swapped: no tab, as it would draw nothing.
    expect(index(unsized, "full").families.map((f) => f.key)).not.toContain(ASR_SWAPPED);
    const calm = [...at("asr_throughput", 2500, 0), ...at("asr_throughput", 10000, 0),
                  ...at("asr_throughput", 29900, 2, "upper_bound")];
    expect(index(calm, "full").families.map((f) => f.key)).not.toContain(ASR_SWAPPED);
    // The parameters both types name: the clip's, and the decode both run.
    expect(drawn.caption).toEqual([G("Tiny-1B"), "audio_channels=1", "audio_sample_rate_hz=16000",
      "decode_tokens=256"]);
    // An ASR row never reads on the text Swap, whose prompt is a prefill length.
    expect(view(ix, { ...none, benchmark: SWAPPED }).charts.flatMap((c) => drawnRows([c], new Set()).size)).toEqual([1]);
  });

  test("the ASR swap tab neither counts nor filters by a model whose every job its rule sets aside", () => {
    const at = (name: string, ms: number, sizing: string) => job("asr_throughput", [["ttft", 300, "ms"]], {
      model_descriptor: gguf(name), parameters: { audio_duration_ms: ms, audio_sample_rate_hz: 16000, audio_channels: 1 },
      observations: { max_swap_bytes: 1e9, asr_context_sizing: sizing } });
    const ix = index([...at("Tiny-Audio", 2500, "exact"), ...at("Tiny-Audio", 10000, "exact"),
                      ...at("Wide-Audio", 2500, "upper_bound"), ...at("Wide-Audio", 10000, "upper_bound")], "full");
    expect(ix.families.find((f) => f.key === ASR_SWAPPED)!.models).toBe(1);
    // Picked from another tab, it filters nothing here.
    const picked = view(ix, { ...none, benchmark: ASR_SWAPPED, model: [D("Wide-Audio")] });
    expect([picked.empty, lines(picked.charts)[0]!.legend.map((s) => s.label)]).toEqual([null, [G("Tiny-Audio")]]);
    expect(picked.notes).toContain("Only models that swapped on a device type and runtime are drawn: 1 of 1. A run's "
      + "swap is read once per job, 0 where it held none.");
  });

  test("a swap bar title names the metric after the type, as the unit follows; a line panel's keeps the type", () => {
    const at = (type: string, tokens: number, batch: number, gb: number, model: string) => job(type,
      [[type, 100, "tokens/sec"]], { parameters: { prefill_tokens: tokens, batch_size: batch },
                                     observations: { max_swap_bytes: gb * 1e9 }, model_descriptor: gguf(model) });
    const rows = ["Tiny-1B", "Big-7B"].flatMap((model, k) => [
      ...at("prefill_throughput", 256, 1, k, model), ...at("prefill_throughput", 512, 1, k + 1, model),
      ...at("decode_throughput", 256, 1, k, model), ...at("decode_throughput", 512, 1, k + 1, model),
      // One prompt length at batch 2: a lone combination, drawn as bars after the lines.
      ...at("prefill_throughput", 512, 2, k + 2, model), ...at("decode_throughput", 512, 2, k + 3, model),
    ]);
    const drawn = view(index(rows, "full"), { ...none, benchmark: SWAPPED });
    expect(drawn.charts.map((c) => [c.form, c.group, c.title])).toEqual([
      ["line", "batch_size=1", ""],
      ["bars", "batch_size=2", "prefill_tokens=512 · prefill throughput · max swap"],
      ["bars", "batch_size=2", "prefill_tokens=512 · decode throughput · max swap"],
    ]);
    // Its unit tops the axis, so the title stays the type's; the metric is for the screen reader's caption.
    expect(lines(drawn.charts)[0]!.panels.map((p) => [p.title, p.metric?.label ?? null])).toEqual([
      ["Bench A · prefill throughput", "max swap"], ["Bench A · decode throughput", "max swap"]]);
    // One type alone draws no layered chart, so its bars' title names only the combination.
    const single = view(index(rows.filter((r) => r.benchmark_type === "prefill_throughput"), "full"),
                        { ...none, benchmark: SWAPPED });
    expect(single.charts.map((c) => [c.form, c.group, c.title]))
      .toEqual([["line", "batch_size=1", ""], ["bars", "batch_size=2", "prefill_tokens=512"]]);
    // A memory chart's layer is the pool it measures: no metric after it.
    const memoryChart = lines(show([...memory("Box", BOX, { host: 0.4, gpu: 6 }),
                                    ...memory("Box", BOX, { host: 0.5, gpu: 7 }, 512)]).charts)[0]!;
    expect(memoryChart.panels.map((p) => [p.title, p.metric?.label ?? null]))
      .toEqual([["Box · host memory", null], ["Box · GPU memory", null]]);
  });
});


describe("catalog names", () => {
  const digest = (c: string) => c.repeat(64);
  const NAMES: DescriptorNames = {
    [digest("a")]: { display: "Tiny-1B::gguf::q4_k_m", name: "Tiny-1B", synonyms: [] },
    [digest("b")]: { display: "llamacpp-v0.7.0::macos-arm64", name: "llamacpp-v0.7.0", synonyms: [] },
  };
  const TINY = { descriptor: G("Tiny-1B"), name: NAMES[digest("a")]! };
  const RUNTIME = { descriptor: MAC_RT, name: NAMES[digest("b")]! };
  /** A decode row of the named model on the named runtime, unless `over` says otherwise. */
  const decode = (tokens: number, over: Partial<ResultRow> = {}) => row({
    benchmark_type: "decode_throughput", metric: "decode_throughput", value: tokens, parameters: { prefill_tokens: tokens },
    model_descriptor_sha256: digest("a"), runtime_descriptor_sha256: digest("b"), ...over,
  });
  const swept = (over: Partial<ResultRow> = {}) => [256, 512].map((tokens) => decode(tokens, over));
  const other = { model_descriptor: gguf("Other-1B"), model_descriptor_sha256: digest("c") };
  const named = (rows: ResultRow[]) =>
    view(index(rows, "full", {}, NAMES), { ...none, benchmark: "decode_throughput" });

  test("a named model's legend entry draws its name and reads as its display; its key stays the descriptor's", () => {
    const drawn = named([...swept(), ...swept(other)]);
    const legend = lines(drawn.charts)[0]!.legend.map((s) => [s.key, s.label, s.parts]);
    expect(legend).toHaveLength(2);
    expect(legend).toEqual(expect.arrayContaining([
      [G("Other-1B"), G("Other-1B"), [G("Other-1B")]],
      [G("Tiny-1B"), "Tiny-1B::gguf::q4_k_m", [TINY]],
    ]));
    expect(drawn.caption).toEqual([]);
  });

  test("a model all series share is named in the caption", () => {
    expect(named(swept()).caption).toEqual([TINY]);
  });

  test("a named runtime's lines lead with its name, its flags after; an unnamed one reads `runtime = …`", () => {
    const flags = { requested_runtime_flags: { threads: 8 } as never };
    const box = { runtime_descriptor: BOX, runtime_descriptor_sha256: digest("d") };
    // Two models a runtime, so its panel holds two series and is not folded into one with the other's.
    const drawn = named([...swept(flags), ...swept({ ...flags, ...other }), ...swept(box), ...swept({ ...box, ...other })]);
    const variant = `${MAC_RT} + ${canonical({ threads: 8 })}`;
    expect(drawn.runtimeLines).toEqual({
      [variant]: [RUNTIME, `runtime flags = ${canonical({ threads: 8 })}`],
      [BOX_RT]: [`runtime = ${BOX_RT}`],
    });
    expect(lines(drawn.charts)[0]!.panels.map((p) => p.runtime)).toEqual(
      [[RUNTIME, `runtime flags = ${canonical({ threads: 8 })}`], [`runtime = ${BOX_RT}`]]);
  });

  test("a bar names a named runtime by its name, in its gutter and its label", () => {
    const one = (over: Partial<ResultRow> = {}) => decode(256, over);
    const drawn = named([one(), one({ runtime_descriptor: BOX, runtime_descriptor_sha256: null })]);
    const drawnBars = bars(drawn.charts)[0]!.bars.map((b) => [b.nameParts, b.label]);
    expect(drawnBars).toHaveLength(2);
    expect(drawnBars).toEqual(expect.arrayContaining([
      [[BOX_RT], `Bench A · ${BOX_RT}`], [[RUNTIME], "Bench A · llamacpp-v0.7.0::macos-arm64"],
    ]));
  });

  test("two runtimes under one display stay apart in a bar's gutter", () => {
    const twin: DescriptorNames = { ...NAMES, [digest("e")]: RUNTIME.name };
    const rows = [decode(256), decode(256, { runtime_descriptor: BOX, runtime_descriptor_sha256: digest("e") })];
    const drawn = view(index(rows, "full", {}, twin), { ...none, benchmark: "decode_throughput" });
    const gutters = bars(drawn.charts)[0]!.bars.map((b) => b.nameParts);
    expect(gutters).toHaveLength(2);
    expect(gutters).toEqual(expect.arrayContaining([[RUNTIME], [{ descriptor: BOX_RT, name: RUNTIME.name }]]));
  });

  test("equal runtimes take the name of whichever of them carries one", () => {
    // A failed row carries the planner's scheme-less URL, so the two are one runtime.
    const [https, bare] = [archive("https://example.com/llama.tgz"), archive("example.com/llama.tgz")];
    const drawn = named([...swept({ runtime_descriptor: https, runtime_descriptor_sha256: null }),
                         ...swept({ runtime_descriptor: bare })]);
    expect(drawn.runtimeLines).toEqual({ [canonical(https)]: [{ descriptor: canonical(bare), name: RUNTIME.name }] });
  });

  test("a descriptor takes its name from any of its rows that carries a digest", () => {
    const drawn = named([decode(256, { model_descriptor_sha256: null }), decode(512), ...swept(other)]);
    const entry = lines(drawn.charts)[0]!.legend.find((s) => s.key === G("Tiny-1B"))!;
    expect(entry.parts).toEqual([TINY]);
  });

  test("a folded series and a collapsed bar name a named runtime by its name", () => {
    const box = { runtime_descriptor: BOX, runtime_descriptor_sha256: null };
    const folded = lines(named([...swept(), ...swept(box)]).charts)[0]!;
    const legend = folded.legend.map((s) => s.parts);
    expect(legend).toHaveLength(2);
    expect(legend).toEqual(expect.arrayContaining([["Bench A", RUNTIME], ["Bench A", BOX_RT]]));
    const collapsed = bars(named([decode(256), decode(512, box)]).charts);
    expect(collapsed.map((c) => c.bars.map((b) => b.nameParts)))
      .toEqual([[["prefill_tokens=256", RUNTIME], ["prefill_tokens=512", BOX_RT]]]);
  });

  test("a row that lacks a parameter count carries its model's name, as the charts draw it", () => {
    const lacking = { model_params_total_millions: null, model_params_active_millions: null };
    const missing = index([decode(256, { ...lacking, model_descriptor_sha256: null }), decode(512, lacking),
                           decode(256, { ...lacking, ...other })], "full", {}, NAMES).missing!;
    expect(missing.params.map((m) => m.name)).toEqual([TINY, TINY, null]);
  });

  test("a thinking mode follows the name", () => {
    const thinking = { model_flags: { benchmark_type: "eval", model_type: "gguf_text", enable_thinking: true } as ModelFlags };
    const drawn = named([...swept(thinking), ...swept(other)]);
    const entry = lines(drawn.charts)[0]!.legend.find((s) => s.key.startsWith(G("Tiny-1B")))!;
    expect([entry.label, entry.parts]).toEqual(["Tiny-1B::gguf::q4_k_m · thinking", [TINY, "thinking"]]);
  });
});
