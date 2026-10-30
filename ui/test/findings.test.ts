import { describe, expect, test } from "vitest";
import { e2eText, pairingText, type E2eFinding, type Pairing } from "../src/lib/findings";

describe("the E2E check's words", () => {
  const WHAT = "model, device, runtime and prefill length";
  const WITHIN = "Measured end-to-end latency is within ±5 % of prefill + decode time for";
  const NOT = "Measured end-to-end latency is not within ±5 % of prefill + decode time for";
  const UNESTIMABLE = "a rate or time ≤ 0, or no decode_tokens";
  const NONE = { e2e: 0, decode: 0, prefill: 0 };
  /** `n` combinations, all checked and within, unless `over` says otherwise. */
  const checked = (n: number, over: Partial<Extract<E2eFinding, { kind: "checked" }>> = {}): string => e2eText({
    kind: "checked", vl: false, total: n, complete: n, unestimated: 0, missing: NONE, batched: false, within: n,
    slower: null, faster: null, ...over,
  });

  test("not offered, it says why", () => {
    expect(e2eText({ kind: "withheld" })).toBe("Not offered: device names are withheld at your access.");
    expect(e2eText({ kind: "lacking", types: ["decode throughput", "prefill throughput"] }))
      .toBe("Not offered: the plan has no decode throughput or prefill throughput results.");
  });

  test("all within, the one, and batch size where it varies", () => {
    expect(checked(3)).toBe(`${WITHIN} all 3 combinations of ${WHAT}.`);
    expect(checked(1)).toBe(`${WITHIN} the one combination of ${WHAT}.`);
    expect(checked(2, { batched: true }))
      .toBe(`${WITHIN} all 2 combinations of model, device, runtime, prefill length and batch size.`);
  });

  test("the rest by direction, `up to` the farthest of several", () => {
    expect(checked(4, { within: 1, slower: { n: 2, max: 6.4 }, faster: { n: 1, max: 13.2 } }))
      .toBe(`${WITHIN} 1 of 4 combinations of ${WHAT}; 2 ran slower, by up to 6.4 %, and 1 faster, by 13.2 %.`);
    expect(checked(2, { within: 1, faster: { n: 1, max: 8 } }))
      .toBe(`${WITHIN} 1 of 2 combinations of ${WHAT}; 1 ran faster, by 8.0 %.`);
    expect(checked(2, { within: 0, slower: { n: 1, max: 10 }, faster: { n: 1, max: 20 } }))
      .toBe(`${WITHIN} none of the 2 combinations of ${WHAT}; 1 ran slower, by 10.0 %, and 1 faster, by 20.0 %.`);
    expect(checked(1, { within: 0, slower: { n: 1, max: 7.1 } }))
      .toBe(`${NOT} the one combination of ${WHAT}: it ran slower, by 7.1 %.`);
    expect(checked(1, { within: 0, faster: { n: 1, max: 7.1 } }))
      .toBe(`${NOT} the one combination of ${WHAT}: it ran faster, by 7.1 %.`);
  });

  test("what could not be checked follows the finding", () => {
    expect(checked(3, { complete: 2, within: 2, missing: { e2e: 0, decode: 1, prefill: 1 } }))
      .toBe(`${WITHIN} all 2 combinations of ${WHAT} that could be checked. 1 more lacks decode throughput and `
        + "prefill throughput.");
    expect(checked(5, { complete: 3, within: 2, slower: { n: 1, max: 10 }, missing: { e2e: 1, decode: 1, prefill: 1 } }))
      .toBe(`${WITHIN} 2 of 3 combinations of ${WHAT}; 1 ran slower, by 10.0 %. 2 more lack a measurement: `
        + "end-to-end latency in 1, decode throughput in 1, prefill throughput in 1.");
    expect(checked(4, { unestimated: 2, within: 2 })).toBe(`${WITHIN} all 2 combinations of ${WHAT} that could be `
      + `checked. 2 more have all three but no estimate (${UNESTIMABLE}).`);
    expect(checked(2, { unestimated: 1, within: 1 })).toBe(`${WITHIN} the one combination of ${WHAT} that could be `
      + `checked. 1 more has all three but no estimate (${UNESTIMABLE}).`);
  });

  test("with nothing to check, it says what is missing in place of a finding", () => {
    expect(checked(0)).toBe("No result names a prefill length, so there is nothing to check.");
    expect(checked(1, { complete: 0, within: 0, missing: { e2e: 0, decode: 1, prefill: 1 } }))
      .toBe(`The one combination of ${WHAT} lacks decode throughput and prefill throughput.`);
    expect(checked(3, { complete: 0, within: 0, missing: { e2e: 2, decode: 2, prefill: 2 } }))
      .toBe(`None of the 3 combinations of ${WHAT} has all three measurements: end-to-end latency is missing in 2, `
        + "decode throughput in 2, prefill throughput in 2.");
    expect(checked(2, { unestimated: 2, within: 0 }))
      .toBe(`None of the 2 combinations of ${WHAT} with all three measurements could be estimated (${UNESTIMABLE}).`);
    const alone = `The one combination of ${WHAT} with all three measurements could not be estimated (${UNESTIMABLE}).`;
    expect(checked(1, { unestimated: 1, within: 0 })).toBe(alone);
    expect(checked(2, { complete: 1, unestimated: 1, within: 0, missing: { e2e: 0, decode: 1, prefill: 1 } }))
      .toBe(`${alone} 1 more lacks decode throughput and prefill throughput.`);
  });

  test("the VL check counts jobs, against prompt + decode time, its prefill side the time to first token", () => {
    const vl = (n: number, over: Partial<Extract<E2eFinding, { kind: "checked" }>> = {}) => checked(n, { vl: true, ...over });
    const VL_WITHIN = "Measured end-to-end latency is within ±5 % of prompt + decode time for";
    expect(vl(72)).toBe(`${VL_WITHIN} all 72 VL throughput jobs.`);
    expect(vl(1)).toBe(`${VL_WITHIN} the one VL throughput job.`);
    expect(vl(1, { within: 0, faster: { n: 1, max: 7.1 } })).toBe("Measured end-to-end latency is not within ±5 % of "
      + "prompt + decode time for the one VL throughput job: it ran faster, by 7.1 %.");
    expect(vl(5, { complete: 3, within: 2, slower: { n: 1, max: 10 }, missing: { e2e: 2, decode: 0, prefill: 0 } }))
      .toBe(`${VL_WITHIN} 2 of 3 VL throughput jobs; 1 ran slower, by 10.0 %. 2 more lack a measurement: `
        + "end-to-end latency in 2.");
    expect(vl(1, { complete: 0, within: 0, missing: { e2e: 0, decode: 0, prefill: 1 } }))
      .toBe("The one VL throughput job lacks time to first token.");
    expect(vl(0)).toBe("No VL throughput job has a measurement to check.");
  });
});

