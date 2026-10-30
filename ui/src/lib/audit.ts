// The audit log's listing: its filters, which the server applies and the
// address keeps under the route's own keys, and how a row draws who acted,
// what they did, and the records it named.

import { ownOrg } from "./orgs";
import { campaignLink, clientLink, href, orgLink, planLink, userLink, vaultLink } from "./routes";
import type { Choice } from "./table";
import type { AuditActor, AuditEvent, AuditTarget, OwnUserView } from "./api-types";

export const OUTCOMES: Choice[] = [
  { value: "succeeded", label: "succeeded" },
  { value: "refused", label: "refused" },
  { value: "failed", label: "failed" },
];

export const ACTOR_KINDS: Choice[] = [
  { value: "user", label: "people" },
  { value: "client", label: "devices" },
  { value: "unverified", label: "unverified" },
  { value: "host", label: "host operator" },
];

/**
 * The actor kinds that the listing reads when the address names no kind: people and unverified callers. Devices and
 * the host's scheduled commands write many more events than people do, so a reader selects them.
 */
export const DEFAULT_ACTORS = ["user", "unverified"];

export interface AuditFilters {
  /** Empty for every outcome. */
  outcome: string[];
  /** Empty for every kind; `null` where the address names none, which reads `DEFAULT_ACTORS`. */
  actor: string[] | null;
  /** One record id: the events naming it, or acting as it. */
  target: string;
  /** The page size as the address gave it. */
  limit: string;
}

/** A comma-separated list's values, each once, blanks dropped. */
const listed = (value: string | null): string[] =>
  [...new Set((value ?? "").split(",").map((each) => each.trim()).filter(Boolean))];

export const auditFilters = (query: URLSearchParams): AuditFilters => ({
  outcome: listed(query.get("outcome")),
  actor: query.has("actor") ? listed(query.get("actor")) : null,
  target: query.get("target")?.trim() ?? "",
  limit: query.get("limit")?.trim() ?? "",
});

export const effectiveActors = (filters: AuditFilters): string[] => filters.actor ?? DEFAULT_ACTORS;

/** The route's query: the default kinds spelled out, since it reads no `actor` as every kind. */
export function auditQuery(filters: AuditFilters, cursor: string | null): string {
  const actors = effectiveActors(filters);
  const pairs: [string, string][] = [];
  if (filters.outcome.length) pairs.push(["outcome", filters.outcome.join(",")]);
  if (actors.length) pairs.push(["actor", actors.join(",")]);
  if (filters.target) pairs.push(["target", filters.target]);
  if (filters.limit) pairs.push(["limit", filters.limit]);
  if (cursor) pairs.push(["cursor", cursor]);
  pairs.push(["names", "true"]);
  return new URLSearchParams(pairs).toString();
}

/** The listing at `path` under `filters`, from `cursor`; `actor=` (every kind) stays apart from none (the default). */
export const auditLink = (path: string, filters: AuditFilters, cursor: string | null = null): string =>
  href(path, [
    ["target", filters.target], ["outcome", filters.outcome.join(",")],
    ...(filters.actor ? [["actor", filters.actor.join(",")] as [string, string]] : []),
    ["limit", filters.limit], ["cursor", cursor ?? ""],
  ], ["actor"]);

const ORDER = { outcome: OUTCOMES, actor: ACTOR_KINDS };

/** `filters` with `value` chosen in `key` or let go of, in the select's order; the default kinds name none. */
export function toggled(filters: AuditFilters, key: "outcome" | "actor", value: string, on: boolean): AuditFilters {
  const from = key === "actor" ? effectiveActors(filters) : filters.outcome;
  const rank = (each: string) => {
    const at = ORDER[key].findIndex((option) => option.value === each);
    return at < 0 ? ORDER[key].length : at;
  };
  const chosen = [...from.filter((each) => each !== value), ...(on ? [value] : [])].sort((a, b) => rank(a) - rank(b));
  return { ...filters, [key]: key === "actor" && chosen.join() === DEFAULT_ACTORS.join() ? null : chosen };
}

/** `options`, then each chosen value they lack, so one the address carries can be let go of. */
export const offered = (options: Choice[], chosen: string[]): Choice[] => [
  ...options,
  ...chosen.filter((value) => !options.some((option) => option.value === value))
    .map((value) => ({ value, label: value })),
];

