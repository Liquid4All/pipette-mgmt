// A plan's cells on its Overview: each job's request
// (`GET /api/v1/plans/{id}/jobs?all=true`), the ids of one model, runtime, type and
// flags collected into a row, grouped by model, then runtime, then each flag
// group, then benchmark type, drawn as one table whose cells span the rows of
// their group, the values of a level that run alike listed in one cell. Each row
// counts its jobs by state and, once read, what their results came to; the
// header totals the table.

import { nameOf } from "./descriptor-names";
import { doneOf } from "./format";
import { parameterValue } from "./plans";
import { canonical, causeText, knobs } from "./report";
import type { PlanJobsWithResults } from "./api";
import type {
  BenchmarkFlags, DescriptorDisplay, DocumentState, FleetDetail, JobState, ModelFlags, PlanCell, PlanJob, RuntimeFlags,
} from "./api-types";

/** A flag group, and the table column that draws it. */
export type FlagKind = "runtime" | "model" | "benchmark";
const KINDS: FlagKind[] = ["runtime", "model", "benchmark"];
const FIELD = { runtime: "runtime_flags", model: "model_flags", benchmark: "benchmark_flags" } as const;
type Flags = RuntimeFlags | ModelFlags | BenchmarkFlags;

/** Where a cell's bottom edge falls: on its model's last row, the table's, or neither. */
export type Edge = "model" | "table" | null;

/** A cell drawn on the first row of its group, spanning the group's rows. */
export interface Spanned<T> {
  value: T;
  span: number;
  end: Edge;
}

/** How many of a row's jobs stand in each state, and in all. */
export type JobCounts = Partial<Record<JobState, number>> & { total: number };

/** What a row's jobs came to once their results are read. */
export interface ResultCounts {
  /** Jobs with a measured row and no failed one, as the Reports count them. */
  measured: number;
  /** Why the failed jobs did, as their rows say: each cause worded, first seen first, drawn under the jobs' count. */
  causes: [string, number][];
  /** Finished jobs whose rows pipette-duckdb has not published yet. */
  pending: number;
}

/** What a row counts of a job; a job read without its result counts no result. */
type Counted = Pick<PlanJob, "planned" | "state">
  & Partial<Pick<PlanJob, "result" | "planned_runtime_sha256" | "planned_model_sha256">>;

/** `jobs` by state, and by result where `fleet` says the results were read. */
function tally(jobs: readonly Counted[], fleet: FleetDetail | null): { jobs: JobCounts; results: ResultCounts | null } {
  const counts: JobCounts = { total: 0 };
  const results: ResultCounts = { measured: 0, causes: [], pending: 0 };
  const causes = new Map<string, number>();
  for (const { state, result } of jobs) {
    counts.total++;
    counts[state] = (counts[state] ?? 0) + 1;
    if (!fleet) continue;
    if (result?.status !== "scored") {
      if (state === "finished") results.pending++;
      continue;
    }
    const failed = result.rows.find((row) => row.failed);
    if (failed) {
      // A job the queue still holds keeps an earlier run's rows: its cause would explain no failed count.
      if (state !== "failed") continue;
      const cause = causeText(failed.failure_source, fleet);
      causes.set(cause, (causes.get(cause) ?? 0) + 1);
    } else if (result.rows.some((row) => Number.isFinite(row.value))) {
      results.measured++;
    }
  }
  results.causes = [...causes];
  return { jobs: counts, results: fleet ? results : null };
}

/** One row of the table: a benchmark type and its ids, under the cells that group them. */
export interface GroupedRow {
  key: string;
  /** Each grouping cell with every value it merges, or null on a row a cell above spans. */
  /** The descriptors as stored. */
  model: Spanned<string[]> | null;
  runtime: Spanned<string[]> | null;
  flags: Record<FlagKind, Spanned<[string, string][][]> | null>;
  benchmark_type: string | null;
  benchmark_ids: string[];
  /** The jobs whose request falls in the row, merged values' included. */
  jobs: JobCounts;
  /** What those jobs' results came to; null where the results were not read. */
  results: ResultCounts | null;
  end: Edge;
}

