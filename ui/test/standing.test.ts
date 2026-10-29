import { describe, expect, test } from "vitest";
import { starvedLine } from "../src/lib/plans";
import { counted, expiryText, starvedNow } from "../src/lib/standing";
import type { JobState, PlanView, ProgressSnapshot, StarvedGroup } from "../src/lib/api-types";

const NOW = Date.parse("2026-10-08T01:00:00Z");
const plan = (over: Partial<PlanView> = {}) => ({
  status: "active", cancel_requested: false, created_at: "2026-10-07T19:48:00Z", terminal_at: null,
  progress_snapshot: null, ...over,
}) as PlanView;
const snapshot = (computed_at: string, starved_groups: StarvedGroup[] = []): ProgressSnapshot => ({
  computed_at, starved_groups, counts: { total: 6, finished: 1, running: 0, available: 4, failed: 1, cancelled: 0 },
});

describe("the count's freshness", () => {
  test("is due only while the plan runs, and stale past ten minutes before it was read", () => {
    for (const status of ["creating", "complete", "cancelled"] as const) expect(counted(plan({ status }), NOW)).toBeNull();
    expect(counted(plan({ progress_snapshot: snapshot("2026-10-08T00:58:00Z") }), NOW))
      .toEqual({ at: "2026-10-08T00:58:00Z", stale: false });
    expect(counted(plan({ progress_snapshot: snapshot("2026-10-08T00:45:00Z") }), NOW)?.stale).toBe(true);
    expect(counted(plan({ progress_snapshot: snapshot("2026-10-08T00:50:00Z") }), NOW)?.stale).toBe(false);
    expect(counted(plan({ created_at: "2026-10-08T00:59:00Z" }), NOW)).toEqual({ at: null, stale: false });
    expect(counted(plan({ created_at: "2026-10-08T00:30:00Z" }), NOW)).toEqual({ at: null, stale: true });
    expect(counted(plan({ status: "cancelling" }), NOW)).not.toBeNull();
  });
});

describe("the starved jobs", () => {
  const group: StarvedGroup = { job_ids: ["j1", "j2", "j3", "j4", "j5", "j6"], requires: ["os:macos"] };
  const jobs = (states: JobState[]) => states.map((state, i) => ({ job_id: `j${i + 1}`, state }));
  const waiting = plan({ status: "pending_clients", progress_snapshot: snapshot("2026-10-08T00:58:00Z", [group]) });

  test("are those of the last count still queued now, a group with none left dropped", () => {
    const now = starvedNow(waiting, jobs(["finished", "failed", "available", "available", "available", "available"]));
    expect(now.map((g) => g.job_ids)).toEqual([["j3", "j4", "j5", "j6"]]);
    expect(starvedLine(now[0]!, "full")).toBe("4 job(s) requiring os:macos");
    expect(starvedLine(now[0]!, "withheld")).toBe("4 job(s) requirement set withheld");
    expect(starvedNow(waiting, null)).toEqual([group]);
    expect(starvedNow(waiting, jobs(["finished", "finished", "running", "failed", "cancelled", "finished"]))).toEqual([]);
  });

  test("are none where the plan no longer waits on a device", () => {
    expect(starvedNow({ ...waiting, status: "active", cancel_requested: true }, null)).toEqual([]);
    for (const status of ["cancelling", "cancelled", "complete"] as const) expect(starvedNow({ ...waiting, status }, null)).toEqual([]);
  });
});

describe("the jobs' expiry", () => {
  const range = (earliest_expires_at: string | null, latest_expires_at: string | null) => ({ earliest_expires_at, latest_expires_at });
  test("is one time, a range, or never, the exact ends its title", () => {
    expect(expiryText(range("2026-11-06T00:00:00Z", "2026-11-06T00:00:00Z"), NOW))
      .toEqual({ text: "in 28 d", title: "2026-11-06 00:00:00Z" });
    expect(expiryText(range("2026-10-10T01:00:00Z", "2026-10-14T01:00:00Z"), NOW))
      .toEqual({ text: "in 2 d – in 6 d", title: "2026-10-10 01:00:00Z → 2026-10-14 01:00:00Z" });
    expect(expiryText(range(null, null), NOW)).toEqual({ text: "never", title: "never" });
    expect(expiryText(range("2026-10-07T22:00:00Z", null), NOW).text).toBe("3 h ago – never");
    expect(expiryText(range("2026-11-06T00:00:00Z", "2026-11-06T00:59:00Z"), NOW))
      .toEqual({ text: "in 28 d", title: "2026-11-06 00:00:00Z → 2026-11-06 00:59:00Z" });
  });
});
