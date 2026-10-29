// Helpers for a test of the column picks. The app keeps the picks in the local storage of the browser.
import { onTestFinished, vi } from "vitest";
import { PickedColumns } from "../src/lib/picked.svelte";
import { drawnColumns } from "../src/lib/table";

/** Replace `localStorage` with an empty store for the running test. Return the map that holds the stored values. */
export function stubStorage(): Map<string, string> {
  const kept = new Map<string, string>();
  vi.stubGlobal("localStorage", {
    getItem: (key: string) => kept.get(key) ?? null,
    setItem: (key: string, value: string) => void kept.set(key, value),
    removeItem: (key: string) => void kept.delete(key),
  });
  onTestFinished(() => { vi.unstubAllGlobals(); });
  return kept;
}

/** The keys that a table of `columns` draws with the picks kept at `storageKey`. */
export const drawnKeys = (storageKey: string, columns: readonly { key: string; shown?: boolean }[]): string[] =>
  drawnColumns(columns, new PickedColumns(storageKey, columns).picks).map((column) => column.key);
