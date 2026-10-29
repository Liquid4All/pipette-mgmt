// The Jobs tab's table: a line per result row of each job, its columns, issues and filters.

import { duration, when } from "./format";
import {
  SHOWN_UNIT, bytesText, digits, expiresText, failedText, leaseText, relativePercent, sheetValue,
  spreadPercent, stddevText,
} from "./plans";
import {
  BEYOND_LIMIT, beyondAudioLimit, canonical, descriptorText, deviceNames, deviceType, knobsText, lowerIsBetter,
} from "./report";
import { offeredChoices } from "./table";
import { tabLink, type TabBase } from "./tabs";
import { nameOf, nameTexts, type DescriptorNames } from "./descriptor-names";
import { namedLabel, namedAs } from "./vault";
import type { PlanJobWithResult } from "./api";
import type { DescriptorDisplay, JobState, ParentJob, ResultRow } from "./api-types";

export type Listed = Pick<PlanJobWithResult, "job_id" | "state" | "result" | "planned"> &
  Partial<Pick<PlanJobWithResult, "queue" | "plan" | "planned_runtime_sha256" | "planned_model_sha256">>;

/** One line of the table: a job and one row of its latest run, or the job alone where it has none. */
export interface Line<J extends Listed = Listed> {
  job: J;
  row: ResultRow | null;
}

/** Each job's rows as lines, in the plan's order and the rows' own. */
export const linesOf = <J extends Listed>(jobs: readonly J[]): Line<J>[] =>
  jobs.flatMap((job): Line<J>[] => (job.result.status === "scored" && job.result.rows.length
    ? job.result.rows.map((row) => ({ job, row })) : [{ job, row: null }]));

/** The line's row in each earlier run, newest first; null where a run has none. */
export function earlierRows({ job, row }: Line): { run: ParentJob; row: ResultRow | null }[] {
  if (!row || job.result.status !== "scored") return [];
  const parameters = canonical(row.parameters);
  return job.result.earlier_runs.map(({ job: run, rows }) => ({
    run, row: rows.find((r) => r.metric === row.metric && canonical(r.parameters) === parameters) ?? null,
  }));
}

/** How a line's value moved from its newest earlier run's: in percent, or in points on a ratio. */
export interface Change {
  text: string;
  /** Grey within 3, percent or points, as noise. */
  tone: "better" | "worse" | "flat";
}
export function changeOf(line: Line): Change | null {
  const now = line.row;
  const was = earlierRows(line)[0]?.row;
  if (!now || !was || now.failed || was.failed || now.value === null || was.value === null) return null;
  const ratio = now.unit === "ratio";
  if (!ratio && !was.value) return null;
  const by = ratio ? (now.value - was.value) * 100 : ((now.value - was.value) / Math.abs(was.value)) * 100;
  const tone = Math.abs(by) <= 3 ? "flat" : by > 0 === lowerIsBetter(now.unit) ? "worse" : "better";
  const sign = by > 0 ? "+" : by < 0 ? "−" : "±";
  return { text: `${sign}${Math.abs(by).toFixed(1)}${ratio ? " pp" : "%"}`, tone };
}

/** What a line's result came to: a failed row fails it, a finite value measures it. */
export type Outcome = "measured" | "failed" | "none";
export function outcomeOf({ row }: Pick<Line, "row">): Outcome {
  if (!row) return "none";
  if (row.failed) return "failed";
  return Number.isFinite(row.value) ? "measured" : "none";
}

/** A line's benchmark id: its job's plan cell's, else its row's. */
export const benchmarkOf = ({ job, row }: Line): string => job.planned?.benchmark ?? row?.benchmark_id ?? "";

/** Use planned descriptors if the row is absent or failed. Clients often omit descriptors when a job fails. */
const plannedShows = ({ row }: Line): boolean => !row || row.failed === true;

/** Use reported descriptors when present. For a failed or absent row, use the plan for each missing descriptor. */
const modelOf = (line: Line): string =>
  descriptorText(line.row?.model_descriptor ?? (plannedShows(line) ? line.job.planned?.model : null));
const runtimeOf = (line: Line): string =>
  descriptorText(line.row?.runtime_descriptor ?? (plannedShows(line) ? line.job.planned?.runtime : null));
/** The digest of the runtime that `runtimeOf` shows: the row's where the row reports one, else the planned one. */
const runtimeDigestOf = (line: Line): string | null =>
  line.row?.runtime_descriptor != null ? line.row.runtime_descriptor_sha256
    : plannedShows(line) ? line.job.planned_runtime_sha256 ?? null : null;
/** The digest of the model that `modelOf` shows: the row's where the row reports one, else the planned one. */
const modelDigestOf = (line: Line): string | null =>
  line.row?.model_descriptor != null ? line.row.model_descriptor_sha256
    : plannedShows(line) ? line.job.planned_model_sha256 ?? null : null;
/** The catalog names of the runtime that a line shows, or null for a runtime with no name. */
export const runtimeNameOf = (line: Line, ctx: Context): DescriptorDisplay | null =>
  nameOf(ctx.descriptorNames, runtimeDigestOf(line));
