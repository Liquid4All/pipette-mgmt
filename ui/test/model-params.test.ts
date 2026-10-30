import { describe, expect, test } from "vitest";
import { counts, noted } from "../src/lib/model-params";

describe("model params", () => {
  test("an empty active count is left to the server, a dense model's", () => {
    expect(counts(4512, null)).toEqual({ total_millions: 4512 });
    expect(counts(4512, undefined)).toEqual({ total_millions: 4512 });
    expect(counts(8340, 1500)).toEqual({ total_millions: 8340, active_millions: 1500 });
  });

  test("a blank note is left out, another trimmed", () => {
    expect(noted("  ")).toEqual({});
    expect(noted(" MoE: card states 1.5B active ")).toEqual({ note: "MoE: card states 1.5B active" });
  });
});
