import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import { ColumnWidths, DEFAULT_WIDTH, MIN_WIDTH, storedWidths } from "../src/lib/widths.svelte";

describe("the column widths kept in this browser", () => {
  test.each<[string, string | null, Record<string, number>]>([
    ["none, before any is kept", null, {}],
    ["none from text that is not JSON", "{", {}],
    ["none from a list", "[200]", {}],
    ["none from a number", "200", {}],
    ["each width kept", '{"name":200,"owner":96}', { name: 200, owner: 96 }],
    ["the narrowest a column goes", `{"name":${MIN_WIDTH}}`, { name: MIN_WIDTH }],
    ["a width under the narrowest dropped", `{"name":${MIN_WIDTH - 1},"owner":96}`, { owner: 96 }],
    ["a value that is not a number dropped", '{"name":"200","owner":null,"status":96}', { status: 96 }],
  ])("%s", (_, text, widths) => {
    expect(storedWidths(text)).toEqual(widths);
  });

  describe("a table's widths", () => {
    let store: Map<string, string>;
    beforeEach(() => {
      store = new Map();
      vi.stubGlobal("localStorage", {
        getItem: (key: string) => store.get(key) ?? null,
        setItem: (key: string, value: string) => store.set(key, value),
        removeItem: (key: string) => store.delete(key),
      });
    });
    afterEach(() => vi.unstubAllGlobals());

    test("draw the table's own layout until a width is set", () => {
      const widths = new ColumnWidths("t.widths");
      expect(widths.sized).toBe(false);
      expect(widths.grip("name", () => ({})).width).toBeUndefined();
    });

    test("a first drag keeps every measured width, and a column with none takes the default", () => {
      const widths = new ColumnWidths("t.widths");
      widths.grip("name", () => ({ name: 210.4, owner: 30, "#action": 90 })).start();
      expect(widths.widths).toEqual({ name: 210, owner: MIN_WIDTH, "#action": 90 });
      expect(widths.sized).toBe(true);
      expect(widths.of("status")).toBe(DEFAULT_WIDTH);
    });

    test("a later measure leaves the widths already set", () => {
      const widths = new ColumnWidths("t.widths");
      widths.set("name", 300);
      widths.fill({ name: 120, owner: 80 });
      expect(widths.widths).toEqual({ name: 300, owner: 80 });
    });

    test("a drag sets its column's width, no narrower than the narrowest, and is kept once it ends", () => {
      const widths = new ColumnWidths("t.widths");
      const grip = widths.grip("name", () => ({ name: 200 }));
      grip.start();
      grip.to(10);
      expect(widths.widths.name).toBe(MIN_WIDTH);
      expect(store.has("t.widths")).toBe(false);
      grip.to(250);
      grip.done();
      expect(new ColumnWidths("t.widths").widths).toEqual({ name: 250 });
    });

    test("the default widths let go of those kept", () => {
      store.set("t.widths", '{"name":250}');
      const widths = new ColumnWidths("t.widths");
      widths.reset();
      expect(widths.sized).toBe(false);
      expect(store.has("t.widths")).toBe(false);
    });
  });
});
