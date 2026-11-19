// The campaign group pages: an owner's groups on its campaign listing's Groups tab (`GET /api/v1/campaign-groups`),
// and one group's tabs (`GET /api/v1/campaign-groups/{group_id}`): its members with the owner's other campaigns to
// add, its policies (`…/policies`), their activity as their preview (`…/policy-preview`) and their recorded re-runs
// (`…/rerun-results`) read it, and its Settings.

import type { BarTab } from "./bar";
import { labelPairs, labelText, mayLabel, stateOf } from "./campaigns";
import type { Crumb } from "./crumbs";
import { principalOf, type Under } from "./owned";
import { bytesText, digits } from "./plans";
import { campaignPlanLink, pathOf } from "./routes";
import { csvOf, type Choice, type Raw, type TableSpec } from "./table";
import type {
  CampaignGroupPolicy, CampaignView, FailureCode, MemberPolicyPreview, ModelParamsGap, OwnUserView, PolicyCheckCounts,
  PolicyFiling, PolicyKind, PolicyMemberState, PolicyRerun, PolicyRerunSummary, PolicySettings, PrincipalOwner,
  SetCampaignGroupPolicyRequest, SmallerFailure, UserPermission,
} from "./api-types";

/** Where an owner's campaign listing at `base` lists its groups. */
export const groupsPath = (base: string): string => `${base}/groups`;

/** An owner's campaign listing's tabs: its campaigns, which it opens on, and its groups. */
export const listingTabs = (base: string, groups: boolean): BarTab[] => [
  { key: "campaigns", label: "Campaigns", href: base, current: !groups, start: false },
  { key: "groups", label: "Groups", href: groupsPath(base), current: groups, start: false },
];

const GROUP_TABS = {
  campaigns: { page: "campaign-group", label: "Campaigns" },
  policies: { page: "campaign-group-policies", label: "Policies" },
  activity: { page: "campaign-group-activity", label: "Activity" },
  settings: { page: "campaign-group-settings", label: "Settings" },
} as const;

/** A group page's tabs, each an address of its own. */
export type GroupTab = keyof typeof GROUP_TABS;

/**
 * A group's tabs: its Campaigns, Policies and Activity, and set apart its Settings for a reader who may change them
 * (`settings`). The open tab is listed whatever, so the bar always says where the reader is.
 */
export function groupTabs(group_id: string, current: GroupTab, settings: boolean): BarTab[] {
  const tab = (name: GroupTab, start = false): BarTab => ({
    key: name, label: GROUP_TABS[name].label, current: current === name, start,
    href: pathOf({ page: GROUP_TABS[name].page, group_id }),
  });
  return [tab("campaigns"), tab("policies"), tab("activity"),
          ...(settings || current === "settings" ? [tab("settings", true)] : [])];
}

/** The Groups tab of the listing a group's page sits `under`; `null` under `/app/campaigns`, which has no such tab. */
export const groupsCrumb = (under: Under): Crumb | null =>
  (under.href === pathOf({ page: "campaigns" }) ? null : { label: "Groups", href: groupsPath(under.href) });

/**
 * Whether `reader` may create `owner`'s groups and change their members and policies, as the routes judge filing:
 * `submit_plans`, and `owner` is the reader or its organization unless it holds `manage_any_orgs`. The route still
 * decides.
 */
export const mayFile = (reader: OwnUserView, owner: PrincipalOwner): boolean =>
  reader.may.file_plans && (reader.may.administer_roster === "every"
    || (owner.type === "org" ? owner.org_id === reader.org_id : owner.user_id === reader.user_id));

/**
 * Whether the group page offers the rename and the delete, which take a campaign rename's rights (`mayLabel`). An
 * organization's administrator also administers a disabled member's group, the only other account's group it can
 * open unless it reads every plan.
 */
export const mayAdminister = (reader: OwnUserView, owner: PrincipalOwner): boolean =>
  mayLabel(reader, owner)
  || (owner.type === "user" && owner.user_id !== reader.user_id && reader.may.administer_roster === "own"
    && reader.may.read_plans !== "every");

/** Each member with the plan `listed` reads for it. */
export function memberRows(members: readonly CampaignView[], listed: readonly CampaignView[]): CampaignView[] {
  const read = new Map(listed.map((row) => [row.campaign_id, row]));
  return members.map((member) => ({ ...member, plan: read.get(member.campaign_id)?.plan ?? member.plan }));
}