/** A principal or record as a cell draws it. */
export interface Shown {
  text: string;
  href: string | null;
  /** The id behind a name, on hover. */
  title: string | undefined;
  /** `text` is a bare id. */
  id: boolean;
}

/**
 * An actor by the name the page carries, linked to its page, else its bare id: the server names only what the
 * reader may see. One with no record reads as what it claimed.
 */
export function actorShown(actor: AuditActor, names: Record<string, string>): Shown {
  switch (actor.type) {
    case "user": {
      const name = names[actor.user_id];
      return { text: name ?? actor.user_id, href: name ? userLink(actor.user_id) : null,
               title: `${actor.user_id}, key ${actor.key_id}`, id: !name };
    }
    case "client": {
      const name = names[actor.client_id];
      return { text: name ?? actor.client_id, href: name ? clientLink(actor.client_id) : null,
               title: name ? actor.client_id : undefined, id: !name };
    }
    case "unverified":
      return { text: actor.claimed_id ? `unverified (claimed ${actor.claimed_id})` : "unverified", href: null,
               title: undefined, id: false };
    case "host":
      return { text: `host operator ${actor.os_user}@${actor.hostname}`, href: null, title: undefined, id: false };
  }
}

/** What a row did: the route's summary or the request, the command; the status only where it did not succeed. */
export function actionShown(event: Pick<AuditEvent, "action" | "outcome" | "summary">): {
  text: string; title: string; status: number | null;
} {
  const { action } = event;
  if (action.type === "cli") {
    // An organization's log clears the arguments, which can name another organization.
    const args = action.args.length ? action.args : [action.command];
    return { text: event.summary || `pipette-mgmt ${action.command}`, title: ["pipette-mgmt", ...args].join(" "),
             status: null };
  }
  if (action.type === "first_clerk_sign_in") {
    const count = action.prior_keys.length;
    return { text: `First Clerk sign-in, to an account holding ${count} key${count === 1 ? "" : "s"}`,
             title: action.prior_keys.map((key) => `${key.key_id} (${key.key_details})`).join("\n"), status: null };
  }
  return { text: event.summary || `${action.method} ${action.route}`, title: `${action.method} ${action.path}`,
           status: event.outcome === "succeeded" ? null : action.status };
}

export function targetId(target: AuditTarget): string {
  switch (target.type) {
    case "org": return target.org_id;
    case "user": return target.user_id;
    case "client": return target.client_id;
    case "campaign": return target.campaign_id;
    case "plan": return target.plan_id;
    case "job": return target.job_id;
    case "vault_resource": return target.resource_id;
  }
}

/**
 * A target's page, an organization by its handle. `names` covers every kind but plans and jobs, so an unnamed
 * one is out of the reader's reach or gone; a job has no page of its own.
 */
export function targetHref(target: AuditTarget, names: Record<string, string>): string | null {
  const id = targetId(target);
  const name = names[id];
  switch (target.type) {
    case "plan": return planLink(id);
    case "job": return null;
    case "org": return name ? orgLink({ slug: name, org_id: id }) : null;
    case "user": return name ? userLink(id) : null;
    case "client": return name ? clientLink(id) : null;
    case "campaign": return name ? campaignLink(id) : null;
    case "vault_resource": return name ? vaultLink(id) : null;
  }
}

export function targetShown(target: AuditTarget, names: Record<string, string>): Shown {
  const id = targetId(target);
  const name = names[id];
  return { text: name ?? id, href: targetHref(target, names), title: name ? id : undefined, id: !name };
}

/**
 * A record page's "Audit": the events naming record `id` or acting as it, of every kind, on the log the reader
 * administers; `null` for a reader administering none.
 */
export function auditAction(reader: Pick<OwnUserView, "may" | "org_id" | "org_slug">, id: string) {
  const filters: AuditFilters = { outcome: [], actor: [], target: id, limit: "" };
  const own = reader.may.administer_roster === "own" ? ownOrg(reader) : null;
  const path = reader.may.administer_roster === "every" ? "/app/server/audit" : own ? orgLink(own, "/audit") : null;
  return path ? { href: auditLink(path, filters), label: "Audit" } : null;
}
