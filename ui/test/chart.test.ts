import { describe, expect, test } from "vitest";
import {
  axis, barAxis, barPath, channel, exactText, labelled, markPath, median, niceTicks, scale, tickText, tipPlace,
  valueText, withUnit, xScale,
} from "../src/lib/chart";

describe("the chart arithmetic", () => {
  test("a scale maps its domain linearly, and an empty one onto its start", () => {
    const x = scale(0, 100, 74, 882);
    expect(x(0)).toBe(74);
    expect(x(50)).toBe(478);
    expect(x(100)).toBe(882);
    expect(scale(5, 5, 10, 20)(7)).toBe(10);
  });

  test("ticks fall at 1, 2, 2.5 or 5 times a power of ten", () => {
    expect(niceTicks(0, 100, 4)).toEqual({ step: 25, ticks: [0, 25, 50, 75, 100] });
    expect(niceTicks(0, 865, 4)).toEqual({ step: 250, ticks: [0, 250, 500, 750] });
    expect(niceTicks(0.1, 0.9, 4)).toEqual({ step: 0.2, ticks: [0.2, 0.4, 0.6, 0.8] });
    expect(niceTicks(-12, 7, 4).ticks).toEqual([-10, -5, 0, 5]);
    expect(niceTicks(3, 3)).toEqual({ step: 1, ticks: [3] });
    // A span below its values' precision returns, without repeating a tick.
    expect(niceTicks(1, 1 + 2 * Number.EPSILON).ticks).toEqual([1]);
  });

  test("an axis ends on the tick either side of the data and keeps zero, a band and a reference in range", () => {
    expect(axis([100, 200])).toEqual({ lo: 100, hi: 200, step: 20, ticks: [100, 120, 140, 160, 180, 200] });
    // No value is past the last tick: 2 450 reads under 2 500, not over 2 000.
    expect(axis([1240, 2450], { zero: true })).toEqual({ lo: 0, hi: 2500, step: 500, ticks: [0, 500, 1000, 1500, 2000, 2500] });
    expect(axis([-2210, 380], { zero: true }).ticks).toEqual([-3000, -2000, -1000, 0, 1000]);

    // Wider than the band, so small values leave it a strip.
    const banded = axis([1, 2], { band: 5 });
    expect(banded.lo).toBeLessThanOrEqual(-7.5);
    expect(banded.hi).toBeGreaterThanOrEqual(7.5);
    expect(banded.ticks).toEqual([-10, -5, 0, 5, 10]);

    const speedup = axis([1.4, 2.2], { include: [1] });
    expect(speedup.lo).toBe(1);

    // Gaps are skipped; no data draws 0…1; one value is widened, not divided by zero.
    expect(axis([null, 4, null, 8]).ticks).toEqual([4, 5, 6, 7, 8]);
    expect(axis([]).ticks.length).toBeGreaterThan(1);
    const flat = axis([50, 50]);
    expect(flat.hi).toBeGreaterThan(flat.lo);

    // Speedups a rounding off 1× are as flat: the axis returns, its ticks distinct.
    for (const v of [1 + Number.EPSILON, 1 + 4 * Number.EPSILON, 1 + 1e-12]) {
      const { ticks } = axis([v], { include: [1] });
      expect(ticks.length).toBeGreaterThan(1);
      expect(new Set(ticks).size).toBe(ticks.length);
    }
  });

  test("a bar axis runs from 0, to 100 at least for a percentage", () => {
    expect(barAxis([42, 88], 100)).toMatchObject({ lo: 0, hi: 100, ticks: [0, 25, 50, 75, 100] });
    expect(barAxis([120], 100).hi).toBe(120);
    expect(barAxis([3.2]).hi).toBe(3.2);
    expect(barAxis([]).hi).toBe(1);
  });

  test("a count prints whole, or with the decimals a median of counts needs", () => {
    expect([exactText(64), exactText(2088), exactText(64.5), exactText(null)]).toEqual(["64", "2088", "64.5", "—"]);
  });

  test("ticks print in full at the step's precision, scaled only from a million", () => {
    expect(tickText(0, 250)).toBe("0");
    expect(tickText(750, 250)).toBe("750");
    expect(tickText(2.5, 0.5)).toBe("2.5");
    expect(tickText(0.375, 0.125)).toBe("0.375");
    expect(tickText(1500, 500)).toBe("1,500");
    expect(tickText(20000, 5000)).toBe("20,000");
    expect(tickText(-5000, 5000)).toBe("-5,000");
    expect(tickText(7500, 2500)).toBe("7,500");
    expect(tickText(2_500_000, 500_000)).toBe("2.5M");
    expect(tickText(3_000_000, 1_000_000)).toBe("3M");
    expect(tickText(2e10, 5e9)).toBe("20G");
  });

  test("x runs linearly from 0, so each step of a doubling ladder is twice the one before", () => {
    const ladder = xScale([256, 512, 1024, 2048, 4096, 8192], 0, 100);
    expect(ladder(0)).toBe(0);
    expect(ladder(8192)).toBeLessThan(100);
    expect(ladder(1024) - ladder(512)).toBeCloseTo(2 * (ladder(512) - ladder(256)));
    expect(ladder(8192) - ladder(4096)).toBeCloseTo(16 * (ladder(512) - ladder(256)));
    // With no scale to show, a lone rung sits mid-axis.
    expect(xScale([8192], 0, 100)(8192)).toBe(50);
    expect(xScale([4, 4], 0, 100)(4)).toBe(50);
    expect(xScale([], 0, 100)(5)).toBe(50);
  });

  test("an x label crowding the last one drawn is left out, the first always drawn", () => {
    expect(labelled([11.8, 23.5, 47, 94, 188, 376])).toEqual([true, false, true, true, true, true]);
    expect(labelled([0, 13.9, 14])).toEqual([true, false, true]);
    expect(labelled([])).toEqual([]);
  });

  test("a title keeps its unit unless it already names it", () => {
    expect(withUnit("decode speedup", "×")).toBe("decode speedup (×)");
    expect(withUnit("Extra E2E time as % of expected total", "%")).toBe("Extra E2E time as % of expected total");
    expect(withUnit("e2e_ms", "ms")).toBe("e2e_ms (ms)");
  });

  test("a value prints whole from 10 000, else with decimals by size", () => {
    expect(valueText(123456.7)).toBe("123457");
    expect(valueText(137.65)).toBe("137.7");
    expect(valueText(98.1)).toBe("98.10");
    expect(valueText(0.4417)).toBe("0.442");
    expect(valueText(null)).toBe("—");
    expect(valueText(Number.NaN)).toBe("—");
  });

  test("the median skips gaps and averages the middle pair", () => {
    expect(median([3, 1, 2])).toBe(2);
    expect(median([4, 1, 3, 2])).toBe(2.5);
    expect(median([null, 5, Number.NaN])).toBe(5);
    expect(median([])).toBeNull();
  });

  test("a slot takes the eight hues before the next shape, and both repeat past 48", () => {
    expect(channel(0)).toEqual({ hue: 1, shape: 0 });
    expect(channel(7)).toEqual({ hue: 8, shape: 0 });
    expect(channel(8)).toEqual({ hue: 1, shape: 1 });
    expect(channel(47)).toEqual({ hue: 8, shape: 5 });
    expect(channel(48)).toEqual(channel(0));
  });

  test("every marker shape and a bar draw as a closed path", () => {
    for (let shape = 0; shape < 6; shape++) expect(markPath(shape, 10, 10, 4)).toMatch(/^M.*Z$/);
    expect(barPath(280, 30, 120, 14, 4)).toMatch(/^M280 30H.*Z$/);
    expect(barPath(280, 30, 0, 14, 4)).toBe("M280 30");
  });

  test("a tooltip sits below right of the pointer, and flips where the viewport ends", () => {
    expect(tipPlace(100, 100, 200, 50, 1440, 900)).toEqual({ left: 114, top: 114 });
    expect(tipPlace(1300, 880, 200, 50, 1440, 900)).toEqual({ left: 1086, top: 816 });
    // Wider than either side: kept on screen at the left edge.
    expect(tipPlace(100, 100, 1500, 50, 1440, 900).left).toBe(8);
  });
});