const isTable = (v: unknown): v is Record<string, unknown> => typeof v === "object" && v !== null && !Array.isArray(v);
const isScalar = (v: unknown) => typeof v === "string" || typeof v === "number" || typeof v === "boolean";

/**
 * A list of scalars a line each, but an argv option keeps its values on its
 * line: `--cache-ram 0`, while env names stand alone.
 */
function argvLines(list: readonly unknown[]): string {
  const out: string[] = [];
  for (const item of list.map(String)) {
    const last = out.length - 1;
    if (last >= 0 && !item.startsWith("-") && out[last]!.startsWith("-")) out[last] += ` ${item}`;
    else out.push(item);
  }
  return out.join("\n");
}

/**
 * A flag group's knobs as `[key, text]` pairs: a nested table's keys dotted
 * (`readiness.skip_thermal`), a list of scalars (argv, env names) as its
 * `argvLines`, anything else as a parameter reads.
 */
export function flagEntries(flags: Flags | null): [string, string][] {
  const entries = (table: Record<string, unknown>, prefix: string): [string, string][] =>
    Object.entries(table).flatMap(([key, value]): [string, string][] => {
      const name = `${prefix}${key}`;
      if (isTable(value) && Object.keys(value).length) return entries(value, `${name}.`);
      if (Array.isArray(value) && value.length && value.every(isScalar)) return [[name, argvLines(value)]];
      return [[name, parameterValue(value)]];
    });
  const set = knobs(flags);
  return set ? entries(set, "") : [];
}

/** The flag groups any of `cells` sets, runtime first: the columns the table draws. */
export const flagKinds = (cells: readonly PlanCell[]): FlagKind[] =>
  KINDS.filter((kind) => cells.some((cell) => flagEntries(cell[FIELD[kind]]).length));

/** `map`'s value at `key`, made by `make` where it has none. */
function at<K, V>(map: Map<K, V>, key: K, make: () => V): V {
  let value = map.get(key);
  if (value === undefined) map.set(key, (value = make()));
  return value;
}

/** The cells of one model, runtime, benchmark type and flags: what the table draws a row for. */
type Leaf = Omit<PlanCell, "benchmark"> & {
  /** In the order the matrix first lists each, so leaves with the same ids list them alike. */
  benchmark_ids: string[];
};

/** `cells` collected into leaves, first-seen, each id once: an entry setting no knob joins the cell with none. */
function leavesOf(cells: readonly PlanCell[]): Leaf[] {
  const rank = new Map<string, number>();
  const byKey = new Map<string, Leaf>();
  for (const { benchmark, ...rest } of cells) {
    at(rank, benchmark, () => rank.size);
    const key = canonical([rest.model, rest.runtime, rest.benchmark_type, ...KINDS.map((kind) => knobs(rest[FIELD[kind]]))]);
    const leaf = at(byKey, key, () => ({ ...rest, benchmark_ids: [] }));
    if (!leaf.benchmark_ids.includes(benchmark)) leaf.benchmark_ids.push(benchmark);
  }
  // A variant edit re-lists a model's missing ids in its own order; one order lets alike leaves merge.
  for (const leaf of byKey.values()) leaf.benchmark_ids.sort((a, b) => rank.get(a)! - rank.get(b)!);
  return [...byKey.values()];
}

/** What a level groups: a leaf, or a cell placed under one. */
type Grouped = Omit<PlanCell, "benchmark" | "benchmark_type">;

/** What each level groups by, outermost first; equal values compare equal whatever their key order. */
const LEVELS: ((leaf: Grouped) => string)[] = [
  (leaf) => canonical(leaf.model), (leaf) => canonical(leaf.runtime),
  ...KINDS.map((kind) => (leaf: Grouped) => canonical(knobs(leaf[FIELD[kind]]))),
];

/** `keys` as one key, whatever order they came in. */
const setKey = (keys: string[]) => JSON.stringify([...keys].sort());