/** The catalog names of the model that a line shows, or null for a model with no name. */
export const modelNameOf = (line: Line, ctx: Context): DescriptorDisplay | null =>
  nameOf(ctx.descriptorNames, modelDigestOf(line));
/** The catalog names of the descriptor that a column shows on a line, or null for a column with no catalog name. */
export const columnNameOf = (key: string, line: Line, ctx: Context): DescriptorDisplay | null =>
  key === "model_descriptor" ? modelNameOf(line, ctx) : key === "runtime_descriptor" ? runtimeNameOf(line, ctx) : null;

/** A line's device: the lease holder while its job is held, else the one that ran its row. */
export const clientOf = ({ job, row }: Line): string | null => job.queue?.lease?.client_id ?? row?.client_id ?? null;

/** Each of a job's reported descriptors or flags, `kind` of every row of its run. */
const reported = (job: Listed,
                  kind: "model_descriptor" | "runtime_descriptor" | "runtime_flags" | "requested_runtime_flags"): string[] =>
  job.result.status === "scored" ? job.result.rows.map((row) => descriptorText(row[kind])).filter(Boolean) : [];

/** How far a line's spread runs relative to its value, by pipette-datasheet's tiers (`stddevCovTier`). */
export type Tier = "fine" | "slight" | "notable" | "significant" | "unreliable";
const TIERS: [Tier, number, string][] = [
  ["fine", 1, "≤1%"], ["slight", 3, "1–3%"], ["notable", 5, "3–5%"], ["significant", 10, "5–10%"],
  ["unreliable", Infinity, ">10%"],
];
export const spreadOf = ({ row }: Pick<Line, "row">): number | null =>
  row ? spreadPercent(row.value, row.value_stddev, row.unit) : null;
export function tierOf(spread: number | null): Tier | null {
  if (spread === null) return null;
  return TIERS.find(([, top]) => spread <= top)![0];
}

/** The state from which a device's run is suspect: Apple's `serious` and up, Android's `moderate` and up. */
const HOT_FROM = { apple: "serious", android: "moderate" };
const ORDER = {
  apple: ["nominal", "fair", "serious", "critical"],
  android: ["none", "light", "moderate", "severe", "critical", "emergency", "shutdown"],
};
/** A run's repetitions' worst thermal state, read before or after them, and whose scale it is on. */
function thermal(row: ResultRow | null, when: "before" | "after"): { scale: "apple" | "android"; state: string } | null {
  for (const scale of ["apple", "android"] as const) {
    const read = row?.device[`${scale}_${scale === "apple" ? "thermal_state" : "thermal_status"}_${when}`];
    const states = (Array.isArray(read) ? read : [read]).filter((s): s is string => typeof s === "string");
    if (states.length) {
      const rank = (s: string) => ORDER[scale].indexOf(s);
      return { scale, state: states.reduce((a, b) => (rank(b) > rank(a) ? b : a)) };
    }
  }
  return null;
}

/** The swap a row's run held at its most, in bytes; null where it held none or read none. */
function swapBytes(row: ResultRow | null): number | null {
  const swap = row?.observations.max_swap_bytes;
  return typeof swap === "number" && swap > 0 ? swap : null;
}

/** Whether a line's run swapped, as the `swap` issue reads it. */
export const swapped = ({ row }: Pick<Line, "row">): boolean => swapBytes(row) !== null;

/** What may make a line's figure untrustworthy, beside a failure. */
export type IssueKey = "failed" | "spread" | "swap" | "thermal" | "corpus" | "power" | "params" | "audio_limit";
export const ISSUES: Record<IssueKey, string> = {
  failed: "failed", spread: "stddev over 5%", swap: "swapped", thermal: "ran hot", corpus: "corpus mismatch",
  power: "power save or low battery", params: "no parameter count", audio_limit: BEYOND_LIMIT,
};
export interface Issue {
  key: IssueKey;
  text: string;
}

export function issuesOf(line: Line): Issue[] {
  const { job, row } = line;
  const out: Issue[] = [];
  if (row?.failed || (!row && job.state === "failed")) out.push({ key: "failed", text: "failed" });
  const spread = spreadOf(line);
  if (spread !== null && spread > 5) out.push({ key: "spread", text: `stddev ±${digits(spread, 0, 1)}%` });
  const swap = swapBytes(row);
  if (swap !== null) out.push({ key: "swap", text: `swap ${bytesText(swap)}` });
  const hot = thermal(row, "after");
  if (hot && ORDER[hot.scale].indexOf(hot.state) >= ORDER[hot.scale].indexOf(HOT_FROM[hot.scale])) out.push({ key: "thermal", text: `thermal ${hot.state}` });
  if (row?.observations.dataset_digest_matched === false) out.push({ key: "corpus", text: "corpus mismatch" });
  // An ASR clip past the model's documented audio limit: a scenario, not the model's documented use.
  if (row && beyondAudioLimit(row)) out.push({ key: "audio_limit", text: BEYOND_LIMIT });
  // The model parameter table sizes the model; a failed row has no figure for the count to qualify.
  if (row && !row.failed && row.value !== null && row.model_params_total_millions === null) {
    out.push({ key: "params", text: "no parameter count" });
  }
  const battery = row?.device.battery_level;
  if (row?.device.power_save_mode === true) out.push({ key: "power", text: "power save" });
  else if (typeof battery === "number" && battery < 20 && row?.device.power_state === "not_charging") {
    out.push({ key: "power", text: `battery ${battery}%` });
  }
  return out;
}

