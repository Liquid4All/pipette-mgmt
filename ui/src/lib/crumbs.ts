// A page's breadcrumbs: the pages above it, as the tree branches them. A step
// without a page of its own, such as **Me**, is drawn as plain words.

import { handle } from "./format";
import { pathOf } from "./routes";

export interface Crumb {
  label: string;
  href?: string;
}

export const ME: readonly Crumb[] = [{ label: "Me" }];
export const SERVER: readonly Crumb[] = [{ label: "Server" }];
export const ORGS: Readonly<Crumb> = { label: "Organizations", href: pathOf({ page: "orgs" }) };
export const USERS: Readonly<Crumb> = { label: "Users", href: pathOf({ page: "users" }) };

/** An organization's steps, by its record or, before that resolves, the handle an address named. */
export function orgTrail(org: { slug: string; org_id: string } | string): readonly Crumb[] {
  const [label, reference] = typeof org === "string" ? [org, org] : [org.slug, handle(org)];
  return [ORGS, { label, href: pathOf({ page: "org", org: reference }) }];
}
