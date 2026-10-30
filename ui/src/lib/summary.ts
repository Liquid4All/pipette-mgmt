// The Overview's summary of a plan's results: how each device type's run went,
// how many of its lines each issue marks, and which scorer versions scored it.

import { duration, textOrder } from "./format";
import { ISSUES, issuesOf, type IssueKey, type Line } from "./jobs";
import { deviceType } from "./report";
import { compareRaw } from "./table";
import type { ResultRow } from "./api-types";

/** A job no device's make can be told for: one the server failed, or whose device reported no other row. */
export const NO_DEVICE = "no device";

/** One device type's run, as its result rows report it. */
export interface DeviceTypeRun {
  type: string;
  devices: number;
  jobs: number;
  /** What its devices spent, a job's run counted once; null where no job reports any. */
  device_time_ms: number | null;
  /** The jobs that report a device time: fewer than `jobs`, the time is theirs alone. */
  timed: number;
}

/**
 * Each answered job's device type, by job id: its first row's make. A failed row reports no make, so its job takes the
 * type its device's other rows report, `no device` where none does.
 */
export function jobTypes(rows: readonly ResultRow[]): Map<string, string> {
  const byClient = new Map<string, string>();
  for (const row of rows) {
    if (row.device_name && row.client_id && !byClient.has(row.client_id)) byClient.set(row.client_id, deviceType(row));
  }
  const typeOf = (row: ResultRow) =>
    (row.device_name ? deviceType(row) : (row.client_id && byClient.get(row.client_id)) || NO_DEVICE);
  const out = new Map<string, string>();
  for (const row of rows) if (!out.has(row.job_id)) out.set(row.job_id, typeOf(row));
  return out;
}

/** The jobs that ran on device type `type` (`jobTypes`): a job with no row yet ran on none. */
export const jobsOn = <J extends { job_id: string }>(jobs: readonly J[], types: ReadonlyMap<string, string>,
                                                     type: string): J[] =>
  jobs.filter((job) => types.get(job.job_id) === type);

/** Each device type's run, in type order, `no device` last, each job under its type (`types`, as `jobTypes` reads). */
export function deviceTypeRuns(rows: readonly ResultRow[], types = jobTypes(rows)): DeviceTypeRun[] {
  interface Job { type: string; client: string | null; time: number | null }
  const jobs = new Map<string, Job>();
  for (const row of rows) {
    const job = jobs.get(row.job_id) ?? { type: types.get(row.job_id) ?? NO_DEVICE, client: row.client_id, time: null };
    jobs.set(row.job_id, job);
    job.time ??= row.device_time_ms;
  }
  const runs = new Map<string, DeviceTypeRun & { clients: Set<string> }>();
  for (const job of jobs.values()) {
    const run = runs.get(job.type)
      ?? { type: job.type, devices: 0, jobs: 0, device_time_ms: null, timed: 0, clients: new Set<string>() };
    runs.set(job.type, run);
    if (job.client && job.type !== NO_DEVICE) run.clients.add(job.client);
    run.jobs++;
    if (job.time !== null) {
      run.device_time_ms = (run.device_time_ms ?? 0) + job.time;
      run.timed++;
    }
  }
  return [...runs.values()].map(({ clients, ...run }) => ({ ...run, devices: clients.size }))
    .sort((a, b) => Number(a.type === NO_DEVICE) - Number(b.type === NO_DEVICE) || textOrder(a.type, b.type));
}

/** Every type's run as one, a device counted once (`deviceCount`). */
export function allRuns(runs: readonly DeviceTypeRun[], rows: readonly ResultRow[]): Omit<DeviceTypeRun, "type"> {
  return runs.reduce((sum, run) => ({
    ...sum, jobs: sum.jobs + run.jobs, timed: sum.timed + run.timed,
    device_time_ms: run.device_time_ms === null ? sum.device_time_ms : (sum.device_time_ms ?? 0) + run.device_time_ms,
  }), { devices: deviceCount(rows), jobs: 0, timed: 0, device_time_ms: null as number | null });
}

/**
 * What a device type's run, or every type's, ran on, under the Progress bar: `3 devices · device time 1h 14m (21 of 312
 * jobs)`; the time only where a job reports one, and how many did where some did not.
 */
export function devicesText(run: Omit<DeviceTypeRun, "type">): string {
  const devices = `${run.devices} ${run.devices === 1 ? "device" : "devices"}`;
  if (run.device_time_ms === null) return devices;
  const some = run.timed < run.jobs ? ` (${run.timed} of ${run.jobs} jobs)` : "";
  return `${devices} · device time ${duration(run.device_time_ms)}${some}`;
}

/** The devices over every type: one that ran on two, say before and after an OS update, counts once. */
export const deviceCount = (rows: readonly ResultRow[]): number =>
  new Set(rows.flatMap((row) => (row.device_name && row.client_id ? [row.client_id] : []))).size;

/**
 * Each scorer version that the rows carry, with the number of jobs that it scored, as the plan header lists them:
 * `1.2.3 ×40, 1.3.0 ×2`. A job counts once for each version. The versions sort in numeric order, so `1.10.0` follows
 * `1.9.0`. A row without a version counts toward none, and only an eval row has a version. Empty if no row has one.
 */
export function scorersText(rows: readonly ResultRow[]): string {
  const jobs = new Map<string, Set<string>>();
  for (const row of rows) {
    if (!row.score_runtime_version) continue;
    const scored = jobs.get(row.score_runtime_version) ?? new Set<string>();
    jobs.set(row.score_runtime_version, scored);
    scored.add(row.job_id);
  }
  return [...jobs].sort(([a], [b]) => compareRaw(a, b, 1))
    .map(([version, scored]) => `${version} ×${scored.size}`).join(", ");
}

/** How many lines, and jobs, an issue marks: what the Jobs tab keeps under that `issues` choice. */
export interface IssueCount {
  key: IssueKey;
  label: string;
  lines: number;
  jobs: number;
}

/** Every issue's count over `lines`, in the filter's order. */
export function issueCounts(lines: readonly Line[]): IssueCount[] {
  const marked = new Map<IssueKey, { lines: number; jobs: Set<string> }>();
  for (const line of lines) {
    for (const { key } of issuesOf(line)) {
      const count = marked.get(key) ?? { lines: 0, jobs: new Set<string>() };
      marked.set(key, count);
      count.lines++;
      count.jobs.add(line.job.job_id);
    }
  }
  return (Object.keys(ISSUES) as IssueKey[]).map((key) =>
    ({ key, label: ISSUES[key], lines: marked.get(key)?.lines ?? 0, jobs: marked.get(key)?.jobs.size ?? 0 }));
}
