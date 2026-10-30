import { describe, expect, test } from "vitest";
import { linesOf } from "../src/lib/jobs";
import {
  allRuns, deviceCount, deviceTypeRuns, devicesText, issueCounts, jobsOn, jobTypes, scorersText,
} from "../src/lib/summary";
import type { PlanJobWithResult } from "../src/lib/api";
import type { ResultRow } from "../src/lib/api-types";

const PHONE = { chip_model: "Apple A19 Pro", ram_bytes: 12291735552, os_name: "iOS", os_version: "27.0.1", form_factor: "phone" };
const BOX = { chip_model: "Ryzen 9", ram_bytes: 64 * 2 ** 30, os_name: "Linux", form_factor: "desktop" };
const A19 = "Apple A19 Pro · 11.4 GB RAM · iOS 27.0.1 · phone";

const row = (job_id: string, over: Partial<ResultRow> = {}): ResultRow => ({
  job_id, benchmark_id: "bench", benchmark_type: "decode_throughput", metric: "decode_throughput", value: 10,
  value_stddev: null, unit: "tokens/sec", failed: false, failed_reason: null, failure_source: null,
  model_params_total_millions: 1000, model_params_active_millions: 1000, model_descriptor: null, runtime_descriptor: null, runtime_descriptor_sha256: null, model_descriptor_sha256: null,
  model_flags: null, runtime_flags: null, benchmark_flags: null, model_vault_descriptor: null, requested_model_flags: null,
  requested_runtime_flags: null, requested_benchmark_flags: null, eval_metadata: null, client_extras: null, parameters: {},
  observations: {}, client_id: "p-1", device_name: "iPhone 17 Pro", device: PHONE, client_version: null, submitted_at: null,
  scored_at: null, device_time_ms: null, rescored_from: null, run_submitted_at: null,
  score_runtime_version: null, ...over,
});
const failed = (job_id: string, client_id: string | null): ResultRow =>
  row(job_id, { value: null, failed: true, failure_source: "expired", client_id, device_name: null, device: {} });

describe("the Overview's device types", () => {
  test("count each type's devices, jobs and device time, a job's run once, whatever each device is named", () => {
    const runs = deviceTypeRuns([
      row("job-1", { device_time_ms: 4000 }), row("job-1", { metric: "ttft", device_time_ms: 4000 }),
      row("job-2", { client_id: "p-2", device_name: "bos-17pro-1", device_time_ms: 6000 }),
      row("job-3", { client_id: "b-1", device_name: "Box", device: BOX }),
    ]);
    expect(runs).toEqual([
      { type: A19, devices: 2, jobs: 2, device_time_ms: 10000, timed: 2 },
      { type: "Ryzen 9 · 64.0 GB RAM · Linux · desktop", devices: 1, jobs: 1, device_time_ms: null, timed: 0 },
    ]);
  });

  test("time a type by the jobs that report a device time alone, and say how many did", () => {
    const [run] = deviceTypeRuns([row("job-1", { device_time_ms: 4000 }), row("job-2"), row("job-3")]);
    expect([run!.device_time_ms, run!.timed, run!.jobs]).toEqual([4000, 1, 3]);
  });

  test("count a failed job under the type its device's other rows report, and under no device where none does", () => {
    const runs = deviceTypeRuns([row("job-1"), failed("job-2", "p-1"), failed("job-3", "system"), failed("job-4", null)]);
    expect(runs.map((run) => [run.type, run.devices, run.jobs])).toEqual([[A19, 1, 2], ["no device", 0, 2]]);
  });

  test("say what a type, or every type, ran on: its devices, and device time with how many jobs reported one", () => {
    const rows = [row("job-1", { device_time_ms: 60_000 }), row("job-2", { client_id: "p-2" }),
                  row("job-3", { client_id: "b-1", device_name: "Box", device: BOX, device_time_ms: 120_000 })];
    const runs = deviceTypeRuns(rows);
    expect([...runs.map(devicesText), devicesText(allRuns(runs, rows))]).toEqual([
      "2 devices · device time 1m 00s (1 of 2 jobs)", "1 device · device time 2m 00s",
      "3 devices · device time 3m 00s (2 of 3 jobs)",
    ]);
    expect(devicesText(deviceTypeRuns([row("job-1")])[0]!)).toBe("1 device");
  });

  test("type each job by its rows' make, a failed one by its device's other rows, or no device", () => {
    const types = jobTypes([row("job-1"), failed("job-2", "p-1"), failed("job-3", "system")]);
    expect([...types]).toEqual([["job-1", A19], ["job-2", A19], ["job-3", "no device"]]);
  });

  test("narrow the jobs to those that ran on a type, a job with no row yet on none", () => {
    const types = jobTypes([row("job-1"), failed("job-2", "p-1"),
                            row("job-3", { client_id: "b-1", device_name: "Box", device: BOX })]);
    const jobs = ["job-1", "job-2", "job-3", "job-4"].map((job_id) => ({ job_id }));
    expect(jobsOn(jobs, types, A19).map((job) => job.job_id)).toEqual(["job-1", "job-2"]);
  });

  test("count a device once over every type, as one updated mid-plan reports two", () => {
    const updated = { ...PHONE, os_version: "27.1" };
    const rows = [row("job-1"), row("job-2", { device: updated }), row("job-3", { client_id: "p-2" }), failed("job-4", null)];
    expect([deviceTypeRuns(rows).map((run) => run.devices), deviceCount(rows)]).toEqual([[2, 1, 0], 2]);
  });
});

describe("the plan's scorer versions", () => {
  const scored = (job_id: string, version: string | null, metric = "accuracy") =>
    row(job_id, { benchmark_type: "eval", metric, score_runtime_version: version });

  test.each([
    ["no eval row", [row("job-1"), row("job-2")], ""],
    ["a job's rows once", [scored("job-1", "1.2.3"), scored("job-1", "1.2.3", "f1"), scored("job-2", "1.2.3")],
     "1.2.3 ×2"],
    ["no row without a version", [scored("job-1", "1.2.3"), scored("job-2", null), row("job-3")], "1.2.3 ×1"],
    ["the versions in version order", [scored("job-1", "1.10.0"), scored("job-2", "1.9.0"), scored("job-3", "1.9.0")],
     "1.9.0 ×2, 1.10.0 ×1"],
  ])("count %s", (_, rows, text) => {
    expect(scorersText(rows)).toBe(text);
  });
});

describe("the Overview's checks", () => {
  test("count every issue's lines and jobs, in the filter's order, none where no line has it", () => {
    const job = (job_id: string, rows: ResultRow[]) =>
      ({ job_id, state: "finished", result: { status: "scored", rows }, planned: null }) as unknown as PlanJobWithResult;
    const swapped = { observations: { max_swap_bytes: 2 ** 20 } };
    const counts = issueCounts(linesOf([
      job("job-1", [row("job-1", swapped), row("job-1", { ...swapped, metric: "ttft" })]),
      job("job-2", [row("job-2", { model_params_total_millions: null })]), job("job-3", [failed("job-3", "p-1")]),
    ]));
    expect(counts.map((c) => [c.label, c.lines, c.jobs])).toEqual([
      ["failed", 1, 1], ["stddev over 5%", 0, 0], ["swapped", 2, 1], ["ran hot", 0, 0], ["corpus mismatch", 0, 0],
      ["power save or low battery", 0, 0], ["no parameter count", 1, 1], ["beyond the model's audio limit", 0, 0],
    ]);
  });
});