/** What the add offers: each of `listed` that has the group's owner and is not a member, searchable by its labels. */
export function memberOffers(listed: readonly CampaignView[],
                             group: { owner: PrincipalOwner; members: readonly Pick<CampaignView, "campaign_id">[] }): Choice[] {
  const members = new Set(group.members.map((member) => member.campaign_id));
  return listed
    .filter((row) => principalOf(row.owner) === principalOf(group.owner) && !members.has(row.campaign_id))
    .map((row) => ({
      value: row.campaign_id,
      label: stateOf(row) === "active" ? row.name : `${row.name} (${stateOf(row)})`,
      searchText: labelPairs(row.labels).map(labelText).join("\n"),
    }));
}

/** The most re-runs a policy gives a job, as the server caps it (`pipette_plan_types::MAX_RERUNS`). */
export const MAX_RERUNS = 15;

/**
 * Whether a `kind` policy re-runs or marks jobs, as `PolicyKind::reruns_or_marks` says; model_params and smaller_failed
 * only flag.
 */
export const rerunsOrMarks = (kind: PolicyKind): boolean => kind !== "model_params" && kind !== "smaller_failed";

/** Whether a `kind` policy takes its re-runs in `max_reruns`; failed takes them per failure code in its settings. */
const rerunsOwn = (kind: PolicyKind): boolean => rerunsOrMarks(kind) && kind !== "failed";

/** The fewest re-runs a `kind` policy takes, as `PolicyKind::min_reruns` sets them. */
export const minReruns = (kind: PolicyKind): number => (kind === "swap" || !rerunsOwn(kind) ? 0 : 1);

/** The most re-runs a `kind` policy takes, as `PolicyKind::max_reruns` sets them. */
export const maxReruns = (kind: PolicyKind): number => (rerunsOwn(kind) ? MAX_RERUNS : 0);

/** Every failure code, in the server's order, each a box of the failed policy. */
export const FAILURE_CODES = [
  "unspecified", "out_of_memory", "attempt_timeout", "runtime_error", "device_error", "device_not_ready",
  "harness_error",
] as const satisfies readonly FailureCode[];

/** `codes` as one phrase, `a`, `a or b`, `a, b or c`, in FAILURE_CODES order so one set reads the same in any order. */
const anyOf = (codes: readonly FailureCode[]): string => orList(FAILURE_CODES.filter((code) => codes.includes(code)));

/** `parts` as one phrase, `a`, `a or b`, `a, b or c`. */
const orList = (parts: readonly string[]): string =>
  (parts.length > 1 ? `${parts.slice(0, -1).join(", ")} or ${parts.at(-1)}` : parts.join(""));

/** A check's settings, failed's codes as the ones it re-runs, as a re-run records them. */
type Rule = Omit<PolicySettings, "codes"> & { codes?: readonly FailureCode[] | null };

/** The codes failed's `codes` lists, in FAILURE_CODES order. */
const listed = (codes: PolicySettings["codes"]): FailureCode[] => FAILURE_CODES.filter((code) => codes?.[code] != null);

/**
 * A check's title, what it finds, and what follows: for a check that re-runs, what becomes of a job it still finds
 * after the last re-run.
 */
interface Check {
  title: (rule: Rule) => string;
  finds: (rule: Rule) => string;
  still: string;
}

/** Whether `rule` weighs a row of short repetitions against a percent of its own. */
const short = ({ short_repetition_ms: ms, short_repetition_percent: percent }: Rule): boolean =>
  ms != null && percent != null;