/** What columns and filters read beyond a line: the read's names, and the devices' types. */
export interface Context {
  names: Record<string, string>;
  /** Each device's type, by client id, as [`deviceTypes`] reads it. */
  types: Record<string, string>;
  /** Each type's short name, as Results prints it; a type without one is named in full. */
  typeNames?: ReadonlyMap<string, string>;
  /** The catalog names of each descriptor digest, as the read gives them for the reader. */
  descriptorNames?: DescriptorNames;
}

/** The short names of the device types `lines` report, by type. */
export const deviceTypeNames = (lines: readonly Line[]): Map<string, string> =>
  deviceNames(lines.flatMap(({ row }) => (row?.device_name ? [row] : [])));

/** Each device's type, the make its result rows report, by client id. */
export function deviceTypes(lines: readonly Line[]): Record<string, string> {
  const out: Record<string, string> = {};
  for (const line of lines) {
    const id = clientOf(line);
    if (id && line.row?.device_name) out[id] ??= deviceType(line.row);
  }
  return out;
}

/** A line's device type, from its row or else its device's other rows; a device that reported none, by its id. */
const deviceTypeOf = (line: Line, ctx: Context): string => {
  if (line.row?.device_name) return deviceType(line.row);
  const id = clientOf(line);
  return id ? ctx.types[id] ?? id : "";
};

type Raw = string | number | boolean | null;

/** One column of the table and the CSV: its raw value for the file, and the text the table shows where it differs. */
export interface Column {
  key: string;
  label: string;
  /** Drawn once for its job, spanning the job's lines. */
  job: boolean;
  /** Shown until the reader picks the columns. */
  shown: boolean;
  /** Figures, lined up on the right. */
  num?: boolean;
  /** Stored JSON, a descriptor or flags, which reads whole on fewer lines. */
  json?: boolean;
  raw: (line: Line, ctx: Context) => Raw;
  text?: (line: Line, ctx: Context) => string;
}

const bytes = (read: (line: Line) => unknown) => (line: Line) => {
  const n = read(line);
  return typeof n === "number" ? bytesText(n) : "";
};
const flags = (kind: "model_flags" | "runtime_flags" | "benchmark_flags") => ({ job }: Line) =>
  descriptorText(job.planned?.[kind]) || null;
type Stored = "model_flags" | "runtime_flags" | "benchmark_flags" | "requested_model_flags" | "requested_runtime_flags"
  | "requested_benchmark_flags" | "model_vault_descriptor" | "eval_metadata" | "client_extras";
const stored = (key: Stored, label: string, shown = false): Column => ({
  key: `row_${key}`, label, job: false, shown, json: true, raw: ({ row }) => descriptorText(row?.[key]) || null,
});

