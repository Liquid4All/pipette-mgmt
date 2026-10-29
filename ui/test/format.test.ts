import { describe, expect, test } from "vitest";
import type { Counts, ProgressSnapshot } from "../src/lib/api-types";
import { ago, doneOf, duration, progress, progressOf, when } from "../src/lib/format";

describe("format", () => {
  test("a timestamp prints in UTC to the second, or an em-rule when there is none", () => {
    expect(when("2026-10-03T21:04:05.123456Z")).toBe("2026-10-03 21:04:05Z");
    expect(when("2026-10-03T23:04:05+02:00")).toBe("2026-10-03 21:04:05Z");
    expect(when(null)).toBe("—");
    expect(when(undefined)).toBe("—");
    expect(when("not a time")).toBe("not a time");
  });

  test("a duration prints its two largest units, the second one padded", () => {
    expect(duration(0)).toBe("0s");
    expect(duration(999)).toBe("0s");
    expect(duration(42_000)).toBe("42s");
    expect(duration(184_250)).toBe("3m 04s");
    expect(duration(7_500_000)).toBe("2h 05m");
    expect(duration(180_000_000)).toBe("50h 00m");
    expect(duration(null)).toBe("—");
  });

  const snapshot = (counts: Counts): ProgressSnapshot => ({ computed_at: "2026-10-03T21:04:05Z", counts, starved_groups: [] });

  test("progress leaves out the zeros but done, and counts a failed job as done", () => {
    const counts = { total: 12, finished: 3, running: 2, available: 0, failed: 1, cancelled: 0 };
    expect(progress({ progress_snapshot: snapshot(counts) })).toBe("4 done, 1 failed · 2 running (of 12)");
    expect(doneOf(counts)).toBe(4);
    expect(doneOf({})).toBe(0);
    const over = { total: 3, finished: 3, running: 0, available: 4, failed: 0, cancelled: 0 };
    expect(progress({ progress_snapshot: snapshot(over) })).toBe("3 done · 4 queued (of 3)");
  });

  test("a plan nothing counted yet says so", () => {
    expect(progress({ progress_snapshot: null })).toBe("not computed yet");
    expect(progressOf(null)).toBe("not computed yet");
  });

  test("progress reads the counts a narrow plan row carries as a snapshot's", () => {
    const counts = { total: 5, finished: 1, running: 0, available: 3, failed: 0, cancelled: 1 };
    expect(progressOf(counts)).toBe(progress({ progress_snapshot: snapshot(counts) }));
    expect(progressOf(counts)).toBe("1 done · 3 queued · 1 cancelled (of 5)");
  });

});

describe("relative times", () => {
  const NOW = Date.parse("2026-10-08T01:00:00Z");

  test("say how long ago or how soon in one floored unit", () => {
    expect(ago("2026-10-08T00:59:30Z", NOW)).toBe("just now");
    expect(ago("2026-10-08T00:55:00Z", NOW)).toBe("5 min ago");
    expect(ago("2026-10-08T00:00:00Z", NOW)).toBe("1 h ago");
    expect(ago("2026-10-06T02:00:00Z", NOW)).toBe("47 h ago");
    expect(ago("2026-10-06T00:00:00Z", NOW)).toBe("2 d ago");
    expect(ago("2026-10-14T01:00:00Z", NOW)).toBe("in 6 d");
    expect(ago("x", NOW)).toBe("x");
  });
});