/** Each check, fixed per kind; only spread and failed read their settings. */
const CHECKS: Record<PolicyKind, Check> = {
  swap: {
    title: () => "Swap", finds: () => "device swapped memory during the run",
    still: "If it still swaps, the job is marked failed.",
  },
  spread: {
    title: (rule) => `Stddev over ${rule.percent}%${
      short(rule) ? ` (${rule.short_repetition_percent}% under ${rule.short_repetition_ms} ms)` : ""}`,
    finds: (rule) => `standard deviation is over ${rule.percent}% of its result${short(rule)
      ? `, or ${rule.short_repetition_percent}% where its repetitions average under ${rule.short_repetition_ms} ms`
      : ""}`,
    still: "If it is still over, the job is marked failed.",
  },
  failed: {
    title: ({ codes }) => (codes ? `Failures by type (${anyOf(codes)})` : "Failures by type"),
    finds: ({ codes }) => `device reported a failure coded ${anyOf(codes ?? [])}`,
    still: "If it still fails, the job keeps the failure.",
  },
  ttft_over_e2e: {
    title: () => "TTFT over E2E latency", finds: () => "time to first token is over its end-to-end latency",
    still: "If it still is, the job keeps its last result.",
  },
  model_params: {
    title: () => "Missing model params",
    finds: () => "model has no entry in the model params table, so they show no parameter counts",
    still: "It re-runs and marks nothing: the rows it flags, and the form to add their counts, are on the Activity tab.",
  },
  smaller_failed: {
    title: () => "Failed below a clean larger run",
    finds: () => "job failed while a job of the same cell at larger sizes, such as a longer context, ran with no failure",
    still: "It re-runs and marks nothing, and queue-maintenance does not read it: the jobs it flags are on the Activity "
      + "tab.",
  },
};

/** A policy's title, spread's at its stored settings, failed's without the codes its text names. */
export const policyTitle = ({ kind, settings }: CampaignGroupPolicy): string =>
  CHECKS[kind].title({ ...settings, codes: null });

/** A policy's title, marked when it is off, as the Activity tab heads what its check finds. */
export const stateTitle = (policy: CampaignGroupPolicy): string =>
  (policy.enabled ? policyTitle(policy) : `${policyTitle(policy)} (off)`);

/** How often `n` re-runs re-run a job. */
const times = (n: number): string => (n === 1 ? "once" : `up to ${n} times`);

/** What `policy` does with a job its check finds, as stored. */
export function policyText({ kind, max_reruns: n, settings }: CampaignGroupPolicy): string {
  const { finds, still } = CHECKS[kind];
  if (kind === "failed") {
    const each = listed(settings.codes).map((code) => `${code} ${times(settings.codes?.[code] ?? 0)}`);
    return `Re-runs a job whose device reported a failure coded ${orList(each)}. Its runs in a row that failed with `
      + `any of these codes count toward the limit of its last one. ${still}`;
  }
  const whose = `whose ${finds({ ...settings, codes: null })}`;
  if (!rerunsOrMarks(kind)) return `Flags the result rows ${whose}. ${still}`;
  if (n > 0) return `Re-runs a job ${whose}, ${times(n)}. ${still}`;
  // With no re-run, a check that keeps the result does nothing, which is why the route takes none for swap alone.
  const fate = kind === "swap" ? "is marked failed, with no re-run" : "is left as it is, so this policy does nothing";
  return `A job ${whose} ${fate}.`;
}

/** A policy row's values as shown: the stored ones, or what the reader typed over them. */
export interface PolicyFields {
  max_reruns: string;
  percent: string;
  short_repetition_ms: string;
  short_repetition_percent: string;
  /** Failed's re-runs of each failure code; empty or 0 leaves the code out. */
  codes: Partial<Record<FailureCode, string>>;
}

/** Failed's codes as typed, each one that re-runs with its re-runs. */
const typedCodes = (codes: PolicyFields["codes"]): Partial<Record<FailureCode, number>> =>
  Object.fromEntries(FAILURE_CODES.filter((code) => Number(codes[code] || 0) !== 0)
    .map((code) => [code, Number(codes[code])]));

/**
 * A write over `stored`, sent as `base`. `"off"` and `"on"` send the stored values: the route keeps them on a Turn off,
 * and they are what the preview counted for a Turn on.
 */
export function policyBody(stored: CampaignGroupPolicy,
                           fields: PolicyFields | "off" | "on"): SetCampaignGroupPolicyRequest {
  const base = { enabled: stored.enabled, max_reruns: stored.max_reruns, settings: stored.settings };
  if (fields === "off" || fields === "on") return { ...base, enabled: fields === "on", base };
  const { percent, short_repetition_ms: ms, short_repetition_percent: short } = fields;
  const rule = ms === "" && short === "" ? {} : { short_repetition_ms: Number(ms), short_repetition_percent: Number(short) };
  return {
    enabled: stored.enabled,
    max_reruns: Number(fields.max_reruns),
    settings: stored.kind === "spread" ? { percent: Number(percent), ...rule }
      : stored.kind === "failed" ? { codes: typedCodes(fields.codes) } : {},
    base,
  };
}

