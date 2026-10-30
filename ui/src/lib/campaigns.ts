// The campaign pages as the classic console drew them: the listings and the
// new-campaign form (`campaigns.html`, `new_campaign.html`) over `GET /api/v1/campaigns`
// and `POST /api/v1/campaigns`, and one campaign (`campaign.html`) over the routes under
// `/api/v1/campaigns/{campaign_id}`; and its plan document, which the classic console never edited.

import { ApiError, type IngestProgress } from "./api";
import type { Crumb } from "./crumbs";
import { doneOf, duration, textOrder } from "./format";
import type { OrgRef } from "./orgs";
import { ownedQuery, ownerBranch, ownerName, principalOf, underTrail, type OwnedBranch, type OwnedScope } from "./owned";
import { PLAN_STATUSES, bytesText, doneShare, shownStatus, statusChoices } from "./plans";
import { reaches } from "./rights";
import { campaignLink, campaignPlanLink, pathOf, type OwnedPages } from "./routes";
import type { TableSpec } from "./table";
import { percent } from "./upload";
import type {
  CampaignState, CampaignView, DeviceTimeTotal, NamedCampaignOverview, OwnUserView, OwnerMatch, PlanView, PlannedImport, PrincipalOwner,
  RevisionDryRun, UpdateCampaignRequest,
} from "./api-types";

/**
 * The text of a device-time total on the campaign page. The text is the
 * duration, or an em-rule if no job reported a value. If the sum leaves out
 * jobs, the text adds their count.
 */
export function deviceTimeText(total: DeviceTimeTotal): string {
  const sum = duration(total.device_time_ms);
  const n = total.unreported_job_count;
  return n > 0 ? `${sum} (${n} ${n === 1 ? "job" : "jobs"} reported none)` : sum;
}

/** The campaign listing under each branch. */
export const CAMPAIGN_PAGES: OwnedPages = { mine: "me-campaigns", org: "org-campaigns", all: "campaigns" };

/**
 * A plan page's trail: Plans, or for a campaign's plan the campaign, under its
 * owner's listing once `detail` is read (never, for a reader refused it).
 */
export function planTrail(plan_id: string, campaign_id: string | null, detail: NamedCampaignOverview | null,
                          reader: Pick<OwnUserView, "user_id">, known: (OrgRef | null)[]): readonly Crumb[] {
  if (!campaign_id) return [{ label: "Plans", href: pathOf({ page: "plans" }) }];
  const campaign = { label: detail?.name ?? "Campaign", href: campaignPlanLink(campaign_id, plan_id) };
  return detail ? [...underTrail(ownerBranch(reader, detail.owner, known, CAMPAIGN_PAGES), "Campaigns"), campaign]
    : [campaign];
}

/**
 * A campaign's status: `archived` once archived, else what its plan is doing, `not_started` before its first and
 * `""` where the reader may not read it.
 */
const campaignStatus = (row: CampaignView): string =>
  (stateOf(row) === "archived" ? "archived"
    : row.plan ? shownStatus(row.plan.status, row.plan.counts) : row.current_plan_id ? "" : "not_started");

/** A campaign's own state, beside its plan's status; a record never archived carries none. */
export const stateOf = (row: Pick<CampaignView, "state">): CampaignState => row.state ?? "active";

/** A campaign's progress as the listing prints it: its plan's jobs done of all, `936/936`. */
export const progressText = (row: CampaignView): string =>
  (row.plan?.counts ? `${doneOf(row.plan.counts)}/${row.plan.counts.total}` : "—");

/** A record's labels as pairs, in key order. */
export const labelPairs = (labels: Record<string, string> | undefined): [string, string][] =>
  Object.entries(labels ?? {}).sort(([a], [b]) => textOrder(a, b));

/** A label as a listing prints and filters it: `key=value`, the key alone for an empty value. */
export const labelText = ([key, value]: readonly [string, string]): string => (value ? `${key}=${value}` : key);

