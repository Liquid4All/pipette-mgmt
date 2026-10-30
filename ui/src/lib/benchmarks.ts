// The private benchmark pages (`benchmarks.html`, `benchmark.html`,
// `new_benchmark.html`): whose catalogs a reader reads and writes, the
// listing's rows, and what the forms send. Each gate copies the Rust rule it
// names; the routes still decide.

import { ApiError } from "./api";
import { textOrder } from "./format";
import { administersOrg, bySlug, ownOrg, type OrgRef } from "./orgs";
import { benchmarkLink, legacyOrgFilter } from "./routes";
import type { TableSpec } from "./table";
import type { OrgBenchmarks, OwnUserView } from "./api-types";

/** Which catalogs a listing draws (the classic `Branch`). */
export type BenchmarkBranch = { kind: "all" } | { kind: "org"; slug: string };

/** Whether the reader reads every organization's catalog (`read_reach`), rather than its own alone. */
export const readsEveryCatalog = (reader: OwnUserView) => reader.may.read_roster === "every";

/** The organizations a listing reads, and the catalog of each it draws. */
export interface BenchmarkCatalogs {
  read: OrgRef[];
  catalogs: { org: OrgRef; catalog: OrgBenchmarks }[];
}

/**
 * The organization `catalog` answered for, as the reader holds it, else asked
 * for; `reference`, the address's handle, stands in for a slug it cannot read.
 */
export const catalogOrg = async (
  catalog: OrgBenchmarks,
  reference: string,
  resolve: (org_id: string) => Promise<OrgRef | null>,
): Promise<OrgRef> =>
  await resolve(catalog.org_id).catch(() => null) ?? { org_id: catalog.org_id, slug: reference };

/**
 * The organizations the new-benchmark form offers, by slug: every one or the
 * reader's own, as its roster administration reaches; else why none.
 */
export function writeOrgs(reader: OwnUserView, every: OrgRef[]): { orgs: OrgRef[]; refusal: string } {
  const reach = reader.may.administer_roster;
  const own = ownOrg(reader);
  const orgs = reach === "every" ? [...every].sort(bySlug) : reach === "own" && own ? [own] : [];
  return orgs.length || reach === "every" ? { orgs, refusal: "" }
    : { orgs, refusal: "you administer no organization's benchmarks" };
}

/**
 * A listing's "New benchmark" action, for a reader who writes one of the
 * catalogs it reads; an organization's listing preselects it, by slug.
 */
export function newBenchmarkAction(reader: OwnUserView, read: OrgRef[], narrowed: OrgRef | null) {
  if (!read.some((org) => administersOrg(reader, org.org_id))) return null;
  const query = narrowed ? `?org=${encodeURIComponent(narrowed.slug)}` : "";
  return { href: `/app/benchmarks/new${query}`, label: "New benchmark" };
}

/** One listing row; `benchmark_type` is `null` for an entry that does not read. */
export interface BenchmarkRow {
  org: OrgRef;
  benchmark_id: string;
  benchmark_type: string | null;
  href: string;
}

export function benchmarkRows(catalogs: BenchmarkCatalogs["catalogs"]): BenchmarkRow[] {
  const row = (org: OrgRef, benchmark_id: string, benchmark_type: string | null): BenchmarkRow =>
    ({ org, benchmark_id, benchmark_type, href: benchmarkLink(org, benchmark_id) });
  return catalogs
    .flatMap(({ org, catalog }) => [
      ...catalog.benchmarks.map((entry) => row(org, entry.benchmark_id, entry.benchmark_type)),
      ...catalog.unreadable_benchmark_ids.map((benchmark_id) => row(org, benchmark_id, null)),
    ])
    .sort((a, b) => textOrder(a.org.slug, b.org.slug) || textOrder(a.benchmark_id, b.benchmark_id));
}

