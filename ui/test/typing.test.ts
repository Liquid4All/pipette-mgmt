import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import { TYPING_MS, Typing } from "../src/lib/typing";

describe("a text filter's delay", () => {
  beforeEach(() => { vi.useFakeTimers(); });
  afterEach(() => { vi.useRealTimers(); });

  test("applies once the typing rests, with what was typed last", () => {
    const typing = new Typing();
    const applied: string[] = [];
    for (const value of ["l", "lf", "lfm"]) {
      typing.later("q", () => applied.push(value));
      vi.advanceTimersByTime(TYPING_MS - 1);
    }
    expect(applied).toEqual([]);
    vi.advanceTimersByTime(1);
    expect(applied).toEqual(["lfm"]);
  });

  test("each box waits on its own", () => {
    const typing = new Typing();
    const applied: string[] = [];
    typing.later("id", () => applied.push("id"));
    vi.advanceTimersByTime(100);
    typing.later("q", () => applied.push("q"));
    vi.advanceTimersByTime(TYPING_MS - 100);
    expect(applied).toEqual(["id"]);
    vi.advanceTimersByTime(100);
    expect(applied).toEqual(["id", "q"]);
  });

  test("Enter applies at once and only once; a page that is gone applies nothing", () => {
    const typing = new Typing();
    const applied: string[] = [];
    typing.later("q", () => applied.push("q"));
    typing.flush("q");
    typing.flush("q");
    vi.advanceTimersByTime(TYPING_MS);
    expect(applied).toEqual(["q"]);
    typing.later("q", () => applied.push("late"));
    typing.clear();
    vi.advanceTimersByTime(TYPING_MS);
    expect(applied).toEqual(["q"]);
  });
});
