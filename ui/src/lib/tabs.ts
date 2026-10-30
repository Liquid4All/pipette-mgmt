// A plan's tabs, on its own pages and its campaign's: the Overview, Results (the
// `analysis` addresses), Jobs (`cells`), History (`revisions`), and Settings. Each
// is an address of its own, so links, back and bookmarks work.

import type { BarTab } from "./bar";
import {
  MISSING, PICKED, reportPairs, SHARED, SWAP, type Missing, type Picks, type ReportFilters, type ReportIndex,
} from "./report";
import { href, pathOf, type Route, type RouteOf } from "./routes";

export type TabName = "overview" | "cells" | "analysis" | "revisions" | "settings";

/** Each tab's page, a Results view's under `report`, on the plan's own addresses and its campaign's. */
const PLAN_PAGES = {
  overview: "plan", cells: "plan-cells", analysis: "plan-analysis", report: "plan-analysis-report",
  revisions: "plan-revisions", settings: "plan-settings",
} as const;
const CAMPAIGN_PAGES = {
  overview: "campaign-plan", cells: "campaign-plan-cells", analysis: "campaign-plan-analysis",
  report: "campaign-plan-analysis-report", revisions: "campaign-plan-revisions", settings: "campaign-plan-settings",
} as const;

type PlanPage = (typeof PLAN_PAGES)[keyof typeof PLAN_PAGES];
type CampaignPage = (typeof CAMPAIGN_PAGES)[keyof typeof CAMPAIGN_PAGES];
export type PlanTabRoute = { [P in PlanPage]: RouteOf<P> }[PlanPage];
export type CampaignTabRoute = { [P in CampaignPage]: RouteOf<P> }[CampaignPage];

/** The plan a tab is of, and the campaign whose pages it is drawn on, if any. */
export interface TabBase {
  plan_id: string;
  campaign_id: string | null;
}

/** A tab, and on Results the view's address segment: null for the bare address, which opens no chart. */
export interface Tab {
  name: TabName;
  view: string | null;
}

const TABS = new Map<string, TabName>([...Object.entries(PLAN_PAGES), ...Object.entries(CAMPAIGN_PAGES)]
  .map(([name, page]) => [page, name === "report" ? "analysis" : name as TabName]));

/** The tab `route` draws; null for a page that is no plan's tab. */
export function tabOf(route: Route): Tab | null {
  const name = TABS.get(route.page);
  if (!name) return null;
  return { name, view: "view" in route ? route.view : null };
}

/** The plan and campaign a tab's route names. */
export const baseOf = (route: PlanTabRoute | CampaignTabRoute): TabBase =>
  ({ plan_id: route.plan_id, campaign_id: "campaign_id" in route ? route.campaign_id : null });

/** The address of tab `name`, on Results of `view` (empty: the bare address), `query`'s pairs after it. */
export function tabLink(base: TabBase, name: TabName, view = "", query: [string, string][] = []): string {
  const pages = base.campaign_id ? CAMPAIGN_PAGES : PLAN_PAGES;
  const page = name === "analysis" && view ? pages.report : pages[name];
  return href(pathOf({ page, ...base, view } as unknown as Route), query);
}

/** The query a link between reports carries: never a metric, a pin or an offset, which are one report's own. */
export const sharedQuery = (query: URLSearchParams): [string, string][] =>
  SHARED.flatMap((key): [string, string][] => (PICKED.includes(key)
    ? query.getAll(key).map((value): [string, string] => [key, value]) : [[key, query.get(key) ?? ""]]));

/**
 * The top bar: the Overview, then Results where the server reads results, Jobs,
 * History where the plan has a campaign whose history the reader may read, and
 * set apart from them Settings where the reader has a write there. The open tab
 * is listed whatever, so the bar always says where the reader is.
 */
export function topBar(base: TabBase, current: Tab,
                       shown: { analysis: boolean; revisions: boolean; settings: boolean },
                       query: URLSearchParams): BarTab[] {
  const tab = (name: TabName, label: string, carried: [string, string][] = [], start = false): BarTab =>
    ({ key: name, label, href: tabLink(base, name, "", carried), current: current.name === name, start });
  return [
    tab("overview", "Overview"),
    ...(shown.analysis || current.name === "analysis" ? [tab("analysis", "Results", sharedQuery(query))] : []),
    tab("cells", "Jobs"),
    ...(shown.revisions || current.name === "revisions" ? [tab("revisions", "History")] : []),
    ...(shown.settings || current.name === "settings" ? [tab("settings", "Settings", [], true)] : []),
  ];
}