/** `listing` narrowed to the campaigns that carry `pair`, as its labels filter keeps it in the address. */
export const labelLink = (listing: string, pair: readonly [string, string]): string =>
  `${listing}?${new URLSearchParams({ label: labelText(pair) })}`;

/** A label typed as `key=value`, split at its first `=`; a key alone has an empty value. */
export function parseLabel(text: string): [string, string] {
  const at = text.indexOf("=");
  return at < 0 ? [text.trim(), ""] : [text.slice(0, at).trim(), text.slice(at + 1).trim()];
}

/** `labels` with `pair` set, in place of its key's earlier value: one value a key. */
export const withLabel = (labels: Record<string, string> | undefined, pair: [string, string]): [string, string][] =>
  [...labelPairs(labels).filter(([key]) => key !== pair[0]), pair];

/** `labels` without `key`'s label. */
export const withoutLabel = (labels: Record<string, string> | undefined, key: string): [string, string][] =>
  labelPairs(labels).filter(([at]) => at !== key);

/**
 * Whether `reader` may label a campaign `owner` owns, as `PATCH /api/v1/campaigns/{id}` judges it but for a
 * disabled member's: its own with `submit_plans`, its organization's as an administrator. The route still decides.
 */
export const mayLabel = (reader: OwnUserView, owner: PrincipalOwner): boolean =>
  (owner.type === "org" ? reaches(reader, reader.may.administer_roster, owner.org_id)
    : reader.may.administer_roster === "every" || (reader.may.file_plans && owner.user_id === reader.user_id));

/** `Labels`' bounds: the pairs a record takes, and a key's and a value's characters. */
export const LABELS_MAX = 50;
export const LABEL_KEY_MAX_CHARS = 128;
export const LABEL_VALUE_MAX_CHARS = 256;
/** What a key or a value may hold, as an AWS tag: letters, digits, spaces and `_ . : / = + - @`. */
const LABEL_TEXT = /^[\p{Alphabetic}\p{N} _.:/=+\-@]*$/u;

/** Why `rows`, trimmed, would be refused as a campaign's labels, or `""`: the route's rules, and a key set twice. */
export function labelsProblem(rows: readonly (readonly [string, string])[]): string {
  if (rows.length > LABELS_MAX) return `Keep to ${LABELS_MAX} labels`;
  const seen = new Set<string>();
  for (const [key, value] of rows.map(([k, v]) => [k.trim(), v.trim()] as const)) {
    if (!key) return "Name every label, or remove the empty row";
    if (seen.has(key)) return `“${key}” is set twice`;
    seen.add(key);
    if ([...key].length > LABEL_KEY_MAX_CHARS) return `Keep a key to ${LABEL_KEY_MAX_CHARS} characters`;
    if ([...value].length > LABEL_VALUE_MAX_CHARS) return `Keep the value of “${key}” to ${LABEL_VALUE_MAX_CHARS} characters`;
    if (!LABEL_TEXT.test(key) || !LABEL_TEXT.test(value)) {
      return `“${key}” may hold letters, digits, spaces and _ . : / = + - @`;
    }
  }
  return "";
}

/** A relabel's body, the rows trimmed and sent as the whole set; refused in the browser where the route would refuse it. */
export function labelsBody(rows: readonly (readonly [string, string])[]): UpdateCampaignRequest {
  const problem = labelsProblem(rows);
  if (problem) throw new Error(problem);
  return { labels: Object.fromEntries(rows.map(([key, value]) => [key.trim(), value.trim()])) };
}

/** The keys `rows` use, each with the values it takes, both sorted: what a label's inputs offer. */
export function labelVocabulary(rows: readonly Pick<CampaignView, "labels">[]): Map<string, string[]> {
  const values = new Map<string, Set<string>>();
  for (const [key, value] of rows.flatMap((row) => labelPairs(row.labels))) {
    values.set(key, (values.get(key) ?? new Set()).add(value));
  }
  return new Map([...values].sort(([a], [b]) => textOrder(a, b))
    .map(([key, of]) => [key, [...of].filter(Boolean).sort(textOrder)]));
}

