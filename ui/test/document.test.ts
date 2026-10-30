import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import {
  editorRows, firstError, isDirty, lineRange, readTextFile, sameDocument, validationNote, validationScheduler,
  type Checked,
} from "../src/lib/document";
import type { PlanIssue, PlanValidation } from "../src/lib/api-types";

const issue = (severity: PlanIssue["severity"], line: number | null = null): PlanIssue =>
  ({ severity, message: `${severity} here`, line, column: line === null ? null : 1 });
const verdict = (job_count: number | null, ...issues: PlanIssue[]): PlanValidation =>
  ({ job_count, gated_model_count: 0, issues });

describe("the editor", () => {
  test("grows with the document between 14 and 40 rows", () => {
    expect(editorRows("")).toBe(14);
    expect(editorRows("a\n".repeat(20))).toBe(22);
    expect(editorRows("a\n".repeat(60))).toBe(40);
  });

  test("selects a line by its 1-based number, the last past the end", () => {
    expect(lineRange("a\nbcd\ne", 2)).toEqual([2, 5]);
    expect(lineRange("a\nbcd\ne", 9)).toEqual([6, 7]);
    expect(lineRange("a\nbcd\ne", 1)).toEqual([0, 1]);
    expect(lineRange("a\nbcd\ne", 0)).toEqual([0, 1]);
  });

  test("opens a file only as UTF-8 text, keeping a BOM as the route does", async () => {
    expect(await readTextFile(new Blob(["benchmarks = []\n"]))).toBe("benchmarks = []\n");
    expect(await readTextFile(new Blob([new Uint8Array([0xef, 0xbb, 0xbf, 0x61])]))).toBe("﻿a");
    await expect(readTextFile(new Blob([new Uint8Array([0x61, 0xff, 0x62])])))
      .rejects.toThrow("The chosen file is not UTF-8 text");
    await expect(readTextFile(new Blob([]))).rejects.toThrow("The chosen file is empty");
  });

  test("reads a CRLF document as the LF one a textarea makes of it", () => {
    expect(sameDocument("a\r\nb\r\n", "a\nb\n")).toBe(true);
    expect(sameDocument("a\rb", "a\nb")).toBe(true);
    expect(sameDocument("a\nb", "a\nb\n")).toBe(false);
  });

  test("asks before replacing only what was typed since the last file or example", () => {
    expect(isDirty("", "x")).toBe(false);
    expect(isDirty(" \n", "x")).toBe(false);
    expect(isDirty("x\r\n", "x\n")).toBe(false);
    expect(isDirty("x y", "x")).toBe(true);
  });

  test("finds the first issue that refuses a document", () => {
    expect(firstError(null)).toBeNull();
    expect(firstError(verdict(2, issue("warning")))).toBeNull();
    expect(firstError(verdict(null, issue("warning"), issue("error", 3)))).toEqual(issue("error", 3));
  });
});

describe("the editor's status line", () => {
  const note = (text: string, rest: Partial<Parameters<typeof validationNote>[0]> = {}) =>
    validationNote({ text, verdict: null, error: "", ...rest });

  test("says how to start a blank document, in red once creating it was refused", () => {
    const start = "Paste a plan document, open a file, or start from an example.";
    expect(note(" ")).toEqual({ tone: "muted", text: start });
    expect(note("", { required: true })).toEqual({ tone: "err", text: start });
    expect(note("", { original: "a = 1" })).toEqual({ tone: "err", text: "The document is empty." });
  });

  test("says an edit that changes nothing is unchanged, whatever its line ends", () => {
    expect(note("a = 1\n", { original: "a = 1\r\n", verdict: verdict(1) })).toEqual({ tone: "muted", text: "Unchanged." });
  });

  test("counts the jobs and warnings of a valid document", () => {
    expect(note("x")).toEqual({ tone: "muted", text: "Checking…" });
    expect(note("x", { verdict: verdict(8) })).toEqual({ tone: "ok", text: "Valid · 8 jobs" });
    expect(note("x", { verdict: verdict(1, issue("warning"), issue("warning")) }))
      .toEqual({ tone: "ok", text: "Valid · 1 job · 2 warnings" });
    expect(note("x", { verdict: verdict(5000, issue("warning")) }))
      .toEqual({ tone: "ok", text: "Valid · 5,000 jobs · 1 warning" });
  });

  test("says a refused document is not valid, and what it expands to where it does", () => {
    expect(note("x", { verdict: verdict(null, issue("error", 2)) })).toEqual({ tone: "err", text: "Not valid" });
    expect(note("x", { verdict: verdict(8, issue("error")) }))
      .toEqual({ tone: "err", text: "Not valid · expands to 8 jobs" });
  });

  test("says a check that could not be made, without refusing the document", () => {
    expect(note("x", { error: "HTTP 503", verdict: verdict(8) }))
      .toEqual({ tone: "muted", text: "Couldn't check the document: HTTP 503" });
  });
});

