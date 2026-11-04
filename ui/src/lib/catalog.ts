// Prepare catalog entries from every readable scope for list and entry views.

import type {
  DescriptorCatalogShortList, DescriptorCatalogShortListItem, DescriptorDigestNames,
  DescriptorEntryListRow, DescriptorKind, DescriptorMemberEvidence, DescriptorMemberKey, DescriptorMemberParameters,
  DescriptorMemberView, DescriptorNameHit, DescriptorWithheldReport, ModelFormat, OwnUserView, RunMark,
} from "./api-types";
import type { BarTab } from "./bar";
import { when } from "./format";
import { reaches } from "./rights";
import { pathOf } from "./routes";
import { compareRaw, type TableSpec } from "./table";

export const catalogPath = pathOf({ page: "server-catalog" });
/**
 * The entry page of `name`, a name as the reader sees it: bare, `global/x` or `acme/x`. The page selects the entry as
 * the server's name lookup does.
 */
export const catalogEntryPath = (name: string) => pathOf({ page: "server-catalog-entry", name });

const GLOBAL = "global";

const entryHref = (entry: Pick<DescriptorEntryListRow, "scope" | "name" | "display_name">) =>
  catalogEntryPath(entry.scope === GLOBAL ? `global/${entry.name}` : entry.display_name);

export type Reader = Pick<OwnUserView, "org_id" | "org_slug" | "may">;

/**
 * The scope badge of an entry or a name: `global`, or the slug of its organization. The reader's own organization takes
 * the reader's slug. The name of another organization's entry starts with that organization's slug, as `acme/x`. A
 * scope with no known slug shows its id.
 */
export function scopeLabel(item: Pick<DescriptorEntryListRow, "scope" | "display_name">, reader: Reader | null): string {
  if (item.scope === GLOBAL) return GLOBAL;
  if (reader?.org_id && item.scope === reader.org_id) return reader.org_slug ?? item.scope;
  const slash = item.display_name.indexOf("/");
  return slash > 0 ? item.display_name.slice(0, slash) : item.scope;
}

/**
 * Whether `reader` may change `entry`: an org entry, where `edit_catalog` reaches the organization of the entry. Only
 * the host CLI changes a global entry.
 */
export const mayEdit = (reader: Reader | null, entry: Pick<DescriptorEntryListRow, "scope">): boolean =>
  !!reader && entry.scope !== GLOBAL && reaches(reader, reader.may.edit_catalog, entry.scope);

/** The tab that the list page shows, by its address: `?kind=model` opens Models. */
export const catalogKindOf = (query: URLSearchParams): DescriptorKind =>
  (query.get("kind") === "model" ? "model" : "runtime");

/** The list page's tabs. The Models tab uses a query value, because a path segment could hide an entry of that name. */
export const catalogTabs = (kind: DescriptorKind): BarTab[] => [
  { key: "runtime", label: "Runtimes", href: catalogPath, current: kind === "runtime", start: false },
  { key: "model", label: "Models", href: `${catalogPath}?kind=model`, current: kind === "model", start: false },
];

/**
 * The full list's server filters. The list holds every scope that the reader can read. The UI includes hidden entries
 * only on request.
 */
