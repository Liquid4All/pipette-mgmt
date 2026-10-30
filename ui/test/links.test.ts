import { describe, expect, test } from "vitest";
import { match } from "../src/lib/routes";

/** Every source file of the app, by its path from this directory. */
const SOURCES = import.meta.glob<string>("../src/**/*", { query: "?raw", import: "default", eager: true });

describe("the app's links", () => {
  test("no app source links to the retired classic console", () => {
    expect(Object.keys(SOURCES)).toContain("../src/App.svelte");
    const written: string[] = [];
    for (const [name, source] of Object.entries(SOURCES)) {
      source.split("\n").forEach((line, i) => {
        if (/["'`(]\/console\b/.test(line)) written.push(`${name}:${i + 1}: ${line.trim()}`);
      });
    }
    expect(written).toEqual([]);
  });

  // A link is written as a literal or a `routes.ts` helper; either way it has to name a page.
  test("every app address written in its sources names a page the app draws", () => {
    const unmatched: string[] = [];
    let seen = 0;
    for (const [name, source] of Object.entries(SOURCES)) {
      source.split("\n").forEach((line, i) => {
        if (/^\s*(\/\/|\*|\/\*)/.test(line)) return;
        for (const [, written] of line.matchAll(/["'`](\/app\/[^"'`\s]*)/g)) {
          // A template's `${…}` after a `/` is a segment, anywhere else a suffix such as a query.
          const path = written!.split(/[?#]/)[0]!.replace(/(\/?)\$\{[^}]*\}/g, (_, slash) => slash ? "/x" : "");
          if (path.startsWith("/app/assets/")) continue;
          seen++;
          if (match(path).page === "missing") unmatched.push(`${name}:${i + 1}: ${written}`);
        }
      });
    }
    expect(seen).toBeGreaterThan(20);
    expect(unmatched).toEqual([]);
  });
});
