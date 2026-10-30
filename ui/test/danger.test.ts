import { describe, expect, test } from "vitest";
import { unlocks } from "../src/lib/danger";

describe("a danger zone's confirmation", () => {
  test("unlocks on its phrase alone, spaces around either aside", () => {
    expect(unlocks(" Nightly v2 ", "Nightly v2")).toBe(true);
    expect(unlocks("Nightly v2", "Nightly v2 ")).toBe(true);
    expect(unlocks("nightly v2", "Nightly v2")).toBe(false);
    expect(unlocks("", "Nightly v2")).toBe(false);
  });
});
