// A listing read whole into the browser, as the Jobs tab reads a plan's jobs:
// filtered by text boxes and selects, sorted by any column and paged here, each
// choice kept in the address.

export type Raw = string | number | boolean | null;

/** One option of a select, and how many rows have it. */
export interface Choice {
  value: string;
  label: string;
  count?: number;
  /** Text the option search matches without adding it to the visible label. */
  searchText?: string;
}

/** Whether an option's label, value, or extra search text contains the query. */
export const choiceMatches = (choice: Choice, query: string): boolean =>
  `${choice.label}\n${choice.value}\n${choice.searchText ?? ""}`.toLowerCase().includes(query.trim().toLowerCase());

/** `options`, then each chosen value they lack at a count of 0, so a choice no row has any more can be unchecked. */
export const offeredChoices = (options: Choice[], chosen: readonly string[], name: (value: string) => string,
): Choice[] => [
  ...options,
  ...chosen.filter((value) => !options.some((option) => option.value === value))
    .map((value) => ({ value, label: name(value), count: 0 })),
];

/** A table's column: its header sorts by `raw`, the value it holds rather than the text drawn. */
export interface TableColumn<R> {
  key: string;
  label: string;
  raw: (row: R) => Raw;
  /** Shown until the reader picks the columns; `false` leaves it to the picker. */
  shown?: boolean;
  /** Figures, lined up on the right. */
  num?: boolean;
  /** How `DataTable` draws `raw` where the listing has no markup for the cell; plain text without one. */
  as?: "id" | "json" | "time" | "state";
  /** The page the cell links to, for the column that carries a record's id. */
  href?: (row: R) => string | null;
}

/** A text box: a row passes where any of its `texts` holds what is typed, case aside. */
export interface TextFilter<R> {
  key: string;
  label: string;
  /** What the empty box says it searches; `any` without one. */
  placeholder?: string;
  /** A longer box, for one that searches several fields. */
  wide?: boolean;
  texts: (row: R) => (string | null | undefined)[];
}

/** A select of several choices: a row passes on any of its `values` chosen, `""` being none. */
export interface ChoiceFilter<R> {
  key: string;
  label: string;
  values: (row: R) => string[];
  name?: (value: string) => string;
  /** The options' order where it is fixed; else by name. */
  order?: readonly string[];
}

/**
 * A filter the server applies, which the page reads from the address and sends: a text box, or a select of
 * `options` whose `""` is the default. The rows read already pass it.
 */
export interface ServerFilter {
  key: string;
  label: string;
  placeholder?: string;
  wide?: boolean;
  options?: readonly Choice[];
}

export interface TableSpec<R> {
  columns: TableColumn<R>[];
  texts?: TextFilter<R>[];
  server?: ServerFilter[];
  choices?: ChoiceFilter<R>[];
}

/**
 * Each text box's text, a server filter's value among them, each select's choices, and the sort column, `-` first
 * for descending, `""` for none.
 */
export interface TableState {
  texts: Record<string, string>;
  choices: Record<string, string[]>;
  sort: string;
}

/** The state the address sets, a sort by a column the table lacks dropped. */
export function tableState<R>(spec: TableSpec<R>, query: URLSearchParams): TableState {
  const sort = query.get("sort") ?? "";
  return {
    texts: Object.fromEntries([...spec.texts ?? [], ...spec.server ?? []].map(({ key }) =>
      [key, query.get(key)?.trim() ?? ""])),
    choices: Object.fromEntries((spec.choices ?? []).map(({ key }) =>
      [key, [...new Set(query.getAll(key).filter(Boolean))]])),
    sort: spec.columns.some((column) => column.key === sort.replace(/^-/, "")) ? sort : "",
  };
}

/** The columns that the reader toggled, by key. `true` draws a column and `false` hides it. */
export type Picks = Record<string, boolean>;

/** The picks that draw exactly `keys` among `columns`. */
export const picksOf = (columns: readonly { key: string }[], keys: readonly string[]): Picks =>
  Object.fromEntries(columns.map((column) => [column.key, keys.includes(column.key)]));

/**
 * The columns drawn, in the table's order. A column in `picks` follows its pick. The table draws any other column
 * unless it has `shown: false`.
 */
export const drawnColumns = <C extends { key: string; shown?: boolean }>(columns: readonly C[],
                                                                       picks: Readonly<Picks> | null): C[] =>
  columns.filter((column) => (picks && Object.hasOwn(picks, column.key) ? picks[column.key]
    : column.shown !== false));

/** Whether any filter is set; the sort is not one. */
export const filtering = (state: TableState): boolean =>
  Object.values(state.texts).some(Boolean) || Object.values(state.choices).some((chosen) => chosen.length);

/** `state` as address pairs, a select's key repeated per choice. */
export const statePairs = (state: TableState): [string, string][] => [
  ...Object.entries(state.texts),
  ...Object.entries(state.choices).flatMap(([key, chosen]) => chosen.map((value): [string, string] => [key, value])),
  ["sort", state.sort],
];