type Reports = Pick<ReportIndex, "benchmarks" | "families" | "swapped" | "missing">;

/** The family a Results address names: its own, or a benchmark type split in families (`eval`) at its first. */
const opened = (view: string, index: Reports): string =>
  (index.benchmarks.some((f) => f.key === view) ? view : index.benchmarks.find((f) => f.types.includes(view))?.key)
  ?? view;

/**
 * What a Results address draws: the charts, opened on a family's (the bare address: on none, `""`), Swap,
 * Missing data, or a view the plan lacks.
 */
export type Report =
  | { kind: "charts"; family: string }
  | { kind: "swap" }
  | { kind: "missing"; missing: Missing }
  | { kind: "unknown"; view: string };

export function reportOf(view: string | null, index: Reports): Report {
  if (view === null) return { kind: "charts", family: "" };
  const open = opened(view, index);
  if (open === SWAP) return index.swapped ? { kind: "swap" } : { kind: "unknown", view: open };
  if (open === MISSING) return index.missing ? { kind: "missing", missing: index.missing } : { kind: "unknown", view: open };
  const charted = index.families.some((f) => f.key === open && !f.disabled);
  return charted ? { kind: "charts", family: open } : { kind: "unknown", view: open };
}

/** The segment of the All rows report that Results replaced with the Jobs tab. */
const ROWS = "rows";

/**
 * Where an address of the results page before the tabs lands, every other key
 * kept. Its table's rows (`rows=all` or `#rows`) land on Jobs, which lists them.
 */
export function legacyResults(base: TabBase, query: URLSearchParams, hash: string): string {
  const graph = query.get("graph") ?? "";
  if (query.get("rows") === "all" || hash === "#rows") return legacyRows(base, query);
  const rest = [...query].filter(([key]) => key !== "graph" && key !== "rows");
  return tabLink(base, "analysis", graph, rest);
}

/**
 * Where an address of the Reports tab, before it was Results, lands: the same
 * report, its query kept; its All rows on Jobs, which lists the rows.
 */
export const legacyReports = (base: TabBase, view: string, query: URLSearchParams): string =>
  (view === ROWS ? legacyRows(base, query) : tabLink(base, "analysis", view, [...query]));

/** All rows' address as Jobs reads it: its outcome filter alone, the others naming what Jobs filters by other names. */
function legacyRows(base: TabBase, query: URLSearchParams): string {
  const status = query.get("status");
  return tabLink(base, "cells", "", status === "failed" || status === "measured" ? [["result", status]] : []);
}

/** The one device type the Overview's address picks, of `types`: none where it names several, or one they lack. */
export function pickedDevice(query: URLSearchParams, types: readonly string[]): string {
  const [type, ...more] = query.getAll("device");
  return type !== undefined && !more.length && types.includes(type) ? type : "";
}

/**
 * The Overview's device types over Progress: every type, then each alone by its short name, its make on hover, as
 * `device` names one, which Results' bar reads too; none where the results name no type.
 */
export function progressBar(base: TabBase, types: readonly string[], names: ReadonlyMap<string, string>,
                            chosen: string): BarTab[] {
  if (!types.length) return [];
  return [
    { key: "", label: "All devices", href: tabLink(base, "overview"), current: !chosen, start: false },
    ...types.map((type, i): BarTab => ({ key: type, label: names.get(type) ?? type, title: type,
                                         href: tabLink(base, "overview", "", [["device", type]]), current: type === chosen,
                                         start: i === 0 })),
  ];
}

/**
 * The results page's device types as a bar: every type, then each alone by its short name, its make on hover; none
 * where the plan charts none. Every type is spelled out, as an address naming none opens a chart past 12
 * panels on its first.
 */
export function deviceBar(base: TabBase, view: string, filters: ReportFilters, device: Picks | null,
                          names: ReadonlyMap<string, string>): BarTab[] {
  const types = device?.options ?? [];
  if (!types.length) return [];
  const to = (chosen: string[]) => tabLink(base, "analysis", view, reportPairs({ ...filters, device: chosen }));
  const { chosen } = device!;
  // Every type drawn is all of them, by name or not; else each drawn type is lit.
  const all = !chosen.length || (types.length > 1 && chosen.length === types.length);
  return [
    { key: "", label: "All devices", href: to(types), current: all, start: false },
    ...types.map((type, i): BarTab => ({ key: type, label: names.get(type) ?? type, title: type, href: to([type]),
                                         current: !all && chosen.includes(type), start: i === 0 })),
  ];
}
