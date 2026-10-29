// The app's addresses: each page's pattern, the route an address names, and
// the address of a route. Pure, so vitest imports it; `route.svelte.ts` holds
// the address bar itself.

import { handle } from "./format";

/** Each page's address; `:name` is a parameter, one whole path segment. */
const PATTERNS = {
  account: "/app/account",
  "account-keys": "/app/account/keys",
  "login-cli": "/app/login/cli",
  orgs: "/app/orgs",
  "server-users": "/app/server/users",
  "server-devices": "/app/server/devices",
  "server-catalog": "/app/server/catalog",
  "server-catalog-entry": "/app/server/catalog/:name",
  "server-model-params": "/app/server/model-params",
  "server-audit": "/app/server/audit",
  "vault-new": "/app/vault/new",
  vault: "/app/vault",
  "vault-runtimes": "/app/vault/runtimes",
  "vault-resource": "/app/vault/:resource_id",
  "me-vault": "/app/me/vault",
  "me-vault-runtimes": "/app/me/vault/runtimes",
  "org-vault": "/app/orgs/:org/vault",
  "org-vault-runtimes": "/app/orgs/:org/vault/runtimes",
  clients: "/app/clients",
  client: "/app/clients/:client_id",
  "me-devices": "/app/me/devices",
  "org-devices": "/app/orgs/:org/devices",
  users: "/app/users",
  "user-new": "/app/users/new",
  user: "/app/users/:user_id",
  "user-plans": "/app/users/:user_id/plans",
  "user-permissions": "/app/users/:user_id/permissions",
  "user-keys": "/app/users/:user_id/keys",
  "user-preauth": "/app/users/:user_id/preauth-keys",
  "user-settings": "/app/users/:user_id/settings",
  "org-members": "/app/orgs/:org/members",
  "org-audit": "/app/orgs/:org/audit",
  org: "/app/orgs/:org",
  "org-new": "/app/orgs/new",
  benchmarks: "/app/benchmarks",
  "benchmark-new": "/app/benchmarks/new",
  benchmark: "/app/benchmarks/:org/:benchmark_id",
  "org-benchmarks": "/app/orgs/:org/benchmarks",
  plans: "/app/plans",
  plan: "/app/plans/:plan_id",
  "plan-cells": "/app/plans/:plan_id/cells",
  "plan-results": "/app/plans/:plan_id/results",
  "plan-analysis": "/app/plans/:plan_id/analysis",
  "plan-analysis-report": "/app/plans/:plan_id/analysis/:view",
  "plan-reports": "/app/plans/:plan_id/reports",
  "plan-report": "/app/plans/:plan_id/reports/:view",
  "plan-revisions": "/app/plans/:plan_id/revisions",
  "plan-settings": "/app/plans/:plan_id/settings",
  campaigns: "/app/campaigns",
  "campaign-new": "/app/campaigns/new",
  campaign: "/app/campaigns/:campaign_id",
  "campaign-plan": "/app/campaigns/:campaign_id/plans/:plan_id",
  "campaign-plan-cells": "/app/campaigns/:campaign_id/plans/:plan_id/cells",
  "campaign-plan-analysis": "/app/campaigns/:campaign_id/plans/:plan_id/analysis",
  "campaign-plan-analysis-report": "/app/campaigns/:campaign_id/plans/:plan_id/analysis/:view",
  "campaign-plan-reports": "/app/campaigns/:campaign_id/plans/:plan_id/reports",
  "campaign-plan-report": "/app/campaigns/:campaign_id/plans/:plan_id/reports/:view",
  "campaign-plan-revisions": "/app/campaigns/:campaign_id/plans/:plan_id/revisions",
  "campaign-plan-settings": "/app/campaigns/:campaign_id/plans/:plan_id/settings",
  "campaign-document": "/app/campaigns/:campaign_id/document",
  "me-campaigns": "/app/me/campaigns",
  "org-campaigns": "/app/orgs/:org/campaigns",
} as const;

/** Addresses that draw another's page in place rather than redirect, so a query like `?clerk=1` survives. */
const ALIASES: Record<string, Page> = {
  "/app": "me-campaigns",
};

type Patterns = typeof PATTERNS;
export type Page = keyof Patterns;

