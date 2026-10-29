// The plan results report: every result row of a plan, one chart family per
// benchmark type, filtered and drawn in the browser. `index` runs once per
// load, the Swap tab's figures and the result checks' findings with it; `view`
// draws the charts from it. `findings.ts` words the checks.

import { channel, exactText, finite, HUES, median, SHAPES, valueText } from "./chart";
import {
  BAND, e2eText, pairingText, UNESTIMABLE, type E2eFinding, type Pairing, type Spread,
} from "./findings";
import { textOrder } from "./format";
import { nameOf as catalogNameOf, type DescriptorNames } from "./descriptor-names";
import { bytesText, failedText, parameterValue } from "./plans";
import { namedAs, parsed, unlabelled } from "./vault";
import type { DescriptorDisplay, FailureSource, FleetDetail, ModelFlags, ResultRow, UnknownValue } from "./api-types";

type Json = Record<string, unknown>;

const isObject = (v: unknown): v is Json => typeof v === "object" && v !== null && !Array.isArray(v);
const text = (v: unknown): string | null => (typeof v === "string" && v !== "" ? v : null);

function parse(descriptor: string): Json | null {
  const v = parsed(descriptor);
  return isObject(v) ? v : null;
}

/** JSON with every object's keys sorted, so equal descriptors compare equal as text. */
export function canonical(v: unknown): string {
  if (Array.isArray(v)) return `[${v.map(canonical).join(",")}]`;
  if (isObject(v)) return `{${Object.keys(v).sort().map((k) => `${JSON.stringify(k)}:${canonical(v[k])}`).join(",")}}`;
  return JSON.stringify(v) ?? "null";
}

/** A value the server could not type, which it hands over as stored. */
const isUnknown = (v: unknown): v is UnknownValue => isObject(v) && v.type === "unknown" && "value" in v;

/** A typed field's JSON: the value itself, or an unknown one's stored value. */
export const storedJson = (v: unknown): unknown => (isUnknown(v) ? v.value : v);

/**
 * A descriptor or flags as the text everything keys them by: canonical JSON, so equal ones read alike however stored;
 * text that is no JSON object as is; `""` for none.
 */
export function descriptorText(d: unknown): string {
  if (d == null) return "";
  const v = storedJson(d);
  return typeof v !== "string" ? canonical(v) : parse(v) ? canonical(parse(v)) : v;
}

/** A descriptor that has a catalog name. A label shows the name. A click on the name shows the descriptor. */
export interface NamedPart {
  /** The descriptor as stored. */
  descriptor: string;
  name: DescriptorDisplay;
}

/** A label as the charts draw it, part by part: text, and descriptors under their catalog names. */
export type Label = (string | NamedPart)[];

/** A label part as plain text: a catalog name by its display. */
export const partText = (part: string | NamedPart): string => (typeof part === "string" ? part : part.name.display);

/** A label part as a key: a catalog name by its descriptor, so two descriptors with one display stay apart. */
export const partKey = (part: string | NamedPart): string => (typeof part === "string" ? part : part.descriptor);

/** A label as plain text: its parts joined by ` · `. A tooltip and the screen-reader tables show this text. */
export const labelText = (label: Label): string => label.map(partText).filter(Boolean).join(" · ");

/** A descriptor's catalog name, found by its descriptor text; null for a descriptor that has none. */
type Catalog = (descriptor: string) => DescriptorDisplay | null;

const UNNAMED: Catalog = () => null;

/** What a model reads as: its descriptor as stored, which the model filter goes by, and its label, which the charts' series do. */
export interface ModelId {
  /** Its descriptor as stored, the first seen of its equal models (`modelIdentity`). */
  id: string;
  /**
   * The model as plain text. Its series use this text as their key. The text holds, in this order:
   * - what its vault sources read as (`namedAs`)
   * - the descriptor as stored
   * - its thinking mode, if its flags name one
   */
  label: string;
  /** What a legend draws: its catalog name where it has one, else `label`'s text; then its thinking mode. */
  parts: Label;
}

/** Equal models: the JSON with keys sorted and no `custom_label`, which a job staged before sources carried one lacks. */
function modelIdentity(descriptor: string): string {
  const d = parse(descriptor);
  return d ? canonical(unlabelled(d)) : descriptor;
}

/**
 * Each model descriptor's identity, in first-seen order: the first-seen descriptor of its equal models, labelled by
 * what their vault sources read as (`namedAs`), then by that descriptor, as a filter's choice lists one (`namedLabel`).
 * It draws as the first catalog name (`catalog`) of its equal models, where one has a name.
 */
export function modelLabels(descriptors: Iterable<string>, names: Record<string, string> = {},
                            catalog: Catalog = UNNAMED): Map<string, ModelId> {
  const byIdentity = new Map<string, string[]>();
  for (const raw of new Set(descriptors)) push(byIdentity, modelIdentity(raw), raw);
  const named = (raw: string) => namedAs(raw, names).map(({ name }) => name);
  return new Map([...byIdentity.values()].flatMap((raws) => {
    const vault = raws.map(named).find((found) => found.length) ?? [];
    const label = [...vault, raws[0] || "no model"].join(" ");
    const part = namedPart(raws, catalog);
    const model: ModelId = { id: raws[0]!, label, parts: [part ?? label] };
    return raws.map((raw): [string, ModelId] => [raw, model]);
  }));
}

/**
 * Equal runtimes: the JSON with keys sorted, a URL's scheme dropped, as failed rows carry the planner's scheme-less
 * one, and no `custom_label`.
 */