/** Every label `vocabulary` holds as `key=value`, a key used with no value alone: what the one-box add offers. */
export const knownLabels = (vocabulary: ReadonlyMap<string, readonly string[]>): string[] =>
  [...vocabulary].flatMap(([key, values]) => (values.length ? values.map((value) => labelText([key, value])) : [key]));

/**
 * The listing whose labels a campaign `owner` owns shares: its organization's, or the reader's own; none for
 * another account's campaign.
 */
export function labelScope(reader: Pick<OwnUserView, "user_id">, owner: PrincipalOwner): OwnedScope | null {
  if (owner.type === "org") return { kind: "org", org_id: owner.org_id };
  return owner.user_id === reader.user_id ? { kind: "me" } : null;
}

/** The campaign listing's columns and filters; `owners` where its rows name their owner. */
export function campaignTable(names: Record<string, string>, owners: boolean): TableSpec<CampaignView> {
  const owner = (row: CampaignView) => ownerName(row.owner, names);
  return {
    columns: [
      { key: "campaign_id", label: "campaign_id", raw: (row) => row.campaign_id, shown: false,
        href: (row) => campaignLink(row.campaign_id) },
      { key: "name", label: "name", raw: (row) => row.name },
      ...(owners ? [{ key: "owner", label: "owner", raw: owner }] : []),
      { key: "status", label: "status", raw: campaignStatus },
      { key: "progress", label: "progress", raw: (row) => doneShare(row.plan?.counts) },
      { key: "labels", label: "labels", raw: (row) => labelPairs(row.labels).map(labelText).join(", ") || null },
      { key: "created_at", label: "created_at", raw: (row) => row.created_at, as: "time" },
      { key: "updated_at", label: "updated_at", raw: (row) => row.updated_at, as: "time" },
    ],
    texts: [
      { key: "q", label: "search", placeholder: "campaign id, name or label", wide: true,
        texts: (row) => [row.campaign_id, row.name, ...labelPairs(row.labels).map(labelText)] },
    ],
    // `campaignQuery` sends them; a plan id finds its campaign, superseded or archived.
    server: [
      { key: "plan_id", label: "plan id", placeholder: "current or superseded", wide: true },
      { key: "state", label: "state",
        options: [{ value: "", label: "active" }, { value: "archived", label: "archived" }, { value: "all", label: "all" }] },
    ],
    choices: [
      { key: "status", label: "status",
        values: (row) => (row.plan && stateOf(row) !== "archived" ? statusChoices(row.plan.status, row.plan.counts)
          : [campaignStatus(row)]),
        name: (value) => value.replaceAll("_", " "), order: [...PLAN_STATUSES, "not_started", "archived"] },
      { key: "label", label: "labels", values: (row) => labelPairs(row.labels).map(labelText) },
      ...(owners ? [{ key: "owner", label: "owner", values: (row: CampaignView) => [principalOf(row.owner)],
                      name: (id: string) => names[id] ?? id }] : []),
    ],
  };
}

/**
 * The `GET /api/v1/campaigns` query for `branch`, with the plan each campaign runs: with `planId`, the one campaign
 * that plan was filed into, current or superseded, in any state; else the campaigns in `state`, `all` for every one.
 */
export const campaignQuery = (branch: OwnedBranch, cursor: string | null, limit: string, planId = "",
                              state: CampaignState | "all" = "active"): string =>
  ownedQuery(branch, [["plan", "true"], ...(planId ? [["plan_id", planId] as [string, string]]
    : state === "all" ? [] : [["state", state] as [string, string]])], cursor, limit);

/** The listing's "New campaign": an organization's opens the form with that organization as the owner. */
export const newCampaignAction = (branch: OwnedBranch) => ({
  href: branch.kind === "org" ? `/app/campaigns/new?${new URLSearchParams({ owner: branch.org_id })}`
    : "/app/campaigns/new",
  label: "New campaign",
});

