// The plan pages as the classic console drew them (`plans.html`, `plan.html`,
// `plan_body.html`, `results.html`), over the `/api/v1/plans` routes.

import { campaignLink, planLink } from "./routes";
import type { TableSpec } from "./table";
import { doneOf } from "./format";
import type { PlanJobsWithResults } from "./api";
import type {
  Counts, DocumentState, FailureSource, FleetDetail, JobQueue, Lease, OwnUserView, PlanStatus, PlanView,
  RevisionChange, StarvedGroup,
} from "./api-types";

const counted = (n: number, noun: string) => `${n} ${noun}${n === 1 ? "" : "s"}`;

/** A revision's change in a sentence, as `pipette-mgmt plans status` prints it. */
export function changeText(change: RevisionChange): string {
  switch (change.type) {
    case "rerun":
      if (change.answered === "failed") return `Re-run of all failed jobs (${change.job_count}).`;
      if (change.answered === "all") return `Re-run of all finished or failed jobs (${change.job_count}).`;
      return `Re-run of ${counted(change.job_count, "job")}.`;
    case "revert":
      return `Reverted to ${change.to_plan_id}.`;
    case "recalculate":
      return change.all_evals
        ? `Recalculation of all finished evals (${change.job_count}).`
        : `Recalculation of ${counted(change.job_count, "eval")}.`;
    case "variants":
    case "document": {
      const head = change.type === "variants"
        ? `Variant edit: ${change.removed_variant_count} removed, ${change.added_variant_count} added.`
          + (change.restored ?? []).map((variant) => ` Restored variant ${variant.variant_index} of ${variant.plan_id}.`).join("")
        : "Document revised.";
      const earlier = change.carry_earlier ? " Carries runs of earlier plans." : "";
      const named = change.inherit_from?.length ? ` Inherits runs of ${change.inherit_from.join(", ")}.` : "";
      return head + earlier + named;
    }
  }
}

/** A plan's status as the app draws it: a complete plan says where any of its jobs failed. */
export type ShownStatus = PlanStatus | "complete_with_failures";

/** Every status a listing filters on, in `PlanStatus::iter` order, the drawn one after the status it is read off. */
export const PLAN_STATUSES: ShownStatus[] =
  ["creating", "active", "pending_clients", "cancelling", "complete", "complete_with_failures", "cancelled"];

export const shownStatus = (status: PlanStatus, counts: Pick<Counts, "failed"> | null | undefined): ShownStatus =>
  (status === "complete" && counts?.failed ? "complete_with_failures" : status);

/** The status filter's choices a plan is kept under: its drawn status, and `complete` too where that has failures. */
export const statusChoices = (status: PlanStatus, counts: Pick<Counts, "failed"> | null | undefined): ShownStatus[] => {
  // The stored status and the server's `status` filter stay `complete`, so that choice keeps these plans too.
  const shown = shownStatus(status, counts);
  return shown === "complete_with_failures" ? ["complete", shown] : [shown];
};

/** The share of its jobs a plan is done with, which a listing sorts by; null where none is counted. */
export const doneShare = (counts: Counts | null | undefined): number | null =>
  (counts?.total ? doneOf(counts) / counts.total : null);

/** What a listing is narrowed to, as its address carries it; an empty value is no filter. */
export interface PlanFilters {
  scope: string;
  status: string;
  submitted_by: string;
  campaign_id: string;
}

/** The filters in `query`, absent ones empty. */
export const planFilters = (query: URLSearchParams): PlanFilters => ({
  scope: query.get("scope") ?? "",
  status: query.get("status") ?? "",
  submitted_by: query.get("submitted_by") ?? "",
  campaign_id: query.get("campaign_id") ?? "",
});

/** The filters as the listing's address keeps them, in the classic `plans_href` order. */
export const filterPairs = (filters: PlanFilters): [string, string][] =>
  [["submitted_by", filters.submitted_by], ["campaign_id", filters.campaign_id], ["scope", filters.scope],
   ["status", filters.status]];

/**
 * The `GET /api/v1/plans` query for `filters`: newest first, as the classic listing
 * reads, each submitter named.
 */
export function planQuery(filters: PlanFilters, cursor: string | null, limit: string): string {
  const pairs: [string, string][] = [["order", "newest"], ["names", "true"]];
  for (const [key, value] of filterPairs(filters)) if (value) pairs.push([key, value]);
  if (limit) pairs.push(["limit", limit]);
  if (cursor) pairs.push(["cursor", cursor]);
  return new URLSearchParams(pairs).toString();
}