export function catalogListQuery(prefix: string, showHidden: boolean, kind: DescriptorKind): string {
  const query = new URLSearchParams({
    descriptor_kind: kind,
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

export type ScopeOf = (row: DescriptorEntryListRow) => string;

const nameColumns = (scopeOf: ScopeOf): TableSpec<DescriptorEntryListRow>["columns"] => [
  { key: "name", label: "Name", raw: (row) => row.display_name,
    href: entryHref },
  { key: "scope", label: "Scope", raw: scopeOf, as: "state" },
];

const scopeChoice = (scopeOf: ScopeOf) => ({ key: "scope", label: "Scope", values: (row: DescriptorEntryListRow) =>
  [scopeOf(row)] });

export const catalogTable = (scopeOf: ScopeOf): TableSpec<DescriptorEntryListRow> => ({
  columns: [
    ...nameColumns(scopeOf),
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
    scopeChoice(scopeOf),
    { key: "kind", label: "Kind", values: (row) => [row.kind], order: ["stable", "daily", "hand-made"] },
    { key: "state", label: "State", values: (row) => [row.state], order: ["pinned", "active", "hidden"] },
    { key: "type", label: "Runtime type", values: (row) => row.descriptor_types },
  ],
});

/**
 * The model tab's table. Every model entry is hand-made and has no offer rules, so the table has no kind column and
 * no offer-rule column.
 */
export const modelCatalogTable = (scopeOf: ScopeOf): TableSpec<DescriptorEntryListRow> => ({
  columns: [
    ...nameColumns(scopeOf),
    { key: "state", label: "State", raw: (row) => row.state === "active" ? null : row.state, as: "state" },
    { key: "release_time", label: "Release", raw: (row) => row.release_time, as: "time" },
    { key: "created_at", label: "Created", raw: (row) => row.created_at, as: "time" },
    { key: "created_by", label: "Creator", raw: (row) => row.created_by },
    { key: "member_count", label: "Members", raw: (row) => row.member_count, num: true },
    { key: "flavors", label: "Formats and quantizations", raw: formatText },
  ],
  server: [{ key: "prefix", label: "Name prefix", placeholder: "starts with…", wide: true }],
  choices: [
    scopeChoice(scopeOf),
    { key: "state", label: "State", values: (row) => [row.state], order: ["pinned", "active", "hidden"] },
    { key: "type", label: "Model type", values: (row) => row.descriptor_types },
  ],
});

/**
 * Each tab's table, and the storage key for its picked columns. The tabs have different columns, so each tab has its
 * own key.
 */
export const catalogTabSpecs = (scopeOf: ScopeOf):
  Record<DescriptorKind, { table: TableSpec<DescriptorEntryListRow>; columnsKey: string }> => ({
  runtime: { table: catalogTable(scopeOf), columnsKey: "pipette-mgmt.app.catalog-columns" },
  model: { table: modelCatalogTable(scopeOf), columnsKey: "pipette-mgmt.app.catalog-model-columns" },
});

/** A row's key in the list. A global entry and an org entry can have the same name. */
export const catalogRowId = (row: DescriptorEntryListRow): string => `${row.scope}/${row.name}`;

/** One model member of an entry page's table. */
export interface ModelMemberRow {
  format: ModelFormat;
  quantization: string;
  member: DescriptorMemberView;
}

type ModelKey = Pick<ModelMemberRow, "format" | "quantization">;

/** The order of model keys: by format, then by quantization, with the numbers in each name compared by size. */
const modelKeyOrder = (a: ModelKey, b: ModelKey): number =>
  compareRaw(a.format, b.format, 1) || compareRaw(a.quantization, b.quantization, 1);

/** An entry's model members, sorted by format and then by quantization. */
export function modelMemberRows(members: readonly DescriptorMemberView[]): ModelMemberRow[] {
  return members.flatMap((member) => member.member_key.kind === "model"
    ? [{ format: member.member_key.format, quantization: member.member_key.quantization, member }] : [])
    .sort(modelKeyOrder);
}

/** One format of a model entry, with its quantizations in order. */
export interface FormatGroup {
  format: ModelFormat;
  quantizations: string[];
}

/** A list row's model members, as its formats in order, each with its quantizations in order. */
export function formatGroups(row: DescriptorEntryListRow): FormatGroup[] {
  const keys = row.member_summaries.flatMap((member) => member.member_key.kind === "model"
    ? [member.member_key] : []).sort(modelKeyOrder);
  const formats = [...new Set(keys.map((key) => key.format))];
  return formats.map((format) => ({
    format, quantizations: keys.filter((key) => key.format === format).map((key) => key.quantization),
  }));
}

/** The text of a list row's formats and quantizations, as `gguf q4_0 q8_0 · mlx 4bit`. */
export const formatText = (row: DescriptorEntryListRow): string =>
  formatGroups(row).map((group) => [group.format, ...group.quantizations].join(" ")).join(" · ");

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

/**
 * The label of a member's run evidence on every device. A success on other hardware suggests that a build works, and
 * says little about one device.
 */
export const ANY_DEVICE_LABEL = "any device (weaker evidence)";

/** One run as text: its time and the client version that ran it. */
export function runText(run: RunMark | null | undefined): string {
  if (!run) return "none";
  return run.client_version ? `${when(run.at)}, client ${run.client_version}` : when(run.at);
}

/** A member's last success or last failure on the short list's device as text. */
export function thisDeviceText(member: DescriptorMemberView, side: "last_success" | "last_failure"): string {
  const runs = member.evidence?.this_device;
  return runs ? runText(runs[side]) : "unavailable";
}

/** A member's run evidence on every device as text, or `null` when the answer gives no evidence. */
export function anyDeviceText(evidence: DescriptorMemberEvidence | null | undefined): string | null {
  if (!evidence) return null;
  const { last_success_at, devices_with_success } = evidence.any_device;
  if (!devices_with_success) return "no success";
  const devices = `${devices_with_success} ${devices_with_success === 1 ? "device" : "devices"}`;
  return `last success ${when(last_success_at)}; a success on ${devices}`;
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
  /** The name as the reader sees it. */
  name: string;
  scope: string;
  kind: DescriptorNameHit["kind"];
  href: string;
}

/**
 * Return other readable names of the entry's kind from every scope. Exclude the entry itself and repeated hits.
 * `scopeOf` gives each name's scope badge.
 */
export function catalogSynonyms(entry: DescriptorEntryListRow, names: DescriptorDigestNames,
                                scopeOf: (hit: DescriptorNameHit) => string): CatalogSynonym[] {
  const seen = new Set<string>();
  return (entry.members ?? []).flatMap((member) => (names.names[member.digest] ?? []).flatMap((hit) => {
    if (hit.descriptor_kind !== entry.descriptor_kind
        || (hit.scope === entry.scope && hit.name.toLowerCase() === entry.name.toLowerCase())) return [];
    const label = memberLabel(member.member_key);
    const key = `${member.digest}/${hit.scope}/${hit.name.toLowerCase()}`;
    if (seen.has(key)) return [];
    seen.add(key);
    return [{ member: label, name: hit.display_name, scope: scopeOf(hit), kind: hit.kind,
              href: entryHref(hit) }];
  }));
}