/** Why the route would refuse `max_reruns` as typed for a `kind` policy, or `""`. */
export function rerunsProblem(kind: PolicyKind, max_reruns: string): string {
  const [least, most] = [minReruns(kind), maxReruns(kind)];
  if (!/^\d+$/.test(max_reruns) || Number(max_reruns) < least || Number(max_reruns) > most) {
    return `Enter a whole number of re-runs from ${least} to ${most}`;
  }
  return "";
}

/** Why the route would refuse `percent` as typed, or `""`; only spread takes one. */
export function percentProblem(kind: PolicyKind, percent: string): string {
  const share = Number(percent);
  if (kind === "spread" && !(Number.isFinite(share) && share > 0)) return "Enter a percent above 0";
  return "";
}

/** Why the route would refuse spread's short repetition time and percent as typed, or `""`: it takes both or neither. */
export function shortProblem(kind: PolicyKind, ms: string, percent: string): string {
  if (kind !== "spread" || (ms === "" && percent === "")) return "";
  const positive = (typed: string) => typed !== "" && Number.isFinite(Number(typed)) && Number(typed) > 0;
  return positive(ms) && positive(percent) ? "" : "Enter a repetition time and its percent above 0, or neither";
}

/** Why the route would refuse failed's codes as typed, or `""`: at least one, each with 1 to MAX_RERUNS re-runs. */
export function codesProblem(kind: PolicyKind, codes: PolicyFields["codes"]): string {
  if (kind !== "failed") return "";
  const typed = Object.values(codes).filter((n) => n !== "" && n !== undefined);
  if (typed.some((n) => !/^\d+$/.test(n) || Number(n) > MAX_RERUNS)) {
    return `Enter a whole number of re-runs from 0 to ${MAX_RERUNS} for each failure code`;
  }
  return Object.keys(typedCodes(codes)).length ? "" : "Enter re-runs for at least one failure code";
}

/**
 * The grants the policy preview takes that `reader` lacks: `read_plans` for the members' plans, and `read_clients` for
 * the device failures the failed check counts.
 */
export const previewLacks = (reader: OwnUserView): UserPermission[] => [
  ...(reader.may.read_plans === "none" ? ["read_plans" as const] : []),
  ...(reader.may.read_fleet ? [] : ["read_clients" as const]),
];

/** A member campaign's state in the preview: checked, or why not and what becomes of it. */
const MEMBER_STATES: Record<PolicyMemberState, (row: MemberPolicyPreview) => string> = {
  evaluated: () => "checked",
  other_owner: () => "another owner: left as it is",
  no_plan: () => "archived, or no plan with a document: left as it is",
  not_ready: () => "plan being created: not checked yet",
  cancelled: () => "plan stopped: left as it is",
  unreadable: () => "not read: pipette-duckdb failed or timed out",
  waiting_for_devices: ({ filing_plan_id, unclaimed_job_count: n }) =>
    `waiting for devices to take ${n} job${n === 1 ? "" : "s"} of its last group filing, ${filing_plan_id}`,
  filing_not_advanced: ({ filing_plan_id }) => `its last group filing, ${filing_plan_id}, did not advance: held`,
  refused: ({ plan_id }) => `plan ${plan_id} cannot be filed again: left as it is`,
};

const memberState = (row: MemberPolicyPreview): string => MEMBER_STATES[row.state](row);

/** What one check's policy alone would do with the jobs it flags in a member campaign. */
export function checkText({ kind, flagged, rerun, marked, kept }: PolicyCheckCounts): string {
  if (kind === "model_params") return `${flagged} job${flagged === 1 ? "" : "s"} whose rows lack model params`;
  if (kind === "smaller_failed") return `${flagged} failed job${flagged === 1 ? "" : "s"} below a clean larger run`;
  if (!flagged) return "0 flagged";
  // A job left aside, for good or until its re-run chain reads, counts as flagged alone.
  const skipped = flagged - rerun - marked - kept;
  return `${flagged} flagged: ${rerun} re-run, ${marked} marked failed, ${kept} kept${skipped ? `, ${skipped} skipped` : ""}`;
}

/** What a Turn on of `policy` says before it writes: its rule as stored, and when it acts. */
export function turnOnText(policy: CampaignGroupPolicy): string {
  if (policy.kind === "smaller_failed") return policyText(policy);
  return `${policyText(policy)} From the next queue-maintenance run on, it `
    + `${rerunsOrMarks(policy.kind) ? "acts on" : "logs the rows that lack model params in"} each campaign's current `
    + "plan, jobs that finished before it was turned on included.";
}