/** The plans `userId` filed, as the account page lists them. */
export const filedQuery = (userId: string, cursor: string | null, limit: string): string =>
  planQuery({ scope: "", status: "", submitted_by: userId, campaign_id: "" }, cursor, limit);

/**
 * The scope a listing was taken at, as `PlanScope::label` echoes it: the one
 * asked for, else every plan for a reader of every plan, else its own.
 */
export function takenScope(reader: OwnUserView, asked: string): "all" | "mine" {
  if (asked === "all" || asked === "mine") return asked;
  return reader.may.read_plans === "every" ? "all" : "mine";
}

/** Why the account page's plans are empty, as `user.html` words it. */
export const filedEmpty = (scope: "all" | "mine"): string =>
  scope === "mine"
    ? "No plans that this account filed into a campaign you may see. Its other plans need read_any_plans."
    : "No plans filed by this account.";

/** The longest failure reason a page draws: the app's own bound, 4 KiB as the classic pages drew. */
export const MAX_DRAWN_REASON_BYTES = 4096;

/** `text` cut to its last `max` UTF-8 bytes behind an ellipsis, as plan-types' `keep_tail` cuts it. */
export function keepTail(text: string, max = MAX_DRAWN_REASON_BYTES): string {
  const bytes = new TextEncoder().encode(text);
  if (bytes.length <= max) return text;
  const ellipsis = "…";
  let start = bytes.length - Math.max(0, max - new TextEncoder().encode(ellipsis).length);
  // Forward to the next character's first byte, as `ceil_char_boundary` does.
  while (start < bytes.length && (bytes[start]! & 0xc0) === 0x80) start++;
  return ellipsis + new TextDecoder().decode(bytes.subarray(start));
}

const FAILED_BY: Record<FailureSource, string> = {
  client: "reported by its client",
  expired: "expired",
  all_clients_denied: "every client denied it",
  unmatchable: "no client can run it",
  lease_lost: "retired for losing its leases",
};

/** A source withheld below read_clients arrives as null, so it reads plain `failed`. */
export const failedText = (source: FailureSource | null): string =>
  source ? `failed: ${FAILED_BY[source]}` : "failed";

/** Why pipette-duckdb read no results, as a notice says it. */
export const readFailure = (status: "timed_out" | "failed"): string =>
  status === "timed_out" ? "it timed out" : "it failed";

/** A clause list as `matching::flags_label` and `groups_label` write it. */
const flags = (names: string[]) => names.join(", ");
const groups = (sets: string[][]) => sets.map((set) => `[${flags(set)}]`).join(" & ");

/** A starved group's eligibility in one line (`StarvedClauses::describe`). */
export function describeClauses(group: StarvedGroup): string {
  const clauses = [
    group.requires?.length ? `requiring ${flags(group.requires)}` : null,
    group.excludes?.length ? `excluding flags ${flags(group.excludes)}` : null,
    group.any_of?.length ? `with any-of ${groups(group.any_of)}` : null,
    group.not_all_of?.length ? `with not-all-of ${groups(group.not_all_of)}` : null,
    group.clients?.length ? `pinned to ${flags(group.clients)}` : null,
    group.exclude_clients?.length ? `excluding clients ${flags(group.exclude_clients)}` : null,
  ].filter((clause) => clause !== null);
  return clauses.length ? clauses.join(" ") : "with no eligibility";
}

/**
 * One starved group as `plans status` prints it. Below the full tier a group
 * keeps its jobs alone, so its clauses read as withheld.
 */
export function starvedLine(group: StarvedGroup, fleet: FleetDetail): string {
  const what = fleet === "full" ? describeClauses(group) : "requirement set withheld";
  return `${group.job_ids.length} job(s) ${what}`;
}

/** The note `plan_body.html` draws in place of the document, or `null` where it draws the text or nothing. */
export function documentNote(state: DocumentState, fleet: FleetDetail): { text: string; fault: boolean } | null {
  if (state === "present") {
    return fleet === "full" ? null
      : { text: "The plan document is withheld: reading it needs read_clients or read_any_clients.", fault: false };
  }
  if (state === "collected") return { text: "The plan document was collected.", fault: false };
  if (state === "mismatch") return { text: "The stored plan document does not match its digest.", fault: true };
  return null;
}