describe("the pairing's words", () => {
  const BOTH = "ran speculative and baseline on one device";
  const paired = (paired: number, split: number, one: number): string =>
    pairingText({ kind: "paired", benchmarks: paired + split + one, paired, split, one } satisfies Pairing);

  test("which paired, and why the rest have no speedup", () => {
    expect(paired(3, 1, 1)).toBe(`3 of 5 benchmarks ${BOTH}, so their speedups compare like with like. The other 2 have `
      + "no speedup: 1 ran the two on different devices only, 1 ran only one of them.");
    expect(paired(1, 0, 0)).toBe(`The one benchmark ${BOTH}, so its speedups compare like with like.`);
    expect(paired(2, 0, 0)).toBe(`All 2 benchmarks ${BOTH}, so their speedups compare like with like.`);
    expect(paired(1, 0, 1)).toBe(`1 of 2 benchmarks ${BOTH}, so their speedups compare like with like. `
      + "The other one has no speedup: it ran only one of them.");
    expect(paired(0, 1, 1)).toBe(`None of the 2 benchmarks ${BOTH}, so none has a speedup: 1 ran the two on `
      + "different devices only, 1 ran only one of them.");
    expect(paired(0, 1, 0)).toBe("The one benchmark did not run speculative and baseline on one device, so it has no "
      + "speedup: it ran the two on different devices only.");
    expect(pairingText({ kind: "withheld" })).toBe("Not paired: device names are withheld at your access.");
  });
});