/** What a Turn off of a policy that is on, of `kind`, changes. */
export function turnOffText(kind: PolicyKind): string {
  if (kind === "smaller_failed") return "Turning it off only marks it off; the Activity tab still shows these jobs.";
  return rerunsOrMarks(kind)
    ? "Turning it off files no new re-runs or marks from the next queue-maintenance run; re-runs it already filed still run."
    : "Turning it off stops queue-maintenance logging these rows from its next run; the Activity tab still shows them.";
}

/** What `kind`'s policy alone would do now with each member campaign of `rows`, a preview's, named by `members`. */
export function checkLines(kind: PolicyKind, rows: readonly MemberPolicyPreview[],
                           members: readonly CampaignView[]): string[] {
  const names = new Map(members.map((member) => [member.campaign_id, member.name]));
  return rows.map((row) => {
    const counts = row.checks.find((check) => check.kind === kind);
    const did = row.state === "evaluated" && counts ? checkText(counts) : memberState(row);
    return `${names.get(row.campaign_id) ?? row.campaign_id}: ${did}`;
  });
}

/** What the policies that are on would do together with a member campaign's jobs. */
export function togetherText({ rerun_job_ids, marked_job_ids, skipped }: MemberPolicyPreview): string {
  const aside = Object.keys(skipped).length;
  return `${rerun_job_ids.length} re-run, ${marked_job_ids.length} marked failed${aside ? `, ${aside} skipped` : ""}`;
}

/**
 * The preview's table: a member campaign a row, named by `names`, and a check a column under its policy's title. Only a
 * checked member has counts; an unreadable one keeps its answered jobs and deadline.
 */
export function policyPreviewTable(policies: readonly CampaignGroupPolicy[],
                                   names: Record<string, string>): TableSpec<MemberPolicyPreview> {
  const checked = (row: MemberPolicyPreview) => row.state === "evaluated";
  return {
    columns: [
      { key: "name", label: "Campaign", raw: (row) => names[row.campaign_id] ?? row.campaign_id },
      { key: "state", label: "State", raw: memberState },
      { key: "answered", label: "Answered jobs", num: true,
        raw: (row) => (checked(row) || row.state === "unreadable" ? row.answered_job_count : null) },
      { key: "waiting", label: "Waiting for rows", num: true, raw: (row) => (checked(row) ? row.waiting_job_count : null) },
      { key: "deadline", label: "Earliest deadline", raw: (row) => row.earliest_deadline },
      ...policies.map((policy) => ({
        key: policy.kind, label: stateTitle(policy),
        raw: (row: MemberPolicyPreview) => {
          const counts = row.checks.find((check) => check.kind === policy.kind);
          return checked(row) && counts ? checkText(counts) : null;
        },
      })),
      { key: "together", label: "Policies on", raw: (row) => (checked(row) ? togetherText(row) : null) },
    ],
  };
}

/** A member campaign of a preview that the policies revised, with that revision. */
export type FiledMember = MemberPolicyPreview & { last_filing: PolicyFiling };

/** The members of `rows`, a preview's, that the policies revised, the newest revision first. */
export const recentFilings = (rows: readonly MemberPolicyPreview[]): FiledMember[] =>
  rows.filter((row): row is FiledMember => !!row.last_filing)
    .sort((a, b) => Date.parse(b.last_filing.filed_at) - Date.parse(a.last_filing.filed_at));

/** The recent revisions' table: a revised member campaign a row, named by `names`. */
export function filingsTable(names: Record<string, string>): TableSpec<FiledMember> {
  return {
    columns: [
      { key: "name", label: "Campaign", raw: (row) => names[row.campaign_id] ?? row.campaign_id },
      { key: "plan", label: "Plan", raw: (row) => row.last_filing.plan_id,
        href: (row) => campaignPlanLink(row.campaign_id, row.last_filing.plan_id) },
      { key: "filed_at", label: "Filed", raw: (row) => row.last_filing.filed_at, as: "time" },
      { key: "queued", label: "Jobs queued", num: true, raw: (row) => row.last_filing.queued_job_count },
    ],
  };
}

/** A key whose model params entry the member campaigns' rows lack, summed over them. */
export interface ParamsGap extends ModelParamsGap {
  campaign_ids: string[];
}