/** The withheld tier's notice, once for the jobs and the results, naming what results hide only where they are read. */
export const withheldNotice = (results: boolean) =>
  `Fleet detail is withheld: reading clause sets, lease holders${results ? ", devices and failure reasons" : ""} ` +
  "needs read_clients or read_any_clients.";

/** Whether the plan's page offers a cancel: to a holder of its grant, on a plan neither terminal nor cancelling. */
export const cancelOffered = (status: PlanStatus, may: boolean): boolean =>
  may && status !== "complete" && status !== "cancelled" && status !== "cancelling";

/** True if the reader has at least one write in the Settings of the plan: a note change, a re-run, or a cancel that no one has requested yet. */
export const planWrites = (plan: Pick<PlanView, "status" | "cancel_requested">,
                           jobs: Pick<PlanJobsWithResults, "rerun_campaign_id" | "may_cancel" | "may_edit_note">): boolean =>
  jobs.may_edit_note || !!jobs.rerun_campaign_id
  || (cancelOffered(plan.status, jobs.may_cancel) && !plan.cancel_requested);

/** Units pipette-datasheet shows in another (`fmtUnit`): a ratio as %, bytes as MB, ms as seconds. */
export const SHOWN_UNIT: Record<string, string> = { ratio: "%", bytes: "MB", ms: "secs" };

export const digits = (n: number, min: number, max: number) =>
  n.toLocaleString("en-US", { minimumFractionDigits: min, maximumFractionDigits: max });

/** `n` of `unit` in the unit [`SHOWN_UNIT`] names. */
const scaled = (n: number, unit: string): number =>
  (unit === "ratio" ? n * 100 : unit === "bytes" ? n / 2 ** 20 : unit === "ms" ? n / 1000 : n);

/** A number in `unit` as pipette-datasheet prints it (`fmtValue`), grouped as en-US whatever the reader's locale. */
export function sheetValue(n: number, unit: string): string {
  if (unit === "ratio") return scaled(n, unit).toFixed(2);
  if (unit === "bytes") return digits(scaled(n, unit), 0, 0);
  if (unit === "ms") return digits(scaled(n, unit), 3, 3);
  if (unit === "tokens/sec") return digits(n, 2, 2);
  return digits(n, 0, 2);
}

/** A value with its unit as pipette-datasheet shows them (`fmtValueWithUnitLabel`): `40.80%`, `0.058 secs`. */
export function withUnit(value: number, unit: string): string {
  const label = SHOWN_UNIT[unit] ?? unit;
  return `${sheetValue(value, unit)}${label === "%" ? "%" : label ? ` ${label}` : ""}`;
}

/**
 * A stddev in the unit its value shows in, to three significant digits so a small one never reads as zero, and
 * that unit named: a ratio's in percentage points, which a relative percent is not.
 */
export function stddevText(stddev: number, unit: string): string {
  const n = scaled(stddev, unit);
  const label = unit === "ratio" ? "pp" : SHOWN_UNIT[unit] ?? unit;
  return `±${n ? digits(Number(n.toPrecision(3)), 0, 20) : "0"}${label ? ` ${label}` : ""}`;
}

/** The stddev relative to the value, in percent; none on a zero value or no stddev. */
export function relativePercent(value: number | null, stddev: number | null): number | null {
  return value === null || stddev === null || !value ? null : Math.abs(stddev / value) * 100;
}

/** [`relativePercent`] where pipette-datasheet tiers it (`stddevCovTier`): never on a ratio. */
export const spreadPercent = (value: number | null, stddev: number | null, unit: string): number | null =>
  (unit === "ratio" ? null : relativePercent(value, stddev));

/** A measured value with its unit, then its stddev absolute and relative; an em-rule for none. */
export function measured(value: number | null, stddev: number | null, unit: string): string {
  if (value === null) return "—";
  if (stddev === null) return withUnit(value, unit);
  const relative = relativePercent(value, stddev);
  return `${withUnit(value, unit)} ${stddevText(stddev, unit)}${relative === null ? "" : ` (±${digits(relative, 0, 1)}%)`}`;
}