/** The leaves under a path of levels: the next level's branches, or past the last level the leaves themselves. */
interface Tree {
  branches: Branch[];
  leaves: Leaf[];
  /** Equal for trees that draw alike, whatever order their parts came in. */
  key: string;
}

/** The values of one level whose trees below are alike, merged into one cell; a row for each, first-seen. */
interface Branch {
  members: Leaf[];
  /** Each member's value at the level, as it groups by. */
  keys: string[];
  below: Tree;
}

/** `leaves` from level `depth` down, each level's values that have the same tree below merged into one branch. */
function tree(leaves: readonly Leaf[], depth: number): Tree {
  const level = LEVELS[depth];
  if (!level) {
    return { branches: [], leaves: [...leaves], key: setKey(leaves.map((l) => canonical([l.benchmark_type, l.benchmark_ids]))) };
  }
  const byValue = new Map<string, Leaf[]>();
  for (const leaf of leaves) at(byValue, level(leaf), () => []).push(leaf);
  const merged = new Map<string, Branch>();
  for (const [value, group] of byValue) {
    const below = tree(group, depth + 1);
    const branch = at(merged, below.key, () => ({ members: [], below, keys: [] }));
    branch.members.push(group[0]!);
    branch.keys.push(value);
  }
  const branches = [...merged.values()];
  return { branches, leaves: [], key: setKey(branches.map((b) => `${setKey(b.keys)} ${b.below.key}`)) };
}

/** A table row for each leaf, with the branch each level starts on it and the rows that branch spans. */
function lines(t: Tree): { starts: ({ branch: Branch; span: number } | null)[]; leaf: Leaf }[] {
  if (!t.branches.length) return t.leaves.map((leaf) => ({ starts: [], leaf }));
  return t.branches.flatMap((branch) => {
    const inner = lines(branch.below);
    return inner.map(({ starts, leaf }, i) => ({ starts: [i ? null : { branch, span: inner.length }, ...starts], leaf }));
  });
}

/**
 * The table: model, then runtime, runtime flags, model flags and benchmark
 * flags, a row per benchmark type with its ids. Values of a level whose rows
 * below are alike share one cell, so what differs is what splits the table.
 */
export function grouped(cells: readonly PlanCell[], jobs: readonly Counted[] = [],
                        fleet: FleetDetail | null = null): GroupedRow[] {
  const drawn = lines(tree(leavesOf(cells), 0));
  const placed = jobs.flatMap((job) =>
    job.planned ? [{ job, planned: job.planned, keys: LEVELS.map((level) => level(job.planned!)) }] : []);
  /** The branch each level is in at the row drawn, which a row a cell above spans inherits. */
  const branches: Branch[] = [];
  const last = drawn.length - 1;
  let modelEnd = 0;
  return drawn.map(({ starts, leaf }, i) => {
    starts.forEach((start, level) => { if (start) branches[level] = start.branch; });
    const merged = branches.map((branch) => new Set(branch.keys));
    const mine = placed.filter(({ planned, keys }) =>
      planned.benchmark_type === leaf.benchmark_type && leaf.benchmark_ids.includes(planned.benchmark)
      && keys.every((key, level) => merged[level]!.has(key)));
    modelEnd = starts[0] ? i + starts[0].span - 1 : modelEnd;
    const edge = (end: number): Edge => (end === last ? "table" : end === modelEnd ? "model" : null);
    const cell = <T>(level: number, value: (leaf: Leaf) => T): Spanned<T[]> | null => {
      const start = starts[level];
      return start ? { value: start.branch.members.map(value), span: start.span, end: edge(i + start.span - 1) } : null;
    };
    return {
      key: String(i),
      model: cell(0, (r) => canonical(r.model)),
      runtime: cell(1, (r) => canonical(r.runtime)),
      flags: {
        runtime: cell(2, (r) => flagEntries(r.runtime_flags)),
        model: cell(3, (r) => flagEntries(r.model_flags)),
        benchmark: cell(4, (r) => flagEntries(r.benchmark_flags)),
      },
      benchmark_type: leaf.benchmark_type,
      benchmark_ids: leaf.benchmark_ids,
      ...tally(mine.map(({ job }) => job), fleet),
      end: edge(i),
    };
  });
}

