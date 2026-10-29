// The global catalog's list and entry pages, one tab for runtimes and one for
// models, and the short list on a device's page. The API supplies release order,
// member precedence and the offered members; these helpers only prepare and
// draw their views.

import type {
  DescriptorCatalogShortList, DescriptorCatalogShortListItem, DescriptorDigestNames, DescriptorEntryListRow,
  DescriptorKind, DescriptorMemberKey, DescriptorMemberParameters, DescriptorMemberView, DescriptorNameHit,
  DescriptorWithheldReport, ModelFormat,
} from "./api-types";
import type { BarTab } from "./bar";
import { pathOf } from "./routes";
import type { TableSpec } from "./table";

export const catalogPath = pathOf({ page: "server-catalog" });
export const catalogEntryPath = (name: string) => pathOf({ page: "server-catalog-entry", name });

/** The tab that the list page shows, by its address: `?kind=model` opens Models. */
export const catalogKindOf = (query: URLSearchParams): DescriptorKind =>
  (query.get("kind") === "model" ? "model" : "runtime");

/** The list page's tabs. The Models tab uses a query value, because a path segment could hide an entry of that name. */
export const catalogTabs = (kind: DescriptorKind): BarTab[] => [
  { key: "runtime", label: "Runtimes", href: catalogPath, current: kind === "runtime", start: false },
  { key: "model", label: "Models", href: `${catalogPath}?kind=model`, current: kind === "model", start: false },
];

/** The full list's server filters. The UI includes hidden entries only on request. */
export function catalogListQuery(prefix: string, showHidden: boolean, kind: DescriptorKind): string {
  const query = new URLSearchParams({
    scope: "global", descriptor_kind: kind,
    state: showHidden ? "active,pinned,hidden" : "active,pinned", limit: "200",
  });
  if (prefix.trim()) query.set("prefix", prefix.trim());
  return query.toString();
}

export const memberLabel = (key: DescriptorMemberKey): string =>
  key.kind === "runtime" ? key.flavor ?? "default" : `${key.format}::${key.quantization}`;

export const flavors = (row: DescriptorEntryListRow): string =>
  row.member_summaries.map((member) => memberLabel(member.member_key)).join(", ");

/** A missing or unsupported rule needs an operator's attention. */
export function offerWarning(row: DescriptorEntryListRow): string | null {
  const missing = row.member_summaries.filter((member) => member.coverage === "no_rule").length;
  const unsupported = row.member_summaries.filter((member) => member.coverage === "unsupported").length;
  if (!missing && !unsupported) return null;
  return [missing ? `${missing} without an offer rule` : "", unsupported ? `${unsupported} unsupported` : ""]
    .filter(Boolean).join("; ");
}

export const catalogTable: TableSpec<DescriptorEntryListRow> = {
  columns: [
    { key: "name", label: "Name", raw: (row) => row.name, href: (row) => catalogEntryPath(row.name) },
    { key: "kind", label: "Kind", raw: (row) => row.kind, as: "state" },
    { key: "state", label: "State", raw: (row) => row.state === "active" ? null : row.state, as: "state" },
    { key: "release_time", label: "Release", raw: (row) => row.release_time, as: "time" },
    { key: "created_at", label: "Created", raw: (row) => row.created_at, as: "time" },
    { key: "created_by", label: "Creator", raw: (row) => row.created_by },
    { key: "member_count", label: "Members", raw: (row) => row.member_count, num: true },
    { key: "flavors", label: "Flavors", raw: flavors },
    { key: "offer_rule", label: "Offer rules", raw: offerWarning },
  ],
  server: [{ key: "prefix", label: "Name prefix", placeholder: "starts with…", wide: true }],
  choices: [
    { key: "kind", label: "Kind", values: (row) => [row.kind], order: ["stable", "daily", "hand-made"] },
    { key: "state", label: "State", values: (row) => [row.state], order: ["pinned", "active", "hidden"] },
    { key: "type", label: "Runtime type", values: (row) => row.descriptor_types },
  ],
};

/**
 * The model tab's table. Every model entry is hand-made and has no offer rules, so the table has no kind column and
 * no offer-rule column.
 */
