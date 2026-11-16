import { describe, expect, test } from "vitest";
import { spreadOf, swapped } from "../src/lib/jobs";
import { ttftOver } from "../src/lib/report";
import type { ResultRow } from "../src/lib/api-types";
import cases from "./policy-checks.json";

// The campaign group policies run these checks on the server (`crates/pipette-pipeline/src/group_policies.rs`), which
// reads the same fixture, so a job is flagged there exactly where the console flags it. `failed` is the server's alone.
const flagged = (rows: ResultRow[], percent: number): string[] => [
  ...(rows.some((row) => swapped({ row })) ? ["swap"] : []),
  ...(rows.some((row) => {
    const spread = spreadOf({ row });
    return spread !== null && spread > percent;
  }) ? ["spread"] : []),
  ...(ttftOver(rows).size ? ["ttft_over_e2e"] : []),
];

describe("the campaign group policy checks", () => {
  // The rows hold only the fields the checks read.
  test.each(cases)("$name", ({ rows, percent, flagged: expected }) => {
    expect(flagged(rows as unknown as ResultRow[], percent)).toEqual(expected);
  });
});