/** The benchmark listing's columns and filters; `orgs` where its rows name their organization. */
export function benchmarkTable(orgs: boolean): TableSpec<BenchmarkRow> {
  return {
    columns: [
      { key: "benchmark_id", label: "benchmark_id", raw: (row) => row.benchmark_id, href: (row) => row.href },
      ...(orgs ? [{ key: "organization", label: "organization", raw: (row: BenchmarkRow) => row.org.slug }] : []),
      { key: "benchmark_type", label: "benchmark_type", raw: (row) => row.benchmark_type },
    ],
    texts: [{ key: "id", label: "benchmark_id", texts: (row) => [row.benchmark_id] }],
    choices: [
      { key: "type", label: "benchmark_type", values: (row) => [row.benchmark_type ?? "unreadable"] },
      ...(orgs ? [{ key: "organization", label: "organization", values: (row: BenchmarkRow) => [row.org.slug] }] : []),
    ],
  };
}

/** What an empty listing says, as `benchmarks.html` words it; `read` is the organizations the reader reads. */
export function benchmarksEmpty(branch: BenchmarkBranch, read: number): string {
  if (!read) return "You belong to no organization, so no private benchmark is yours to read.";
  return branch.kind === "org" ? `${branch.slug} keeps no private benchmarks.` : "No private benchmarks you may see.";
}

/** Where a link carrying the old `org` filter lands (the classic `benchmarks`): that organization's listing, the paging kept. */
export const legacyCatalogFilter = (query: URLSearchParams) =>
  legacyOrgFilter(query, "org-benchmarks", ["limit", "offset"]);

/**
 * The entry `benchmark_id` names in `catalog`, found by key as
 * the classic `benchmarks::detail` read it, so one that does not read is still found
 * (`benchmark_type` `null`); the route's `404` otherwise.
 */
export function entryOf(catalog: OrgBenchmarks, benchmark_id: string): { benchmark_type: string | null } {
  const entry = catalog.benchmarks.find((each) => each.benchmark_id === benchmark_id);
  if (entry) return { benchmark_type: entry.benchmark_type };
  if (catalog.unreadable_benchmark_ids.includes(benchmark_id)) return { benchmark_type: null };
  throw new ApiError(404, `organization ${catalog.org_id} keeps no benchmark ${benchmark_id}`);
}

/** How `id` breaks `BenchmarkId`'s grammar, in its words; `null` when it does not. */
function idFault(id: string): string | null {
  const first = id[0];
  if (first === undefined) return "must not be empty";
  if (!/[A-Za-z0-9]/.test(first)) return `must start with [A-Za-z0-9], not '${first}'`;
  const ch = [...id].find((c) => !/[A-Za-z0-9_.-]/.test(c));
  return ch === undefined ? null : `contains '${ch}'; allowed: [A-Za-z0-9][A-Za-z0-9_.-]*`;
}

/**
 * The typed id, trimmed as the classic `create_benchmark` trimmed it and held to
 * `BenchmarkId`'s grammar, since the route would refuse it as a path it cannot parse.
 */
export function typedBenchmarkId(typed: string): string {
  const id = typed.trim();
  const fault = idFault(id);
  if (fault) throw new Error(`invalid benchmark_id: benchmark id ${fault}`);
  return id;
}

/** The id an entry's address names, refused as the classic `path_benchmark_id` refused it, before any read. */
export function pathBenchmarkId(raw: string): string {
  const fault = idFault(raw);
  if (fault) throw new ApiError(400, `invalid benchmark_id: benchmark id ${fault}`);
  return raw;
}

/** A refused create: a `412` is an id the organization keeps, worded as the classic `create_benchmark` worded it, with its page. */
export function createRefused(e: unknown, org: OrgRef, benchmark_id: string): { message: string; kept: string | null } | null {
  if (!(e instanceof ApiError) || e.status !== 412) return null;
  return { message: `${org.slug} already keeps a benchmark ${benchmark_id}`, kept: benchmarkLink(org, benchmark_id) };
}

/** A refused replace: a `412` is an entry deleted since the page drew it, which the edit does not write back. */
export const editRefused = (e: unknown, benchmark_id: string): string | null =>
  e instanceof ApiError && e.status === 412
    ? `${benchmark_id} was deleted meanwhile, so nothing was written: create it again from New benchmark.`
    : null;
