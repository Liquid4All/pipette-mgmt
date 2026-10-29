import { describe, expect, test } from "vitest";
import { orgLink, pathOf } from "../src/lib/routes";
import { canonical, meLeaves, orgBranches, orgLeaves, serverLeaves } from "../src/lib/tree";
import type { Route } from "../src/lib/routes";

const own = { org_id: "org_own", slug: "acme" };
const other = { org_id: "org_other", slug: "beta" };
const away = { org_id: "org_away", slug: "gamma" };

describe("tree", () => {
  test("the own organization draws its leaves, the others are links", () => {
    expect(orgBranches(own, [other], null)).toEqual([{ org: own, open: true }, { org: other, open: false }]);
  });

  test("the organization a page sits under opens", () => {
    expect(orgBranches(own, [other], other)).toEqual([{ org: own, open: true }, { org: other, open: true }]);
  });

  test("one the tree does not list joins it while a page sits under it", () => {
    expect(orgBranches(own, [], away)).toEqual([{ org: own, open: true }, { org: away, open: true }]);
    expect(orgBranches(null, [], null)).toEqual([]);
  });

  test("each leaf opens its page in the app", () => {
    const reads = { roster: true, fleet: true, audit: true };
    expect(meLeaves(reads).map((leaf) => leaf.href)).toEqual(
      ["/app/me/campaigns", "/app/me/vault", "/app/me/devices", "/app/account"]);
    expect(orgLeaves({ org_id: "org_new", slug: "new" }, reads).map((leaf) => leaf.href)).toEqual([
      "/app/orgs/org_new/campaigns", "/app/orgs/org_new/vault",
      "/app/orgs/org_new/benchmarks", "/app/orgs/org_new/members", "/app/orgs/org_new/devices",
      "/app/orgs/org_new/audit", "/app/orgs/org_new",
    ]);
    expect(orgLink(other)).toBe("/app/orgs/beta");
    expect(serverLeaves(true, true, true, true).map((leaf) => leaf.href)).toEqual(
      ["/app/orgs", "/app/server/users", "/app/server/devices", "/app/server/catalog", "/app/server/model-params", "/app/server/audit"]);
  });

  test("a page reached by its organization's id marks the leaf its handle names", () => {
    const byId: Route = { page: "org-devices", org: "org_away" };
    expect(canonical(byId, away)).toEqual({ page: "org-devices", org: "gamma" });
    expect(pathOf(canonical(byId, away))).toBe(orgLeaves(away, { roster: true, fleet: true, audit: false })[4]?.href);
    expect(canonical(byId, null)).toEqual(byId);
    expect(canonical({ page: "account" }, away)).toEqual({ page: "account" });
    // The settings page by id marks its Settings leaf, and the organization joins the tree.
    const overview = pathOf(canonical({ page: "org", org: "org_away" }, away));
    expect(overview).toBe(orgLeaves(away, { roster: true, fleet: false, audit: false }).at(-1)?.href);
    expect(orgBranches(own, [], away)).toContainEqual({ org: away, open: true });
  });

  test("leaves follow the reader's grants", () => {
    expect(meLeaves({ roster: false, fleet: false, audit: false }).map((leaf) => leaf.label)).toEqual(
      ["Campaigns", "Vault", "Account"]);
    expect(orgLeaves(own, { roster: false, fleet: true, audit: false }).map((leaf) => leaf.label)).toEqual(
      ["Campaigns", "Vault", "Benchmarks"]);
    expect(orgLeaves(own, { roster: true, fleet: false, audit: true }).map((leaf) => leaf.label)).toEqual(
      ["Campaigns", "Vault", "Benchmarks", "Members", "Audit", "Settings"]);
    expect(serverLeaves(false, true, false, false).map((leaf) => leaf.label)).toEqual(["Unclaimed devices", "Catalog"]);
    expect(serverLeaves(false, false, true, false).map((leaf) => leaf.label)).toEqual(["Catalog", "Model params"]);
    expect(serverLeaves(false, false, false, true).map((leaf) => leaf.label)).toEqual(["Catalog", "Audit log"]);
    expect(serverLeaves(false, false, false, false).map((leaf) => leaf.label)).toEqual(["Catalog"]);
  });
});