/** Why the form is refused, as `POST /api/v1/campaigns`'s gate would, or `""`. */
export const newCampaignRefusal = (reader: OwnUserView): string =>
  reader.may.file_plans ? "" : "Creating a campaign needs the submit_plans permission.";

export interface OwnerOption {
  value: string;
  label: string;
}

/** The owners the form offers: the reader, and the organization it belongs to. */
export function newCampaignOwners(reader: OwnUserView): OwnerOption[] {
  const org = reader.may.file_plans && reader.org_id ? reader.org_id : null;
  return [{ value: "", label: "me" },
    ...(org ? [{ value: org, label: `${reader.org_slug ?? org} (organization)` }] : [])];
}

/** The owner `?owner=` chooses: one the form offers, else the reader. */
export const chosenOwner = (offered: OwnerOption[], asked: string | null): string =>
  offered.find((option) => option.value === asked)?.value ?? "";

export interface NewCampaign {
  name: string;
  owner: string;
  note: string;
}

/**
 * `POST /api/v1/campaigns`'s query for `form`. An empty field is left out: the route
 * refuses an empty `owner`, and reads the reader for none.
 */
export function createQuery(form: NewCampaign): string {
  const problem = nameProblem(form.name) || noteProblem(form.note);
  if (problem) throw new Error(problem);
  const pairs: [string, string][] = [["name", form.name]];
  if (form.owner) pairs.push(["owner", form.owner]);
  if (form.note.trim()) pairs.push(["note", form.note.trim()]);
  return new URLSearchParams(pairs).toString();
}

/**
 * The plan reads of a campaign's page, or `null` where the plan scope refused
 * them: the page draws that plan as withheld, not as missing
 * (`campaigns::reads::overview`).
 */
export async function unlessWithheld<T>(read: Promise<T>): Promise<T | null> {
  try {
    return await read;
  } catch (e) {
    if (e instanceof ApiError && (e.status === 403 || e.status === 404)) return null;
    throw e;
  }
}

/**
 * `shown`'s reads, where it is a plan of the campaign (the classic `campaign_context`):
 * one of the walked history, or past the walk's ceiling one that names the
 * campaign. A plan filed into it that never ran, or another's, is not found.
 */
export function campaignPlan<T extends { plan: Pick<PlanView, "campaign_id"> }>(
  detail: Pick<NamedCampaignOverview, "campaign_id" | "history" | "history_truncated">, shown: string, read: T | null,
): T | null {
  if (detail.history.some((row) => row.plan_id === shown)) return read;
  if (detail.history_truncated && read?.plan.campaign_id === detail.campaign_id) return read;
  throw new ApiError(404, `campaign ${detail.campaign_id} has run no plan ${shown}`);
}

/**
 * The query of a document revision of `base`, the plan it was read from. The
 * function omits a blank note. The route treats a missing note as no note.
 */
export function revisionQuery(base: string, note: string, carryEarlier = false, inheritFrom: string[] = []): string {
  const pairs: [string, string][] = [["base_plan_id", base]];
  if (note.trim()) pairs.push(["note", note]);
  if (carryEarlier) pairs.push(["carry_earlier", "true"]);
  if (inheritFrom.length) pairs.push(["inherit_from", inheritFrom.join(",")]);
  return new URLSearchParams(pairs).toString();
}

/** A plan a revision may inherit finished runs from (`inherit_from`), as the picker names it. */
export interface InheritSource {
  plan_id: string;
  campaign_id: string | null;
  /** The campaign's name, where the reader lists the campaign. */
  campaign: string | null;
  /** Its finished jobs, where the plan has been counted. */
  finished: number | null;
}

/** The plan each listed campaign runs, but `except`'s, where the reader may read that plan. */
export const inheritOffers = (rows: readonly CampaignView[], except: string): InheritSource[] =>
  rows.flatMap((row) => (row.campaign_id !== except && row.current_plan_id && row.plan
    ? [{ plan_id: row.current_plan_id, campaign_id: row.campaign_id, campaign: row.name,
         finished: row.plan.counts?.finished ?? null }]
    : []));