/** Every column, in the table's order; the actions column is the tab's, never picked or exported. */
export const COLUMNS: Column[] = [
  { key: "job", label: "job", job: true, shown: true, raw: ({ job }) => job.job_id },
  { key: "state", label: "state", job: true, shown: true, raw: ({ job }) => job.state },
  { key: "model_descriptor", label: "model", job: false, shown: true, json: true, raw: (line) => modelOf(line) || null },
  { key: "model_catalog_name", label: "model catalog name", job: false, shown: false,
    raw: (line, ctx) => modelNameOf(line, ctx)?.display ?? null },
  { key: "runtime_descriptor", label: "runtime", job: false, shown: true, json: true,
    raw: (line) => runtimeOf(line) || null },
  { key: "runtime_catalog_name", label: "runtime catalog name", job: false, shown: false,
    raw: (line, ctx) => runtimeNameOf(line, ctx)?.display ?? null },
  { key: "benchmark_type", label: "benchmark type", job: true, shown: true,
    raw: ({ job, row }) => row?.benchmark_type ?? job.planned?.benchmark_type ?? null },
  { key: "benchmark", label: "benchmark", job: true, shown: true, raw: benchmarkOf },
  { key: "metric", label: "metric", job: false, shown: true, raw: ({ row }) => row?.metric ?? null },
  { key: "parameters", label: "parameters", job: false, shown: true, json: true,
    raw: ({ row }) => (row && Object.keys(row.parameters).length ? canonical(row.parameters) : null) },
  { key: "value", label: "value", job: false, shown: true, num: true, raw: ({ row }) => row?.value ?? null,
    text: ({ row }) => (row && row.value !== null ? sheetValue(row.value, row.unit) : "") },
  { key: "unit", label: "unit", job: false, shown: true, raw: ({ row }) => row?.unit ?? null,
    text: ({ row }) => (row ? SHOWN_UNIT[row.unit] ?? row.unit : "") },
  { key: "stddev", label: "stddev", job: false, shown: false, num: true, raw: ({ row }) => row?.value_stddev ?? null,
    text: ({ row }) => (row && row.value_stddev !== null ? stddevText(row.value_stddev, row.unit) : "") },
  { key: "spread", label: "stddev %", job: false, shown: true, num: true,
    raw: ({ row }) => {
      const relative = row && relativePercent(row.value, row.value_stddev);
      return relative == null ? null : Number(relative.toFixed(2));
    },
    text: ({ row }) => {
      const relative = row && relativePercent(row.value, row.value_stddev);
      return relative == null ? "" : `±${digits(relative, 0, 1)}%`;
    } },
  { key: "issues", label: "issues", job: false, shown: true,
    raw: (line) => issuesOf(line).map((issue) => issue.text).join("; ") || null },
  { key: "failure", label: "failure", job: false, shown: true,
    raw: ({ row }) => (row?.failed ? failedText(row.failure_source) : null) },
  { key: "failed_reason", label: "failure reason", job: false, shown: true, raw: ({ row }) => row?.failed_reason ?? null },
  { key: "failure_source", label: "failure source", job: false, shown: true, raw: ({ row }) => row?.failure_source ?? null },
  { key: "device", label: "device", job: true, shown: true,
    raw: (line, ctx) => {
      const id = clientOf(line);
      return (id && ctx.names[id]) || line.row?.device_name || null;
    } },
  // The make its rows report, as Results groups devices, by its short name; in full in the CSV.
  { key: "device_type", label: "device type", job: true, shown: true, raw: (line, ctx) => deviceTypeOf(line, ctx) || null,
    text: (line, ctx) => {
      const type = deviceTypeOf(line, ctx);
      return ctx.typeNames?.get(type) ?? type;
    } },
  { key: "device_name", label: "device name", job: false, shown: true, raw: ({ row }) => row?.device_name ?? null },
  { key: "device_time", label: "device time", job: false, shown: true, num: true, raw: ({ row }) => row?.device_time_ms ?? null,
    text: ({ row }) => (row && row.device_time_ms !== null ? duration(row.device_time_ms) : "") },
  { key: "swap", label: "max swap", job: false, shown: true, num: true,
    raw: ({ row }) => (row?.observations.max_swap_bytes as Raw | undefined) ?? null,
    text: bytes(({ row }) => row?.observations.max_swap_bytes) },
  { key: "runtime_flags", label: "planned runtime flags", job: true, shown: true, json: true, raw: flags("runtime_flags") },
  { key: "model_flags", label: "planned model flags", job: true, shown: true, json: true, raw: flags("model_flags") },
  { key: "benchmark_flags", label: "planned benchmark flags", job: true, shown: true, json: true, raw: flags("benchmark_flags") },
  stored("requested_runtime_flags", "requested runtime flags", true),
  stored("requested_model_flags", "requested model flags", true),
  stored("requested_benchmark_flags", "requested benchmark flags", true),
  stored("runtime_flags", "reported runtime flags", true),
  stored("model_flags", "reported model flags", true),
  stored("benchmark_flags", "reported benchmark flags", true),
  // The plan cell's descriptors, as stored.
  { key: "model", label: "planned model", job: true, shown: false, json: true,
    raw: ({ job }) => descriptorText(job.planned?.model) || null },
  { key: "runtime", label: "planned runtime", job: true, shown: false, json: true,
    raw: ({ job }) => descriptorText(job.planned?.runtime) || null },
  stored("model_vault_descriptor", "model vault descriptor"),
  { key: "client_id", label: "client", job: true, shown: false, raw: clientOf },
  { key: "host_memory", label: "max resident", job: false, shown: false, num: true,
    raw: ({ row }) => (row?.observations.max_host_bytes as Raw | undefined) ?? null,
    text: bytes(({ row }) => row?.observations.max_host_bytes) },
  { key: "params_total", label: "params (M)", job: false, shown: false, num: true,
    raw: ({ row }) => row?.model_params_total_millions ?? null },
  { key: "params_active", label: "active params (M)", job: false, shown: false, num: true,
    raw: ({ row }) => row?.model_params_active_millions ?? null },
  { key: "client_version", label: "client version", job: false, shown: false, raw: ({ row }) => row?.client_version ?? null },
  { key: "scored_at", label: "scored", job: false, shown: false, raw: ({ row }) => row?.scored_at ?? null },
  { key: "submitted_at", label: "submitted", job: false, shown: false, raw: ({ row }) => row?.submitted_at ?? null },
  // When the device submitted the run: `submitted_at`, except on a re-scored copy, where it is the time of the run that the copy scores again.
  { key: "run_submitted_at", label: "run submitted", job: false, shown: false,
    raw: ({ row }) => row?.run_submitted_at ?? null },
  { key: "rescored_from", label: "recalculated from", job: false, shown: false, raw: ({ row }) => row?.rescored_from ?? null },
  // The version of the scoring service that scored an eval row; empty on every other row.
  { key: "score_runtime_version", label: "scorer version", job: false, shown: false,
    raw: ({ row }) => row?.score_runtime_version ?? null },
  stored("eval_metadata", "eval metadata"),
  stored("client_extras", "client extras"),
  { key: "expires", label: "expires", job: true, shown: false,
    raw: ({ job }) => (job.queue ? expiresText(job.queue, (iso) => iso) : null),
    text: ({ job }) => (job.queue ? expiresText(job.queue, when) : "") },
  { key: "lease", label: "lease", job: true, shown: false,
    raw: ({ job }) => (job.queue?.lease ? leaseText(job.queue.lease, (iso) => iso) : null),
    text: ({ job }) => (job.queue?.lease ? leaseText(job.queue.lease, when) : "") },
  { key: "lapsed_for_plan", label: "lapsed for plan", job: true, shown: false,
    raw: ({ job }) => (job.plan?.lapsed
      ? `expired ${job.plan.lapsed.expires_at}, recorded ${job.plan.lapsed.recorded_at}` : null),
    text: ({ job }) => (job.plan?.lapsed
      ? `expired ${when(job.plan.lapsed.expires_at)}, recorded ${when(job.plan.lapsed.recorded_at)}` : "") },
];

