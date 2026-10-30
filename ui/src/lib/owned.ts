// The listings of owned records, campaigns and vault resources, under the
// classic console's branches: what each narrows by, what an
// empty one says, and the listing a record's page sits under.

import { ME, orgTrail, type Crumb } from "./crumbs";
import { handle } from "./format";
import { pathOf, type OwnedPages, type Route } from "./routes";
import type { OrgRef } from "./orgs";
import type { OwnUserView, PrincipalOwner } from "./api-types";

/** The records a listing reads: everything in reach, the reader's own, or one organization's. */
export type OwnedScope = { kind: "all" } | { kind: "me" } | { kind: "org"; org_id: string };

/** An owned listing as drawn: its scope, an organization's with the handle the page names it by. */
export type OwnedBranch = Exclude<OwnedScope, { kind: "org" }> | { kind: "org"; org_id: string; slug: string };

/**
 * The query of `branch`'s listing, newest first as the classic listings are,
 * with `extra` asked of each row. A branch's rows share one owner, so only
 * everything's names owners (the classic `owned_query` narrows the rest).
 */
export function ownedQuery(branch: OwnedScope, extra: [string, string][], cursor: string | null,
                           limit: string): string {
  const pairs: [string, string][] = [["order", "newest"], ...extra];
  if (branch.kind === "all") pairs.push(["names", "true"]);
  if (branch.kind === "me") pairs.push(["owner", "mine"]);
  if (branch.kind === "org") pairs.push(["owner", "org"], ["org_id", branch.org_id]);
  if (limit) pairs.push(["limit", limit]);
  if (cursor) pairs.push(["cursor", cursor]);
  return new URLSearchParams(pairs).toString();
}

/** Why `branch` drew none of `records`, as the classic listings word it. */
export function ownedEmpty(branch: OwnedBranch, records: string): string {
  switch (branch.kind) {
    case "me": return `You own no ${records}.`;
    case "org": return `${branch.slug} owns no ${records} you may see.`;
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
 * The listing a record `owner` owns sits under (the classic `owner_branch`): the
 * reader's own for its own record, an organization's the tree draws (`known`)
 * for one it owns, else everything's.
 */
export function ownerBranch(reader: Pick<OwnUserView, "user_id">, owner: PrincipalOwner, known: (OrgRef | null)[],
                            pages: OwnedPages): Under {
  if (owner.type === "user" && owner.user_id === reader.user_id) {
    return { href: pathOf({ page: pages.mine } as Route), section: ME };
  }
  const org = owner.type === "org" ? known.find((each) => each?.org_id === owner.org_id) : null;
  if (org) return { href: pathOf({ page: pages.org, org: handle(org) } as Route), section: orgTrail(org) };
  return { href: pathOf({ page: pages.all } as Route), section: [] };
}

/** An owner's id, whichever kind it is: the `principal_id` a record names it by. */
export const principalOf = (owner: PrincipalOwner): string => (owner.type === "org" ? owner.org_id : owner.user_id);

/** An owner as a row names it: by the body's `names`, else its id. */
export const ownerName = (owner: PrincipalOwner, names: Record<string, string>): string =>
  names[principalOf(owner)] ?? principalOf(owner);
