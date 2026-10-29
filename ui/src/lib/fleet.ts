// The fleet filters as the classic console's form spelled them (`clients.html`):
// in the address bar, so a bookmark reproduces the listing, and passed on to
// `GET /api/v1/clients` (`docs/api/devices/list_clients.md`).

import { clientLink } from "./routes";
import type { TableSpec } from "./table";
import type { ClientListRow } from "./api-types";

export const CLAUSES = [
  { key: "tag", label: "tag", placeholder: "us-east",
    help: "One tag per field. A client must carry every tag." },
  { key: "requires", label: "requires", placeholder: "os:ios runtime:llama_cpp",
    help: "Whitespace separates the flags. A client must carry every flag." },
  { key: "any_of", label: "any of", placeholder: "gpu:a17pro gpu:a18pro",
    help: "One field is one group. A client must carry at least one flag from each group. " +
          "This clause only narrows, so it needs a requires flag beside it." },
  { key: "excludes", label: "excludes", placeholder: "gpu:a17pro",
    help: "Whitespace separates the flags. Any one of these flags rejects a client. " +
          "This clause only narrows, so it needs a requires flag beside it." },
  { key: "not_all_of", label: "not all of", placeholder: "os:ios gpu:a17pro",
    help: "One field is one group. This clause rejects a client that carries every flag in a group. " +
          "This clause only narrows, so it needs a requires flag beside it." },
] as const;

export type ClauseKey = (typeof CLAUSES)[number]["key"];

export interface FleetFilters {
  /** `null` until the reader picks one, which reads as `approved`; `""` is every status. */
  status: string | null;
  /** Each clause's fields as typed, blank ones included. */
  clauses: Record<ClauseKey, string[]>;
  /** The page size as the address gives it, `""` for the route's default; Apply and the pager keep it. */
  limit: string;
}

// A fleet is read to see what can take work; the route itself defaults to every status.
export const DEFAULT_STATUS = "approved";

/** The filters `query` carries. */
export function readFilters(query: URLSearchParams): FleetFilters {
  const clauses = Object.fromEntries(CLAUSES.map(({ key }) => [key, query.getAll(key)])) as
    Record<ClauseKey, string[]>;
  return { status: query.get("status"), clauses, limit: query.get("limit") ?? "" };
}

/** The status the listing is read at: the default until the reader picks one. */
export const effectiveStatus = (filters: FleetFilters) => filters.status ?? DEFAULT_STATUS;

/** Each clause's typed fields, in order, a blank one dropped. */
function occurrences(filters: FleetFilters): [string, string][] {
  // The route refuses an empty flag list (`400`), so a blank field is never sent.
  return CLAUSES.flatMap(({ key }) =>
    filters.clauses[key].map((value) => value.trim()).filter(Boolean).map((value): [string, string] => [key, value]));
}

/**
 * The filters as the page's own query pairs. The status is always present once
 * picked, so `status=` (every status) stays apart from no status (the default).
 */
export function pagePairs(filters: FleetFilters): [string, string][] {
  return [["status", effectiveStatus(filters)], ...occurrences(filters), ["limit", filters.limit]];
}

/** The devices a listing draws (the classic `Branch`); an org's `count` is `OrgView.client_count`, `null` unread. */
export type DeviceBranch =
  | { kind: "all" }
  | { kind: "me" }
  | { kind: "org"; org_id: string; slug: string; count: number | null }
  | { kind: "server" };

/** The `GET /api/v1/clients` pairs that narrow the fleet to `branch`. */
function scope(branch: DeviceBranch): [string, string][] {
  switch (branch.kind) {
    case "all": return [];
    case "me": return [["owner", "mine"]];
    case "org": return [["org_id", branch.org_id]];
    case "server": return [["owned", "false"]];
  }
}

/** The `GET /api/v1/clients` query for `branch`'s devices, from `cursor` on. */
export function fleetQuery(branch: DeviceBranch, filters: FleetFilters, cursor: string | null): string {
  const status = effectiveStatus(filters);
  const pairs: [string, string][] = [...scope(branch), ...occurrences(filters)];
  // The route reads an absent status as every status.
  if (status) pairs.unshift(["status", status]);
  if (filters.limit) pairs.push(["limit", filters.limit]);
  if (cursor) pairs.push(["cursor", cursor]);
  return new URLSearchParams(pairs).toString();
}

/**
 * Why a page of `branch` drew no device, as `clients.html` words it. A cursor
 * page carries no total, so an empty branch is told apart only with no filter
 * at all, or by an organization's count.
 */
export function emptyNotice(branch: DeviceBranch, filters: FleetFilters, cursor: string | null): string {
  const status = effectiveStatus(filters);
  if (needsRequires(filters)) {
    return "Capability filters need at least one requires flag. Other clauses only narrow the selection.";
  }
  if (branch.kind === "org" && branch.count === 0) {
    return `${branch.slug} owns no devices, directly or through a member.`;
  }
  if (cursor) return "No clients at this position. Go back to the first page.";
  if (filtering(filters)) {
    return branch.kind === "server" ? "No unclaimed devices match these filters." : "No clients match these filters.";
  }
  switch (branch.kind) {
    case "all": return status ? `No client is ${status}.` : "No clients registered.";
    case "me": return status ? `You own no ${status} devices.` : "You own no devices.";
    case "org": return status ? `${branch.slug} owns no ${status} devices.`
      : `${branch.slug} owns no devices, directly or through a member.`;
    case "server": return status ? `No unclaimed device is ${status}.` : "Every device has an owner.";
  }
}

