import { describe, expect, test } from "vitest";
import { offsetPager, offsetPaging, unsized } from "../src/lib/pager";
import { latest } from "../src/lib/components/guard";

describe("pager", () => {
  test("a counted listing goes either way and says where it is", () => {
    expect(offsetPager(0, 50, 50, 120)).toEqual({ previous: null, next: 50, range: "1–50 of 120" });
    expect(offsetPager(100, 20, 50, 120)).toEqual({ previous: 50, next: null, range: "101–120 of 120" });
  });

  test("a filtered listing goes on while there are more, and counts nothing", () => {
    expect(offsetPager(50, 50, 50, null, true)).toEqual({ previous: 0, next: 100, range: "51–100" });
    expect(offsetPager(30, 10, 50, null)).toEqual({ previous: 0, next: null, range: "31–40" });
  });

  test("pages by the address, 50 rows unless asked, never past 200", () => {
    expect(offsetPaging(new URLSearchParams(""))).toEqual({ offset: 0, limit: 50, asked: "" });
    expect(offsetPaging(new URLSearchParams("offset=50&limit=10"))).toEqual({ offset: 50, limit: 10, asked: "10" });
    expect(offsetPaging(new URLSearchParams("offset=-3&limit=900")).limit).toBe(200);
    expect(offsetPaging(new URLSearchParams("offset=x&limit=0"))).toEqual({ offset: 0, limit: 50, asked: "0" });
  });

  test("a sized listing's way back to the default size keeps its filters, from the first page", () => {
    expect(unsized("/app/plans", new URLSearchParams("status=running&limit=500&cursor=plan_9"), 400))
      .toBe("/app/plans?status=running");
    // `status=` (every status) is a filter of its own on the device listings.
    expect(unsized("/app/clients", new URLSearchParams("status=&limit=0"), 400)).toBe("/app/clients?status=");
    expect(unsized("/app/vault", new URLSearchParams("limit=abc"), 400)).toBe("/app/vault");
  });

  test("a listing at the default size has no way to offer", () => {
    expect(unsized("/app/plans", new URLSearchParams("status=running"), 400)).toBeNull();
    expect(unsized("/app/plans", new URLSearchParams("limit="), 400)).toBeNull();
  });

  test("a failure other than a refused request does not blame the size", () => {
    const sized = new URLSearchParams("limit=500");
    expect(unsized("/app/plans", sized, 403)).toBeNull();
    expect(unsized("/app/plans", sized, 500)).toBeNull();
    // No answer at all: the network failed.
    expect(unsized("/app/plans", sized, null)).toBeNull();
  });

  test("past the end there is nothing to count, and the way back stays", () => {
    expect(offsetPager(200, 0, 50, 120)).toEqual({ previous: 150, next: null, range: null });
    expect(offsetPager(0, 0, 50, 0)).toEqual({ previous: null, next: null, range: null });
  });
});

describe("stale loads", () => {
  test("only the latest start may draw", () => {
    const start = latest();
    const first = start();
    expect(first()).toBe(true);
    const second = start();
    expect(first()).toBe(false);
    expect(second()).toBe(true);
  });
});
