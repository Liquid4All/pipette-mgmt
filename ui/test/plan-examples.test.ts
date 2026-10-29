import { describe, expect, test } from "vitest";
import { PLAN_EXAMPLES, TITLES } from "../src/lib/plan-examples";

const FILES = import.meta.glob<string>("../src/lib/plan-examples/*.toml", { query: "?raw", import: "default", eager: true });
const stems = Object.keys(FILES).map((path) => path.replace(/^.*\/(.+)\.toml$/, "$1"));

describe("the plan examples", () => {
  test("name every file, and only files there are", () => {
    expect(Object.keys(TITLES).sort()).toEqual(stems.sort());
    expect(PLAN_EXAMPLES.map((example) => example.id)).toEqual(Object.keys(TITLES));
  });

  test.each(PLAN_EXAMPLES.map((example) => [example.id, example.text]))(
    "%s reads as a document to file as it is", (_, text) => {
      expect(text.endsWith("\n")).toBe(true);
      expect(text).toMatch(/^\[\[variants\]\]$/m);
      expect(text).toMatch(/^requires = /m);
      // Nothing to replace before filing, and no roster of one fleet's devices.
      expect(text).not.toMatch(/^\s*#/m);
      expect(text).not.toMatch(/clients|REPLACE|example-org/);
    });
});
