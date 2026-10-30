// The result checks' findings: the figures `report.ts` counts once per load, and the words Results says
// them in, kept apart so rewording leaves the counting, and its tests, alone.

import { valueText } from "./chart";

/** How far, in %, measured E2E latency may stray from prefill + decode time and still agree. */
export const BAND = 5;
/** Why a combination with all three inputs has no estimate. */
export const UNESTIMABLE = "a rate or time ≤ 0, or no decode_tokens";

/** The combinations that strayed one way, and the farthest, in % to a tenth. */
export interface Spread {
  n: number;
  max: number;
}

/**
 * The E2E check over each combination of model, column, prefill length and batch size, or the VL check over each
 * vl_throughput job; `lacking` names its types.
 */
export type E2eFinding =
  | { kind: "withheld" }
  | { kind: "lacking"; types: string[] }
  | {
    kind: "checked";
    /** The VL check: each a job's own three measurements, its prefill side the time to first token. */
    vl: boolean;
    /** Combinations with any of the three; of them, those with all three; of those, the ones with no estimate. */
    total: number;
    complete: number;
    unestimated: number;
    /** Combinations lacking each. */
    missing: { e2e: number; decode: number; prefill: number };
    /** More than one batch size among them. */
    batched: boolean;
    /** The estimated ones within ±`BAND` %, and the rest by direction; null where none strayed that way. */
    within: number;
    slower: Spread | null;
    faster: Spread | null;
  };

/** How vl_spec_decode's benchmarks paired: both modes on one device, on different devices only, or one mode only. */
export type Pairing =
  | { kind: "withheld" }
  | { kind: "paired"; benchmarks: number; paired: number; split: number; one: number };

/** `a`, `a and b`, `a, b and c`. */
export const listed = (items: string[]): string =>
  (items.length < 2 ? items.join("") : `${items.slice(0, -1).join(", ")} and ${items[items.length - 1]}`);

/** An E2E check's finding, the text one's or VL's, as its Results tab leads with it. */
export function e2eText(f: E2eFinding): string {
  if (f.kind === "withheld") return "Not offered: device names are withheld at your access.";
  if (f.kind === "lacking") return `Not offered: the plan has no ${f.types.join(" or ")} results.`;

  const what = `model, device, runtime${f.batched ? ", prefill length and batch size" : " and prefill length"}`;
  const [one, many, sum] = f.vl ? ["VL throughput job", "VL throughput jobs", "prompt + decode time"]
    : [`combination of ${what}`, `combinations of ${what}`, "prefill + decode time"];
  const lacking = ([["e2e", "end-to-end latency"], ["decode", "decode throughput"],
                    ["prefill", f.vl ? "time to first token" : "prefill throughput"]] as const)
    .filter(([key]) => f.missing[key]).map(([key, name]) => ({ name, n: f.missing[key] }));
  const unchecked = f.total - f.complete;
  const lacks = !unchecked ? ""
    : unchecked === 1 ? `1 more lacks ${listed(lacking.map((l) => l.name))}.`
    : `${unchecked} more lack a measurement: ${lacking.map((l) => `${l.name} in ${l.n}`).join(", ")}.`;
  const tails = (estimable: boolean) => [lacks, estimable && f.unestimated
    ? `${f.unestimated} more ${f.unestimated === 1 ? "has" : "have"} all three but no estimate (${UNESTIMABLE}).` : ""];
  const sentences = (lead: string, estimable = true) => [lead, ...tails(estimable)].filter(Boolean).join(" ");

  if (!f.total) {
    return f.vl ? "No VL throughput job has a measurement to check."
      : "No result names a prefill length, so there is nothing to check.";
  }
  if (!f.complete) {
    if (f.total === 1) return `The one ${one} lacks ${listed(lacking.map((l) => l.name))}.`;
    return `None of the ${f.total} ${many} has all three measurements: `
      + lacking.map((l, i) => `${l.name}${i ? "" : " is missing"} in ${l.n}`).join(", ") + ".";
  }
  const n = f.complete - f.unestimated;
  if (!n) {
    return sentences(f.complete === 1
      ? `The one ${one} with all three measurements could not be estimated (${UNESTIMABLE}).`
      : `None of the ${f.complete} ${many} with all three measurements could be estimated (${UNESTIMABLE}).`, false);
  }

  const by = (s: Spread) => `by ${s.n === 1 ? "" : "up to "}${s.max.toFixed(1)} %`;
  const off = [f.slower ? `${f.slower.n} ran slower, ${by(f.slower)}` : "",
               f.faster ? `${f.faster.n}${f.slower ? "" : " ran"} faster, ${by(f.faster)}` : ""]
    .filter(Boolean).join(", and ");
  const lead = `Measured end-to-end latency is within ±${BAND} % of ${sum} for`;
  const checked = unchecked || f.unestimated ? " that could be checked" : "";
  if (n === 1) {
    const strayed = f.slower ?? f.faster;
    return sentences(!strayed ? `${lead} the one ${one}${checked}.`
      : `Measured end-to-end latency is not within ±${BAND} % of ${sum} for the one ${one}${checked}: it ran `
        + `${f.slower ? "slower" : "faster"}, ${by(strayed)}.`);
  }
  if (f.within === n) return sentences(`${lead} all ${n} ${many}${checked}.`);
  return sentences(`${lead} ${f.within ? `${f.within} of` : "none of the"} ${n} ${many}; ${off}.`);
}

/** How the speculative benchmarks paired, and why the rest have no speedup, as their Results tab leads with it. */
export function pairingText(p: Pairing): string {
  if (p.kind === "withheld") return "Not paired: device names are withheld at your access.";
  const { benchmarks: n, paired, split, one } = p;
  const both = "ran speculative and baseline on one device";
  const like = "so their speedups compare like with like";
  // Why the rest have no speedup: each count, or what a lone one did.
  const why = (lone: boolean) => (lone ? (split ? "it ran the two on different devices only" : "it ran only one of them")
    : [split ? `${split} ran the two on different devices only` : "", one ? `${one} ran only one of them` : ""]
      .filter(Boolean).join(", "));
  if (paired === n) {
    return n === 1 ? `The one benchmark ${both}, so its speedups compare like with like.` : `All ${n} benchmarks ${both}, ${like}.`;
  }
  if (paired) {
    const rest = n - paired;
    return `${paired} of ${n} benchmarks ${both}, ${like}. `
      + (rest === 1 ? `The other one has no speedup: ${why(true)}.` : `The other ${rest} have no speedup: ${why(false)}.`);
  }
  return n === 1 ? `The one benchmark did not run speculative and baseline on one device, so it has no speedup: ${why(true)}.`
    : `None of the ${n} benchmarks ${both}, so none has a speedup: ${why(false)}.`;
}
