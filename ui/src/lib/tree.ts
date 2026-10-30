// What the tree draws (the classic console's `layout.html`): each leaf and the
// branch it opens.

import { handle } from "./format";
import { orgLink, orgOf, type Route } from "./routes";

export interface Leaf {
  label: string;
  href: string;
}

export interface OrgBranch<O> {
  org: O;
  /** Drawn with its leaves, rather than as a link to its overview. */
  open: boolean;
}

// Hiding a leaf only spares the reader a refusal: its page still draws what its routes decide.
/** What the tree may draw: the reader's resolved grants it reads. */
export interface Reads {
  roster: boolean;
  fleet: boolean;
  /** The organization's audit log, which its administrator reads. */
  audit: boolean;
}

/**
 * The organizations the tree lists: the reader's own, the others, and one a
 * page sits under that neither holds, which joins while it is open. The own one
 * and the one a page sits under draw their leaves.
 */
export function orgBranches<O extends { org_id: string }>(own: O | null, others: O[], at: O | null): OrgBranch<O>[] {
  const listed = [...(own ? [own] : []), ...others];
  const visiting = at && !listed.some((org) => org.org_id === at.org_id) ? [at] : [];
  return [...listed, ...visiting].map((org) =>
    ({ org, open: org.org_id === own?.org_id || org.org_id === at?.org_id }));
}

/** The reader's own leaves under **Me**. */
export const meLeaves = (reads: Reads): Leaf[] => [
  { label: "Campaigns", href: "/app/me/campaigns" },
  { label: "Vault", href: "/app/me/vault" },
  ...(reads.fleet ? [{ label: "Devices", href: "/app/me/devices" }] : []),
  { label: "Account", href: "/app/account" },
];

/** An open organization's leaves, each its own address. */
export function orgLeaves(org: { slug: string; org_id: string }, reads: Reads): Leaf[] {
  return [
    { label: "Campaigns", href: orgLink(org, "/campaigns") },
    { label: "Vault", href: orgLink(org, "/vault") },
    { label: "Benchmarks", href: orgLink(org, "/benchmarks") },
    ...(reads.roster ? [{ label: "Members", href: orgLink(org, "/members") }] : []),
    ...(reads.roster && reads.fleet ? [{ label: "Devices", href: orgLink(org, "/devices") }] : []),
    ...(reads.audit ? [{ label: "Audit", href: orgLink(org, "/audit") }] : []),
    ...(reads.roster ? [{ label: "Settings", href: orgLink(org) }] : []),
  ];
}

/** The **Server** branch: the organizations, and what no account or organization holds. */
export const serverLeaves = (everyOrg: boolean, fleet: boolean, everyPlan: boolean, everyAudit: boolean): Leaf[] => [
  ...(everyOrg ? [{ label: "Organizations", href: "/app/orgs" },
                  { label: "Unaffiliated users", href: "/app/server/users" }] : []),
  ...(fleet ? [{ label: "Unclaimed devices", href: "/app/server/devices" }] : []),
  { label: "Catalog", href: "/app/server/catalog" },
  ...(everyPlan ? [{ label: "Model params", href: "/app/server/model-params" }] : []),
  ...(everyAudit ? [{ label: "Audit log", href: "/app/server/audit" }] : []),
];

/**
 * `route` with its organization `at` named by handle, as the leaves name it,
 * however the address named it.
 */
export const canonical = (route: Route, at: { slug: string; org_id: string } | null): Route =>
  at && orgOf(route) ? { ...route, org: handle(at) } as unknown as Route : route;