/** The rows of `rows`, a preview's members, that lack model params, summed by key in the server's key order. */
export function paramsGaps(rows: readonly MemberPolicyPreview[]): ParamsGap[] {
  const gaps = new Map<string, ParamsGap>();
  for (const { campaign_id, missing_model_params } of rows) {
    for (const { key, row_count, job_count } of missing_model_params) {
      const sum = gaps.get(key) ?? { key, row_count: 0, job_count: 0, campaign_ids: [] };
      gaps.set(key, { key, row_count: sum.row_count + row_count, job_count: sum.job_count + job_count,
                      campaign_ids: [...sum.campaign_ids, campaign_id] });
    }
  }
  return [...gaps.values()].sort((a, b) => (a.key < b.key ? -1 : 1));
}

/** The missing model params' table: a key a row, its member campaigns named by `names`. */
export function gapsTable(names: Record<string, string>): TableSpec<ParamsGap> {
  return {
    columns: [
      { key: "key", label: "Key", raw: (gap) => gap.key, as: "id" },
      { key: "rows", label: "Result rows", num: true, raw: (gap) => gap.row_count },
      { key: "jobs", label: "Jobs", num: true, raw: (gap) => gap.job_count },
      { key: "campaigns", label: "Campaigns", raw: (gap) => gap.campaign_ids.map((id) => names[id] ?? id).join(", ") },
    ],
  };
}

/** A failed job below a clean larger run, with its member campaign. */
export type SmallerFailedRow = SmallerFailure & { campaign_id: string };

/** The failed jobs below a clean larger run of `rows`, a preview's members, in member order. */
export const smallerFailedRows = (rows: readonly MemberPolicyPreview[]): SmallerFailedRow[] =>
  rows.flatMap(({ campaign_id, smaller_failed }) => smaller_failed.map((found) => ({ ...found, campaign_id })));

/** The table of failed jobs below a clean larger run: a job a row, its member campaign named by `names`. */
export function smallerFailedTable(names: Record<string, string>): TableSpec<SmallerFailedRow> {
  return {
    columns: [
      { key: "name", label: "Campaign", raw: (row) => names[row.campaign_id] ?? row.campaign_id },
      { key: "model", label: "Model", raw: (row) => row.model_key },
      { key: "failed", label: "Failed", raw: (row) => row.benchmark_id },
      { key: "failure", label: "Failure",
        raw: (row) => [row.failure_source, row.failure_code].filter(Boolean).join(": ") || null },
      { key: "job", label: "Failed job", raw: (row) => row.job_id, as: "id" },
      { key: "clean", label: "Clean larger run", raw: (row) => row.clean_benchmark_id },
      { key: "clean_job", label: "Clean job", raw: (row) => row.clean_job_id, as: "id" },
    ],
  };
}

/**
 * Where `reader` adds the model params entry `key`: in place, with the gate the model params page takes, or for a vault
 * model, which the table keys by its resource id, on that resource's page. `null` where it may not, as the routes
 * judge it (`docs/api/model-params.md`); they still decide.
 */
export function paramsWrite(reader: OwnUserView, key: string): "form" | "vault" | null {
  // The resource page says whether the reader administers the model's owner, which takes manage_orgs at least.
  if (key.startsWith("vault_")) return reader.may.administer_roster === "none" ? null : "vault";
  return reader.may.create_orgs ? "form" : null;
}

/**
 * A `kind` check's value as the console shows its quantity: swap's as a size, spread's and the TTFT share as percents,
 * and failed's as the run's failure code where it has one, else whether it failed as the policy re-runs; `none` where
 * the rows hold no value.
 */
export function rerunValue(kind: PolicyKind, value: number | null, code: string | null = null): string {
  if (kind === "failed" && code) return `failed: ${code}`;
  if (value === null) return "none";
  if (kind === "swap") return bytesText(value);
  if (kind === "spread") return `±${digits(value, 0, 1)}%`;
  if (kind === "ttft_over_e2e") return `${digits(value, 1, 1)}%`;
  if (kind === "failed") return value >= 1 ? "failed" : "no failure it re-runs";
  return digits(value, 0, 2);
}

/** The title of the policy that filed `row`, spread's at the settings it then had, failed's with the codes it re-ran. */
export const rerunTitle = ({ kind, threshold, short_repetition_ms, short_repetition_percent, codes }: PolicyRerun): string =>
  CHECKS[kind].title({ percent: threshold, short_repetition_ms, short_repetition_percent, codes });

