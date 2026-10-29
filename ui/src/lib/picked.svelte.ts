// The columns a reader picks for a table, kept in this browser under the table's own key. A column that the reader
// toggles keeps that choice. Every other column stays at the default of the table. Two tables can share a key and have
// different columns. Each table then draws a column that only it has at the default of that column.
import { pageStorage } from "./legacy";
import { picksOf, type Picks } from "./table";

/**
 * The picks kept at `key`. A stored list of keys picks exactly those keys among `columns`. A list does not record the
 * columns that the reader saw. A table that reads a list thus hides each column that the list does not name, also a
 * column that the table which saved the list did not have.
 */
function stored(key: string, columns: readonly { key: string }[]): Picks | null {
  try {
    const value: unknown = JSON.parse(pageStorage()?.getItem(key) ?? "null");
    if (Array.isArray(value)) return picksOf(columns, value.filter((k): k is string => typeof k === "string"));
    if (typeof value !== "object" || value === null) return null;
    const picks = Object.fromEntries(Object.entries(value).filter((entry): entry is [string, boolean] =>
      typeof entry[1] === "boolean"));
    return Object.keys(picks).length ? picks : null;
  } catch {
    return null;
  }
}

export class PickedColumns {
  /** The columns that the reader toggled. `true` draws a column and `false` hides it. `null` if the reader toggled none. */
  picks = $state<Picks | null>(null);

  constructor(private readonly storageKey: string, columns: readonly { key: string }[]) {
    this.picks = stored(storageKey, columns);
  }

  /** Draw `key` if `on` is true, or hide it if `on` is false. Every other column keeps its pick or its default. */
  pick(key: string, on: boolean) {
    this.picks = { ...this.picks, [key]: on };
    pageStorage()?.setItem(this.storageKey, JSON.stringify(this.picks));
  }

  reset() {
    this.picks = null;
    pageStorage()?.removeItem(this.storageKey);
  }
}