/** A row's ids as its benchmark cell lists them, under its type: the type's `_` prefix dropped where all share it. */
export function idsUnder(type: string | null, ids: readonly string[]): string[] {
  const prefix = type ? `${type}_` : "";
  return prefix && ids.every((id) => id.startsWith(prefix) && id.length > prefix.length)
    ? ids.map((id) => id.slice(prefix.length)) : [...ids];
}

/**
 * A row's ids as its benchmark cell labels them: `idsUnder`'s, then the longest
 * `_`-ended prefix every one of several shares, drawn once as `shared`.
 */
export function idLabels(type: string | null, ids: readonly string[]): { shared: string; labels: string[] } {
  const under = idsUnder(type, ids);
  if (under.length < 2) return { shared: "", labels: under };
  let shared = under[0]!.slice(0, under[0]!.lastIndexOf("_") + 1);
  while (shared && !under.every((id) => id.startsWith(shared) && id.length > shared.length)) {
    shared = shared.slice(0, shared.slice(0, -1).lastIndexOf("_") + 1);
  }
  return { shared, labels: shared ? under.map((id) => id.slice(shared.length)) : under };
}

/** The table's header: each column's distinct values, and every row's jobs and results summed. */
export interface Totals {
  models: number;
  runtimes: number;
  types: number;
  benchmarks: number;
  jobs: JobCounts;
  results: ResultCounts | null;
}

function totalsOf(cells: readonly PlanCell[], jobs: readonly Counted[], fleet: FleetDetail | null): Totals {
  const distinct = (values: readonly unknown[]) => new Set(values).size;
  return {
    models: distinct(cells.map((c) => canonical(c.model))), runtimes: distinct(cells.map((c) => canonical(c.runtime))),
    types: distinct(cells.flatMap((c) => c.benchmark_type ?? [])), benchmarks: distinct(cells.map((c) => c.benchmark)),
    ...tally(jobs.filter((job) => job.planned), fleet),
  };
}

/** The states a row's jobs are drawn in, in the order their bar fills. */
const STATES = ["finished", "failed", "running", "available", "unknown", "cancelled"] as const;

/**
 * A row's jobs as its cell reads them, a line each: how many are done of all, the failed among them, then each
 * other state that has any. A part's `n` is its own state's jobs, which the bar draws.
 */
export function jobParts(counts: JobCounts): { state: JobState; n: number; text: string }[] {
  return STATES.flatMap((state): { state: JobState; n: number; text: string }[] => {
    const n = counts[state] ?? 0;
    if (state === "finished") return [{ state, n, text: `${doneOf(counts)}/${counts.total} done` }];
    return n ? [{ state, n, text: `${n} ${state === "available" ? "queued" : state}` }] : [];
  });
}

/** Per flag column, its distinct sets, and the `key=text` knobs they all share; none shared under one set. */
function flagSets(rows: readonly GroupedRow[]): { sets: Record<FlagKind, number>; shared: Record<FlagKind, Set<string>> } {
  const sets = { runtime: 0, model: 0, benchmark: 0 };
  const shared = { runtime: new Set<string>(), model: new Set<string>(), benchmark: new Set<string>() };
  for (const kind of KINDS) {
    const all = rows.flatMap((row) => row.flags[kind]?.value ?? []).map((set) => set.map(([k, v]) => `${k}=${v}`));
    sets[kind] = new Set(all.map((set) => JSON.stringify(set))).size;
    if (sets[kind] > 1) shared[kind] = new Set(all[0]!.filter((pair) => all.every((set) => set.includes(pair))));
  }
  return { sets, shared };
}

/** Whether `kind`'s column draws `[key, text]` quieter, every set in it sharing the knob. */
export const sharedKnob = (shared: Record<FlagKind, Set<string>>, kind: FlagKind, [key, text]: [string, string]) =>
  shared[kind].has(`${key}=${text}`);