type ParamNames<P extends string> =
  P extends `${string}:${infer Name}/${infer Rest}` ? Name | ParamNames<`/${Rest}`>
  : P extends `${string}:${infer Name}` ? Name
  : never;

/** The route of `page`: its name, and each parameter decoded. */
export type RouteOf<P extends Page> = { page: P } & { [K in ParamNames<Patterns[P]>]: string };

/** What an address names: a page and its parameters, or no page. */
export type Route = { [P in Page]: RouteOf<P> }[Page] | { page: "missing"; path: string };

interface Entry {
  page: string;
  segments: string[];
}

/** One pattern's segments, `/` split off both ends. */
const segmentsOf = (path: string) => path.split("/").filter(Boolean);

/**
 * The table, a static segment ahead of a parameter at the first place two
 * patterns differ, so `/app/orgs/new` wins over `/app/orgs/:org` in any order.
 * Ties fall to the shorter pattern, which keeps the order total.
 */
export function table(patterns: Record<string, string>): Entry[] {
  const entries = Object.entries(patterns).map(([page, pattern]) =>
    ({ page, segments: segmentsOf(pattern) }));
  const rank = (a: Entry, b: Entry) => {
    for (let i = 0; i < Math.min(a.segments.length, b.segments.length); i++) {
      const [x, y] = [a.segments[i]!.startsWith(":"), b.segments[i]!.startsWith(":")];
      if (x !== y) return x ? 1 : -1;
    }
    return a.segments.length - b.segments.length;
  };
  return entries.sort(rank);
}

const TABLE = table(PATTERNS);

/** `path` without a trailing slash, which an address may carry or not. */
export const normalized = (path: string) => path.replace(/\/+$/, "") || "/";

/** The route `entries` hold for `path`, or `null`; parameters are decoded. */
export function matchIn(entries: Entry[], path: string): Record<string, string> | null {
  const parts = normalized(path).split("/").filter(Boolean);
  for (const { page, segments } of entries) {
    if (segments.length !== parts.length) continue;
    const route: Record<string, string> = { page };
    const fits = segments.every((segment, i) => {
      const part = parts[i]!;
      if (!segment.startsWith(":")) return segment === part;
      try { route[segment.slice(1)] = decodeURIComponent(part); } catch { return false; }
      return true;
    });
    if (fits) return route;
  }
  return null;
}

/** The route `path` names; the query and hash are not part of it. */
export function match(path: string): Route {
  const alias = ALIASES[normalized(path)];
  if (alias) return { page: alias } as Route;
  return (matchIn(TABLE, path) as Route | null) ?? { page: "missing", path };
}

/** The address of `route`, each parameter encoded. */
export function pathOf(route: Route): string {
  if (route.page === "missing") return route.path;
  const params = route as unknown as Record<string, string>;
  return PATTERNS[route.page].replace(/:([^/]+)/g, (_, name: string) =>
    encodeURIComponent(params[name] ?? ""));
}

/** Pages drawn as tabs of one screen, by the page whose address names it: a plan's, on its own pages or its campaign's, and an account's. */
const SCREENS: Partial<Record<Page, Page>> = {
  user: "user", "user-plans": "user", "user-permissions": "user", "user-keys": "user", "user-preauth": "user", "user-settings": "user",
  account: "account", "account-keys": "account",
  plan: "plan", "plan-cells": "plan", "plan-analysis": "plan", "plan-analysis-report": "plan", "plan-revisions": "plan",
  "plan-settings": "plan",
  "campaign-plan": "campaign-plan", "campaign-plan-cells": "campaign-plan", "campaign-plan-analysis": "campaign-plan",
  "campaign-plan-analysis-report": "campaign-plan", "campaign-plan-revisions": "campaign-plan",
  "campaign-plan-settings": "campaign-plan",
};

/** The screen `path` draws: a step between one screen's tabs keeps what it read and where the reader is. */
export function screenOf(path: string): string {
  const route = match(path);
  const page = route.page === "missing" ? undefined : SCREENS[route.page];
  return page ? pathOf({ ...route, page } as Route) : path;
}

/** `path` with `query`'s pairs, the empty ones dropped unless `keep` names them. */
export function href(path: string, query: [string, string][], keep: string[] = []): string {
  const pairs = new URLSearchParams(query.filter(([key, value]) => value !== "" || keep.includes(key)));
  const search = pairs.toString();
  return search ? `${path}?${search}` : path;
}