/** Bytes as pipette-datasheet's `fmtBytes` prints them: binary units, one decimal under 100. */
export function bytesText(n: number): string {
  const units = ["B", "KB", "MB", "GB", "TB"];
  let i = 0;
  let scaled = n;
  while (Math.abs(scaled) >= 1024 && i < units.length - 1) {
    scaled /= 1024;
    i++;
  }
  return `${scaled.toFixed(i === 0 || Math.abs(scaled) >= 100 ? 0 : 1)} ${units[i]}`;
}

/**
 * A parameter's value as the table and the charts write it: a string unquoted,
 * anything else as JSON (the classic template's minijinja spaced lists and said `none`).
 */
export const parameterValue = (value: unknown): string => (typeof value === "string" ? value : JSON.stringify(value));

/** A result row's parameters as `name=value, …`. */
export const parametersText = (parameters: Record<string, unknown>): string =>
  Object.entries(parameters).map(([name, value]) => `${name}=${parameterValue(value)}`).join(", ");

/** A job's lease cell (`plan_body.html`), lapsed once it expires without a heartbeat. */
export const leaseText = (lease: Pick<Lease, "expires_at"> | null, when: (iso: string) => string, now = Date.now()): string =>
  lease ? `until ${when(lease.expires_at)}${Date.parse(lease.expires_at) <= now ? " (lapsed)" : ""}` : "—";

/** A job's expiry cell: the queue's deadline, `null` for never, expired once past, as the queue sweep reads it. */
export const expiresText = ({ expires_at }: Pick<JobQueue, "expires_at">, when: (iso: string) => string, now = Date.now()): string =>
  expires_at === null ? "never" : `${when(expires_at)}${Date.parse(expires_at) < now ? " (expired)" : ""}`;

/**
 * The name of the file that a download saves the plan's results to. The name comes from `filename*` in the server's
 * `Content-Disposition`. If the header has no `filename*`, or its escape is malformed, the name is
 * `<planId>-results.csv`.
 */
export function resultsFilename(planId: string, disposition: string | null): string {
  const encoded = disposition?.match(/filename\*=UTF-8''([^;\s]+)/i)?.[1];
  if (encoded) {
    try {
      return decodeURIComponent(encoded);
    } catch {
      // A malformed escape falls through to the plan's own name.
    }
  }
  return `${planId}-results.csv`;
}

/** A plan listing's columns and filters; `submitters` where its rows name who filed them, by `names`. */
export function planTable(names: Record<string, string>, submitters: boolean): TableSpec<PlanView> {
  const submitter = (plan: PlanView) => (plan.submitted_by ? names[plan.submitted_by] ?? plan.submitted_by : "operator");
  const counts = (plan: PlanView) => plan.progress_snapshot?.counts;
  return {
    columns: [
      { key: "plan_id", label: "plan_id", raw: (plan) => plan.plan_id, href: (plan) => planLink(plan.plan_id) },
      { key: "note", label: "note", raw: (plan) => plan.note ?? null },
      { key: "status", label: "status", raw: (plan) => shownStatus(plan.status, counts(plan)) },
      { key: "created_at", label: "created_at", raw: (plan) => plan.created_at, as: "time" },
      ...(submitters ? [{ key: "submitted_by", label: "submitted_by", raw: submitter }] : []),
      { key: "progress", label: "progress", raw: (plan) => doneShare(counts(plan)) },
      { key: "campaign_id", label: "campaign_id", raw: (plan) => plan.campaign_id ?? null, shown: false,
        href: (plan) => (plan.campaign_id ? campaignLink(plan.campaign_id) : null) },
      { key: "previous_plan_id", label: "previous_plan_id", raw: (plan) => plan.previous_plan_id ?? null, shown: false,
        href: (plan) => (plan.previous_plan_id ? planLink(plan.previous_plan_id) : null) },
      { key: "terminal_at", label: "terminal_at", raw: (plan) => plan.terminal_at, as: "time", shown: false },
    ],
    texts: [
      { key: "id", label: "plan_id", texts: (plan) => [plan.plan_id] },
      { key: "q", label: "note", texts: (plan) => [plan.note] },
    ],
    choices: [
      { key: "status", label: "status", values: (plan) => statusChoices(plan.status, counts(plan)),
        name: (value) => value.replaceAll("_", " "),
        order: PLAN_STATUSES },
      ...(submitters ? [{ key: "by", label: "submitted_by", values: (plan: PlanView) => [submitter(plan)] }] : []),
    ],
  };
}