/**
 * The columns Results' Swap opens on: the run's memory first and the long URIs last, so they never push it out of
 * sight. The rest follow, for the reader to pick.
 */
const SWAP_SHOWN = ["job", "state", "swap", "host_memory", "benchmark", "metric", "value", "unit", "failure", "device",
                    "runtime_descriptor", "model_descriptor"];
export const SWAP_COLUMNS: Column[] = [
  ...SWAP_SHOWN.map((key) => ({ ...COLUMNS.find((column) => column.key === key)!, shown: true })),
  ...COLUMNS.filter((column) => !SWAP_SHOWN.includes(column.key)).map((column) => ({ ...column, shown: false })),
];

/** The `device` fields of its make, in the order a device type names them, before the state a run found it in. */
const SPEC = ["chip_model", "ram_bytes", "gpu_model", "gpu_vram_bytes", "npu_model", "npu_vram_bytes", "form_factor",
              "os_name", "os_version", "os_build"];
const specFirst = (a: string, b: string) =>
  (SPEC.indexOf(a) + 1 || SPEC.length + 1) - (SPEC.indexOf(b) + 1 || SPEC.length + 1) || a.localeCompare(b);

/** The `observations` keys a column of `COLUMNS` reads already. */
const READ = new Set(["max_swap_bytes", "max_host_bytes"]);
const DISCOVERED = /^(device|observations)\./;
const scalar = (v: unknown): Raw => (v === undefined ? null : v === null || typeof v !== "object" ? (v as Raw) : canonical(v));

/**
 * `base` with a column for each field of `device`, shown after the device's name, its make first, and each
 * observation no column reads, last, to pick: as the rows report them, sizes in bytes printed as such.
 */
export function columnsOf(lines: readonly Line[], base: readonly Column[] = COLUMNS): Column[] {
  const rows = lines.flatMap(({ row }) => (row ? [row] : []));
  const of = (prefix: "device" | "observations", shown: boolean): Column[] =>
    [...new Set(rows.flatMap((row) => Object.keys(row[prefix])))].filter((key) => !READ.has(key)).sort(specFirst)
      .map((key) => {
        const values = rows.map((row) => row[prefix][key]).filter((v) => v !== undefined && v !== null);
        const num = values.length > 0 && values.every((v) => typeof v === "number");
        return {
          key: `${prefix}.${key}`, label: `${prefix}.${key}`, job: false, shown, num,
          json: values.some((v) => typeof v === "object"),
          raw: ({ row }) => (row ? scalar(row[prefix][key]) : null),
          ...(num && key.endsWith("_bytes") ? { text: bytes(({ row }) => row?.[prefix][key]) } : {}),
        };
      });
  const at = base.findIndex((column) => column.key === "device_name") + 1 || base.length;
  return [...base.slice(0, at), ...of("device", true), ...base.slice(at), ...of("observations", false)];
}

/** How many jobs the table lists, of `total` where filters narrow it, and in how many rows: `40 of 1,661 jobs (40 rows)`. */
export function listedText(jobs: number, rows: number, total: number | null): string {
  const n = (count: number) => count.toLocaleString("en-US");
  const of = total === null ? n(jobs) : `${n(jobs)} of ${n(total)}`;
  return `${of} ${(total ?? jobs) === 1 ? "job" : "jobs"} (${n(rows)} ${rows === 1 ? "row" : "rows"})`;
}

/** A column's text in the table: its own, else its raw value; empty for none. */
export const cellText = (column: Column, line: Line, ctx: Context): string => {
  if (column.text) return column.text(line, ctx);
  const raw = column.raw(line, ctx);
  return raw === null ? "" : String(raw);
};