export const modelCatalogTable: TableSpec<DescriptorEntryListRow> = {
  columns: [
    { key: "name", label: "Name", raw: (row) => row.name, href: (row) => catalogEntryPath(row.name) },
    { key: "state", label: "State", raw: (row) => row.state === "active" ? null : row.state, as: "state" },
    { key: "release_time", label: "Release", raw: (row) => row.release_time, as: "time" },
    { key: "created_at", label: "Created", raw: (row) => row.created_at, as: "time" },
    { key: "created_by", label: "Creator", raw: (row) => row.created_by },
    { key: "member_count", label: "Members", raw: (row) => row.member_count, num: true },
    { key: "flavors", label: "Formats and quantizations", raw: flavors },
  ],
  server: [{ key: "prefix", label: "Name prefix", placeholder: "starts with…", wide: true }],
  choices: [
    { key: "state", label: "State", values: (row) => [row.state], order: ["pinned", "active", "hidden"] },
    { key: "type", label: "Model type", values: (row) => row.descriptor_types },
  ],
};

/**
 * Each tab's table, and the storage key for its picked columns. The tabs have different columns, so each tab has its
 * own key.
 */
export const CATALOG_TABS: Record<DescriptorKind, { table: TableSpec<DescriptorEntryListRow>; columnsKey: string }> = {
  runtime: { table: catalogTable, columnsKey: "pipette-mgmt.app.catalog-columns" },
  model: { table: modelCatalogTable, columnsKey: "pipette-mgmt.app.catalog-model-columns" },
};

/** One model member of an entry page's table. */
export interface ModelMemberRow {
  format: ModelFormat;
  quantization: string;
  member: DescriptorMemberView;
}

/** An entry's model members, grouped by format in the order that each format first appears. */
export function modelMemberRows(members: readonly DescriptorMemberView[]): ModelMemberRow[] {
  const rows = members.flatMap((member) => member.member_key.kind === "model"
    ? [{ format: member.member_key.format, quantization: member.member_key.quantization, member }] : []);
  const formats = [...new Set(rows.map((row) => row.format))];
  return formats.flatMap((format) => rows.filter((row) => row.format === format));
}

/** The one model key of an entry, with its counts, or `null` when the members have different keys or none. */
export function sharedParameters(members: readonly DescriptorMemberView[]):
  { model_key: string; parameters: DescriptorMemberParameters | null } | null {
  const keys = new Set(members.map((member) => member.model_key ?? null));
  const [key] = keys;
  if (keys.size !== 1 || !key) return null;
  return { model_key: key, parameters: members[0]?.parameters ?? null };
}

/** Parameter counts in millions: the total, and also the active count of a sparse model. */
export function parameterText(parameters: DescriptorMemberParameters | null | undefined): string {
  if (!parameters) return "no parameter counts";
  const total = `${parameters.total_millions.toLocaleString("en-US")}M`;
  return parameters.active_millions === parameters.total_millions
    ? total : `${total} total, ${parameters.active_millions.toLocaleString("en-US")}M active`;
}

/** A device's short list, split into its runtime entries and its model entries. */
export function shortListSections(list: DescriptorCatalogShortList):
  Record<DescriptorKind, DescriptorCatalogShortListItem[]> {
  return {
    runtime: list.items.filter((item) => item.descriptor_kind === "runtime"),
    model: list.items.filter((item) => item.descriptor_kind === "model"),
  };
}

/** What the withheld report counts, or `null` when the device is offered every member. */
export function withheldSummary(report: DescriptorWithheldReport): string | null {
  if (!report.member_count) return null;
  const members = `${report.member_count} ${report.member_count === 1 ? "member" : "members"} withheld`;
  if (!report.entry_count) return members;
  return `${members}; ${report.entry_count} ${report.entry_count === 1 ? "entry offers" : "entries offer"} no member`;
}

export interface CatalogSynonym {
  member: string;
  name: string;
  kind: DescriptorNameHit["kind"];
  href: string;
}

/** Other global names of each member, of the entry's own kind, without self-links or repeated hits. */
export function catalogSynonyms(entry: DescriptorEntryListRow, names: DescriptorDigestNames): CatalogSynonym[] {
  const seen = new Set<string>();
  return (entry.members ?? []).flatMap((member) => (names.names[member.digest] ?? []).flatMap((hit) => {
    if (hit.scope !== "global" || hit.descriptor_kind !== entry.descriptor_kind
        || hit.name.toLowerCase() === entry.name.toLowerCase()) return [];
    const label = memberLabel(member.member_key);
    const key = `${member.digest}/${hit.name.toLowerCase()}`;
    if (seen.has(key)) return [];
    seen.add(key);
    return [{ member: label, name: hit.name, kind: hit.kind, href: catalogEntryPath(hit.name) }];
  }));
}