/** Whether a tag or capability clause is in use, which opens the folded filters. */
export const filtering = (filters: FleetFilters) => occurrences(filters).length > 0;

/**
 * The narrowing clauses without a `requires` flag, which the route answers with
 * no client at all.
 */
export function needsRequires(filters: FleetFilters): boolean {
  const used = new Set(occurrences(filters).map(([key]) => key));
  return !used.has("requires") && ["any_of", "excludes", "not_all_of"].some((key) => used.has(key));
}

/** A row's standing column, as the classic `standing` worded it. */
export const standing = (row: ClientListRow): string =>
  [row.suspended && "suspended", row.reindex_pending && "reindexing"].filter(Boolean).join(", ") || "ok";

/** How recent a device's last-seen time must be for the device to count as active. */
export const ACTIVE_WINDOW_MS = 10 * 60_000;

export const ACTIVITIES = ["active", "inactive", "never"] as const;
export type Activity = (typeof ACTIVITIES)[number];

/**
 * A row's activity at `now`: `active` if the server saw the device within
 * `ACTIVE_WINDOW_MS`, `inactive` if it saw the device before that, and `never`
 * if the server holds no last-seen time for it.
 */
export function activity(row: ClientListRow, now: number): Activity {
  if (!row.last_seen_at) return "never";
  return now - Date.parse(row.last_seen_at) <= ACTIVE_WINDOW_MS ? "active" : "inactive";
}

/** The accounts a page's rows are owned through, each once, for the `owned` column's names. */
export const throughOwners = (rows: ClientListRow[]): string[] =>
  [...new Set(rows.flatMap((row) => (row.owner?.type === "user" ? [row.owner.user_id] : [])))];

/**
 * A device listing's columns and filters; `owned` where it names whom a row is owned through, by `names`.
 * The `activity` column and filter compare each row with `now`.
 */
export function deviceTable(owned: boolean, names: Record<string, string>, now: number): TableSpec<ClientListRow> {
  const through = (row: ClientListRow) =>
    (row.owner?.type === "user" ? names[row.owner.user_id] ?? row.owner.user_id : "directly");
  const list = (items: readonly string[] | undefined) => (items?.length ? JSON.stringify(items) : null);
  return {
    columns: [
      { key: "client_id", label: "client_id", raw: (row) => row.client_id, href: (row) => clientLink(row.client_id) },
      { key: "device_name", label: "device_name", raw: (row) => row.device_name ?? null },
      { key: "status", label: "status", raw: (row) => row.status, as: "state" },
      { key: "activity", label: "activity", raw: (row) => activity(row, now), as: "state" },
      { key: "device_os_name", label: "device_os_name", raw: (row) => row.device_os_name ?? null },
      { key: "device_os_version", label: "device_os_version", raw: (row) => row.device_os_version ?? null, shown: false },
      { key: "device_chip_model", label: "device_chip_model", raw: (row) => row.device_chip_model ?? null },
      ...(owned ? [{ key: "owned", label: "owned", raw: through }] : []),
      { key: "permissions", label: "permissions", raw: (row) => list(row.permissions), as: "json" },
      { key: "standing", label: "standing", raw: standing },
      { key: "backlog", label: "backlog", raw: (row) => row.backlog?.job_count ?? null, num: true },
      { key: "registered_at", label: "registered_at", raw: (row) => row.registered_at, as: "time" },
      { key: "last_seen_at", label: "last_seen_at", raw: (row) => row.last_seen_at, as: "time" },
      { key: "tags", label: "tags", raw: (row) => list(row.tags), as: "json", shown: false },
      { key: "capabilities", label: "capabilities", raw: (row) => list(row.capabilities), as: "json", shown: false },
      { key: "device_form_factor", label: "device_form_factor", raw: (row) => row.device_form_factor ?? null, shown: false },
      { key: "device_ram_bytes", label: "device_ram_bytes", raw: (row) => row.device_ram_bytes ?? null, num: true,
        shown: false },
      { key: "device_gpu_model", label: "device_gpu_model", raw: (row) => row.device_gpu_model ?? null, shown: false },
      { key: "device_npu_model", label: "device_npu_model", raw: (row) => row.device_npu_model ?? null, shown: false },
      { key: "organization", label: "organization", raw: (row) => row.organization || null, shown: false },
      { key: "client_details", label: "client_details", raw: (row) => row.client_details || null, shown: false },
    ],
    texts: [
      { key: "id", label: "client_id", texts: (row) => [row.client_id] },
      { key: "q", label: "device_name, OS or chip",
        texts: (row) => [row.device_name, row.contact_email, row.device_os_name, row.device_chip_model] },
    ],
    choices: [
      { key: "os", label: "device_os_name", values: (row) => [row.device_os_name ?? ""] },
      { key: "chip", label: "device_chip_model", values: (row) => [row.device_chip_model ?? ""] },
      { key: "permission", label: "permissions", values: (row) => row.permissions },
      { key: "standing", label: "standing", values: (row) => [standing(row)] },
      { key: "activity", label: "activity", values: (row) => [activity(row, now)], order: ACTIVITIES },
    ],
  };
}
