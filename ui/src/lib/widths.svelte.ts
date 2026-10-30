// The column widths a reader drags a table's headers to, kept in this browser under the table's own key: the
// table sizes its columns to their content until then.
import { pageStorage } from "./legacy";

/** The narrowest a column goes, wide enough for a short label and its sort arrow. */
export const MIN_WIDTH = 48;

/** The width of a column drawn in a sized table that has no width of its own. */
export const DEFAULT_WIDTH = 160;

/** `width` in whole pixels, at least `MIN_WIDTH`. */
export const clampWidth = (width: number): number => Math.max(MIN_WIDTH, Math.round(width));

/** The widths `text` holds, by column key; a value that is not a width is dropped, and text that is not JSON is none. */
export function storedWidths(text: string | null): Record<string, number> {
  try {
    const value: unknown = JSON.parse(text ?? "null");
    if (typeof value !== "object" || value === null || Array.isArray(value)) return {};
    return Object.fromEntries(Object.entries(value).filter((entry): entry is [string, number] =>
      typeof entry[1] === "number" && Number.isFinite(entry[1]) && entry[1] >= MIN_WIDTH));
  } catch {
    return {};
  }
}

/** The width each header in `row` draws at, by the key in its `data-key`. */
export const headerWidths = (row: HTMLElement | undefined): Record<string, number> =>
  Object.fromEntries([...(row?.querySelectorAll<HTMLElement>("th[data-key]") ?? [])]
    .map((th) => [th.dataset.key ?? "", th.offsetWidth]));

/** How a header's grip sets its column's width. */
export interface Resize {
  /** The width kept, `undefined` while the table sizes the column to its content. */
  width: number | undefined;
  /** Called as a drag starts, so the table can keep the widths it draws. */
  start(): void;
  to(width: number): void;
  /** Called as a drag ends, so the table can save the width. */
  done(): void;
}

export class ColumnWidths {
  /** Each column's width in pixels, by key; empty for the table's own layout. */
  widths = $state<Record<string, number>>({});

  constructor(private readonly storageKey: string) {
    this.widths = storedWidths(pageStorage()?.getItem(storageKey) ?? null);
  }

  /** Whether the reader set any width, so the table draws the widths kept. */
  get sized(): boolean {
    return Object.keys(this.widths).length > 0;
  }

  /** The width `key`'s column draws at in a sized table. */
  of(key: string): number {
    return this.widths[key] ?? DEFAULT_WIDTH;
  }

  /** `key`'s column at `width`, kept once `save` is called. */
  set(key: string, width: number) {
    this.widths = { ...this.widths, [key]: clampWidth(width) };
  }

  /** The `measured` widths of the columns that have none, so a table's first drag starts from its layout. */
  fill(measured: Record<string, number>) {
    const missing = Object.entries(measured).filter(([key]) => !(key in this.widths));
    if (missing.length) {
      this.widths = { ...Object.fromEntries(missing.map(([key, width]) => [key, clampWidth(width)])), ...this.widths };
    }
  }

  /** The grip for `key`'s header; the first drag keeps every width `measure` reads, so the layout holds still. */
  grip(key: string, measure: () => Record<string, number>): Resize {
    return {
      width: this.sized ? this.of(key) : undefined,
      start: () => this.fill(measure()),
      to: (width) => this.set(key, width),
      done: () => this.save(),
    };
  }

  save() {
    pageStorage()?.setItem(this.storageKey, JSON.stringify(this.widths));
  }

  reset() {
    this.widths = {};
    pageStorage()?.removeItem(this.storageKey);
  }
}
