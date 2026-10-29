import { describe, expect, test } from "vitest";
import { ApiError } from "../src/lib/api";
import { PHRASES, unlocks } from "../src/lib/danger";
import {
  REVISION, boxOver, failedCount, filingFailure, filingName, filingStatus, parentLink, rerunIds, rerunQuestion, rowOffers,
} from "../src/lib/rerun";
import type { JobResult, JobState } from "../src/lib/api-types";

describe("the re-run controls", () => {
  test("name what they file as the classic forms do", () => {
    expect(filingName({ verb: "rerun", body: { answered: "failed" } })).toBe("re-run");
    expect(filingName({ verb: "recalculate", body: { all_evals: true } })).toBe("recalculation");
  });

  test("offer a re-run on an answered row and a recalculation where the read says so", () => {
    const scored = (recalculable: boolean): JobResult => ({ status: "scored", rows: [], rescored_from: null, earlier_runs: [], recalculable });
    expect(rowOffers({ state: "finished", result: scored(true) })).toEqual({ rerun: true, recalculate: true });
    expect(rowOffers({ state: "failed", result: scored(false) })).toEqual({ rerun: true, recalculate: false });
    expect(rowOffers({ state: "failed", result: { status: "unknown" } })).toEqual({ rerun: true, recalculate: false });
    for (const state of ["available", "running", "cancelled"] as const) {
      expect(rowOffers({ state, result: { status: "not_run" } })).toEqual({ rerun: false, recalculate: false });
    }
  });

  test("unlock each danger-zone write only on its own phrase", () => {
    expect(unlocks(" re-run all ", PHRASES.all)).toBe(true);
    expect(unlocks("re-run all", PHRASES.evals)).toBe(false);
    expect(unlocks("Re-run all", PHRASES.all)).toBe(false);
    expect(unlocks("", PHRASES.all)).toBe(false);
    expect(unlocks(" cancel plan ", PHRASES.cancel)).toBe(true);
  });

  test("count the wait in whole seconds", () => {
    expect(filingStatus("re-run", 0)).toBe("Filing the re-run… 0s");
    expect(filingStatus("recalculation", 2600)).toBe("Filing the recalculation… 3s");
  });

  test("say a 4xx refused the filing, and a 5xx or no answer may have come after it", () => {
    expect(filingFailure("re-run", new ApiError(409, "plan p holds no failed job to re-run")))
      .toBe("The re-run was refused: 409: plan p holds no failed job to re-run");
    expect(filingFailure("recalculation", new ApiError(502, "Bad Gateway")))
      .toBe("The server failed (502: Bad Gateway). Reload the campaign to see whether the recalculation was filed.");
    expect(filingFailure("re-run", new TypeError("Failed to fetch")))
      .toBe("No answer from the server (Failed to fetch). Reload the campaign to see whether the re-run was filed.");
  });

  test("open a re-run's or a recalculation's parent on its plan's Jobs tab, searched for its id", () => {
    expect(parentLink(null, "plan-1", "job-a")).toBe("/app/plans/plan-1/cells?q=job-a");
    expect(parentLink("cmp_1", "plan/1", "job b"))
      .toBe("/app/campaigns/cmp_1/plans/plan%2F1/cells?q=job+b");
  });
});

describe("the checked jobs a re-run takes", () => {
  const job = (job_id: string, state: JobState) => ({ job: { job_id, state, result: { status: "not_run" } as JobResult } });

  test("are a listing's answered jobs, each once however many lines it has", () => {
    expect(rerunIds([job("a", "finished"), job("a", "finished"), job("b", "running"), job("c", "failed"),
                     job("d", "available"), job("e", "cancelled"), job("f", "unknown")])).toEqual(["a", "c"]);
    expect(rerunIds([])).toEqual([]);
  });

  test("the box over them is checked with every one checked, mixed with some, and never over none", () => {
    expect(boxOver(["a", "b"], new Set(["a", "b", "gone"]))).toEqual({ checked: true, mixed: false });
    expect(boxOver(["a", "b"], new Set(["a"]))).toEqual({ checked: false, mixed: true });
    // A job checked and then filtered out checks nothing listed.
    expect(boxOver(["a", "b"], new Set(["gone"]))).toEqual({ checked: false, mixed: false });
    expect(boxOver([], new Set(["a"]))).toEqual({ checked: false, mixed: false });
  });

  test("ask by the job's id for one and by their count for more, with the danger zone's phrase for every one", () => {
    expect(rerunQuestion(["job-a"], 1))
      .toEqual({ title: "Re-run job-a?", action: "Re-run", text: `This job runs again on a device. ${REVISION}` });
    expect(rerunQuestion(["job-a", "job-b"], 3)).toEqual({
      title: "Re-run 2 jobs?", action: "Re-run 2 jobs",
      text: `Each of the 2 checked jobs runs again on a device. ${REVISION}`,
    });
    expect(rerunQuestion(["job-a", "job-b", "job-c"], 3).phrase).toBe(PHRASES.all);
  });
});

describe("the failed jobs a re-run takes", () => {
  const jobs = (...states: JobState[]) => states.map((state) => ({ state }));

  test("are counted where every answered job is settled", () => {
    expect(failedCount(jobs("failed", "finished", "failed", "available"), "ok", "active")).toBe(2);
    expect(failedCount(jobs("finished"), "ok", "active")).toBe(0);
    // Without pipette-duckdb the states are read from the records.
    expect(failedCount(jobs("failed", "cancelled"), "off", "complete")).toBe(1);
  });

  test("are not counted where a recorded failure may read otherwise", () => {
    expect(failedCount(jobs("finished", "unknown"), "ok", "active")).toBeNull();
    expect(failedCount(jobs("finished", "cancelled"), "ok", "complete")).toBeNull();
    expect(failedCount(jobs("failed"), "timed_out", "active")).toBeNull();
    expect(failedCount(jobs("failed"), "failed", "active")).toBeNull();
  });
});