/** A re-run's check value on the run it replaces, then on the re-run once its rows are read. */
export function changeText(row: PolicyRerun): string {
  const before = rerunValue(row.kind, row.before, row.before_code);
  const read = row.outcome && row.outcome !== "dropped";
  return read ? `${before} → ${rerunValue(row.kind, row.after, row.after_code)}` : before;
}

/** What a re-run came to, at its filing's threshold, or that the campaign dropped it before it had rows. */
export const outcomeText = ({ outcome }: PolicyRerun): string =>
  (outcome === "flagged" ? "still flagged" : outcome ?? "waiting");

/**
 * One policy's re-runs: how many wait for their rows and were dropped without any, how many of those read cleared, are
 * still flagged and failed, then the share of those read that cleared and the median value before and after.
 */
export function rerunSummaryText(summary: PolicyRerunSummary): string {
  const { kind, reruns, waiting, dropped, cleared, flagged, failed } = summary;
  const counts = `${reruns} re-run${reruns === 1 ? "" : "s"}: ${waiting} waiting, ${dropped} dropped; `
    + `${cleared} cleared, ${flagged} still flagged, ${failed} failed`;
  const read = cleared + flagged + failed;
  if (!read) return counts;
  const share = `${counts}; ${digits((cleared / read) * 100, 0, 1)}% cleared`;
  // Failed's values are 1 or 0, so its medians say no more than its counts.
  if (kind === "failed") return share;
  const [before, after] = [summary.median_before, summary.median_after].map((median) => rerunValue(kind, median));
  return `${share}, median ${before} → ${after}`;
}

/** The re-runs' table: a re-run a row, its campaign named by `names`. */
export function rerunsTable(names: Record<string, string>): TableSpec<PolicyRerun> {
  return {
    columns: [
      { key: "name", label: "Campaign", raw: (row) => names[row.campaign_id] ?? row.campaign_id },
      { key: "model", label: "Model key", raw: (row) => row.model_key, as: "id" },
      { key: "benchmark", label: "Benchmark", raw: (row) => row.benchmark_id },
      { key: "policy", label: "Policy", raw: rerunTitle },
      { key: "attempt", label: "Attempt", num: true, raw: (row) => row.attempt },
      // Sorts by the value before, as its text mixes units.
      { key: "change", label: "Before → after", num: true, raw: (row) => row.before },
      { key: "outcome", label: "Outcome", raw: outcomeText },
      { key: "filed", label: "Filed", raw: (row) => row.filed_at, as: "time" },
      { key: "judged", label: "Judged", raw: (row) => row.judged_at, as: "time" },
    ],
    texts: [{ key: "q", label: "model key or benchmark", texts: (row) => [row.model_key, row.benchmark_id] }],
    choices: [
      { key: "campaign", label: "campaign", values: (row) => [row.campaign_id], name: (id) => names[id] ?? id },
      { key: "policy", label: "policy", values: (row) => [rerunTitle(row)] },
      { key: "outcome", label: "outcome", values: (row) => [outcomeText(row)],
        order: ["waiting", "cleared", "still flagged", "failed", "dropped"] },
    ],
  };
}

/** The fields of a re-run in the CSV, as the route names and orders them. */
const RERUN_FIELDS = [
  "campaign_id", "kind", "rerun_job_id", "rerun_of", "plan_id", "filed_at", "attempt", "benchmark_id", "model_key",
  "before", "threshold", "short_repetition_ms", "short_repetition_percent", "codes", "before_code", "after",
  "after_code", "outcome", "judged_at",
] as const satisfies readonly (keyof PolicyRerun)[];

/** A re-run's field as a CSV cell, a list joined by spaces. */
const cell = (value: PolicyRerun[keyof PolicyRerun]): Raw => (Array.isArray(value) ? value.join(" ") : value);

/** `rows` as one CSV file: each re-run's campaign named by `names`, then its fields as the route sends them. */
export const rerunsCsv = (rows: readonly PolicyRerun[], names: Record<string, string>): string =>
  csvOf([["campaign", ...RERUN_FIELDS],
         ...rows.map((row) => [names[row.campaign_id] ?? row.campaign_id, ...RERUN_FIELDS.map((key) => cell(row[key]))])]);