function runtimeIdentity(descriptor: string): string {
  const d = parse(descriptor);
  if (!d) return descriptor;
  const url = text(d.url);
  return canonical(unlabelled(url ? { ...d, url: url.replace(/^https?:\/\//, "") } : d));
}

/** Each runtime descriptor's label: the first-seen descriptor as stored of its equal runtimes, `no runtime` for none. */
export function runtimeLabels(descriptors: Iterable<string>): Map<string, string> {
  const byIdentity = new Map<string, string[]>();
  for (const raw of new Set(descriptors)) push(byIdentity, runtimeIdentity(raw), raw);
  const out = new Map<string, string>();
  for (const raws of byIdentity.values()) for (const raw of raws) out.set(raw, raws[0] || "no runtime");
  return out;
}

/**
 * The first descriptor in `raws` that has a catalog name, with that name. The descriptors in `raws` are equal. Null if
 * none of them has a name.
 */
function namedPart(raws: readonly string[], catalog: Catalog): NamedPart | null {
  for (const descriptor of raws) {
    const name = catalog(descriptor);
    if (name) return { descriptor, name };
  }
  return null;
}

/** Each runtime's catalog name, from the first of its equal descriptors that has one. */
function runtimeNamesOf(runtimes: ReadonlyMap<string, string>, catalog: Catalog): Map<string, NamedPart> {
  const byRuntime = new Map<string, string[]>();
  for (const [raw, runtime] of runtimes) push(byRuntime, runtime, raw);
  return new Map([...byRuntime].flatMap(([runtime, raws]): [string, NamedPart][] => {
    const part = namedPart(raws, catalog);
    return part ? [[runtime, part]] : [];
  }));
}

/** Each runtime's label, from any of its equal descriptors: one staged before sources carried a `custom_label` lacks it. */
function runtimeLabelsOf(descriptors: readonly [string, string][], runtimes: ReadonlyMap<string, string>,
                         names: Record<string, string>): Map<string, string> {
  const out = new Map<string, string>();
  for (const [, raw] of descriptors) {
    const d = parse(raw);
    const label = d && (text(d.custom_label) ?? namedAs(d, names)[0]?.name);
    const runtime = runtimes.get(raw)!;
    if (label && !(out.has(runtime) && textOrder(out.get(runtime)!, label) <= 0)) out.set(runtime, label);
  }
  return out;
}

/** The keys a flag set names its cell by, which the row's own columns already say. */
const FLAG_AXES = new Set(["benchmark_type", "runtime_type", "model_type"]);

/** A variant's flags as its text holds them after its runtime's; `""` for none. */
const flagsOf = (variant: string, runtime: string): string => variant.slice(runtime.length + " + ".length);

/**
 * The lines of a runtime variant, one part for each line. A variant whose runtime has a catalog name reads as its name
 * (`nameOf`). Other variants read as:
 * - its vault name, if it has one
 * - `runtime = …`
 * - then `runtime flags = …`, if the variant has flags
 */
function variantLines(I: Internal, variant: string): Label {
  const runtime = I.variants.get(variant) ?? variant;
  if (I.runtimeNames.has(runtime)) return nameOf(I, variant);
  const flags = flagsOf(variant, runtime);
  const label = labelOf(I, variant);
  return [...(label ? [label] : []), `runtime = ${runtime}`, ...(flags ? [`runtime flags = ${flags}`] : [])];
}

/** A flag set's knobs: its entries without the keys that name its cell; `null` where it sets none. */
export function knobs(flags: unknown): Json | null {
  const set = isObject(flags) ? Object.entries(flags).filter(([key]) => !FLAG_AXES.has(key)) : [];
  return set.length ? Object.fromEntries(set) : null;
}

/** `knobs` as text: canonical JSON, `""` for none. */
export function knobsText(flags: unknown): string {
  const v = storedJson(flags);
  const set = knobs(v);
  return set ? canonical(set) : typeof v === "string" ? v : "";
}

const said = (v: unknown): string => (v == null ? "" : String(v));
const sized = (v: unknown): string => (typeof v === "number" ? bytesText(v) : "");
/** A GPU or NPU as a device type names it: `GPU RTX 4090 24.0 GB`. */
const unit = (word: string, model: unknown, memory: unknown): string => {
  const what = [said(model), sized(memory)].filter(Boolean).join(" ");
  return what && `${word} ${what}`;
};

type Reported = Pick<ResultRow, "device_name" | "device">;

/** A device's make, a part each, `""` where unreported: its chip and RAM, which a short name leads with, then the rest. */
const makeOf = ({ device: d }: Reported): string[] => [
  said(d.chip_model), sized(d.ram_bytes) && `${sized(d.ram_bytes)} RAM`, unit("GPU", d.gpu_model, d.gpu_vram_bytes),
  unit("NPU", d.npu_model, d.npu_vram_bytes), [said(d.os_name), said(d.os_version)].filter(Boolean).join(" "),
  said(d.form_factor),
];
/** The parts of a make every short name carries. */
const LEAD = 2;

/**
 * A row's device type: the make its `device` fields report, so devices alike in it are one whatever each is named.
 * Not the OS build or the state a run found it in; a row that reports no make goes by its `device_name`.
 */
export const deviceType = (row: Reported): string =>
  makeOf(row).filter(Boolean).join(" · ") || row.device_name || "no device";

/**
 * Each device type's short name over `rows`, by [`deviceType`]: its chip and RAM, then only the parts that tell it
 * from another type of that chip and RAM (`Apple A19 Pro · 11.4 GB RAM · iOS 27.1`).
 */
export function deviceNames(rows: Iterable<Reported>): Map<string, string> {
  const makes = new Map<string, string[]>();
  for (const row of rows) {
    const type = deviceType(row);
    if (!makes.has(type)) makes.set(type, makeOf(row));
  }
  const alike = new Map<string, string[][]>();
  for (const make of makes.values()) push(alike, make.slice(0, LEAD).join("\n"), make);
  const out = new Map<string, string>();
  for (const [type, make] of makes) {
    const others = alike.get(make.slice(0, LEAD).join("\n"))!;
    const short = make.filter((part, i) => part && (i < LEAD || others.some((other) => other[i] !== part)));
    out.set(type, short.join(" · ") || type);
  }
  return out;
}

const DRAFTER = new Set(["accept_length", "accept_length_per_request", "draft_acceptance_rate"]);

/** The vl_spec_decode jobs that ran speculatively: those with any drafter metric row, failed ones included. */
export function speculativeJobs(rows: readonly ResultRow[]): Set<string> {
  return new Set(rows.filter((row) => DRAFTER.has(row.metric)).map((row) => row.job_id));
}

const mobileFlavor = (descriptor: string) => /^(android|ios)/.test(text(parse(descriptor)?.flavor) ?? "");

function push<K, V>(map: Map<K, V[]>, key: K, value: V) {
  const list = map.get(key);
  if (list) list.push(value);
  else map.set(key, [value]);
}

function counts<T>(items: Iterable<T>): Map<T, number> {
  const out = new Map<T, number>();
  for (const item of items) out.set(item, (out.get(item) ?? 0) + 1);
  return out;
}

/** The E2E check's family key, which Reports draws. */
export const E2E = "e2e_check";
/** The VL check's family key: each vl_throughput job's span against its own prompt + decode time. */
export const VL_E2E = "vl_e2e_check";
export const SPEC = "vl_spec_decode";
/** The swap chart's family key: the swap each job's run held, over every benchmark type that reports it. */
export const SWAPPED = "max_swap";
/** The VL types' swap chart, over their own prompt's parameters. */
export const VL_SWAPPED = "vl_max_swap";
/** The ASR types' swap chart, over their clip's parameters. */
export const ASR_SWAPPED = "asr_max_swap";
const PREFILL = "prefill_throughput";
const DECODE = "decode_throughput";
const LATENCY = "end_to_end_latency";
const VL = "vl_throughput";
const ASR = "asr_throughput";
const ASR_MEMORY = "asr_max_memory";

/** Each family's key, its label, and the short one a tab or a select names it by. */
const FAMILIES: [string, string, string][] = [
  [PREFILL, "prefill throughput", "Prefill"], [DECODE, "decode throughput", "Decode"],
  [LATENCY, "end-to-end latency", "E2E latency"], [E2E, "E2E latency vs prefill + decode", "E2E latency vs prefill + decode"],
  ["max_memory_usage", "peak memory", "Memory"], [VL, "VL throughput", "VL throughput"],
  [VL_E2E, "VL E2E latency vs prompt + decode", "VL E2E latency vs prompt + decode"],
  ["vl_max_memory", "VL peak memory", "VL memory"], [SPEC, "VL speculative decoding", "VL spec decode"],
  ["asr_throughput", "ASR throughput", "ASR throughput"], ["asr_max_memory", "ASR peak memory", "ASR memory"],
  ["eval", "eval accuracy", "Eval"], ["asr_eval", "ASR error rate", "ASR eval"],
  [SWAPPED, "swap", "Swap"], [VL_SWAPPED, "VL swap", "VL swap"],
  [ASR_SWAPPED, "ASR swap", "ASR swap"],
];
/** The families derived across rows, not one benchmark type's: the checks and swap, in the page's order. */
export const DERIVED_FAMILIES = [E2E, VL_E2E, SWAPPED, VL_SWAPPED, ASR_SWAPPED];
/** The swap charts, text first. */
const SWAPS = [SWAPPED, VL_SWAPPED, ASR_SWAPPED];
/** Each VL and ASR type's own swap chart: its prompt is not a text length, so it charts apart from Swap, over its own
 * parameters. */
const OWN_SWAP = new Map([[VL, VL_SWAPPED], ["vl_max_memory", VL_SWAPPED], [ASR, ASR_SWAPPED], [ASR_MEMORY, ASR_SWAPPED]]);
const FAMILY_LABEL = new Map(FAMILIES.map(([key, label]) => [key, label]));
const SHORT_LABEL = new Map(FAMILIES.map(([key, , short]) => [key, short]));
const FAMILY_RANK = new Map(FAMILIES.map(([key], i) => [key, i]));

/** A benchmark type as a tab names it: `Decode`; a type the report does not know, by its key. */
export const shortLabel = (type: string): string => SHORT_LABEL.get(type) ?? type;

const METRIC_NAMES = new Map([
  ["prefill_throughput", "Prefill throughput"], ["ttft", "Time to first token"], ["decode_throughput", "Decode throughput"],
  ["end_to_end_latency", "E2E latency"], ["e2e_latency", "Prefill + decode time"], ["peak_memory", "Peak memory"],
  ["max_host_usage", "Host memory"], ["max_gpu_usage", "GPU memory"], ["max_npu_usage", "NPU memory"],
  ["decode_speedup", "Decode speedup"], ["e2e_speedup", "E2E speedup"], ["accuracy", "Accuracy"],
  ["prompt_tokens", "Prompt tokens"],
  // VL speculative decoding's: the client's clock, then the runtime's own.
  ["ttft_ms", "Time to first token (client)"], ["runtime_ttft", "Time to first token (runtime)"],
  ["e2e_ms", "E2E latency (client)"], ["runtime_e2e_ms", "E2E latency (runtime)"], ["decode_ms", "Decode time"],
  // asr_eval's headline, a word error rate; `error_rate` where mgmt doesn't know the task.
  ["wer", "Word error rate"], ["error_rate", "Error rate"],
]);

/** A metric in plain words: `Time to first token`; one the report does not know, its key spaced (`Accept length`). */
export const metricName = (key: string): string =>
  METRIC_NAMES.get(key) ?? `${key.charAt(0).toUpperCase()}${key.slice(1).replaceAll("_", " ")}`;

/** The key `chartNames` files a chart under: its family and metric, none for a family that measured nothing. */
export const chartKey = (family: string, metric: string | null) => `${family}\n${metric ?? ""}`;

/**
 * Each benchmark type's charts by `chartKey`, as the section that holds each names it: the metric in
 * plain words, the type first for an eval and where two share the name, unless the name already says the type
 * (`Prefill throughput` beside `VL throughput · prefill throughput`).
 */
export function chartNames(benchmarks: readonly Pick<Family, "key" | "short" | "types" | "metrics">[]): Map<string, string> {
  const charts = benchmarks.flatMap((f) => (f.metrics.length
    ? f.metrics.map((m) => ({ f, metric: m.key as string | null, name: metricName(m.key) }))
    : [{ f, metric: null, name: f.short }]));
  const named = counts(charts.map((c) => c.name));
  const lower = (name: string) => (/^.[a-z]/.test(name) ? name.charAt(0).toLowerCase() + name.slice(1) : name);
  const typed = (f: Pick<Family, "short" | "types">, name: string) => f.types.includes("eval")
    || (named.get(name)! > 1 && !name.toLowerCase().includes(f.short.toLowerCase()));
  return new Map(charts.map(({ f, metric, name }) => [chartKey(f.key, metric),
    metric && typed(f, name) ? `${f.short} · ${lower(name)}` : name]));
}

/** One chart of the results page: a family's metric, or the family alone where it measured nothing. */
export interface ChartRef {
  /** Its `chartKey`. */
  key: string;
  family: Family;
  metric: Metric | null;
  /** What its section is headed, as `chartNames` names a benchmark type's and the E2E check its tab. */
  name: string;
}

/** The plan's charts in the report's order: each benchmark type's metrics, then the checks and swap where offered. */
export function chartRefs(index: Pick<ReportIndex, "benchmarks" | "families">): ChartRef[] {
  const names = chartNames(index.benchmarks);
  const of = (family: Family, metric: Metric | null, name?: string): ChartRef => {
    const key = chartKey(family.key, metric?.key ?? null);
    return { key, family, metric, name: name ?? names.get(key)! };
  };
  const derived = DERIVED_FAMILIES.flatMap((key) => index.families.filter((f) => f.key === key && !f.disabled));
  return [
    ...index.benchmarks.flatMap((f) => (f.metrics.length ? f.metrics.map((m) => of(f, m)) : [of(f, null)])),
    ...derived.map((f) => of(f, f.metrics[0] ?? null, shortLabel(f.key))),
  ];
}

const EVAL_FAMILY = "eval-";
/** The family a row draws in: its benchmark type, or an eval's own by `eval_id`, so each eval is a tab. */
export function familyOf(row: Pick<ResultRow, "benchmark_type" | "parameters">): string {
  const id = row.benchmark_type === "eval" ? row.parameters.eval_id : undefined;
  return typeof id === "string" && id ? `${EVAL_FAMILY}${id}` : row.benchmark_type;
}

/** Whether a model ran with thinking on, from `enable_thinking` in its flags, or the legacy
 * `enable_thinking=<bool>` text an unknown value holds; null where they don't say. */
export function thinkingOf(flags: ModelFlags | UnknownValue | null): boolean | null {
  const v = storedJson(flags);
  if (isObject(v)) return typeof v.enable_thinking === "boolean" ? v.enable_thinking : null;
  const said = /(?:^|[\s,])enable_thinking\s*=\s*(true|false)\b/.exec(typeof v === "string" ? v : "");
  return said ? said[1] === "true" : null;
}

/** The reasoning effort a model was asked for, from `reasoning_effort` in its flags, or the
 * `reasoning_effort=<effort>` text an unknown value holds; null where they don't say. */
export function effortOf(flags: ModelFlags | UnknownValue | null): string | null {
  const v = storedJson(flags);
  if (isObject(v)) return text(v.reasoning_effort);
  const said = /(?:^|[\s,])reasoning_effort\s*=\s*(low|medium|high)\b/.exec(typeof v === "string" ? v : "");
  return said ? said[1]! : null;
}

const MEMORY = new Set(["max_memory_usage", "vl_max_memory", ASR_MEMORY]);
/** The pools a memory job reports, in tie order. */
const POOLS: [string, string][] = [["max_host_usage", "host"], ["max_gpu_usage", "GPU"], ["max_npu_usage", "NPU"]];

/** The metric a family opens on, the first it has; else the one with most measured rows. */
const OPENS_ON: Record<string, string[]> = {
  [PREFILL]: [PREFILL], [DECODE]: [DECODE], [LATENCY]: [LATENCY], max_memory_usage: ["peak_memory"],
  vl_max_memory: ["peak_memory"], [VL]: [LATENCY, PREFILL], [SPEC]: ["decode_speedup", DECODE], eval: ["accuracy"],
  [ASR_MEMORY]: ["peak_memory"], [ASR]: [LATENCY, PREFILL],
  asr_eval: ["wer", "error_rate"],
};

/** A type's metrics in an order of its own, the rest after by name: VL and ASR throughput's as their text twins' tabs run. */
const METRIC_ORDER: Record<string, string[]> = {
  [VL]: [PREFILL, "prompt_tokens", "ttft", DECODE, LATENCY, "e2e_latency"],
  [ASR]: [PREFILL, "prompt_tokens", "ttft", DECODE, LATENCY],
};

/** One sweep a family's charts draw apart, under a heading of its own. */
interface Sweep {
  /** Its heading, before the values its points hold. */
  title: string;
  x: string;
  /** The values its points hold, a pin's in place of one. */
  at: Json;
  /** The parameter it draws a panel per value of, where it has one. */
  by?: string;
}

/**
 * Each family's sweeps, which its charts draw before the points none of them draws: VL throughput's as its catalog
 * runs them, around an image alone, no text and 64 decode tokens.
 */
const SWEEPS: Record<string, Sweep[]> = {
  [VL]: [
    { title: "By image size", x: "image_width", at: { decode_tokens: 64, num_images: 1, text_tokens: 0 } },
    { title: "By image count", x: "num_images", at: { decode_tokens: 64, text_tokens: 0 }, by: "image_width" },
  ],
};
/** The heading of the points no sweep draws. */
const UNSWEPT = "Other parameters";

/** A throughput type's observed token counts by panel title: its media's alone, and the whole prompt's, which the
 * prefill rate counts. */
const TOKENS: Record<string, [string, string][]> = {
  [VL]: [["vl_throughput_image_tokens", "image tokens"], ["vl_throughput_prefill_tokens", "prompt tokens"]],
  [ASR]: [["asr_throughput_audio_tokens", "audio tokens"], ["asr_throughput_prefill_tokens", "prompt tokens"]],
};

/** Whether an ASR row's context was sized from the clip's own audio tokens, as the row observes it. */
const exactPeak = (row: ResultRow): boolean => row.observations.asr_context_sizing === "exact";
/** Whether an ASR row's audio count is the runtime's own, marker-free, as the row observes it. */
const calibrated = (row: ResultRow): boolean => row.observations.asr_audio_tokens_source === "calibrated";
/**
 * Whether an ASR row ran a clip longer than the model's own documentation says it takes (Gemma 4's card: 30 s), as the
 * row observes it: a speed or memory scenario whose transcription quality is not established. Compared, but labelled.
 */
export const beyondAudioLimit = (row: ResultRow): boolean => row.observations.asr_beyond_model_audio_limit === true;
/** The label of a cell, a bar or a job drawn from such a row. */
export const BEYOND_LIMIT = "beyond the model's audio limit";

/** Which of a family's ASR rows a chart compares, and what it says of the jobs it leaves out. */
interface AsrRule {
  keeps: (row: ResultRow) => boolean;
  /** The note when `n` of `of` jobs are compared. */
  note: (n: number, of: number) => string;
  /** The tab's text when none is. */
  empty: string;
}

/**
 * Which ASR rows `family`'s `metric` compares, so two regimes never share a cell; null where every row compares. A
 * peak or swap reading is the clip's own only from an exact-sized context; throughput compares calibrated
 * (marker-free) counts. A pinned context is a cell flag, so its rows chart in a column of their own. Each rule's
 * note says why.
 */
const asrRule = (family: string, metric: string): AsrRule | null =>
  family === ASR_MEMORY && metric === "peak_memory" ? ASR_EXACT_PEAK
    : family === ASR_SWAPPED ? ASR_EXACT_SWAP
      : family === ASR ? ASR_CALIBRATED : null;

/** A metric as charts draw it. */
export interface Metric {
  key: string;
  label: string;
  unit: string;
  /** What a row's value is multiplied by to draw in `unit`. */
  factor: number;
  /** Lower is better: time, memory and an error rate. */
  lower: boolean;
  /** Counts, printed with only the decimals they need. */
  whole?: true;
}

/** One benchmark type's results, or a comparison derived across types. */
export interface Family {
  /** The benchmark type, an eval's `eval-{eval_id}`, or a derived family's key: its tab's address segment. */
  key: string;
  label: string;
  /** What its tab names it by. */
  short: string;
  /** The benchmark types its rows come from (the E2E check: its three). */
  types: string[];
  /** Jobs with a measured row and no failed one; 0 for the E2E check. */
  measured: number;
  /** Jobs with a failed row, which a family that measured nothing links to; 0 for the E2E check. */
  failed: number;
  /** The failed jobs by cause, worded, in first-seen order. */
  causes: [string, number][];
  /** Model labels with a measured result. */
  models: number;
  /** The metrics it can draw, derived ones first; empty when nothing was measured. */
  metrics: Metric[];
  /** The metric drawn when the address names none. */
  metric: string | null;
  /** Its column labels, in column order. */
  columns: string[];
  /** The device types its columns chart, in column order; none for an eval, or below the full tier. */
  devices: string[];
  /** Why it is listed but cannot be drawn ("needs device names"); null when it can. */
  disabled: string | null;
}

/** Whether every row of `family` is an eval's. */
const isEval = (family: Family): boolean => family.types.every((type) => type === "eval");

/** What `index` reads once per load, for `view` to draw from; a class, so `$state` does not proxy it. */
export class ReportIndex {
  /** Families in the report's order, the checks and swap charts among them when listed. */
  declare readonly families: Family[];
  /** The benchmark types' families alone, in that order: a sub-tab each. */
  declare readonly benchmarks: Family[];
  /** Every column with a measured result, plan-wide. */
  declare readonly columns: string[];
  /** Every device type among them. */
  declare readonly devices: string[];
  /** Each device type's short name, which a column and the device bar print, by type. */
  declare readonly deviceNames: Map<string, string>;
  /** The E2E check, plan-wide whatever the filters; null where the plan measured no end-to-end latency. */
  declare readonly e2e: E2eFinding | null;
  /** The VL check, as `e2e`; null where no vl_throughput job measured end-to-end latency. */
  declare readonly vlE2e: E2eFinding | null;
  /** How vl_spec_decode's speculative and baseline jobs paired; null where none measured a job. */
  declare readonly pairing: Pairing | null;
  /** Whether a run held swap, as the `swap` issue of Jobs reads it: Swap lists those lines. */
  declare readonly swapped: boolean;
  /** The measured rows that lack a datum, by what they lack; null where none does. */
  declare readonly missing: Missing | null;

  constructor(fields: Pick<ReportIndex, "families" | "benchmarks" | "columns" | "devices" | "deviceNames" | "e2e"
    | "vlE2e" | "pairing" | "swapped" | "missing">) {
    Object.assign(this, fields);
  }
}

interface Entry {
  row: ResultRow;
  /** The family it draws in, as [`familyOf`] names it. */
  family: string;
  model: ModelId;
  runtime: string;
  /** The runtime with the flags its job asked for, which the runtime filter goes by: `runtime + knobs`. */
  variant: string;
  /** Its device type on its variant: a flag set charts apart from another, as a runtime does. */
  column: string;
  /** Its device type on its runtime, whatever the flags: the E2E check and speculative pairs join jobs across them. */
  runtimeColumn: string;
  params: Json;
  measured: boolean;
}

/** One value to chart: a measured row, a pair or a rung, with one value per panel kind. */
interface Point {
  model: ModelId;
  /** Its runtime and flags, as [`Entry.variant`]; the E2E check's and a speculative pair's, across flag sets, its runtime alone. */
  runtime: string;
  column: string;
  params: Json;
  values: number[];
  /** The result rows it is drawn from. */
  rows: ResultRow[];
}

/** One kind of panel a family draws per column (the E2E check draws two). */
export interface Layer {
  title: string;
  /** The metric, where the title names a benchmark type, not what is measured: the unit after it would misread. */
  metric: Metric | null;
  unit: string;
  /** Axis hints: keep 0 in range, shade ±`band` (%), draw a reference line at `reference`. */
  zero: boolean;
  band: number | null;
  reference: number | null;
}

interface ParamValue {
  /** JSON text, or UNSET where the point lacks it. */
  key: string;
  text: string;
}

interface Param {
  name: string;
  /** Present values in value order, then `unset` when some point lacks a varying one. */
  values: ParamValue[];
  varying: boolean;
}

/** What a family's metric draws, decided once over all its points. */
interface Layout {
  metric: Metric;
  points: Point[];
  /** The runtimes, each with its flags, the points ran on, in text order. */
  runtimes: string[];
  layers: Layer[];
  /** The layers series rank by. */
  rank: number[];
  /** What a point is, for the partial-parameter caption. */
  word: string;
  params: Map<string, Param>;
  x: string | null;
  riding: string[];
  /** The varying parameters that split series (line form) or charts (bar form), before any pin. */
  rest: string[];
  /** Rung text by x value: the value and what rides on it. */
  rungText: Map<number, string>;
  /** The combinations of `rest` that draw apart from the lines, as `chooseX` finds them. */
  lone: Set<string>;
}

interface Internal {
  fleet: FleetDetail;
  entries: Entry[];
  columnOrder: (a: string, b: string) => number;
  columnRuntime: Map<string, string>;
  /** A device column's type, which the device filter goes by. */
  columnDevice: Map<string, string>;
  /** A column's runtime type, as its descriptor stores it: an eval's charts group by it. */
  columnType: Map<string, string>;
  /** Each device type's short name, as the index holds it. */
  deviceNames: Map<string, string>;
  /** Distinct measured model descriptors plan-wide: the model filter needs two. */
  ids: Set<string>;
  /** Every runtime of the plan, alone and with each flag set its jobs asked for, which a drop may name, to its runtime. */
  variants: Map<string, string>;
  /** Each runtime's vault name (`runtimeLabelsOf`). */
  labels: Map<string, string>;
  /** Each runtime's catalog name, where it has one (`runtimeNamesOf`). */
  runtimeNames: Map<string, NamedPart>;
  /** Measured model descriptors by family, sorted. */
  models: Map<string, string[]>;
  rungs: Point[];
  /** E2E rungs with all three inputs, estimated or not. */
  complete: number;
  /** The VL check's, a job each. */
  vlRungs: Point[];
  vlComplete: number;
  pairs: { decode: Point[]; e2e: Point[] } | null;
  layouts: Map<string, Layout | null>;
  splitsCharts: boolean | null;
}

const internals = new WeakMap<ReportIndex, Internal>();

const UNSET = "";
const keyOf = (params: Json, name: string): string =>
  (Object.hasOwn(params, name) ? JSON.stringify(params[name]) : UNSET);
/** A parameter value as its select and the address name it: its JSON text, so no two read alike, else `unset`. */
const pinOf = (v: ParamValue) => (v.key === UNSET ? "unset" : v.key);
/** `name=value`, `unset` where `params` lacks it. */
const paramText = (params: Json, name: string) =>
  `${name}=${Object.hasOwn(params, name) ? parameterValue(params[name]) : "unset"}`;

const PERCENT = new Set(["accuracy", "draft_acceptance_rate", "wer", "error_rate"]);
/** The scored ratios where fewer is better: asr_eval's error rates. */
const ERROR_RATES = new Set(["wer", "error_rate"]);

/** Lower is better for a measured row's unit: time and memory. */
export const lowerIsBetter = (unit: string): boolean => unit === "ms" || unit === "bytes";

/** A measured metric as drawn: bytes in GB, a scored ratio in %; lower is better for time, memory and an error rate. */
function metricOf(key: string, unit: string): Metric {
  const [drawn, factor]: [string, number] = unit === "bytes" ? ["GB", 1e-9] : PERCENT.has(key) ? ["%", 100] : [unit, 1];
  return { key, label: key, unit: drawn, factor, lower: lowerIsBetter(unit) || ERROR_RATES.has(key) };
}

const DERIVED: Record<string, Metric> = {
  peak_memory: { key: "peak_memory", label: "peak memory", unit: "GB", factor: 1e-9, lower: true },
  decode_speedup: { key: "decode_speedup", label: "decode speedup", unit: "×", factor: 1, lower: false },
  e2e_speedup: { key: "e2e_speedup", label: "E2E speedup", unit: "×", factor: 1, lower: false },
  excess: { key: "excess", label: "extra E2E time", unit: "%", factor: 1, lower: true },
  // Ranked most first: the models that swapped hardest lead the first chart.
  max_swap: { key: "max_swap", label: "max swap", unit: "GB", factor: 1e-9, lower: false },
  // Observations, not results: drawn because a VL prefill rate reads across models only beside them. Ranked
  // fewest first, as the rates they divide are not better or worse for it.
  prompt_tokens: { key: "prompt_tokens", label: "image and prompt tokens", unit: "tokens", factor: 1, lower: true,
                   whole: true },
  // ASR's, as VL's: an audio-token count is the encoder's, so a rate reads across models only beside it.
  asr_prompt_tokens: { key: "prompt_tokens", label: "audio and prompt tokens", unit: "tokens", factor: 1, lower: true,
                       whole: true },
};

/** The swap a row's run held at its most, in bytes; null where the run read none. */
const swapOf = (row: ResultRow): number | null => {
  const swap = row.observations.max_swap_bytes;
  return finite(swap) ? swap : null;
};

/** Whether a measured row's swap draws on swap chart `key`: a VL or ASR type's on its own, one with a prefill length on Swap. */
const swapReads = (key: string, e: Entry): boolean => {
  const own = OWN_SWAP.get(e.row.benchmark_type);
  return swapOf(e.row) !== null && (own ? key === own : key === SWAPPED && finite(e.params.prefill_tokens));
};

/** A column's start below the full tier, where a runtime's devices share it. */
const WITHHELD_COLUMN = "devices withheld · ";

/** Columns: Android and iOS first, then by label. */
function columnOrder(mobile: Map<string, boolean>) {
  return (a: string, b: string) => Number(mobile.get(b) ?? false) - Number(mobile.get(a) ?? false) || textOrder(a, b);
}

/**
 * Index every row of a plan's results, once per load: labels, families, pairs, rungs and counts. `names` is the
 * read's, which names the vault resources a model without a `custom_label` names; `catalog` is its catalog names, by
 * the digest a row carries for each descriptor.
 */
export function index(rows: readonly ResultRow[], fleet: FleetDetail, names: Record<string, string> = {},
                      catalog: DescriptorNames = {}): ReportIndex {
  const descriptors = rows.map((row): [string, string] =>
    [descriptorText(row.model_descriptor), descriptorText(row.runtime_descriptor)]);
  const digests = new Map<string, string>();
  for (const [i, row] of rows.entries()) {
    const [model, runtime] = descriptors[i]!;
    const carried: [string, string | null][] = [[model, row.model_descriptor_sha256],
                                                [runtime, row.runtime_descriptor_sha256]];
    for (const [descriptor, digest] of carried) if (digest && !digests.has(descriptor)) digests.set(descriptor, digest);
  }
  const named: Catalog = (descriptor) => catalogNameOf(catalog, digests.get(descriptor));
  const models = modelLabels(descriptors.map(([model]) => model), names, named);
  const runtimes = runtimeLabels(descriptors.map(([, runtime]) => runtime));
  const speculative = speculativeJobs(rows.filter((row) => row.benchmark_type === SPEC));
  const mobile = new Map<string, boolean>();
  const columnRuntime = new Map<string, string>();
  const columnDevice = new Map<string, string>();
  const columnType = new Map<string, string>();
  const variants = new Map<string, string>();
  // Below the full tier a row reports no make.
  const typeNames = fleet === "full" ? deviceNames(rows) : new Map<string, string>();
  const devicesOf = (columns: string[]) => [...new Set(columns.flatMap((column) => columnDevice.get(column) ?? []))];
  // A model run both ways, or at two efforts, is two to compare, so its mode and effort join its label wherever
  // the flags name them.
  const modes = new Map<string, ModelId>();
  const withMode = (model: ModelId, thinking: boolean | null, effort: string | null): ModelId => {
    if (thinking === null && effort === null) return model;
    const mode = [thinking === null ? "" : thinking ? "thinking" : "non-thinking", effort && `effort ${effort}`]
      .filter(Boolean).join(" · ");
    const label = `${model.label} · ${mode}`;
    if (!modes.has(label)) modes.set(label, { ...model, label, parts: [...model.parts, mode] });
    return modes.get(label)!;
  };
  const entries = rows.map((row, i): Entry => {
    const [modelText, runtimeText] = descriptors[i]!;
    const model = withMode(models.get(modelText)!, thinkingOf(row.model_flags), effortOf(row.model_flags));
    const runtime = runtimes.get(runtimeText)!;
    const device = row.benchmark_type === "eval" || fleet === "withheld" ? null : deviceType(row);
    // The flags asked for, not those reported: what a client reports it ran with varies job by job.
    const knobs = knobsText(row.requested_runtime_flags);
    const variant = knobs ? `${runtime} + ${knobs}` : runtime;
    variants.set(variant, runtime);
    // The E2E check and speculative pairs list a runtime alone, which a drop names.
    variants.set(runtime, runtime);
    const on = (ran: string) => (device !== null ? `${typeNames.get(device)!} · ${ran}`
      : row.benchmark_type === "eval" ? ran : `${WITHHELD_COLUMN}${ran}`);
    // A column per flag set, so a model run two ways never charts a median neither run measured.
    const [column, runtimeColumn] = [on(variant), on(runtime)];
    for (const [at, ran] of [[column, variant], [runtimeColumn, runtime]] as const) {
      if (!mobile.has(at)) mobile.set(at, mobileFlavor(runtimeText));
      columnRuntime.set(at, ran);
      columnType.set(at, text(parse(runtimeText)?.type) ?? (runtimeText ? "unknown" : "no runtime"));
      if (device !== null) columnDevice.set(at, device);
    }
    const params = row.benchmark_type === SPEC
      ? { ...row.parameters, mode: speculative.has(row.job_id) ? "speculative" : "baseline" } : row.parameters;
    const measured = !row.failed && finite(row.value);
    return { row, family: familyOf(row), model, runtime, variant, column, runtimeColumn, params, measured };
  });
  const byColumn = columnOrder(mobile);
  const measured = entries.filter((e) => e.measured);

  const types = [...new Set(entries.map((e) => e.family))];
  const typeEntries = new Map(types.map((type) => [type, entries.filter((e) => e.family === type)]));
  const has = (type: string) => (typeEntries.get(type) ?? []).some((e) => e.measured);

  const internal: Internal = {
    fleet, entries, columnOrder: byColumn, columnRuntime, columnDevice, columnType, deviceNames: typeNames, variants,
    labels: runtimeLabelsOf(descriptors, runtimes, names),
    runtimeNames: runtimeNamesOf(runtimes, named),
    ids: new Set(measured.map((e) => e.model.id)),
    models: new Map(),
    rungs: [], complete: 0, vlRungs: [], vlComplete: 0, pairs: null, layouts: new Map(), splitsCharts: null,
  };

  const families: Family[] = [];
  const states = new Map(types.map((type) => [type, jobStates(typeEntries.get(type)!)]));
  for (const type of types) {
    const of = typeEntries.get(type)!;
    const kind = of[0]!.row.benchmark_type;
    const jobs = states.get(type)!;
    const metrics: Metric[] = [];
    // A memory job's pools are panels of one chart, none a chart of its own.
    const repeated = new Set<string>();
    if (MEMORY.has(type)) {
      if (of.some((e) => e.measured && POOLS.some(([pool]) => pool === e.row.metric))) metrics.push(DERIVED.peak_memory!);
      for (const [pool] of POOLS) repeated.add(pool);
    }
    if (type === SPEC && fleet === "full") {
      internal.pairs = pairs(of);
      if (internal.pairs.decode.length) metrics.push(DERIVED.decode_speedup!);
      if (internal.pairs.e2e.length) metrics.push(DERIVED.e2e_speedup!);
    }
    const tokens = TOKENS[type];
    if (tokens && of.some((e) => e.measured && tokens.some(([key]) => finite(e.row.observations[key])))) {
      metrics.push(type === ASR ? DERIVED.asr_prompt_tokens! : DERIVED.prompt_tokens!);
    }
    const rowCounts = counts(of.filter((e) => e.measured).map((e) => e.row.metric));
    const units = new Map(of.filter((e) => e.measured).map((e) => [e.row.metric, e.row.unit]));
    metrics.push(...[...rowCounts.keys()].filter((key) => !repeated.has(key)).sort(textOrder)
      .map((key) => metricOf(key, units.get(key)!)));
    const order = METRIC_ORDER[kind];
    if (order) {
      const at = (key: string) => (order.includes(key) ? order.indexOf(key) : order.length);
      metrics.sort((a, b) => at(a.key) - at(b.key) || textOrder(a.key, b.key));
    }
    const opens = (OPENS_ON[kind] ?? []).find((key) => metrics.some((m) => m.key === key))
      ?? [...rowCounts].sort(([a, n], [b, m]) => m - n || textOrder(a, b))[0]?.[0] ?? null;
    const evalId = type.startsWith(EVAL_FAMILY) ? type.slice(EVAL_FAMILY.length) : null;
    const columns = [...new Set(of.filter((e) => e.measured).map((e) => e.column))].sort(byColumn);
    families.push({
      key: type, label: evalId ? `eval · ${evalId}` : FAMILY_LABEL.get(type) ?? type,
      short: evalId ?? shortLabel(type), types: [kind],
      measured: jobs.measured, failed: jobs.failed.length,
      causes: [...counts(jobs.failed)].map(([source, n]) => [causeText(source, fleet), n]), models: 0,
      metrics, metric: opens, columns, devices: devicesOf(columns), disabled: null,
    });
  }

  let e2e: E2eFinding | null = null;
  if ([PREFILL, DECODE, LATENCY].some((type) => typeEntries.has(type))) {
    const missing = [PREFILL, DECODE, LATENCY].filter((type) => !has(type)).map((type) => FAMILY_LABEL.get(type)!);
    const disabled = fleet === "withheld" ? "needs device names" : missing.length ? `no ${missing.join(" or ")} results` : null;
    const label = FAMILY_LABEL.get(E2E)!;
    if (disabled) {
      // A plan that measured no end-to-end latency has nothing to check.
      if (has(LATENCY)) e2e = fleet === "withheld" ? { kind: "withheld" } : { kind: "lacking", types: missing };
    } else {
      const check = rungs(measured);
      internal.rungs = check.points;
      internal.complete = check.complete;
      e2e = e2eFinding(check, false);
    }
    const columns = [...new Set(internal.rungs.map((p) => p.column))].sort(byColumn);
    families.push({
      key: E2E, label, short: shortLabel(E2E), types: [PREFILL, DECODE, LATENCY], measured: 0, failed: 0, causes: [],
      models: 0, metrics: disabled ? [] : [DERIVED.excess!], metric: disabled ? null : "excess",
      columns, devices: devicesOf(columns), disabled,
    });
  }
  // One job measures all three, so the VL check joins nothing across jobs and needs no device names.
  let vlE2e: E2eFinding | null = null;
  if ((typeEntries.get(VL) ?? []).some((e) => e.measured && e.row.metric === LATENCY)) {
    const check = vlRungs(measured);
    internal.vlRungs = check.points;
    internal.vlComplete = check.complete;
    vlE2e = e2eFinding(check, true);
    const columns = [...new Set(check.points.map((p) => p.column))].sort(byColumn);
    families.push({
      key: VL_E2E, label: FAMILY_LABEL.get(VL_E2E)!, short: shortLabel(VL_E2E), types: [VL], measured: 0, failed: 0,
      causes: [], models: 0, metrics: [DERIVED.excess!], metric: "excess", columns, devices: devicesOf(columns),
      disabled: null,
    });
  }
  // Swap, where any run held some: every benchmark type whose rows read it, a panel each, text, VL and ASR apart as
  // their prompts are sized by different parameters.
  for (const key of SWAPS) {
    const swapping = measured.filter((e) => swapReads(key, e));
    // An ASR swap counts only where its rule compares it, or the tab would offer a chart of nothing.
    const rule = asrRule(key, "max_swap");
    if (!swapping.some((e) => swapOf(e.row)! > 0 && (!rule || rule.keeps(e.row)))) continue;
    const columns = [...new Set(swapping.map((e) => e.column))].sort(byColumn);
    families.push({
      key, label: FAMILY_LABEL.get(key)!, short: shortLabel(key),
      types: [...new Set(swapping.map((e) => e.row.benchmark_type))].sort((a, b) =>
        (FAMILY_RANK.get(a) ?? FAMILIES.length) - (FAMILY_RANK.get(b) ?? FAMILIES.length) || textOrder(a, b)),
      measured: 0, failed: 0, causes: [], models: 0, metrics: [DERIVED.max_swap!], metric: "max_swap",
      columns, devices: devicesOf(columns), disabled: null,
    });
  }
  const rank = (f: Family) => FAMILY_RANK.get(f.key) ?? FAMILY_RANK.get(f.types[0]!) ?? FAMILIES.length;
  families.sort((a, b) => rank(a) - rank(b) || textOrder(a.key, b.key));

  // A check lists the models it checked, so the model filter offers none it draws nothing for.
  const checked = new Map([[E2E, internal.rungs], [VL_E2E, internal.vlRungs]]);
  for (const family of families) {
    // An ASR family counts only the models its rule keeps, where one rule holds for each of its metrics.
    const rules = new Set(family.metrics.map((m) => asrRule(family.key, m.key)));
    const rule = rules.size === 1 ? [...rules][0] : null;
    const ids = checked.get(family.key)?.map((p) => p.model.id) ?? measured.filter((e) =>
      (DERIVED_FAMILIES.includes(family.key) ? family.types.includes(e.row.benchmark_type) : e.family === family.key)
      && (!rule || rule.keeps(e.row))).map((e) => e.model.id);
    internal.models.set(family.key, [...new Set(ids)].sort(textOrder));
    family.models = internal.models.get(family.key)!.length;
  }

  const columns = [...new Set(measured.map((e) => e.column))].sort(byColumn);
  const out = new ReportIndex({
    families,
    benchmarks: families.filter((f) => !DERIVED_FAMILIES.includes(f.key)),
    columns,
    devices: devicesOf(columns),
    deviceNames: typeNames,
    e2e,
    vlE2e,
    pairing: typeEntries.has(SPEC) ? pairing(typeEntries.get(SPEC)!, fleet) : null,
    swapped: rows.some((row) => (swapOf(row) ?? 0) > 0),
    missing: missingOf(entries),
  });
  internals.set(out, internal);
  return out;
}

/** A failure's cause as the Overview counts it. */
export function causeText(source: FailureSource | null, fleet: FleetDetail): string {
  if (!source) return fleet === "withheld" ? "cause withheld" : "no cause given";
  return failedText(source).replace(/^failed: /, "");
}

/** Jobs measured, and each failed job's cause: a job failed when any of its rows did. */
function jobStates(entries: readonly Entry[]): { measured: number; failed: (FailureSource | null)[] } {
  const jobs = new Map<string, { failed: boolean; source: FailureSource | null; measured: boolean }>();
  for (const { row, measured } of entries) {
    const job = jobs.get(row.job_id) ?? { failed: false, source: null, measured: false };
    if (row.failed && !job.failed) {
      job.failed = true;
      job.source = row.failure_source;
    }
    job.measured ||= measured;
    jobs.set(row.job_id, job);
  }
  const all = [...jobs.values()];
  return {
    measured: all.filter((job) => job.measured && !job.failed).length,
    failed: all.filter((job) => job.failed).map((job) => job.source),
  };
}

const poolName = (metric: string) => POOLS.find(([key]) => key === metric)?.[1] ?? metric;

/** The E2E check's rungs, each counted by what it lacks. */
interface Rungs {
  points: Point[];
  total: number;
  complete: number;
  unestimated: number;
  missing: { e2e: number; decode: number; prefill: number };
  /** Distinct batch sizes among them, so the finding names batch size only where it varies. */
  batches: number;
}

/**
 * The E2E check's rungs: the E2E latency each (model, column, prefill length,
 * batch size) measured against prefill + decode.
 */
function rungs(measured: readonly Entry[]): Rungs {
  interface Gathered { model: ModelId; runtime: string; column: string; tokens: number; batch: number; ttft: number[]; tps: number[];
                       decode: number[]; e2e: number[]; d: number[]; rows: ResultRow[] }
  const all = new Map<string, Gathered>();
  let batched = false;
  for (const e of measured) {
    const { benchmark_type: type, metric } = e.row;
    const tokens = e.params.prefill_tokens;
    if (!finite(tokens) || ![PREFILL, DECODE, LATENCY].includes(type)) continue;
    // A batch times differently from a lone request; rows without batch_size ran at 1.
    batched ||= Object.hasOwn(e.params, "batch_size");
    const batch = finite(e.params.batch_size) ? e.params.batch_size : 1;
    const key = `${e.model.label}\n${e.runtimeColumn}\n${tokens}\n${batch}`;
    const rung = all.get(key)
      ?? { model: e.model, runtime: e.runtime, column: e.runtimeColumn, tokens, batch, ttft: [], tps: [], decode: [], e2e: [], d: [],
           rows: [] };
    all.set(key, rung);
    const value = e.row.value!;
    if (type === PREFILL && metric === "ttft") rung.ttft.push(value);
    else if (type === PREFILL && metric === PREFILL) rung.tps.push(value);
    else if (type === DECODE && metric === DECODE) rung.decode.push(value);
    else if (type === LATENCY && metric === LATENCY) {
      rung.e2e.push(value);
      if (finite(e.params.decode_tokens)) rung.d.push(e.params.decode_tokens);
    } else continue;
    rung.rows.push(e.row);
  }
  const points: Point[] = [];
  const missing = { e2e: 0, decode: 0, prefill: 0 };
  let total = 0;
  let complete = 0;
  let unestimated = 0;
  const batches = new Set<number>();
  for (const rung of all.values()) {
    const prefill = !!(rung.ttft.length || rung.tps.length);
    if (!prefill && !rung.decode.length && !rung.e2e.length) continue;
    total++;
    batches.add(rung.batch);
    if (!rung.e2e.length) missing.e2e++;
    if (!rung.decode.length) missing.decode++;
    if (!prefill) missing.prefill++;
    if (!rung.e2e.length || !rung.decode.length || !prefill) continue;
    complete++;
    const excess = estimate(rung.tokens, median(rung.ttft), median(rung.tps), median(rung.decode), median(rung.d),
                            median(rung.e2e));
    if (!excess) {
      unestimated++;
      continue;
    }
    const params = batched ? { prefill_tokens: rung.tokens, batch_size: rung.batch } : { prefill_tokens: rung.tokens };
    points.push({ model: rung.model, runtime: rung.runtime, column: rung.column, params, values: excess, rows: rung.rows });
  }
  return { points, total, complete, unestimated, missing, batches: batches.size };
}

/**
 * The VL check's rungs: each vl_throughput job's end-to-end latency against its own time to first token plus
 * `decode_tokens` at its decode rate, on its own column, as its other charts draw it.
 */
function vlRungs(measured: readonly Entry[]): Rungs {
  const jobs = new Map<string, Entry[]>();
  for (const e of measured) if (e.row.benchmark_type === VL) push(jobs, e.row.job_id, e);
  const points: Point[] = [];
  const missing = { e2e: 0, decode: 0, prefill: 0 };
  let total = 0;
  let complete = 0;
  let unestimated = 0;
  for (const of of jobs.values()) {
    const inputs = of.filter((e) => ["ttft", DECODE, LATENCY].includes(e.row.metric));
    if (!inputs.length) continue;
    total++;
    const value = (metric: string) => median(inputs.filter((e) => e.row.metric === metric).map((e) => e.row.value!));
    const [ttft, decode, e2e] = [value("ttft"), value(DECODE), value(LATENCY)];
    if (e2e === null) missing.e2e++;
    if (decode === null) missing.decode++;
    if (ttft === null) missing.prefill++;
    if (e2e === null || decode === null || ttft === null) continue;
    complete++;
    const { model, variant, column, params } = of[0]!;
    // The time to first token is the prompt's own time, so no token count joins it.
    const excess = estimate(0, ttft, null, decode, finite(params.decode_tokens) ? params.decode_tokens : null, e2e);
    if (!excess) {
      unestimated++;
      continue;
    }
    points.push({ model, runtime: variant, column, params, values: excess, rows: inputs.map((e) => e.row) });
  }
  return { points, total, complete, unestimated, missing, batches: 1 };
}

/** The E2E check's finding; each excess is classed as rounded for print, so an outlier never reads as `5.0 %`. */
function e2eFinding(c: Rungs, vl: boolean): E2eFinding {
  const excess = c.points.map((p) => Math.round(p.values[1]! * 10) / 10);
  const spread = (off: number[]): Spread | null => (off.length ? { n: off.length, max: Math.max(...off) } : null);
  const slower = excess.filter((r) => r > BAND);
  const faster = excess.filter((r) => r < -BAND).map((r) => -r);
  return {
    kind: "checked", vl, total: c.total, complete: c.complete, unestimated: c.unestimated, missing: c.missing,
    batched: c.batches > 1, within: excess.length - slower.length - faster.length, slower: spread(slower),
    faster: spread(faster),
  };
}

/**
 * E2E latency over its estimate, prefill time plus `d` decode tokens, as
 * `[ms, % of the estimate]`; null at a gap. Inputs are cell medians.
 */
export function estimate(tokens: number, ttft: number | null, prefillTps: number | null, decodeTps: number | null,
                         d: number | null, e2e: number | null): [number, number] | null {
  const prefill = finite(ttft) ? ttft : finite(prefillTps) && prefillTps > 0 ? (tokens * 1000) / prefillTps : null;
  if (prefill === null || !finite(decodeTps) || decodeTps <= 0 || !finite(d) || !finite(e2e) || e2e <= 0) return null;
  const expected = prefill + (d * 1000) / decodeTps;
  if (!finite(expected) || expected <= 0) return null;
  return [e2e - expected, ((e2e - expected) / expected) * 100];
}

interface Job {
  model: ModelId;
  runtime: string;
  column: string;
  client: string | null;
  benchmark: string;
  speculative: boolean;
  params: Json;
  values: Map<string, number[]>;
  rows: ResultRow[];
}

/** vl_spec_decode's measured jobs with no failed row, each with its values by metric. */
function specJobs(entries: readonly Entry[]): Job[] {
  const jobs = new Map<string, Job>();
  const failed = new Set<string>();
  for (const e of entries) {
    const { row } = e;
    let job = jobs.get(row.job_id);
    if (!job) {
      const { mode, ...params } = e.params;
      job = { model: e.model, runtime: e.runtime, column: e.runtimeColumn, client: row.client_id, benchmark: row.benchmark_id,
              speculative: mode === "speculative", params, values: new Map(), rows: [] };
      jobs.set(row.job_id, job);
    }
    if (row.failed) failed.add(row.job_id);
    if (e.measured) {
      push(job.values, row.metric, row.value!);
      job.rows.push(row);
    }
  }
  return [...jobs].filter(([id, job]) => !failed.has(id) && job.values.size > 0).map(([, job]) => job);
}

/** A speculative and a baseline job of one benchmark on one client, as speedup points. */
function pairs(entries: readonly Entry[]): { decode: Point[]; e2e: Point[] } {
  const sides = new Map<string, { s: Job[]; b: Job[] }>();
  for (const job of specJobs(entries)) {
    if (job.client === null) continue;
    const key = [job.model.label, job.runtime, job.column, job.client, job.benchmark].join("\n");
    const side = sides.get(key) ?? { s: [], b: [] };
    (job.speculative ? side.s : side.b).push(job);
    sides.set(key, side);
  }
  const decode: Point[] = [];
  const e2e: Point[] = [];
  const of = (jobs: Job[], metric: string) => median(jobs.flatMap((job) => job.values.get(metric) ?? []));
  for (const { s, b } of sides.values()) {
    if (!s.length || !b.length) continue;
    const point = (value: number, metric: string): Point => ({
      model: s[0]!.model, runtime: s[0]!.runtime, column: s[0]!.column, params: s[0]!.params, values: [value],
      rows: [...s, ...b].flatMap((job) => job.rows.filter((row) => row.metric === metric)),
    });
    const [sDecode, bDecode] = [of(s, DECODE), of(b, DECODE)];
    if (sDecode !== null && bDecode !== null && bDecode > 0) decode.push(point(sDecode / bDecode, DECODE));
    const [sE2e, bE2e] = [of(s, "e2e_ms"), of(b, "e2e_ms")];
    if (sE2e !== null && bE2e !== null && sE2e > 0) e2e.push(point(bE2e / sE2e, "e2e_ms"));
  }
  return { decode, e2e };
}

/** How vl_spec_decode's benchmarks paired; null where none measured a job. */
function pairing(entries: readonly Entry[], fleet: FleetDetail): Pairing | null {
  const jobs = specJobs(entries);
  if (!jobs.length) return null;
  if (fleet === "withheld") return { kind: "withheld" };
  const groups = new Map<string, { s: Set<string | null>; b: Set<string | null> }>();
  for (const job of jobs) {
    const key = [job.model.label, job.runtime, job.benchmark].join("\n");
    const group = groups.get(key) ?? { s: new Set(), b: new Set() };
    (job.speculative ? group.s : group.b).add(job.client);
    groups.set(key, group);
  }
  let paired = 0;
  let split = 0;
  let one = 0;
  for (const { s, b } of groups.values()) {
    if (!s.size || !b.size) one++;
    else if ([...s].some((client) => client !== null && b.has(client))) paired++;
    else split++;
  }
  return { kind: "paired", benchmarks: groups.size, paired, split, one };
}

/** The Swap tab's address segment. */
export const SWAP = "swap";
/** The address segment of Missing data, the page of what a measured row lacks. */
export const MISSING = "missing";

/** One measured row that lacks a datum, as its section lists it. */
export interface MissingRow {
  row: ResultRow;
  /** Its model's descriptor as stored: what Jobs' model box matches. */
  descriptor: string;
  /** Its model's catalog name, as the charts draw it (`ModelId.parts`); null for a model that has none. */
  name: NamedPart | null;
}

/** The Missing data tab: a section per datum a measured row can lack, each listing those rows in the plan's order. */
export interface Missing {
  /** Rows with no model parameter count: neither the model parameter table at scoring time nor a model-params row has one. */
  params: MissingRow[];
  /** Every measured row, for the counts' denominator. */
  measured: number;
}

/** Null where no measured row lacks anything a section lists. */
function missingOf(entries: readonly Entry[]): Missing | null {
  const measured = entries.filter((e) => e.measured);
  const params = measured.filter((e) => e.row.model_params_total_millions === null)
    .map((e): MissingRow => ({
      row: e.row, descriptor: descriptorText(e.row.model_descriptor),
      name: e.model.parts.find((part): part is NamedPart => typeof part !== "string") ?? null,
    }));
  return params.length ? { params, measured: measured.length } : null;
}

/**
 * The order x candidates are tried in, before the rest by name. An ASR clip's length comes before the text beside it,
 * which an ASR row shares with VL's, so an ASR chart runs over the clip with a line per text length; no other type
 * has a clip.
 */
const X_ORDER = ["prefill_tokens", "audio_duration_ms", "text_tokens", "batch_size", "image_width", "decode_tokens",
  "draft_length", "num_images"];

/** Parameter values in order: numbers numerically, then text, then `unset`. */
function valueOrder(a: ParamValue, b: ParamValue): number {
  if (a.key === UNSET || b.key === UNSET) return Number(a.key === UNSET) - Number(b.key === UNSET);
  const [x, y] = [JSON.parse(a.key) as unknown, JSON.parse(b.key) as unknown];
  if (finite(x) && finite(y)) return x - y;
  return Number(!finite(x)) - Number(!finite(y)) || textOrder(a.text, b.text);
}

/** Every parameter on `points`: its values and whether it varies. */
function paramsOf(points: readonly Point[]): Map<string, Param> {
  const seen = new Map<string, Map<string, ParamValue>>();
  const present = new Map<string, number>();
  for (const { params } of points) {
    for (const [name, value] of Object.entries(params)) {
      if (!seen.has(name)) seen.set(name, new Map());
      const key = JSON.stringify(value);
      if (!seen.get(name)!.has(key)) seen.get(name)!.set(key, { key, text: parameterValue(value) });
      present.set(name, (present.get(name) ?? 0) + 1);
    }
  }
  const out = new Map<string, Param>();
  for (const name of [...seen.keys()].sort(textOrder)) {
    const values = [...seen.get(name)!.values()].sort(valueOrder);
    const varying = values.length >= 2;
    if (varying && present.get(name)! < points.length) values.push({ key: UNSET, text: "unset" });
    out.set(name, { name, values, varying });
  }
  return out;
}

/** Each series' x values, a series being a model on a column at one combination of `split`, which `combo` keys. */
function seriesRungs(points: readonly Point[], x: string, split: readonly string[]):
  Map<string, { combo: string; xs: Set<number> }> {
  const out = new Map<string, { combo: string; xs: Set<number> }>();
  for (const p of points) {
    const combo = split.map((q) => keyOf(p.params, q)).join("\n");
    const key = `${p.model.label}\n${p.column}\n${combo}`;
    const series = out.get(key) ?? { combo, xs: new Set<number>() };
    series.xs.add(p.params[x] as number);
    out.set(key, series);
  }
  return out;
}

/**
 * The x parameter, what rides on it, and what is left to split series; x null is bar form. Of the candidates that draw
 * a line, the one leaving fewest series a lone point, ties in the report's order.
 */
function chooseX(points: readonly Point[], params: Map<string, Param>, forced: string | null):
  Pick<Layout, "x" | "riding" | "rest" | "lone"> {
  const varying = [...params.values()].filter((p) => p.varying).map((p) => p.name);
  const numeric = (name: string) => points.every((p) => finite(p.params[name]));
  const rank = (name: string) => (X_ORDER.includes(name) ? X_ORDER.indexOf(name) : X_ORDER.length);
  const candidates = forced ? [forced]
    : varying.filter(numeric).sort((a, b) => rank(a) - rank(b) || textOrder(a, b));
  let best: (Pick<Layout, "x" | "riding" | "rest" | "lone"> & { single: number }) | null = null;
  for (const x of candidates) {
    const riding = varying.filter((q) => q !== x && rides(points, q, x));
    const rest = varying.filter((q) => q !== x && !riding.includes(q));
    const series = [...seriesRungs(points, x, rest).values()];
    // A combination none of whose series reaches two rungs is lone, where another's does.
    const swept = new Map<string, boolean>();
    for (const { combo, xs } of series) swept.set(combo, !!swept.get(combo) || xs.size >= 2);
    const unswept = [...swept].filter(([, line]) => !line).map(([combo]) => combo);
    const lone = new Set(unswept.length < swept.size ? unswept : []);
    if (forced) return { x, riding, rest, lone };
    const single = series.filter(({ xs }) => xs.size === 1).length;
    if (series.some(({ xs }) => xs.size >= 2) && (!best || single < best.single)) best = { x, riding, rest, lone, single };
  }
  return best ? { x: best.x, riding: best.riding, rest: best.rest, lone: best.lone }
    : { x: null, riding: [], rest: varying, lone: new Set() };
}

/** `q` rides on `x` when every x value occurs with exactly one q value. */
function rides(points: readonly Point[], q: string, x: string): boolean {
  const seen = new Map<number, string>();
  for (const p of points) {
    const at = p.params[x] as number;
    const value = keyOf(p.params, q);
    if ((seen.get(at) ?? value) !== value) return false;
    seen.set(at, value);
  }
  return true;
}

const layer = (unit: string, zero = true): Layer =>
  ({ title: "", metric: null, unit, zero, band: null, reference: null });
/** Whether a layer shades a band or draws a reference line, which bars cannot. */
const marked = (layers: readonly Layer[]): boolean => layers.some((l) => l.band !== null || l.reference !== null);

/** A family's metric laid out: its points, panel kinds, parameters and axis; null when it has none. */
function layoutOf(I: Internal, family: Family, metric: Metric): Layout | null {
  const cacheKey = `${family.key}\n${metric.key}`;
  if (I.layouts.has(cacheKey)) return I.layouts.get(cacheKey)!;
  let points: Point[];
  let layers = [layer(metric.unit)];
  let word = "jobs";
  let rank = 0;
  if (family.key === E2E || family.key === VL_E2E) {
    // Drawn as a share of the expected total alone: milliseconds read large on a long prompt, the same share or not.
    points = (family.key === E2E ? I.rungs : I.vlRungs).map((p) => ({ ...p, values: [p.values[1]!] }));
    if (family.key === E2E) word = "combinations";
    layers = [{ ...layer("%", false), title: `Extra E2E time as % of expected total (shaded: within ±${BAND} %)`,
                band: BAND, reference: 0 }];
  } else if (SWAPS.includes(family.key)) {
    // A job's swap once, in its benchmark type's panel, for the models that swapped somewhere on a column; an ASR job's
    // only from an exact-sized context, as `asrRule` says.
    const rule = asrRule(family.key, metric.key);
    const reads = I.entries.filter((e) => e.measured && family.types.includes(e.row.benchmark_type)
      && swapReads(family.key, e) && (!rule || rule.keeps(e.row)));
    const swappers = new Set(reads.filter((e) => swapOf(e.row)! > 0).map((e) => `${e.model.id}\n${e.column}`));
    const jobs = new Map<string, Entry[]>();
    for (const e of reads) if (swappers.has(`${e.model.id}\n${e.column}`)) push(jobs, e.row.job_id, e);
    const batched = reads.some((e) => Object.hasOwn(e.params, "batch_size"));
    // VL's prompt, or ASR's clip, by the parameters every type of the chart names, so one type's own (VL throughput's
    // decode_tokens) splits no series. A type the rule left without reads is skipped, or it would empty the intersection.
    const named = family.key !== SWAPPED ? family.types.map((type) => reads.filter((e) => e.row.benchmark_type === type))
      .filter((of) => of.length).map((of) => new Set(of.flatMap((e) => Object.keys(e.params)))) : [];
    const shared = named.length ? [...named[0]!].filter((q) => named.every((names) => names.has(q))).sort(textOrder) : [];
    const prompt = (params: Json): Json => {
      if (family.key !== SWAPPED) {
        return Object.fromEntries(shared.filter((q) => Object.hasOwn(params, q)).map((q) => [q, params[q]]));
      }
      const { prefill_tokens, batch_size } = params;
      return batched ? { prefill_tokens, batch_size: finite(batch_size) ? batch_size : 1 } : { prefill_tokens };
    };
    layers = family.types.map((type) =>
      ({ ...layer(metric.unit), title: FAMILY_LABEL.get(type) ?? type, metric }));
    rank = family.types.indexOf(family.types.reduce((a, b) =>
      (reads.filter((e) => e.row.benchmark_type === b).length > reads.filter((e) => e.row.benchmark_type === a).length ? b : a)));
    points = [...jobs.values()].map((of) => {
      const e = of[0]!;
      const at = family.types.indexOf(e.row.benchmark_type);
      return { model: e.model, runtime: e.variant, column: e.column, params: prompt(e.params),
               values: family.types.map((_, i) => (i === at ? swapOf(e.row)! * metric.factor : Number.NaN)),
               rows: of.map((x) => x.row) };
    });
  } else if (metric.key === "decode_speedup" || metric.key === "e2e_speedup") {
    points = (metric.key === "decode_speedup" ? I.pairs?.decode : I.pairs?.e2e) ?? [];
    word = "pairs";
    layers = [{ ...layer("×", false), reference: 1 }];
  } else {
    // An ASR row compares only beside rows of its regime, as `asrRule` says.
    const rule = asrRule(family.key, metric.key);
    const typed = I.entries.filter((e) => e.measured && e.family === family.key && (!rule || rule.keeps(e.row)));
    if (metric.key === "peak_memory") {
      // A panel per pool reported, each of a column's pools under its device type; a point fills its pool's layer.
      const reported = POOLS.filter(([pool]) => typed.some((e) => e.row.metric === pool));
      const counts = reported.map(([pool]) => typed.filter((e) => e.row.metric === pool).length);
      layers = reported.map(([, name]) => ({ ...layer(metric.unit), title: `${name} memory` }));
      rank = counts.indexOf(Math.max(...counts));
      points = typed.flatMap((e) => {
        const at = reported.findIndex(([pool]) => pool === e.row.metric);
        return at < 0 ? [] : [{ model: e.model, runtime: e.variant, column: e.column, params: e.params,
          values: reported.map((_, i) => (i === at ? e.row.value! * metric.factor : Number.NaN)), rows: [e.row] }];
      });
    } else if (metric.key === "prompt_tokens") {
      // A job's counts once, a panel per count reported: they are the job's observations, on each of its rows.
      const reported = (TOKENS[family.key] ?? []).filter(([key]) => typed.some((e) => finite(e.row.observations[key])));
      const jobs = new Map<string, Entry[]>();
      for (const e of typed) push(jobs, e.row.job_id, e);
      layers = reported.map(([, name]) => ({ ...layer(metric.unit), title: name }));
      points = [...jobs.values()].flatMap((of) => {
        const e = of[0]!;
        const values = reported.map(([key]) => {
          const count = e.row.observations[key];
          return finite(count) ? count : Number.NaN;
        });
        return values.some(finite) ? [{ model: e.model, runtime: e.variant, column: e.column, params: e.params, values,
                                         rows: of.map((x) => x.row) }] : [];
      });
      const counts = reported.map((_, i) => points.filter((p) => finite(p.values[i])).length);
      rank = counts.indexOf(Math.max(...counts));
    } else {
      points = typed.filter((e) => e.row.metric === metric.key).map((e) =>
        ({ model: e.model, runtime: e.variant, column: e.column, params: e.params,
           values: [e.row.value! * metric.factor], rows: [e.row] }));
    }
  }
  if (!points.length) {
    I.layouts.set(cacheKey, null);
    return null;
  }
  const out = laidOut({ metric, layers, rank: [rank], word }, points,
                      family.key === E2E || family.key === SWAPPED ? "prefill_tokens" : null);
  I.layouts.set(cacheKey, out);
  return out;
}

/** `points` in `base`'s panels, over x `forced`, else the x `chooseX` picks, with what rides on it. */
function laidOut(base: Pick<Layout, "metric" | "layers" | "rank" | "word">, points: Point[],
                 forced: string | null): Layout {
  const params = paramsOf(points);
  const chosen = chooseX(points, params, forced);
  // With no line to draw, a band or reference line keeps line form at the first rung every point names.
  const axis = !chosen.x && marked(base.layers) ? X_ORDER.find((q) => points.every((p) => finite(p.params[q]))) : undefined;
  const { x, riding, rest, lone } = axis ? chooseX(points, params, axis) : chosen;
  const rungText = new Map<number, string>();
  if (x) {
    for (const p of points) {
      const at = p.params[x] as number;
      if (rungText.has(at)) continue;
      rungText.set(at, [String(at), ...riding.map((q) => paramText(p.params, q))].join(" · "));
    }
  }
  const runtimes = [...new Set(points.map((p) => p.runtime))].sort(textOrder);
  return { ...base, points, runtimes, params, x, riding, rest, rungText, lone };
}

/** What a results tab's address asks for; an empty value is the default. */
export interface ReportFilters {
  /** The family a tab draws, from its path. */
  benchmark: string;
  metric: string;
  /** Device types to draw, as the address repeats `device`; none draws every type unless that draws more than 12 panels. */
  device: string[];
  /** Runtimes dropped, each with its flags as a chart lists them, as the address repeats `drop`; none drops none. */
  drop: string[];
  /** Model URIs to draw, as the address repeats `model`; none draws every model. */
  model: string[];
  per: string;
  /** Pinned parameters by name, each value its JSON text (`"b"`, `2`), or `unset` for none. */
  params: Record<string, string>;
  /** Which outcome a chart's rows link narrows Jobs to; the charts ignore it. */
  status: string;
}

const PER = ["4", "8", "12", "all"];
const PER_DEFAULT = "8";
const STATUS = ["measured", "failed"];

/** The filters a select writes, one value each. */
const SELECTS = ["per", "status"];

/** The address keys every results tab shares, which a link to another tab carries. */
export const SHARED = ["device", "model", "per", "status"];
/** Those of them the address repeats, a choice each. */
export const PICKED = ["device", "model"];

const NONE: ReportFilters = {
  benchmark: "", metric: "", device: [], drop: [], model: [], per: "", params: {}, status: "",
};

/** The filters the address asks for, as given, on the tab of `benchmark`; `view(…).filters` drops stale ones. */
export function reportFilters(query: URLSearchParams, benchmark: string): ReportFilters {
  const params: Record<string, string> = {};
  for (const key of new Set(query.keys())) {
    if (key.startsWith("p.") && query.get(key)) params[key.slice(2)] = query.get(key)!;
  }
  const asked = (key: string) => query.get(key) ?? "";
  const picked = (key: string) => [...new Set(query.getAll(key).filter(Boolean))];
  return {
    benchmark, metric: asked("metric"), device: picked("device"), drop: picked("drop"), model: picked("model"),
    per: asked("per"), params, status: asked("status"),
  };
}

/** The filters as a chart tab's address keeps them, empty ones included for `href` to drop; the path names the family. */
export const reportPairs = (filters: ReportFilters): [string, string][] => [
  ["metric", filters.metric], ...filters.device.map((type): [string, string] => ["device", type]),
  ...filters.drop.map((variant): [string, string] => ["drop", variant]), ...modelPairs(filters.model),
  ["per", filters.per],
  ...Object.keys(filters.params).sort(textOrder).map((name): [string, string] => [`p.${name}`, filters.params[name]!]),
  ["status", filters.status],
];

/** The address pairs of the chosen models: one `model` each, its descriptor as stored. */
export const modelPairs = (chosen: readonly string[]): [string, string][] => chosen.map((id) => ["model", id]);

/**
 * The runtimes the address drops once the line for `variant` on the chart `filters` name is clicked: dropped there,
 * whatever dropped it is let go of, else it is added.
 */
export function toggledRuntime(index: ReportIndex, filters: ReportFilters, variant: string): string[] {
  const I = internal(index);
  const own = resolve(I, index, filters).layout?.runtimes ?? [];
  const by = filters.drop.filter((d) => matchedRuntimes(I, own, [d]).includes(variant));
  return by.length ? filters.drop.filter((d) => !by.includes(d)) : [...filters.drop, variant];
}

/**
 * `filters` with the control at address `key` (`benchmark`, …, `p.<name>`) set
 * to `value`. Another benchmark opens on its own metric, unpinned: a kept `ttft`
 * or pin would carry across families that happen to share a name.
 */
export function withFilter(filters: ReportFilters, key: string, value: string): ReportFilters {
  if (key === "benchmark") return { ...filters, benchmark: value, metric: "", params: {} };
  // The device, runtime and model choices are lists; a key no select writes changes nothing.
  if (!key.startsWith("p.")) return SELECTS.includes(key) ? { ...filters, [key]: value } : filters;
  const params = { ...filters.params };
  if (value) params[key.slice(2)] = value;
  else delete params[key.slice(2)];
  return { ...filters, params };
}

const internal = (index: ReportIndex): Internal => {
  const found = internals.get(index);
  if (!found) throw new Error("not a report index");
  return found;
};

interface Resolved {
  filters: ReportFilters;
  family: Family | null;
  layout: Layout | null;
  /** The chosen device types the family charts; none draws every type. */
  devices: string[];
  /** The chosen models the family measured; none draws every model. */
  models: string[];
  /** The dropped runtimes the chart ran on. */
  dropped: string[];
  pins: Map<string, ParamValue>;
  /** The varying parameters left unpinned, which split series (line form) or charts (bar form). */
  split: string[];
  per: number;
}

/**
 * The runtimes of `own`, a chart's, that `asked` names: each asked one it ran, and for one it ran with other flags
 * alone (a benchmark type asks for its own, and the E2E check draws across them) every flag set of that runtime.
 */
function matchedRuntimes(I: Internal, own: readonly string[], asked: readonly string[]): string[] {
  const runtimeOf = (variant: string) => I.variants.get(variant) ?? variant;
  return [...new Set(asked.flatMap((variant) => (own.includes(variant) ? [variant]
    : I.variants.has(variant) ? own.filter((ran) => runtimeOf(ran) === runtimeOf(variant)) : [])))];
}

/** The family, metric and filters `asked` resolves to, stale values dropped. */
function resolve(I: Internal, index: ReportIndex, asked: ReportFilters): Resolved {
  const family = index.families.find((f) => f.key === asked.benchmark && !f.disabled) ?? null;
  const metric = family?.metrics.find((m) => m.key === asked.metric);
  const opened = metric ?? family?.metrics.find((m) => m.key === family.metric);
  const layout = family && opened ? layoutOf(I, family, opened) : null;
  const pins = new Map<string, ParamValue>();
  const params: Record<string, string> = {};
  for (const [name, value] of Object.entries(asked.params)) {
    const found = layout?.rest.includes(name) ? layout.params.get(name)?.values.find((v) => pinOf(v) === value) : undefined;
    if (!found) continue;
    pins.set(name, found);
    params[name] = value;
  }
  const per = PER.includes(asked.per) ? asked.per : "";
  const measured = I.models.get(family?.key ?? "") ?? [];
  return {
    filters: {
      benchmark: family ? asked.benchmark : "", metric: metric ? asked.metric : "",
      device: asked.device.filter((type) => index.devices.includes(type)),
      drop: asked.drop.filter((variant) => I.variants.has(variant)),
      model: asked.model.filter((id) => I.ids.has(id)), per, params,
      status: STATUS.includes(asked.status) ? asked.status : "",
    },
    family, layout,
    // The address keeps a choice another family measured; this one draws by its own, every one where none are.
    devices: asked.device.filter((type) => family?.devices.includes(type)),
    models: asked.model.filter((id) => measured.includes(id)),
    dropped: matchedRuntimes(I, layout?.runtimes ?? [], asked.drop),
    pins, split: (layout?.rest ?? []).filter((name) => !pins.has(name)),
    per: per === "all" ? Infinity : Number(per || PER_DEFAULT),
  };
}

/** One chart cell: every value that fell on it. */
export interface Cell {
  median: number;
  n: number;
  min: number;
  max: number;
  /** In ascending order. */
  values: number[];
  /** Its values are counts, printed with only the decimals they need. */
  whole?: true;
  /** A row behind it ran a clip past the model's documented audio limit ([`beyondAudioLimit`]): drawn, but marked. */
  beyond?: true;
}

/** A cell as tooltips and tables print it: `137.7 (n=6: 103.6, …)`, a range past 8 values. */
export function cellText(cell: Cell | null): string {
  if (!cell) return "—";
  const text = textOf(cell);
  if (cell.n === 1) return text(cell.median);
  const spread = cell.n > 8 ? `${text(cell.min)}–${text(cell.max)}` : cell.values.map(text).join(", ");
  return `${text(cell.median)} (n=${cell.n}: ${spread})`;
}

/** A cell's median as a bar's tip prints it. */
export const medianText = (cell: Cell): string => textOf(cell)(cell.median);

const textOf = (cell: Cell) => (cell.whole ? exactText : valueText);

/** The result rows the charts draw, a series the legend turned off (`hidden`) left out: the jobs under a section. */
export function drawnRows(charts: readonly Chart[], hidden: ReadonlySet<string>): Set<ResultRow> {
  const marks = charts.flatMap((chart): { key: string; rows: ResultRow[] }[] =>
    (chart.form === "bars" ? chart.bars : chart.panels.flatMap((panel) => panel.series)));
  return new Set(marks.filter((mark) => !hidden.has(mark.key)).flatMap((mark) => mark.rows));
}

/** The cell of `values`, marked where a row behind it (`rows`) ran a clip past the model's audio limit. */
function cellOf(values: number[], whole = false, rows: readonly ResultRow[] = []): Cell {
  const sorted = [...values].sort((a, b) => a - b);
  return { median: median(sorted)!, n: sorted.length, min: sorted[0]!, max: sorted[sorted.length - 1]!, values: sorted,
           ...(whole ? { whole: true } : {}), ...(rows.some(beyondAudioLimit) ? { beyond: true } : {}) };
}

/** A series' legend entry: its hue (`--series-{hue}`) and marker shape, by its slot in the chart. */
export interface Swatch {
  key: string;
  /** Its label as plain text (`labelText`). */
  label: string;
  /** Its label as the legend draws it. */
  parts: Label;
  hue: number;
  shape: number;
}

/** A series in one panel, a cell (or null) per rung of its chart. */
export interface LineSeries extends Swatch {
  cells: (Cell | null)[];
  /** The result rows its cells are drawn from. */
  rows: ResultRow[];
  /** The result rows behind each cell, in its order. */
  points: ResultRow[][];
}

/** One rung of a line chart's x axis. */
export interface Rung {
  /** The value, which is its tick too. */
  x: number;
  /** The value and what rides on it, for tooltips and table headers: `1024 · decode_tokens=64`. */
  text: string;
}

/** One line panel: a column, or every column once folded. */
export interface LinePanel extends Layer {
  /** Its column's name, runtime and flags, a part per line, under a title that names the device type; none where the title says it all. */
  runtime: Label;
  /** The chart's every member, in slot order, empty ones included. */
  series: LineSeries[];
}

/** A line chart: one rank band of series, drawn in a panel per column with one legend. */
export interface LineChart {
  form: "line";
  /** The heading of the group it draws in, where a section draws several; else absent. */
  group?: string;
  /** The x parameter, the axis title. */
  x: string;
  /** `Rank 1–8 of 23` when the series take more than one chart, `Rank 23 of 23` for a lone last one, else empty. */
  title: string;
  rungs: Rung[];
  legend: Swatch[];
  panels: LinePanel[];
}

/** One bar: a model (and column), or a whole combination once a chart per bar collapses into one. */
export interface Bar {
  key: string;
  /** Its model and column whole, as its tooltip and the screen-reader table print them. */
  label: string;
  /** What its row's gutter prints, whole, as plain text: the model as a legend names it, then the column parts that vary. */
  name: string;
  /** `name` as the gutter draws it. */
  nameParts: Label;
  cell: Cell;
  /** The result rows it is drawn from. */
  rows: ResultRow[];
}

/** A bar chart in one hue: one combination's bars best first, or every combination in order once each would hold one bar. */
export interface BarChart {
  form: "bars";
  /** The heading of the group it draws in, where a section draws several; else absent. */
  group?: string;
  title: string;
  unit: string;
  /** The axis runs to at least this (100 for accuracy). */
  atLeast: number;
  bars: Bar[];
}

export type Chart = LineChart | BarChart;

/** One choice of a select. */
export interface Choice {
  value: string;
  label: string;
  disabled: boolean;
}

/** One select of the filter row. */
export interface Control {
  /** The address key a change writes, for `withFilter`: `benchmark`, …, `p.<name>`. */
  key: string;
  label: string;
  /** The choice in effect. */
  value: string;
  options: Choice[];
  /** It applies to another family of this plan, not this one. */
  disabled: boolean;
}

/** A filter of several choices: what the family has, and those of them chosen. */
export interface Picks {
  options: string[];
  chosen: string[];
}

/** What the results page offers over every chart: the device bar's types and the charts' size; null where no choice. */
export interface PageControls {
  device: Picks | null;
  per: Control | null;
}

/** What one filter state draws. */
export interface ReportView {
  /** The filters in effect, stale values dropped: what `reportPairs` writes for a change. */
  filters: ReportFilters;
  family: Family | null;
  metric: Metric | null;
  /** The device types drawn: those chosen, or the one narrowed to past 12 panels; none for every type. */
  devices: string[];
  /** A select per varying parameter the chart's series split by, which pins it. */
  pins: Control[];
  /** The runtimes the other filters leave the chart, a line each, `runtime + flags` where its job asked for flags. */
  runtimes: { options: string[]; dropped: string[] };
  /** Each of `runtimes`' options as its lines read, a part each: its name first, where it has one. */
  runtimeLines: Record<string, Label>;
  /** Failed jobs when the family measured nothing: it draws only that, linked to them. */
  unmeasured: number | null;
  /** Why nothing is drawn: no result matches the filters, or the E2E check has no rung it could estimate. */
  empty: string | null;
  charts: Chart[];
  /** What every series in view shares, drawn under the chart's heading. */
  caption: Label;
  notes: string[];
}

const MULTI = "cells hold more than one result: variants the result row cannot tell apart (runtime flags, sampling, "
  + "or a re-run)";
const WITHHELD = " and different devices on one runtime (device names are withheld at your access)";
const SAME_TYPE = " and several devices of one type";
const E2E_NOTE = "Runtime flags and client builds are not on a result row, so the page cannot tell whether the three "
  + "measurements ran alike.";
const VL_PREFILL_NOTE = "Counts the whole prompt, image and template tokens included, over a time to first token that "
  + "holds the vision encoding.";
const VL_PREFILL_TOKENS_NOTE = `${VL_PREFILL_NOTE} Prompt tokens charts each model's count.`;
const ASR_PREFILL_NOTE = "Counts the whole prompt, audio and template tokens included, over a time to first token that "
  + "holds the audio encoding. A clip is as many audio tokens as each model's encoder makes of it, which differs between "
  + "models, so a rate reads across them only beside their counts.";
const ASR_PREFILL_TOKENS_NOTE = `${ASR_PREFILL_NOTE} Prompt tokens charts each model's count.`;
const BEYOND_NOTE = `Marked points (a dashed ring; on a bar, a dashed outline) are ${BEYOND_LIMIT}: they ran a clip `
  + "longer than the model's own documentation says it takes, such as Gemma 4 past the 30 s its card documents. They "
  + "show what the runtime did with the whole clip, a speed or memory scenario whose transcription quality is not "
  + "established, not the model's documented use.";
const ASR_EXACT_PEAK: AsrRule = {
  keeps: exactPeak,
  note: (n, of) => `Only peaks whose context was sized from the clip's own audio tokens (exact) are compared: ${n} of `
    + `${of} jobs. An upper-bound context holds KV for audio the clip does not have, and a pinned one is the plan's `
    + "size, so neither is the clip's peak.",
  empty: "No job's context was sized exactly.",
};
const ASR_EXACT_SWAP: AsrRule = {
  keeps: exactPeak,
  note: (n, of) => `Only ASR jobs whose context was sized from the clip's own audio tokens (exact) are compared: ${n} `
    + `of ${of} jobs. An upper-bound or pinned context sizes the KV cache, and with it the swap, to more than the clip.`,
  empty: "No ASR job that read its swap had its context sized exactly.",
};
const ASR_CALIBRATED: AsrRule = {
  keeps: calibrated,
  note: (n, of) => `Only jobs whose audio tokens the runtime counted (calibrated) are compared: ${n} of ${of} jobs. `
    + "A measured count includes the audio markers, and unless the cell pinned its context, that context was sized "
    + "from an upper bound, so the job counted another way and may have run another workload.",
  empty: "No job's audio tokens were counted by the runtime.",
};
const VL_E2E_NOTE = "One job measures all three, so each point checks a run against its own time to first token, image "
  + "encoding included, and decode time.";
const PAIR_NOTE = "Pairs a speculative and a baseline job of one benchmark on one device; other launch settings are not "
  + "on a result row.";
/** Panels the report draws before it opens on one column. */
const MAX_PANELS = 12;
/** Series a chart tells apart before a hue and a shape repeat. */
const DISTINCT = HUES * SHAPES;

interface Drawn {
  charts: Chart[];
  caption: Label;
  /** Cells holding more than one value, of all cells drawn. */
  multi: number;
  cells: number;
  /** The columns in view, in column order. */
  columns: string[];
  /** Every column draws its own panels, so one column draws fewer. */
  perColumn: boolean;
}

/** What `filters` draw from `index`: the family, its charts, caption, notes and controls. */
export function view(index: ReportIndex, filters: ReportFilters): ReportView {
  const I = internal(index);
  const r = resolve(I, index, filters);
  const { family, layout } = r;
  let devices = r.devices;
  let drawn: Drawn | null = null;
  const notes: string[] = [];
  if (layout) {
    drawn = draw(I, r, layout, devices);
    const panels = drawn?.charts.reduce((n, chart) => n + (chart.form === "line" ? chart.panels.length : 1), 0) ?? 0;
    const types = [...new Set(drawn?.columns.flatMap((column) => I.columnDevice.get(column) ?? []))];
    if (!devices.length && drawn?.perColumn && panels > MAX_PANELS && types.length > 1) {
      devices = [types[0]!];
      notes.push(`Showing ${index.deviceNames.get(types[0]!)}; choose device types for the other ${types.length - 1}.`);
      drawn = draw(I, r, layout, devices);
    }
  }
  // After the narrowing, so a line names a runtime of the device types drawn.
  const offered = [...new Set((layout?.points ?? []).filter((p) => kept(I, r, devices, p)).map((p) => p.runtime))]
    .sort(textOrder);
  // A device column pools its type's devices, a withheld-tier one every device; an eval column names none at any tier.
  const devicesHidden = drawn?.columns.some((c) => c.startsWith(WITHHELD_COLUMN));
  const sameType = I.fleet === "full" && !family?.types.includes("eval");
  const pooled = devicesHidden ? WITHHELD : sameType ? SAME_TYPE : "";
  if (drawn?.multi) notes.push(`${drawn.multi} of ${drawn.cells} ${MULTI}${pooled}.`);
  if (drawn?.charts.some((chart) => chart.form === "line" && chart.legend.length > DISTINCT)) {
    notes.push(`A chart holds more than ${DISTINCT} series, so colours and shapes repeat; choose fewer per chart.`);
  }
  if (family?.key === E2E && drawn) notes.push(E2E_NOTE);
  if (family?.key === VL_E2E && drawn) notes.push(VL_E2E_NOTE);
  if (family?.key === VL && layout?.metric.key === PREFILL) {
    notes.push(family.metrics.some((m) => m.key === "prompt_tokens") ? VL_PREFILL_TOKENS_NOTE : VL_PREFILL_NOTE);
  }
  if (family?.key === ASR && layout?.metric.key === PREFILL) {
    notes.push(family.metrics.some((m) => m.key === "prompt_tokens") ? ASR_PREFILL_TOKENS_NOTE : ASR_PREFILL_NOTE);
  }
  const rule = family && asrRule(family.key, layout?.metric.key || r.filters.metric || family.metric || "");
  if (family && rule) {
    const jobs = new Map<string, boolean>();
    for (const e of I.entries) {
      const scoped = SWAPS.includes(family.key) ? family.types.includes(e.row.benchmark_type) && swapReads(family.key, e)
        : e.family === family.key;
      if (e.measured && scoped) jobs.set(e.row.job_id, rule.keeps(e.row));
    }
    const compared = [...jobs.values()].filter(Boolean).length;
    if (compared < jobs.size) notes.push(rule.note(compared, jobs.size));
  }
  const cellsDrawn = (drawn?.charts ?? []).flatMap((chart): Cell[] => (chart.form === "bars"
    ? chart.bars.map((bar) => bar.cell) : chart.panels.flatMap((panel) => panel.series.flatMap((s) => s.cells))
      .filter((c): c is Cell => c !== null)));
  if (cellsDrawn.some((c) => c.beyond)) notes.push(BEYOND_NOTE);
  if (family && SWAPS.includes(family.key) && layout) {
    const drawnModels = new Set(layout.points.map((p) => p.model.id)).size;
    notes.push(`Only models that swapped on a device type and runtime are drawn: ${drawnModels} of `
      + `${I.models.get(family.key)?.length ?? drawnModels}. A run's swap is read once per job, 0 where it held none.`);
  }
  // The finding leads, plan-wide: no filter changes it.
  if (family?.key === E2E && layout && index.e2e?.kind === "checked") notes.unshift(e2eText(index.e2e));
  if (family?.key === VL_E2E && layout && index.vlE2e) notes.unshift(e2eText(index.vlE2e));
  if (family?.key === SPEC && layout && index.pairing) notes.unshift(pairingText(index.pairing));
  if (layout && (layout.metric.key === "decode_speedup" || layout.metric.key === "e2e_speedup")) notes.push(PAIR_NOTE);
  return {
    filters: r.filters,
    family,
    metric: layout?.metric ?? null,
    devices,
    runtimes: { options: offered, dropped: r.dropped.filter((variant) => offered.includes(variant)) },
    runtimeLines: Object.fromEntries(offered.map((variant) => [variant, variantLines(I, variant)])),
    pins: (layout?.rest ?? []).map((name) => select(`p.${name}`, name, r.filters.params[name] ?? "",
      [{ value: "", label: "all", disabled: false },
       ...layout!.params.get(name)!.values.map((v) => ({ value: pinOf(v), label: v.text, disabled: false }))])),
    unmeasured: family && !family.metrics.length && family.key !== E2E ? family.failed : null,
    empty: family?.key === E2E && !layout
      ? (I.complete ? `No combination could be estimated: ${UNESTIMABLE}.` : "No combination has all three measurements.")
      : family?.key === VL_E2E && !layout
        ? (I.vlComplete ? `No job could be estimated: ${UNESTIMABLE}.` : "No job has all three measurements.")
        : rule && family?.metrics.length && !layout ? rule.empty
          : layout && !drawn ? "No results match these filters." : null,
    charts: drawn?.charts ?? [],
    caption: drawn?.caption ?? [],
    notes,
  };
}

const pinsHold = (r: Resolved, params: Json): boolean =>
  [...r.pins].every(([name, value]) => keyOf(params, name) === value.key);

/** The device type, model and pin filters the charts apply, before the runtime's. */
const kept = (I: Internal, r: Resolved, devices: string[], { model, column, params }: Point): boolean =>
  (!devices.length || devices.includes(I.columnDevice.get(column) ?? ""))
  && (!r.models.length || r.models.includes(model.id)) && pinsHold(r, params);

/** The vault name of `key`'s runtime, `key` a runtime or a variant; `""` for none. */
const labelOf = (I: Internal, key: string): string => I.labels.get(I.variants.get(key) ?? key) ?? "";

/**
 * The name of `key`, in the same form as the name of a model. If its runtime has a catalog name, the name is that
 * catalog name and then the flags. If not, the name is the vault name and then the text of `key`.
 */
function nameOf(I: Internal, key: string): Label {
  const runtime = I.variants.get(key) ?? key;
  const named = I.runtimeNames.get(runtime);
  if (!named) return [[labelOf(I, key), key].filter(Boolean).join(" ")];
  const flags = flagsOf(key, runtime);
  return [named, ...(flags ? [`runtime flags = ${flags}`] : [])];
}

/**
 * The names of a column for panels and bars: the short name of its device type, and the lines and the name of its
 * runtime. `whole` is the full name of the column. It names the runtime by its catalog name, if the runtime has one.
 * If not, `whole` is the column text, so a bar's tooltip and its screen-reader row show the runtime as stored.
 */
type ColumnNames = (column: string) => { device: string | null; runtime: Label; name: Label; whole: Label };

/** The family's points that pass the filters, drawn in `layout`'s form or its sweeps' (`sweptParts`); null when none pass. */
function draw(I: Internal, r: Resolved, layout: Layout, devices: string[]): Drawn | null {
  const shown = (p: Point) => kept(I, r, devices, p) && !r.dropped.includes(p.runtime);
  const points = layout.points.filter(shown);
  if (!points.length) return null;
  const columns = [...new Set(points.map((p) => p.column))].sort(I.columnOrder);
  // The device bar names a device type, so the caption names devices only where they are withheld.
  const lead = columns.length === 1 && columns[0]!.startsWith(WITHHELD_COLUMN) ? "devices withheld" : null;
  const variantOf = (column: string) => I.columnRuntime.get(column) ?? column;
  const deviceOf = (column: string) => {
    const of = I.columnDevice.get(column);
    if (of !== undefined) return I.deviceNames.get(of) ?? of;
    return column.startsWith(WITHHELD_COLUMN) ? "devices withheld" : null;
  };
  // What a panel's title and the lines under it say, and a bar's name.
  const names: ColumnNames = (column) => {
    const variant = variantOf(column);
    const [device, name] = [deviceOf(column), nameOf(I, variant)];
    const named = I.runtimeNames.has(I.variants.get(variant) ?? variant);
    return { device, runtime: variantLines(I, variant), name,
             whole: named ? [...(device ? [device] : []), ...name] : [column] };
  };
  // In `laid`'s form, its series split by its own varying parameters left unpinned.
  const drawOf = (on: Point[], at: string[], laid = layout) => {
    const own = { ...r, split: laid.rest.filter((name) => !r.pins.has(name)) };
    return laid.x ? drawLineForm(own, laid, on, at, lead, names, points)
      : drawBars(own, laid, on, at, lead, names, points);
  };
  const whole = drawOf(points, columns);
  const parts = sweptParts(r, layout);
  if (parts.length) {
    // Each part a group of charts on the columns it has; the caption and counts stay the section's.
    const charts = parts.flatMap(({ heading, laid }) => {
      const on = laid.points.filter(shown);
      return on.length ? drawOf(on, columns.filter((c) => on.some((p) => p.column === c)), laid).charts
        .map((chart) => grouped(chart, heading)) : [];
    });
    return { ...whole, charts };
  }
  const types = [...new Set(columns.map((column) => I.columnType.get(column)!))].sort(textOrder);
  if (!r.family || !isEval(r.family) || types.length < 2) return whole;
  // An eval run on several runtime types draws a group of charts per type; the caption and counts stay the section's.
  const charts = types.flatMap((type) => {
    const of = (column: string) => I.columnType.get(column) === type;
    return drawOf(points.filter((p) => of(p.column)), columns.filter(of)).charts
      .map((chart) => grouped(chart, `runtime type = ${type}`));
  });
  return { ...whole, charts };
}

/**
 * The parts `layout`'s points draw in where `r`'s family has sweeps (`SWEEPS`), none where no sweep draws: each sweep
 * over its own x, a panel per `by` value that reaches two x values, then the points none draws, laid out alone as
 * `layoutOf` lays out a family. Pins narrow every part, a pin standing in for a sweep's own value.
 */
function sweptParts(r: Resolved, layout: Layout): { heading: string; laid: Layout }[] {
  const pinned = layout.points.filter((p) => pinsHold(r, p.params));
  const swept = new Set<Point>();
  const L = layout.layers.length;
  const parts = (SWEEPS[r.family?.key ?? ""] ?? []).flatMap((s) => {
    const at = Object.entries(s.at).filter(([name]) => !r.pins.has(name));
    const groups = new Map<string, Point[]>();
    for (const p of pinned) {
      if (finite(p.params[s.x]) && at.every(([name, v]) => keyOf(p.params, name) === JSON.stringify(v))) {
        push(groups, s.by ? keyOf(p.params, s.by) : UNSET, p);
      }
    }
    const drawn = [...groups].filter(([, of]) => new Set(of.map((p) => p.params[s.x])).size >= 2)
      .sort(([a], [b]) => valueOrder({ key: a, text: a }, { key: b, text: b })).map(([, of]) => of);
    if (!drawn.length) return [];
    const members = drawn.flat();
    for (const p of members) swept.add(p);
    // The panels' parameter, and what rides on it (image_height on image_width), leave the points so they split no series.
    const moved = new Set(s.by ? [...paramsOf(members).values()]
      .filter((q) => q.varying && q.name !== s.x && rides(members, q.name, s.by!)).map((q) => q.name) : []);
    // A panel per `by` value and layer, a point's values in its own value's.
    const layers = drawn.flatMap((of) => layout.layers.map((l) =>
      ({ ...l, title: [s.by ? paramText(of[0]!.params, s.by) : "", l.title].filter(Boolean).join(" · ") })));
    const points = drawn.flatMap((of, g) => of.map((p) => ({
      ...p, params: Object.fromEntries(Object.entries(p.params).filter(([q]) => !moved.has(q))),
      values: layers.map((_, i) => (Math.floor(i / L) === g ? p.values[i % L]! : Number.NaN)),
    })));
    const rank = drawn.flatMap((_, g) => layout.rank.map((l) => g * L + l));
    const heading = [s.title, ...Object.keys(s.at).map((name) => paramText(members[0]!.params, name))].join(" · ");
    return [{ heading, laid: laidOut({ metric: layout.metric, layers, rank, word: layout.word }, points, s.x) }];
  });
  const rest = pinned.filter((p) => !swept.has(p));
  if (!parts.length || !rest.length) return parts;
  // Headed, as a sweep is, by what holds one value over its points alone: under p.text_tokens=1024 they are the 256
  // decode tokens', which neither the section's caption nor a label names.
  const one = (of: Point[], q: string) => new Set(of.map((p) => keyOf(p.params, q))).size === 1;
  const apart = [...layout.params.keys()].filter((q) => one(rest, q) && !one(pinned, q));
  const heading = [UNSWEPT, ...apart.map((q) => paramText(rest[0]!.params, q))].join(" · ");
  return [...parts, { heading, laid: laidOut(layout, rest, null) }];
}

/** `chart` in `outer`, before any group it has, its series keyed apart from another group's, as a legend hides them. */
function grouped(chart: Chart, outer: string): Chart {
  const group = [outer, chart.group].filter(Boolean).join(" · ");
  if (chart.form === "bars") return { ...chart, group };
  const key = (k: string) => `${outer}\n${k}`;
  return { ...chart, group, legend: chart.legend.map((s) => ({ ...s, key: key(s.key) })),
           panels: chart.panels.map((p) => ({ ...p, series: p.series.map((s) => ({ ...s, key: key(s.key) })) })) };
}

/** The model part of a label across `models`: none where they share one, which the caption names. */
function modelParts(models: ModelId[]): { part: (m: ModelId) => Label; shared: Label } {
  return new Set(models.map((m) => m.label)).size <= 1 ? { part: () => [], shared: models[0]?.parts ?? [] }
    : { part: (m) => m.parts, shared: [] };
}

/**
 * Caption parts: each parameter with one value across the points in view, by
 * name, counted where some lack it; then what rides on x. `split` names those
 * the labels or chart titles carry.
 */
function sharedParams(layout: Layout, points: readonly Point[], split: readonly string[]): string[] {
  const out: string[] = [];
  for (const param of layout.params.values()) {
    if (param.name === layout.x || layout.riding.includes(param.name) || split.includes(param.name)) continue;
    const keys = counts(points.map((p) => keyOf(p.params, param.name)));
    const present = [...keys.keys()].filter((key) => key !== UNSET);
    if (present.length !== 1) continue;
    const value = `${param.name}=${param.values.find((v) => v.key === present[0])!.text}`;
    const unset = keys.get(UNSET) ?? 0;
    out.push(unset
      ? `${value} (${points.length - unset} of ${points.length} ${layout.word}; unset on the rest)` : value);
  }
  return [...out, ...layout.riding.map((q) => `${q} rides on ${layout.x}`)];
}

/** The caption: `lead`, the one column's device type, the shared model part, then `sharedParams`. */
const captionOf = (layout: Layout, points: readonly Point[], lead: string | null, shared: Label,
                   split: readonly string[]): Label =>
  [...(lead ? [lead] : []), ...shared, ...sharedParams(layout, points, split)];

interface Group {
  key: string;
  model: ModelId;
  params: Json;
  parts: Label;
  /** Values by `cellKey`. */
  cells: Map<string, number[]>;
  columns: Set<string>;
  /** The result rows behind them, by column. */
  rows: Map<string, ResultRow[]>;
  /** The result rows behind each point, by `cellKey`. */
  at: Map<string, ResultRow[]>;
}

const cellKey = (layer: number, column: string, x: number) => `${layer}\n${column}\n${x}`;

/** `items` sorted best first by score, ties by label. */
function ranked<T extends { score: number; label: string }>(items: T[], lower: boolean): T[] {
  return items.sort((a, b) => (lower ? a.score - b.score : b.score - a.score) || textOrder(a.label, b.label));
}

/**
 * The median of a group's cell medians on the ranked layers, in `column` or in all; a group with none there, a pool
 * or benchmark type of its own, by its every layer's.
 */
function score(group: Group, layout: Layout, column: string | null): number {
  const on = (layers: number[]) => median([...group.cells]
    .filter(([at]) => layers.some((l) => at.startsWith(column === null ? `${l}\n` : `${l}\n${column}\n`)))
    .map(([, values]) => median(values)));
  return on(layout.rank) ?? on(layout.layers.map((_, i) => i))!;
}

/**
 * Line form, each lone combination (`Layout.lone`) after the lines, a group headed by its values: drawn as bars split
 * by x, a dot per series saying less, or as lines where a layer has a band or reference line, which bars cannot draw.
 */
function drawLineForm(r: Resolved, layout: Layout, points: Point[], columns: string[], lead: string | null,
                      names: ColumnNames, section: Point[]): Drawn {
  const comboOf = (p: Point) => layout.rest.map((q) => keyOf(p.params, q)).join("\n");
  const combos = new Map<string, Point[]>();
  for (const p of points) if (layout.lone.has(comboOf(p))) push(combos, comboOf(p), p);
  if (!combos.size) return drawLines(r, layout, points, columns, lead, names, section);
  const x = layout.x!;
  const line = points.filter((p) => !layout.lone.has(comboOf(p)));
  const position = (q: string, p: Point) => layout.params.get(q)!.values.findIndex((v) => v.key === keyOf(p.params, q));
  const apart = [...combos.values()].sort((a, b) =>
    layout.rest.reduce((d, q) => d || position(q, a[0]!) - position(q, b[0]!), 0));
  const differing = r.split.filter((q) => new Set(section.map((p) => keyOf(p.params, q))).size > 1);
  const within = (of: Point[]) => r.split.filter((q) => new Set(of.map((p) => keyOf(p.params, q))).size > 1);
  const on = (of: Point[]) => columns.filter((c) => of.some((p) => p.column === c));
  // Headed by the split values that tell the parts apart and hold one value over this part.
  const heading = (of: Point[]) => differing.filter((q) => !within(of).includes(q)).map((q) =>
    `${q}=${layout.params.get(q)!.values.find((v) => v.key === keyOf(of[0]!.params, q))!.text}`).join(" · ");
  const keepsLines = marked(layout.layers);
  // Bars name a column where the section has several, as the lines' panels do.
  const lone = (of: Point[]) => (keepsLines ? drawLines({ ...r, split: [] }, layout, of, on(of), lead, names, section)
    : drawBars({ ...r, split: [x] }, layout, of, columns, lead, names, section));
  const drawn = [
    ...(line.length ? [{ of: line, part: drawLines({ ...r, split: within(line) }, layout, line, on(line), lead, names,
                                                   section) }] : []),
    ...apart.map((of) => ({ of, part: lone(of) })),
  ];
  // Over the section, so a runtime type's charts drawing one part are headed as the others are.
  const parted = new Set(section.map((p) => (layout.lone.has(comboOf(p)) ? comboOf(p) : null))).size > 1;
  const headed = (of: Point[], chart: Chart) => {
    const head = parted ? heading(of) : "";
    return head ? grouped(chart, head) : chart;
  };
  return {
    charts: drawn.flatMap(({ of, part }) => part.charts.map((chart) => headed(of, chart))),
    caption: captionOf(layout, points, lead, modelParts(section.map((p) => p.model)).shared, differing),
    multi: drawn.reduce((n, { part }) => n + part.multi, 0), cells: drawn.reduce((n, { part }) => n + part.cells, 0),
    columns, perColumn: drawn[0]!.part.perColumn,
  };
}

/**
 * `ranked` in charts of `per`, as its select says, the last what is left: an even split would draw 8 and 12 alike.
 * Titled `Rank 1–8 of 23` where there are several, `Rank 23 of 23` for a lone last one, else untitled.
 */
function bands<T>(ranked: T[], per: number): { of: T[]; title: string }[] {
  const n = ranked.length;
  if (n <= per) return [{ of: ranked, title: "" }];
  return Array.from({ length: Math.ceil(n / per) }, (_, k) => {
    const of = ranked.slice(k * per, (k + 1) * per);
    return { of, title: of.length === 1 ? `Rank ${k * per + 1} of ${n}` : `Rank ${k * per + 1}–${k * per + of.length} of ${n}` };
  });
}

/**
 * Line form: series ranked across the columns in view, split into charts of `per`, a panel per column; labelled as
 * `section`, the caption's points, reads, so every group of a section names alike.
 */
function drawLines(r: Resolved, layout: Layout, points: Point[], columns: string[], lead: string | null,
                   names: ColumnNames, section: Point[]): Drawn {
  const x = layout.x!;
  const { split } = r;
  const groups = new Map<string, Group>();
  for (const p of points) {
    const key = [p.model.label, ...split.map((name) => keyOf(p.params, name))].join("\n");
    let group = groups.get(key);
    if (!group) {
      group = { key, model: p.model, params: p.params, parts: [], cells: new Map(), columns: new Set(), rows: new Map(),
                at: new Map() };
      groups.set(key, group);
    }
    group.columns.add(p.column);
    for (const row of p.rows) push(group.rows, p.column, row);
    for (const [i, value] of p.values.entries()) {
      // A memory or swap point fills its own layer only, a VL sweep's its own panel's, and lists its rows there alone.
      if (!finite(value)) continue;
      const at = cellKey(i, p.column, p.params[x] as number);
      push(group.cells, at, value);
      for (const row of p.rows) push(group.at, at, row);
    }
  }
  const all = [...groups.values()];
  const { part, shared } = modelParts(section.map((p) => p.model));
  const differing = split.filter((name) => new Set(section.map((p) => keyOf(p.params, name))).size > 1);
  for (const group of all) {
    const parts = [...part(group.model), ...differing.map((name) => paramText(group.params, name))]
      .filter((p) => p !== "");
    group.parts = parts.length ? parts : group.model.parts;
  }

  const ranks = all.flatMap((g) => [...g.cells].filter(([at]) => layout.rank.some((l) => at.startsWith(`${l}\n`))));
  const multi = ranks.filter(([, values]) => values.length > 1).length;

  const caption = captionOf(layout, points, lead, shared, differing);

  const xs = [...new Set(points.map((p) => p.params[x] as number))].sort((a, b) => a - b);
  const rungs = xs.map((at) => ({ x: at, text: layout.rungText.get(at) ?? String(at) }));
  const cells = (group: Group, i: number, column: string) => xs.map((at) => {
    const values = group.cells.get(cellKey(i, column, at));
    return values ? cellOf(values, layout.metric.whole, group.at.get(cellKey(i, column, at))) : null;
  });

  // Folded, each column's only series is drawn as one series of a single panel, named by its column.
  const fold = columns.length >= 2 && columns.every((c) => all.filter((g) => g.columns.has(c)).length <= 1);
  let members: { key: string; group: Group; column: string | null; score: number; label: string; parts: Label }[];
  const member = (key: string, group: Group, column: string | null, parts: Label) =>
    ({ key, group, column, score: score(group, layout, column), label: labelText(parts), parts });
  if (fold) {
    const only = (c: string) => all.find((g) => g.columns.has(c))!;
    // Named where the caption does not name it all.
    const named = !shared.length || differing.length > 0;
    members = columns.map((c) => {
      const { device, name } = names(c);
      const column = [...(device ? [device] : []), ...name].filter((p) => p !== "");
      return member(c, only(c), c, [...(column.length ? column : [c]), ...(named ? only(c).parts : [])]);
    });
  } else {
    members = all.map((g) => member(g.key, g, null, g.parts));
  }
  ranked(members, layout.metric.lower);

  const charts: LineChart[] = [];
  for (const { of: chart, title } of bands(members, r.per)) {
    const legend = chart.map((m, slot) => ({ key: m.key, label: m.label, parts: m.parts, ...channel(slot) }));
    // Titled by its column, a lone one in view too, so a chart reads alike whatever the device bar chose; folded,
    // a panel holds every column and is titled by what it draws.
    const panel = (column: string | null, l: Layer, i: number): LinePanel => {
      const { device, runtime } = column === null ? { device: null, runtime: [] } : names(column);
      return {
        ...l,
        title: [device, l.title].filter(Boolean).join(" · ") || layout.metric.label,
        runtime,
        series: chart.map((m, slot) => ({ ...legend[slot]!, cells: cells(m.group, i, column ?? m.column!),
                                          rows: m.group.rows.get(column ?? m.column!) ?? [],
                                          points: xs.map((at) => m.group.at.get(cellKey(i, column ?? m.column!, at)) ?? []) })),
      };
    };
    // No panel for a column, or a layer in it (a pool its device did not report), none of the chart's series has.
    const drawn = (i: number, column: string | null) => chart.some((m) =>
      [...m.group.cells.keys()].some((at) => at.startsWith(`${i}\n${column ?? m.column!}\n`)));
    const panels = fold ? layout.layers.flatMap((l, i) => (drawn(i, null) ? [panel(null, l, i)] : []))
      : columns.filter((c) => chart.some((m) => m.group.columns.has(c)))
        .flatMap((c) => layout.layers.flatMap((l, i) => (drawn(i, c) ? [panel(c, l, i)] : [])));
    charts.push({ form: "line", x, title, rungs, legend, panels });
  }
  return { charts, caption, multi, cells: ranks.length, columns, perColumn: !fold };
}

/**
 * Bar form: a chart per combination of the unpinned varying parameters and layer, as a layer is a panel of its own in
 * line form; a bar per model (and column), labelled as `section`, the caption's points, reads.
 */
function drawBars(r: Resolved, layout: Layout, points: Point[], columns: string[], lead: string | null,
                  names: ColumnNames, section: Point[]): Drawn {
  const { split } = r;
  const withColumn = columns.length > 1;
  const layered = layout.layers.length > 1;
  type Tally = { model: ModelId; column: string; values: number[]; rows: ResultRow[] };
  type Combo = { keys: string[]; layer: number };
  const combos = new Map<string, Combo & { bars: Map<string, Tally> }>();
  for (const p of points) {
    const keys = split.map((name) => keyOf(p.params, name));
    // A memory or swap point is valued in its own layer alone.
    for (const [layer, value] of p.values.entries()) {
      if (!finite(value)) continue;
      const id = [...keys, ...(layered ? [layer] : [])].join("\n");
      const combo = combos.get(id) ?? { keys, layer, bars: new Map() };
      combos.set(id, combo);
      const key = `${p.model.label}\n${withColumn ? p.column : ""}`;
      const bar = combo.bars.get(key) ?? { model: p.model, column: p.column, values: [], rows: [] };
      bar.values.push(value);
      bar.rows.push(...p.rows);
      combo.bars.set(key, bar);
    }
  }
  const position = (i: number, key: string) => layout.params.get(split[i]!)!.values.findIndex((v) => v.key === key);
  const ordered = [...combos.values()].sort((a, b) =>
    a.keys.reduce((d, key, i) => d || position(i, key) - position(i, b.keys[i]!), 0) || a.layer - b.layer);
  const title = (combo: Combo) => [split.map((name, i) =>
    `${name}=${layout.params.get(name)!.values.find((v) => v.key === combo.keys[i])!.text}`).join(", "),
    layered ? layout.layers[combo.layer]!.title : ""].filter(Boolean).join(" · ");
  const { part, shared } = modelParts(section.map((p) => p.model));
  const label = (bar: Tally): Label => [...part(bar.model), ...(withColumn ? names(bar.column).whole : [])];
  const varies = (of: (c: string) => string | null) => withColumn && new Set(columns.map(of)).size > 1;
  const [byDevice, byRuntime] = [varies((c) => names(c).device), varies((c) => names(c).name.map(partKey).join("\n"))];
  const name = (bar: Tally): Label => {
    const { device, name: runtime } = names(bar.column);
    return [...part(bar.model), ...(byDevice && device ? [device] : []), ...(byRuntime ? runtime : [])];
  };
  // The layer's metric after its title, where it names one: the unit follows.
  const chartTitle = (combo: Combo) => [title(combo), layered ? layout.layers[combo.layer]!.metric?.label : null]
    .filter(Boolean).join(" · ") || layout.metric.label;

  const unit = layout.metric.unit;
  const atLeast = layout.metric.key === "accuracy" ? 100 : 0;
  const all = ordered.flatMap((combo) => [...combo.bars.values()]);
  let charts: BarChart[] = ordered.map((combo) => ({
    form: "bars", title: chartTitle(combo), unit, atLeast,
    bars: ranked([...combo.bars].map(([key, bar]) => {
      const cell = cellOf(bar.values, layout.metric.whole, bar.rows);
      const [labelled, named] = [label(bar), name(bar)];
      return { key, label: labelText(labelled.length ? labelled : bar.model.parts),
               ...nameFields(named.length ? named : bar.model.parts), cell, rows: bar.rows, score: cell.median };
    }), layout.metric.lower).map(({ key, label, name, nameParts, cell, rows }) => ({ key, label, name, nameParts, cell, rows })),
  }));
  // A chart per bar says less than one chart of them all, in combination order.
  const collapsed = charts.length > 1 && charts.every((chart) => chart.bars.length === 1);
  if (collapsed) {
    charts = [{
      form: "bars", title: layout.metric.label, unit, atLeast,
      bars: ordered.map((combo, i) => {
        const bar = [...combo.bars.values()][0]!;
        const titled = (rest: Label): Label => [title(combo), ...rest].filter((p) => p !== "");
        return { key: [...combo.keys, ...(layered ? [combo.layer] : [])].join("\n"), label: labelText(titled(label(bar))),
                 ...nameFields(titled(name(bar))), cell: charts[i]!.bars[0]!.cell, rows: bar.rows };
      }),
    }];
  }
  // An eval's bars rank in charts of `per`, as lines do; collapsed, they run in combination order, not by rank.
  if (!collapsed && r.family && isEval(r.family)) {
    charts = charts.flatMap((chart) => bands(chart.bars, r.per).map(({ of, title: rank }) =>
      ({ ...chart, title: [chart.title, rank].filter(Boolean).join(" · "), bars: of })));
  }
  return {
    charts, caption: captionOf(layout, points, lead, shared, split),
    multi: all.filter((bar) => bar.values.length > 1).length, cells: all.length, columns, perColumn: false,
  };
}

/** A bar's gutter name, as plain text and as drawn. */
const nameFields = (parts: Label): Pick<Bar, "name" | "nameParts"> => ({ name: labelText(parts), nameParts: parts });

/** Whether `per` applies to any chart of the plan: a family's lines, or an eval's bars. */
function splitsCharts(I: Internal, index: ReportIndex): boolean {
  I.splitsCharts ??= index.families.some((f) => !f.disabled
    && (isEval(f) || f.metrics.some((m) => !!layoutOf(I, f, m)?.x)));
  return I.splitsCharts;
}

const select = (key: string, label: string, value: string, options: Choice[], disabled = false): Control =>
  ({ key, label, value, options, disabled });
const plain = (values: string[]) => values.map((value) => ({ value, label: value, disabled: false }));

/** The filter row the page's charts share: a control where the plan has a choice to offer. */
export function pageControls(index: ReportIndex, filters: ReportFilters): PageControls {
  const I = internal(index);
  return {
    device: index.devices.length
      ? { options: index.devices, chosen: filters.device.filter((type) => index.devices.includes(type)) } : null,
    per: splitsCharts(I, index)
      ? select("per", "per chart", PER.includes(filters.per) ? filters.per : PER_DEFAULT, plain(PER)) : null,
  };
}