/** `state` with no filter set, its sort kept. */
export const cleared = (state: TableState): TableState => ({
  texts: Object.fromEntries(Object.keys(state.texts).map((key) => [key, ""])),
  choices: Object.fromEntries(Object.keys(state.choices).map((key) => [key, []])),
  sort: state.sort,
});

/** `state` with `value` chosen in `key`, or let go of where `on` is false. */
export const toggledIn = (state: TableState, key: string, value: string, on: boolean): TableState => {
  const chosen = state.choices[key] ?? [];
  return { ...state, choices: { ...state.choices, [key]: on ? [...chosen, value] : chosen.filter((v) => v !== value) } };
};

/** Whether `row` passes every filter set. */
export function keepsRow<R>(spec: TableSpec<R>, state: TableState, row: R): boolean {
  return (spec.texts ?? []).every(({ key, texts }) => {
    const want = state.texts[key]?.toLowerCase();
    return !want || texts(row).some((text) => text?.toLowerCase().includes(want));
  }) && (spec.choices ?? []).every(({ key, values }) => {
    const chosen = state.choices[key] ?? [];
    return !chosen.length || values(row).some((value) => chosen.includes(value));
  });
}

/** Each select's options over all of `rows`, counted, and those of `chosen` no row has. */
export function choicesOf<R>(spec: TableSpec<R>, rows: readonly R[], chosen: TableState["choices"],
): Record<string, Choice[]> {
  return Object.fromEntries((spec.choices ?? []).map(({ key, values, name = (v) => v, order }) => {
    const counts = new Map<string, number>();
    for (const row of rows) for (const value of new Set(values(row))) if (value) counts.set(value, (counts.get(value) ?? 0) + 1);
    const found = [...counts].map(([value, count]) => ({ value, label: name(value), count }));
    return [key, offeredChoices(order ? order.flatMap((value) => found.filter((choice) => choice.value === value))
      : found.sort((a, b) => a.label.localeCompare(b.label, "en", { numeric: true })), chosen[key] ?? [], name)];
  }));
}

/** The sort a click on `key`'s header asks for: ascending, then descending, then none. */
export const nextSort = (sort: string, key: string): string =>
  (sort === key ? `-${key}` : sort === `-${key}` ? "" : key);

/** How a header names its sort to assistive technology, or `undefined` unsorted. */
export const ariaSort = (sort: string, key: string): "ascending" | "descending" | undefined =>
  (sort === key ? "ascending" : sort === `-${key}` ? "descending" : undefined);

/** How two raw values order: numbers by size, the rest as text with its numbers by size, none last either way. */
export function compareRaw(a: Raw, b: Raw, sign: number): number {
  const none = (v: Raw) => v === null || v === "";
  if (none(a) || none(b)) return Number(none(a)) - Number(none(b));
  if (typeof a === "number" && typeof b === "number") return sign * (a - b);
  return sign * String(a).localeCompare(String(b), "en", { numeric: true });
}

/** `items` by `sort` over `columns`, ties in the order they came; as they came with no sort. */
export function sortBy<T>(items: readonly T[], columns: readonly { key: string; raw: (item: T) => Raw }[],
                          sort: string): T[] {
  const column = columns.find((c) => c.key === sort.replace(/^-/, ""));
  if (!column) return [...items];
  const sign = sort.startsWith("-") ? -1 : 1;
  return items.map((item) => ({ item, raw: column.raw(item) }))
    .sort((a, b) => compareRaw(a.raw, b.raw, sign)).map(({ item }) => item);
}

/** The rows `state` keeps, in its order. */
export const tableView = <R>(spec: TableSpec<R>, state: TableState, rows: readonly R[]): R[] =>
  sortBy(rows.filter((row) => keepsRow(spec, state, row)), spec.columns, state.sort);

/** Rows past which a table kept in the page offers its filters; a shorter one is read at a glance. */
export const FILTERS_FROM = 10;

/** A table kept in the page, before the reader sorts or filters it. */
export const unsorted = <R>(spec: TableSpec<R>): TableState => tableState(spec, new URLSearchParams());

/** A cursor listing's page, as every listing route answers one. */
export interface CursorPage<T> {
  items: T[];
  next_cursor: string | null;
  unreadable: number;
  names?: Record<string, string> | null;
}

/** The most rows a listing route answers on a page (`list_max_limit`'s default). */
export const ROUTE_PAGE_MAX = "200";

/** `query` at `cursor`'s page, the first with none. */
export const withCursor = (query: string, cursor: string | null): string =>
  (cursor ? `${query}${query ? "&" : ""}${new URLSearchParams({ cursor })}` : query);

/** Every page of a cursor listing, read in turn: the rows, the names and the unreadable records of all of them. */
export async function everyPage<T>(read: (cursor: string | null) => Promise<CursorPage<T>>): Promise<CursorPage<T>> {
  const whole: CursorPage<T> = { items: [], next_cursor: null, unreadable: 0, names: {} };
  let cursor: string | null = null;
  do {
    const page: CursorPage<T> = await read(cursor);
    whole.items.push(...page.items);
    whole.unreadable += page.unreadable;
    Object.assign(whole.names!, page.names ?? {});
    cursor = page.next_cursor;
  } while (cursor);
  return whole;
}
