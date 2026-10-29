// Which organization an address names, by the classic console's rule
// (the classic `handle`): its handle, or its id; and what its forms send.

import { ApiError } from "./api";
import { handle, textOrder } from "./format";
import { reaches } from "./rights";
import { orgLink } from "./routes";
import type { TableSpec } from "./table";
import type { CreateOrgRequest, OrgView, OwnUserView, UpdateOrgRequest } from "./api-types";

/** An organization as the tree and a page's branch name it. */
export type OrgRef = Pick<OrgView, "org_id" | "slug">;

/** The reader's own organization, which `GET /api/v1/users/me` names. */
export const ownOrg = (reader: Pick<OwnUserView, "org_id" | "org_slug">): OrgRef | null =>
  reader.org_id && reader.org_slug ? { org_id: reader.org_id, slug: reader.org_slug } : null;

/** Whether the reader administers `org_id`'s roster. */
export const administersOrg = (reader: OwnUserView, org_id: string | null | undefined) =>
  reaches(reader, reader.may.administer_roster, org_id);

/** Organizations by slug, as Rust orders a `str`. */
export const bySlug = (a: OrgRef, b: OrgRef): number => textOrder(a.slug, b.slug);

/** Whether `reference` names `org`. */
export const names = (org: OrgRef, reference: string): boolean =>
  handle(org) === reference || org.org_id === reference;

/** The one of `known` that `reference` names, or `null`. */
export const knownOrg = <T extends OrgRef>(reference: string, known: (T | null)[]): T | null =>
  known.find((org): org is T => org !== null && names(org, reference)) ?? null;

/**
 * The organization `reference` names: one the reader already holds, else (or,
 * with `fresh`, always) what `fetch` (`GET /api/v1/orgs/{reference}`) answers; `null`
 * on its `404`. Any other failure is thrown, since it says nothing about the name.
 */
export async function resolveOrg<T extends OrgRef, F extends OrgRef>(
  reference: string,
  known: (T | null)[],
  fetch: (reference: string) => Promise<F>,
  fresh = false,
): Promise<T | F | null> {
  const held = knownOrg(reference, known);
  if (held && !fresh) return held;
  try {
    return await fetch(held?.org_id ?? reference);
  } catch (e) {
    if (e instanceof ApiError && e.status === 404) return null;
    throw e;
  }
}

/**
 * Whether a failed read of an organization means there is none: a `404`, or a
 * `403` on one the reader does not hold, as absent and out of reach read alike.
 * A `403` on one it holds (`held`) refuses a grant, and is said as such.
 */
export const absentOrg = (e: unknown, held: boolean): boolean =>
  e instanceof ApiError && (e.status === 404 || (e.status === 403 && !held));

/** The rename's body, refused as the classic `edit_org` refused a blank name. */
export function renameBody(display_name: string): UpdateOrgRequest {
  const name = display_name.trim();
  if (!name) throw new Error("Name the organization");
  return { display_name: name };
}

/** A handle as `OrgSlug` takes one, refused in the classic `create_org`'s words rather than the body parser's. */
function orgSlug(typed: string): string {
  const slug = typed.trim().replace(/[A-Z]/g, (c) => c.toLowerCase());
  const refuse = (why: string) => new Error(`invalid handle: organization slug ${why}`);
  if (!slug) throw refuse("must not be empty");
  if (slug.startsWith("org_")) {
    throw refuse(`"${slug}" may not start with "org_", which is how an organization id is spelled`);
  }
  const bytes = new TextEncoder().encode(slug).length;
  if (bytes > 64) throw refuse(`"${slug}" is too long (${bytes} chars, max 64)`);
  const ch = [...slug].find((c) => !/[a-z0-9_-]/.test(c));
  if (ch !== undefined) throw refuse(`"${slug}" contains invalid character '${ch}'; allowed: [a-z0-9_-] (no '/')`);
  return slug;
}

/** The create's body: a blank display name is left out, which names it after its handle. */
export function createBody(slug: string, display_name: string): CreateOrgRequest {
  const name = display_name.trim();
  return name ? { slug: orgSlug(slug), display_name: name } : { slug: orgSlug(slug) };
}

/** Why the create form is refused, as `POST /api/v1/orgs` would refuse it (`manage_any_orgs`); empty when it is not. */
export const createOrgRefusal = (reader: OwnUserView): string =>
  reader.may.create_orgs ? "" : "user lacks the manage_any_orgs permission";

/** The organization listing's columns and filters. */
export const ORG_TABLE: TableSpec<OrgView> = {
  columns: [
    { key: "org_id", label: "org_id", raw: (row) => row.org_id, href: (row) => orgLink(row) },
    { key: "slug", label: "slug", raw: (row) => row.slug },
    { key: "display_name", label: "display_name", raw: (row) => row.display_name },
    { key: "status", label: "status", raw: (row) => row.status, as: "state" },
    { key: "member_count", label: "member_count", raw: (row) => row.member_count ?? null, num: true },
    { key: "client_count", label: "client_count", raw: (row) => row.client_count ?? null, num: true },
    { key: "created_at", label: "created_at", raw: (row) => row.created_at, as: "time" },
  ],
  texts: [
    { key: "id", label: "org_id", texts: (row) => [row.org_id] },
    { key: "q", label: "slug or display_name", texts: (row) => [row.slug, row.display_name] },
  ],
  choices: [{ key: "status", label: "status", values: (row) => [row.status], order: ["active", "disabled"] }],
};