/** The offer `typed` names: by its plan's id, its campaign's id, or else the newest campaign of that name. */
export function namedOffer(offers: readonly InheritSource[], typed: string): InheritSource | undefined {
  const asked = typed.trim();
  return offers.find((offer) => offer.plan_id === asked || offer.campaign_id === asked)
    ?? offers.find((offer) => offer.campaign === asked);
}

/** `plan`, read by its id, as a source, named by its campaign where `offers` lists that campaign. */
export function planSourceOf(plan: Pick<PlanView, "plan_id" | "campaign_id" | "progress_snapshot">,
                             offers: readonly InheritSource[]): InheritSource {
  const campaign_id = plan.campaign_id ?? null;
  return {
    plan_id: plan.plan_id, campaign_id,
    campaign: offers.find((offer) => campaign_id && offer.campaign_id === campaign_id)?.campaign ?? null,
    finished: plan.progress_snapshot?.counts.finished ?? null,
  };
}

/** The label that the picker shows for `source`. The label has the campaign and the count of finished jobs. */
export const sourceText = (source: InheritSource): string =>
  [source.campaign ?? (source.campaign_id ? source.campaign_id : "no campaign"),
   source.finished === null ? null : `${source.finished.toLocaleString("en")} finished`]
    .filter(Boolean).join(" · ");

/** Where a campaign's own address opens: its plan `head`'s Overview. */
export const headAddress = (campaign_id: string, head: string): string =>
  pathOf({ page: "campaign-plan", campaign_id, plan_id: head });

/** The longest campaign name the server takes, in characters (`api_v1::campaigns::NAME_MAX_CHARS`). */
export const NAME_MAX_CHARS = 4096;

/** Why `name` would be refused as a campaign's, or `""`: the route's own checks, made as it is typed. */
export function nameProblem(name: string): string {
  const trimmed = name.trim();
  if (!trimmed) return "Name the campaign";
  if ([...trimmed].length > NAME_MAX_CHARS) return `Keep the name to ${NAME_MAX_CHARS.toLocaleString("en")} characters`;
  return "";
}

/** The maximum length of a plan note, in characters. The server enforces it (`api_v1::plans::NOTE_MAX_CHARS`). */
export const NOTE_MAX_CHARS = 4096;

/** Return the reason the server would reject `note` as a plan note, or `""` if the server accepts it. A blank note is valid and the request omits it (`check_note`). */
export function noteProblem(note: string): string {
  if ([...note.trim()].length > NOTE_MAX_CHARS) return `Keep the note to ${NOTE_MAX_CHARS.toLocaleString("en")} characters`;
  if (note.includes("\0")) return "Remove the NUL character from the note";
  return "";
}

/** A rename's body, refused in the browser where the route would refuse it. */
export function renameBody(name: string): UpdateCampaignRequest {
  const problem = nameProblem(name);
  if (problem) throw new Error(problem);
  return { name };
}

/**
 * The page's word for a hand-over to `to` whose re-read failed with `e`: a
 * `403` means the write took the campaign out of reach, anything else is no hand-over's doing.
 */
export const handedAway = (e: unknown, to: Pick<OwnerMatch, "label">): string | null =>
  e instanceof ApiError && e.status === 403
    ? `This campaign was handed over to ${to.label}, which leaves it outside your reach.`
    : null;

/** Whether the reader may revert the campaign to `plan_id`: a plan of its history other than the one it runs. */
export const revertOffered = (detail: Pick<NamedCampaignOverview, "may_edit_variants" | "current_plan_id">,
                              plan_id: string): boolean =>
  detail.may_edit_variants && !!detail.current_plan_id && plan_id !== detail.current_plan_id;