describe("the editor's checks", () => {
  interface Asked { text: string; signal: AbortSignal; resolve: (v: PlanValidation) => void; reject: (e: unknown) => void }

  function harness() {
    const asks: Asked[] = [];
    let state: Checked = { on: null, verdict: null, error: "", pending: false };
    const checks = validationScheduler({
      ask: (text, signal) => new Promise((resolve, reject) => asks.push({ text, signal, resolve, reject })),
      onChange: (next) => (state = next),
    });
    return { checks, asks, state: () => state };
  }

  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  test("ask once typing pauses, for the last text typed", async () => {
    const { checks, asks, state } = harness();
    checks.schedule("a");
    expect(state().pending).toBe(true);
    await vi.advanceTimersByTimeAsync(300);
    checks.schedule("ab");
    await vi.advanceTimersByTimeAsync(499);
    expect(asks).toEqual([]);
    await vi.advanceTimersByTimeAsync(1);
    expect(asks.map((each) => each.text)).toEqual(["ab"]);
    asks[0]!.resolve(verdict(2));
    await vi.advanceTimersByTimeAsync(0);
    expect(state()).toEqual({ on: "ab", verdict: verdict(2), error: "", pending: false });
  });

  test("stop at a blank document, forgetting the last answer", async () => {
    const { checks, asks, state } = harness();
    const first = checks.now("a");
    asks[0]!.resolve(verdict(1));
    await first;
    checks.schedule("b");
    checks.schedule(" \n");
    await vi.advanceTimersByTimeAsync(1000);
    expect(asks).toHaveLength(1);
    expect(state()).toEqual({ on: null, verdict: null, error: "", pending: false });
  });

  test("never ask again for the text last accepted, or one in flight", async () => {
    const { checks, asks } = harness();
    const first = checks.now("a");
    const again = checks.now("a");
    checks.schedule("a");
    expect(asks).toHaveLength(1);
    asks[0]!.resolve(verdict(3));
    expect(await first).toEqual(verdict(3));
    expect(await again).toEqual(verdict(3));
    expect(await checks.now("a")).toEqual(verdict(3));
    checks.schedule("a");
    await vi.advanceTimersByTimeAsync(1000);
    expect(asks).toHaveLength(1);
  });

  test("settle on the text last answered when typing comes back to it", async () => {
    const { checks, asks, state } = harness();
    const first = checks.now("a");
    asks[0]!.resolve(verdict(1));
    await first;
    checks.schedule("ab");
    expect(state().pending).toBe(true);
    checks.schedule("a");
    expect(state().pending).toBe(false);
    await vi.advanceTimersByTimeAsync(1000);
    expect(asks).toHaveLength(1);
  });

  test("ask again on demand for a refused text, not for an accepted one", async () => {
    const { checks, asks } = harness();
    const refused = verdict(null, issue("error", 1));
    const first = checks.now("a");
    asks[0]!.resolve(refused);
    expect(await first).toEqual(refused);
    // The benchmark it lacked was added since.
    const again = checks.now("a");
    expect(asks).toHaveLength(2);
    asks[1]!.resolve(verdict(2));
    expect(await again).toEqual(verdict(2));
    expect(await checks.now("a")).toEqual(verdict(2));
    expect(asks).toHaveLength(2);
  });

  test("ask at once on demand, dropping the wait", async () => {
    const { checks, asks } = harness();
    checks.schedule("a");
    const answer = checks.now("a");
    expect(asks.map((each) => each.text)).toEqual(["a"]);
    await vi.advanceTimersByTimeAsync(1000);
    expect(asks).toHaveLength(1);
    asks[0]!.resolve(verdict(1));
    expect(await answer).toEqual(verdict(1));
  });

  test("abort the request a newer text overtakes, and drop its late answer", async () => {
    const { checks, asks, state } = harness();
    const first = checks.now("a");
    checks.schedule("b");
    expect(asks[0]!.signal.aborted).toBe(true);
    await vi.advanceTimersByTimeAsync(500);
    asks[1]!.resolve(verdict(2));
    await vi.advanceTimersByTimeAsync(0);
    // A server that answers anyway: its caller has the answer, the screen does not.
    asks[0]!.resolve(verdict(1));
    expect(await first).toEqual(verdict(1));
    expect(state()).toMatchObject({ on: "b", verdict: verdict(2), pending: false });
  });

  test("abort the request in flight at a blank document, and drop its late answer", async () => {
    const { checks, asks, state } = harness();
    const first = checks.now("a");
    checks.schedule(" ");
    expect(asks[0]!.signal.aborted).toBe(true);
    asks[0]!.resolve(verdict(1));
    expect(await first).toEqual(verdict(1));
    expect(state()).toEqual({ on: null, verdict: null, error: "", pending: false });
  });

  test("answer null for a check that could not be made, and say why", async () => {
    const { checks, asks, state } = harness();
    const answer = checks.now("a");
    asks[0]!.reject(new Error("HTTP 503"));
    expect(await answer).toBeNull();
    expect(state()).toEqual({ on: "a", verdict: null, error: "HTTP 503", pending: false });
    // Not cached: the next look asks again.
    void checks.now("a");
    expect(asks).toHaveLength(2);
  });

  test("keep what is on screen when stopped", async () => {
    const { checks, asks, state } = harness();
    void checks.now("a");
    asks[0]!.resolve(verdict(1));
    await vi.advanceTimersByTimeAsync(0);
    checks.schedule("b");
    checks.stop();
    await vi.advanceTimersByTimeAsync(1000);
    expect(asks).toHaveLength(1);
    expect(state()).toMatchObject({ on: "a", verdict: verdict(1) });
  });
});
