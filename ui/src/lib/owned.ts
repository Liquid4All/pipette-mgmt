// The listings of owned records, campaigns and vault resources, under the
// classic console's branches: what each narrows by, what an
// empty one says, and the listing a record's page sits under.

import { orgTrail, type Crumb } from "./crumbs";
import { handle } from "./format";
import { pathOf, type OwnedPages, type Route } from "./routes";
import { orgName, type OrgRef } from "./orgs";
import type { PrincipalOwner } from "./api-types";

/** The records a listing reads: everything in reach, or one organization's. */
export type OwnedScope = { kind: "all" } | { kind: "org"; org_id: string };

/** An owned listing as drawn: its scope, an organization's with the handle the page names it by. */
export type OwnedBranch = Exclude<OwnedScope, { kind: "org" }> | ({ kind: "org" } & OrgRef);

/**
 * The query of `branch`'s listing, newest first as the classic listings are,
 * with `extra` asked of each row. A branch's rows share one owner, so only
 * everything's names owners (the classic `owned_query` narrows the rest).
 */
export function ownedQuery(branch: OwnedScope, extra: [string, string][], cursor: string | null,
                           limit: string): string {
  const pairs: [string, string][] = [["order", "newest"], ...extra];
  if (branch.kind === "all") pairs.push(["names", "true"]);
  if (branch.kind === "org") pairs.push(["owner", "org"], ["org_id", branch.org_id]);
  if (limit) pairs.push(["limit", limit]);
  if (cursor) pairs.push(["cursor", cursor]);
  return new URLSearchParams(pairs).toString();
}

/** Why `branch` drew none of `records`, as the classic listings word it. */
export function ownedEmpty(branch: OwnedBranch, records: string): string {
  switch (branch.kind) {
    case "org": return `${orgName(branch)} owns no ${records} you may see.`;
    case "all": return `No ${records} you may see.`;
  }
}

/** The listing a record's page sits under, and the section above that listing. */
export interface Under {
  href: string;
  section: readonly Crumb[];
}

export const underTrail = (under: Under, label: string): readonly Crumb[] => [...under.section, { label, href: under.href }];

/**
 * The listing a record `owner` owns sits under (the classic `owner_branch`): its
 * organization's where the tree draws it (`known`), else everything's.
 */
export function ownerBranch(owner: PrincipalOwner, known: (OrgRef | null)[], pages: OwnedPages): Under {
  const org = known.find((each) => each?.org_id === principalOf(owner));
  if (org) return { href: pathOf({ page: pages.org, org: handle(org) } as Route), section: orgTrail(org) };
  return { href: pathOf({ page: pages.all } as Route), section: [] };
}

/** An owner's organization id: the `principal_id` a record names it by. */
export const principalOf = (owner: PrincipalOwner): string => owner.org_id;

/** An owner as a row names it: by the body's `names`, else its id. */
export const ownerName = (owner: PrincipalOwner, names: Record<string, string>): string =>
  names[principalOf(owner)] ?? principalOf(owner);