/**
 * This function creates an edit that restores variant `index` of `source`
 * into plan `base`. It appends the variant or replaces the head's variant
 * `replace`.
 */
export function restoreEdit(base: string, source: string, index: number, replace: number | null):
  { query: string; toml: string } {
  const query = new URLSearchParams({ base_plan_id: base, restore: `${source}:${index}` }).toString();
  return { query, toml: replace === null ? "" : `remove = [${replace}]\n` };
}

/** The sentence that follows `revisionSummary` in a confirmation: what filing any revision does to the campaign and to the plan that it runs. */
export const REVISION_EFFECT = "The campaign moves onto the new plan. The server cancels the current plan.";

/** What filing a previewed revision would do to the plan it replaces, in a sentence. */
export function revisionSummary(preview: RevisionDryRun): string {
  if (preview.unchanged) return "Filing it files nothing: this is the document the campaign runs.";
  const stopped = preview.dropped_job_ids.length;
  const requirementsRerun = preview.requirements_rerun_job_count;
  const again = preview.rerun_job_count + preview.recalculated_job_count + requirementsRerun;
  const restored = preview.restored_job_count;
  const inherited = preview.inherited_job_count;
  const satisfied = preview.satisfied_job_count;
  if (!preview.new_job_count && !stopped && !again && !restored && !inherited && !satisfied) {
    return "Filing it changes no job: every job carries over as it is.";
  }
  const parts = [
    `keeps ${preview.carried_job_count} of the current plan's jobs, finished ones included`,
    ...(restored ? [`brings back ${restored} finished from earlier plans`] : []),
    ...(inherited ? [`takes ${inherited} finished from the plans it inherits from`] : []),
    ...(satisfied ? [`takes ${satisfied} finished from earlier runs of the same cell`] : []),
    ...(preview.new_job_count ? [`queues ${preview.new_job_count} new`] : []),
    ...(preview.rerun_job_count ? [`runs ${preview.rerun_job_count} again`] : []),
    ...(requirementsRerun ? [`runs ${requirementsRerun} finished again (failed, or their device or build is not known to meet the requirements)`] : []),
    ...(preview.recalculated_job_count ? [`scores ${preview.recalculated_job_count} again`] : []),
    `stops ${stopped}`,
  ];
  return `Filing it ${parts.join(", ")}.`;
}

/** `document` with the variant blocks of `plan_id` appended, under a line that names the plan. */
export function withVariantsOf(document: string, plan_id: string, blocks: string): string {
  return `${document.trimEnd()}\n\n# The variants of ${plan_id}.\n${blocks.trim()}\n`;
}

/** The variants a dry run found repeating an earlier one, in a sentence; empty when none does. */
export function repeatedText(indices: number[]): string {
  if (!indices.length) return "";
  if (indices.length === 1) return `Variant ${indices[0]} repeats an earlier variant, so it adds no job.`;
  return `Variants ${indices.slice(0, -1).join(", ")} and ${indices.at(-1)} repeat earlier variants, so they add no job.`;
}

/** What filing does to one gated model of a dry run: it downloads the model, or it reuses the vault records. */
export function importText(entry: PlannedImport): string {
  return entry.action === "fetch"
    ? `${entry.hf_ref}: downloads ${bytesText(entry.bytes)}`
    : `${entry.hf_ref}: reuses ${entry.resource_ids.join(", ")}`;
}

/** The key of the file that a `progress` line names: one progress line for each key. */
export const importKey = (line: IngestProgress) => `${line.hf_ref} ${line.file}`;

/**
 * The text that the page shows for the file that `line` names. The text shows
 * the first 7 characters of the commit in `hf_ref`.
 */
export function importLine({ hf_ref, file, bytes_done, bytes_total }: IngestProgress): string {
  const source = hf_ref.slice(0, hf_ref.lastIndexOf("@") + 8);
  return bytes_done >= bytes_total
    ? `${file} (${source}): downloaded`
    : `${file} (${source}): downloading ${percent(bytes_done, bytes_total)}`;
}