/** One `key: value` label of a flag set; an argv list gives one an option. */
export interface KnobLabel {
  key: string;
  value: string;
  /** Every set of the column shares the knob, so it tells no row apart. */
  shared: boolean;
}

/** A flag set's labels, the knobs that tell its rows apart first, those every set shares after. */
export function knobLabels(entries: readonly [string, string][], kind: FlagKind,
                           shared: Record<FlagKind, Set<string>>): KnobLabel[] {
  const labels = entries.flatMap((entry) => {
    const quiet = sharedKnob(shared, kind, entry);
    return entry[1].split("\n").map((value) => ({ key: entry[0], value, shared: quiet }));
  });
  return [...labels.filter((label) => !label.shared), ...labels.filter((label) => label.shared)];
}

/** A plan's cells as the Overview draws them; a class, so a Loader's `$state` does not proxy its rows. */
export class Cells {
  /** What the store holds of the plan's document, which the Overview's facts say. */
  readonly document: DocumentState;
  /** Null where there is nothing to draw: no job names a spec this server reads. */
  readonly rows: GroupedRow[] | null;
  /** The flag columns the table draws. */
  readonly kinds: FlagKind[];
  readonly totals: Totals;
  /** Per flag column, its distinct sets, and the knobs they all share (`flagSets`). */
  readonly sets: Record<FlagKind, number>;
  readonly shared: Record<FlagKind, Set<string>>;
  /** Jobs with no request, which no row counts. */
  readonly unplaced: number;
  /** The read's names: a vault resource's id to its record's name. */
  readonly names: Record<string, string>;
  /** The catalog names of each planned runtime, by the runtime as stored, as the read gives them for the reader. */
  readonly runtimeNames: ReadonlyMap<string, DescriptorDisplay>;
  /** The catalog names of each planned model, by the model as stored, as the read gives them for the reader. */
  readonly modelNames: ReadonlyMap<string, DescriptorDisplay>;
  /** The digest of each planned runtime, by the runtime as stored. `/by-digest` reads the names by these digests. */
  readonly runtimeDigests: ReadonlyMap<string, string>;
  /** The digest of each planned model, by the model as stored. `/by-digest` reads the names by these digests. */
  readonly modelDigests: ReadonlyMap<string, string>;

  constructor({ document_state, jobs, results_read, fleet_detail, names, descriptor_names }:
    Pick<PlanJobsWithResults, "document_state" | "fleet_detail">
    & { jobs: readonly Counted[]; results_read?: PlanJobsWithResults["results_read"];
        names?: PlanJobsWithResults["names"]; descriptor_names?: PlanJobsWithResults["descriptor_names"] }) {
    this.document = document_state;
    this.names = names ?? {};
    this.runtimeNames = new Map(jobs.flatMap(({ planned, planned_runtime_sha256 }) => {
      const name = nameOf(descriptor_names, planned_runtime_sha256);
      return planned && name ? [[canonical(planned.runtime), name] as const] : [];
    }));
    this.modelNames = new Map(jobs.flatMap(({ planned, planned_model_sha256 }) => {
      const name = nameOf(descriptor_names, planned_model_sha256);
      return planned && name ? [[canonical(planned.model), name] as const] : [];
    }));
    this.runtimeDigests = new Map(jobs.flatMap(({ planned, planned_runtime_sha256 }) =>
      (planned && planned_runtime_sha256 ? [[canonical(planned.runtime), planned_runtime_sha256] as const] : [])));
    this.modelDigests = new Map(jobs.flatMap(({ planned, planned_model_sha256 }) =>
      (planned && planned_model_sha256 ? [[canonical(planned.model), planned_model_sha256] as const] : [])));
    const cells = jobs.flatMap(({ planned }) => (planned ? [planned] : []));
    const fleet = results_read === "ok" ? fleet_detail : null;
    this.rows = cells.length ? grouped(cells, jobs, fleet) : null;
    this.kinds = flagKinds(cells);
    this.totals = totalsOf(cells, jobs, fleet);
    ({ sets: this.sets, shared: this.shared } = flagSets(this.rows ?? []));
    this.unplaced = jobs.length - cells.length;
  }
}