/** A page under one organization and nothing else. */
type OrgPage = { [P in Page]: [ParamNames<Patterns[P]>] extends ["org"] ? P : never }[Page];

/**
 * Where a link carrying a classic listing's old `org` filter lands: `page`
 * of that organization, the `kept` query keys carried over. `null` without one.
 */
export function legacyOrgFilter(query: URLSearchParams, page: OrgPage, kept: string[]): string | null {
  const org = query.get("org");
  if (!org) return null;
  const carried = new URLSearchParams();
  for (const key of kept) {
    const value = query.get(key);
    if (value) carried.set(key, value);
  }
  const search = carried.toString();
  return pathOf({ page, org } as Route) + (search ? `?${search}` : "");
}

/** A page with no parameter. */
type PlainPage = { [P in Page]: [ParamNames<Patterns[P]>] extends [never] ? P : never }[Page];

/** An owned listing's three branches: the reader's, an organization's, every record's. */
export interface OwnedPages {
  mine: PlainPage;
  org: OrgPage;
  all: PlainPage;
}

/**
 * Where a link carrying a classic owned listing's old `owner` or `org_id`
 * filter lands, as the classic console redirected it: the reader's own branch, the
 * organization's, or for `owner=org` alone the reader's organization, else
 * every record's. `limit` is kept; `null` without a filter.
 */
export function legacyOwnerFilter(query: URLSearchParams, pages: OwnedPages,
                                  reader: { org_id?: string | null; org_slug?: string | null }): string | null {
  const owner = query.get("owner") || null;
  const org_id = query.get("org_id") || null;
  if (owner !== "mine" && owner !== "org" && !org_id) return null;
  // `GET /api/v1/users/me` may name the reader's organization without its slug; its id is an address too.
  const own = reader.org_id
    ? (reader.org_slug ? handle({ slug: reader.org_slug, org_id: reader.org_id }) : reader.org_id)
    : null;
  const org = owner === "mine" ? null : org_id ?? own;
  const route = owner === "mine" ? { page: pages.mine }
    : org ? { page: pages.org, org }
    : { page: pages.all };
  const path = pathOf(route as Route);
  const limit = query.get("limit");
  return limit ? `${path}?${new URLSearchParams({ limit })}` : path;
}

/** The address of an organization's overview, or of `rest` under it, by its handle. */
export const orgLink = (org: { slug: string; org_id: string }, rest = "") =>
  `/app/orgs/${encodeURIComponent(handle(org))}${rest}`;

/** The address of an account's page. */
export const userLink = (userId: string) => `/app/users/${encodeURIComponent(userId)}`;

/** The address of a vault resource's page. */
export const vaultLink = (resourceId: string) => `/app/vault/${encodeURIComponent(resourceId)}`;

/** The address of a device's page. */
export const clientLink = (clientId: string) => `/app/clients/${encodeURIComponent(clientId)}`;

/** The address of an organization's private benchmark, the organization by handle. */
export const benchmarkLink = (org: { slug: string; org_id: string }, benchmarkId: string) =>
  `/app/benchmarks/${encodeURIComponent(handle(org))}/${encodeURIComponent(benchmarkId)}`;

/** The address of a plan's page, or of `rest` under it. */
export const planLink = (planId: string, rest = "") => `/app/plans/${encodeURIComponent(planId)}${rest}`;

/** The address of a plan on its campaign's page, `rest` (a query, a hash) after it. */
export const campaignPlanLink = (campaignId: string, planId: string, rest = "") =>
  `/app/campaigns/${encodeURIComponent(campaignId)}/plans/${encodeURIComponent(planId)}${rest}`;

/** The address of a campaign's page, or of `rest` under it. */
export const campaignLink = (campaignId: string, rest = "") =>
  `/app/campaigns/${encodeURIComponent(campaignId)}${rest}`;

/** Whether `href` is an address in this app, rather than one it hands to the browser. */
export const inApp = (href: string) =>
  href === "/app" || (href.startsWith("/app/") && !href.startsWith("/app/assets/"));

/** The organization handle a route sits under, which the tree opens. */
export function orgOf(route: Route): string | null {
  const org = (route as unknown as Record<string, unknown>).org;
  return typeof org === "string" ? org : null;
}