/**
 * A CSV field, quoted where it holds a comma, a quote or a line break; text led by
 * `=`, `+`, `-`, `@`, a tab or a carriage return gets a `'`, so a spreadsheet reads no formula.
 */
const field = (value: Raw | undefined): string => {
  let text = value === null || value === undefined ? "" : String(value);
  if (typeof value === "string" && /^[=+\-@\t\r]/.test(text)) text = `'${text}`;
  return /[",\r\n]/.test(text) ? `"${text.replaceAll('"', '""')}"` : text;
};

/** `lines` as one CSV file in `columns`, with each column's raw value and label. */
export const linesCsv = (lines: readonly Line[], columns: readonly Column[], ctx: Context): string =>
  [columns.map((column) => field(column.label)),
   ...lines.map((line) => columns.map((column) => field(column.raw(line, ctx))))]
    .map((cells) => cells.join(",")).join("\r\n") + "\r\n";

export const TEXT_KEYS = ["q"] as const;
export const CHOICE_KEYS = [
  "state", "result", "benchmark", "benchmark_type", "device", "model_descriptor", "runtime_descriptor", "model_flags", "runtime_flags",
  "benchmark_flags", "metric", "spread", "issue",
] as const;
export type TextKey = (typeof TEXT_KEYS)[number];
export type ChoiceKey = (typeof CHOICE_KEYS)[number];
/**
 * Each text box's text, and each select's choices, a key repeated per choice in the address; and the column the
 * table is sorted by, `-` first for descending, empty for the plan's order.
 */
export type JobFilters = Record<TextKey, string> & Record<ChoiceKey, string[]> & { sort: string };

/** The descriptor and flag selects, and their labels. */
export const REPORTED: [ChoiceKey, string][] = [
  ["model_descriptor", "model"], ["runtime_descriptor", "runtime"], ["model_flags", "model flags"],
  ["runtime_flags", "runtime flags"], ["benchmark_flags", "benchmark flags"],
];

/** The sort the address sets, empty where it names no column, a column of the rows' own (`columnsOf`) included. */
export function sortOf(query: URLSearchParams): string {
  const key = query.get("sort")?.replace(/^-/, "") ?? "";
  return COLUMNS.some((column) => column.key === key) || DISCOVERED.test(key) ? query.get("sort")! : "";
}

/** The filters the address sets, each empty where it sets none, a choice named twice once. */
export const jobFilters = (query: URLSearchParams): JobFilters => ({
  ...Object.fromEntries(TEXT_KEYS.map((key) => [key, query.get(key)?.trim() ?? ""])) as Record<TextKey, string>,
  ...Object.fromEntries(CHOICE_KEYS.map((key) => [key, [...new Set(query.getAll(key).filter(Boolean))]])) as
    Record<ChoiceKey, string[]>,
  sort: sortOf(query),
});

/** Whether any filter is set. */
export const filtering = (filters: JobFilters): boolean =>
  TEXT_KEYS.some((key) => filters[key]) || CHOICE_KEYS.some((key) => filters[key].length);

/** `filters` with `value` chosen in `key`, or let go of where `on` is false. */
export const toggled = (filters: JobFilters, key: ChoiceKey, value: string, on: boolean): JobFilters =>
  ({ ...filters, [key]: on ? [...filters[key], value] : filters[key].filter((v) => v !== value) });

/** `filters` as address pairs, from job `offset`; `limit` is the page size as the address gave it. */
export const filterPairs = (filters: JobFilters, limit: string, offset = 0): [string, string][] => [
  ...TEXT_KEYS.map((key): [string, string] => [key, filters[key]]),
  ...CHOICE_KEYS.flatMap((key) => filters[key].map((value): [string, string] => [key, value])),
  ["sort", filters.sort], ["limit", limit], ["offset", offset ? String(offset) : ""],
];

/** The Jobs tab under `filters`, from job `offset`. */
export const cellsLink = (base: TabBase, filters: JobFilters, limit: string, offset = 0): string =>
  tabLink(base, "cells", "", filterPairs(filters, limit, offset));

/** The Jobs tab narrowed to `chosen` alone, a text box's text or a select's choices, from its first job. */
export const cellsWith = (base: TabBase,
                          chosen: Partial<Record<TextKey, string> & Record<ChoiceKey, readonly string[]>>): string =>
  cellsLink(base, { ...jobFilters(new URLSearchParams()), ...chosen } as JobFilters, "");

/**
 * The Jobs filters that keep the result rows `drawn`, which a chart's link there carries: their benchmarks and
 * metrics, measured, and their device types, models and runtimes where those are fewer than `all` the measured rows
 * of those benchmarks and metrics have.
 */
export function drawnFilters(drawn: ReadonlySet<ResultRow>, all: readonly ResultRow[]): Partial<Record<ChoiceKey, string[]>> {
  const values = (rows: readonly ResultRow[], read: (row: ResultRow) => string) =>
    [...new Set(rows.map(read).filter(Boolean))];
  const rows = [...drawn];
  const [benchmark, metric] = [values(rows, (row) => row.benchmark_id), values(rows, (row) => row.metric)];
  const among = all.filter((row) => benchmark.includes(row.benchmark_id) && metric.includes(row.metric)
    && outcomeOf({ row }) === "measured");
  const narrowed = (read: (row: ResultRow) => string) => {
    const [some, every] = [values(rows, read), values(among, read)];
    return some.length < every.length ? some : [];
  };
  return {
    benchmark, metric, result: ["measured"], device: narrowed((row) => (row.device_name ? deviceType(row) : "")),
    model_descriptor: narrowed((row) => descriptorText(row.model_descriptor)),
    runtime_descriptor: narrowed((row) => descriptorText(row.runtime_descriptor)),
  };
}

/** The longest address a chart's link to Jobs spells its models in: past it a proxy may refuse the page's reload. */
const LINK_MAX = 6000;

/** The Jobs tab under [`drawnFilters`]; without the models where naming them would run the address past `LINK_MAX`. */
export function drawnLink(base: TabBase, drawn: ReadonlySet<ResultRow>, all: readonly ResultRow[]): string {
  const filters = drawnFilters(drawn, all);
  const link = cellsWith(base, filters);
  return link.length <= LINK_MAX ? link : cellsWith(base, { ...filters, model_descriptor: [] });
}

/** How two raw values order: numbers by size, the rest as text with its numbers by size, none last either way. */
function compareRaw(a: Raw, b: Raw, sign: number): number {
  const none = (v: Raw) => v === null || v === "";
  if (none(a) || none(b)) return Number(none(a)) - Number(none(b));
  if (typeof a === "number" && typeof b === "number") return sign * (a - b);
  return sign * String(a).localeCompare(String(b), "en", { numeric: true });
}

/** `lines` by `sort`'s column of `columns`, ties in the order they came; as they came with no sort. */
export function sortLines<L extends Line>(lines: readonly L[], sort: string, ctx: Context,
                                          columns: readonly Column[] = COLUMNS): L[] {
  const column = columns.find((c) => c.key === sort.replace(/^-/, ""));
  if (!column) return [...lines];
  const sign = sort.startsWith("-") ? -1 : 1;
  return lines.map((line) => ({ line, raw: column.raw(line, ctx) }))
    .sort((a, b) => compareRaw(a.raw, b.raw, sign)).map(({ line }) => line);
}

/** `descriptors`, and what each reads as (`namedAs`). */
const withDescriptorNames = (descriptors: string[], ctx: Context): string[] =>
  [...descriptors, ...descriptors.flatMap((descriptor) => namedAs(descriptor, ctx.names).map(({ name }) => name))];

/**
 * The catalog names of a job's runtimes and models, with their synonyms: the planned ones and those its run reported.
 */
const catalogNameTexts = (job: Listed, ctx: Context): string[] =>
  [job.planned_runtime_sha256, job.planned_model_sha256, ...(job.result.status === "scored"
    ? job.result.rows.flatMap((row) => [row.runtime_descriptor_sha256, row.model_descriptor_sha256]) : [])]
    .flatMap((digest) => nameTexts(nameOf(ctx.descriptorNames, digest)));

/**
 * The search box searches:
 * - the job ID;
 * - stored planned and reported model and runtime descriptors and their vault names;
 * - runtime and model catalog names and synonyms;
 * - planned, requested, and reported runtime flags.
 */
const TEXTS: Record<TextKey, (job: Listed, ctx: Context) => (string | null | undefined)[]> = {
  q: (job, ctx) => [
    job.job_id,
    ...withDescriptorNames([...reported(job, "model_descriptor"), descriptorText(job.planned?.model),
                            ...reported(job, "runtime_descriptor"), descriptorText(job.planned?.runtime)], ctx),
    ...catalogNameTexts(job, ctx),
    descriptorText(job.planned?.runtime_flags), ...reported(job, "runtime_flags"),
    ...reported(job, "requested_runtime_flags"),
  ],
};

/** Each select's values on a line, `""` where it has none. */
const CHOSEN: Record<ChoiceKey, (line: Line, ctx: Context) => string[]> = {
  state: ({ job }) => [job.state],
  result: (line) => [outcomeOf(line)],
  benchmark: (line) => [benchmarkOf(line)],
  benchmark_type: ({ job, row }) => [row?.benchmark_type ?? job.planned?.benchmark_type ?? ""],
  // By type, as Results draws a device: devices alike in make are one choice.
  device: (line, ctx) => [deviceTypeOf(line, ctx)],
  model_descriptor: (line) => [modelOf(line)],
  runtime_descriptor: (line) => [runtimeOf(line)],
  // Flags a run ran without read `null`, which is a choice of its own.
  model_flags: ({ row }) => (row ? [descriptorText(row.model_flags) || "null"] : []),
  // As the job asked for them, whose knobs Results draws a runtime by: what a client reports varies job by job.
  runtime_flags: ({ job, row }) => [knobsText(job.planned?.runtime_flags ?? row?.requested_runtime_flags) || "null"],
  benchmark_flags: ({ row }) => (row ? [descriptorText(row.benchmark_flags) || "null"] : []),
  metric: ({ row }) => [row?.metric ?? ""],
  spread: (line) => [tierOf(spreadOf(line)) ?? ""],
  issue: (line) => issuesOf(line).map((issue) => issue.key),
};

/** Whether `line` passes every filter set: a text box by any part of its text, case aside; a select by any choice. */
export function keeps(line: Line, filters: JobFilters, ctx: Context): boolean {
  const holds = (key: TextKey) => {
    const want = filters[key].toLowerCase();
    return !want || TEXTS[key](line.job, ctx).some((text) => text?.toLowerCase().includes(want));
  };
  return TEXT_KEYS.every(holds)
    && CHOICE_KEYS.every((key) => !filters[key].length || CHOSEN[key](line, ctx).some((v) => filters[key].includes(v)));
}

/** One option of a select, and how many jobs (or result rows, on a row's select) have it. */
export interface JobChoice {
  value: string;
  label: string;
  count?: number;
  searchText?: string;
}

const STATE_LABELS: Record<JobState, string> = {
  finished: "finished", failed: "failed", running: "running", available: "queued", unknown: "unknown",
  cancelled: "cancelled",
};
const OUTCOME_LABELS: Record<Outcome, string> = { measured: "measured", failed: "failed", none: "no result" };
const TIER_LABELS = Object.fromEntries(TIERS.map(([tier, , label]) => [tier, label])) as Record<Tier, string>;

/**
 * Each select's options over all of `lines`, counted: jobs for a job's select, rows for a row's. Then those of
 * `chosen` no line has.
 */
export function jobChoices(lines: readonly Line[], ctx: Context, chosen: Partial<Record<ChoiceKey, readonly string[]>>,
): Record<ChoiceKey, JobChoice[]> {
  // Unordered ones by label, its numbers by size (`_256` before `_1024`), as the other listings' selects order theirs.
  const count = (key: ChoiceKey, perJob: boolean, label: (value: string) => string, order?: readonly string[]) => {
    const seen = new Map<string, Set<unknown>>();
    for (const line of lines) {
      for (const value of CHOSEN[key](line, ctx)) {
        if (!value) continue;
        if (!seen.has(value)) seen.set(value, new Set());
        seen.get(value)!.add(perJob ? line.job : line);
      }
    }
    const found = [...seen].map(([value, of]) => ({ value, label: label(value), count: of.size }));
    return offeredChoices(order ? order.flatMap((value) => found.filter((choice) => choice.value === value))
      : found.sort((a, b) => a.label.localeCompare(b.label, "en", { numeric: true })), chosen[key] ?? [], label);
  };
  const stored = (key: ChoiceKey) => count(key, true, (v) => v);
  // A descriptor with a catalog name reads under that name first. A descriptor that names the vault reads under what
  // it names. The search finds a descriptor by both.
  const described = (key: ChoiceKey, nameOfLine: (line: Line, ctx: Context) => DescriptorDisplay | null,
                     textOf: (line: Line) => string) => {
    const names = new Map(lines.flatMap((line) => {
      const name = nameOfLine(line, ctx);
      return name ? [[textOf(line), name] as const] : [];
    }));
    return count(key, true, (v) => [names.get(v)?.display, namedLabel(v, ctx.names)].filter(Boolean).join(" "))
      .map((choice) => ({ ...choice, searchText: names.get(choice.value)?.synonyms.join("\n") }));
  };
  // A value the address holds may be none the labels know.
  const labelled = (labels: Record<string, string>) => (v: string) => labels[v] ?? v;
  return {
    state: count("state", true, labelled(STATE_LABELS), Object.keys(STATE_LABELS)),
    result: count("result", false, labelled(OUTCOME_LABELS), Object.keys(OUTCOME_LABELS)),
    benchmark: stored("benchmark"),
    benchmark_type: stored("benchmark_type"),
    device: count("device", true, (id) => ctx.names[id] ?? ctx.typeNames?.get(id) ?? id),
    model_descriptor: described("model_descriptor", modelNameOf, modelOf),
    runtime_descriptor: described("runtime_descriptor", runtimeNameOf, runtimeOf),
    model_flags: stored("model_flags"),
    runtime_flags: stored("runtime_flags"),
    benchmark_flags: stored("benchmark_flags"),
    metric: count("metric", false, (v) => v),
    spread: count("spread", false, labelled(TIER_LABELS), TIERS.map(([tier]) => tier)),
    issue: count("issue", false, labelled(ISSUES), Object.keys(ISSUES)),
  };
}

/**
 * `lines` by job, in order: the groups the table draws and pages, a job's cells spanning its lines. A sort by a
 * row's column can part a job's lines, so a job may head more than one group.
 */
export function byJob<J extends Listed>(lines: readonly Line<J>[]): { job: J; lines: Line<J>[] }[] {
  const groups: { job: J; lines: Line<J>[] }[] = [];
  for (const line of lines) {
    const last = groups.at(-1);
    if (last?.job === line.job) last.lines.push(line);
    else groups.push({ job: line.job, lines: [line] });
  }
  return groups;
}
