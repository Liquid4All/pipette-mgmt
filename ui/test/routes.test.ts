import { describe, expect, test } from "vitest";
import {
  benchmarkLink, campaignLink, campaignPlanLink, clientLink, href, inApp, legacyOwnerFilter, match,
  matchIn, orgLink, pathOf, planLink, screenOf, table, userLink, vaultLink, type Route,
} from "../src/lib/routes";

/** The page an address draws, its query and hash aside. */
const drawn = (address: string) => match(address.split(/[?#]/)[0]!).page;

describe("routes", () => {
  test("each registered page reads back from its own address", () => {
    const routes: Route[] = [
      { page: "account" }, { page: "account-keys" }, { page: "login-cli" }, { page: "orgs" }, { page: "server-users" }, { page: "server-devices" },
      { page: "server-catalog" }, { page: "server-catalog-entry", name: "My.Build-1" },
      { page: "server-model-params" }, { page: "server-audit" }, { page: "org-audit", org: "acme" },
      { page: "vault-new" }, { page: "clients" }, { page: "me-devices" }, { page: "org-devices", org: "acme" },
      { page: "client", client_id: "ev1_abc" }, { page: "users" }, { page: "user-new" },
      { page: "user", user_id: "user_1" }, { page: "user-plans", user_id: "user_1" },
      { page: "user-permissions", user_id: "user_1" }, { page: "user-keys", user_id: "user_1" }, { page: "user-preauth", user_id: "user_1" },
      { page: "user-settings", user_id: "user_1" }, { page: "org-members", org: "acme" }, { page: "org", org: "acme" },
      { page: "org-new" }, { page: "benchmarks" }, { page: "benchmark-new" },
      { page: "benchmark", org: "acme", benchmark_id: "eval_ifbench_2026.06.1" }, { page: "org-benchmarks", org: "acme" },
      { page: "vault" }, { page: "vault-resource", resource_id: "vault_1" }, { page: "me-vault" },
      { page: "org-vault", org: "acme" }, { page: "plans" }, { page: "plan", plan_id: "plan-0195" },
      { page: "plan-cells", plan_id: "plan-0195" }, { page: "plan-results", plan_id: "plan-0195" },
      { page: "plan-analysis", plan_id: "plan-0195" },
      { page: "plan-analysis-report", plan_id: "plan-0195", view: "decode_throughput" },
      { page: "plan-reports", plan_id: "plan-0195" },
      { page: "plan-report", plan_id: "plan-0195", view: "decode_throughput" },
      { page: "plan-revisions", plan_id: "plan-0195" }, { page: "plan-settings", plan_id: "plan-0195" },
      { page: "campaigns" }, { page: "campaign-new" }, { page: "campaign", campaign_id: "cmp_1" },
      { page: "campaign-plan", campaign_id: "cmp_1", plan_id: "plan-1" },
      { page: "campaign-plan-cells", campaign_id: "cmp_1", plan_id: "plan-1" },
      { page: "campaign-plan-analysis", campaign_id: "cmp_1", plan_id: "plan-1" },
      { page: "campaign-plan-analysis-report", campaign_id: "cmp_1", plan_id: "plan-1", view: "eval" },
      { page: "campaign-plan-reports", campaign_id: "cmp_1", plan_id: "plan-1" },
      { page: "campaign-plan-report", campaign_id: "cmp_1", plan_id: "plan-1", view: "rows" },
      { page: "campaign-plan-revisions", campaign_id: "cmp_1", plan_id: "plan-1" },
      { page: "campaign-plan-settings", campaign_id: "cmp_1", plan_id: "plan-1" },
      { page: "campaign-document", campaign_id: "cmp_1" },
      { page: "me-campaigns" }, { page: "org-campaigns", org: "acme" },
    ];
    for (const route of routes) expect(match(pathOf(route))).toEqual(route);
  });

  test("a trailing slash names the same page", () => {
    expect(match("/app/orgs/")).toEqual({ page: "orgs" });
    expect(match("/app/server/devices//")).toEqual({ page: "server-devices" });
  });

  test("a catalog entry name is one encoded path segment", () => {
    expect(pathOf({ page: "server-catalog-entry", name: "My Build" })).toBe("/app/server/catalog/My%20Build");
    expect(match("/app/server/catalog/My%20Build")).toEqual({ page: "server-catalog-entry", name: "My Build" });
  });

  test("the app's root is an alias of the reader's campaigns", () => {
    expect(match("/app/")).toEqual({ page: "me-campaigns" });
    expect(match("/app")).toEqual({ page: "me-campaigns" });
    expect(pathOf(match("/app/"))).toBe("/app/me/campaigns");
  });

  test("an address with no page is missing, as typed", () => {
    expect(match("/app/nowhere/")).toEqual({ page: "missing", path: "/app/nowhere/" });
    expect(match("/app/orgs/acme/elsewhere")).toEqual({ page: "missing", path: "/app/orgs/acme/elsewhere" });
  });

  const entries = table({
    benchmark: "/app/benchmarks/:org/:id",
    org: "/app/orgs/:org",
    "new-org": "/app/orgs/new",
    members: "/app/orgs/:org/members",
  });

  test("a static segment wins over a parameter, in any order", () => {
    expect(matchIn(entries, "/app/orgs/new")).toEqual({ page: "new-org" });
    expect(matchIn(entries, "/app/orgs/acme")).toEqual({ page: "org", org: "acme" });
    expect(matchIn(entries, "/app/orgs/new/members")).toEqual({ page: "members", org: "new" });
  });

  test("a shorter pattern between two does not break the order", () => {
    const split = table({ org: "/app/orgs/:org", orgs: "/app/orgs", "new-org": "/app/orgs/new" });
    expect(matchIn(split, "/app/orgs/new")).toEqual({ page: "new-org" });
    expect(matchIn(split, "/app/orgs")).toEqual({ page: "orgs" });
  });

  test("every registration order matches alike", () => {
    const patterns: [string, string][] = [
      ["org", "/app/orgs/:org"], ["orgs", "/app/orgs"], ["new-org", "/app/orgs/new"],
      ["members", "/app/orgs/:org/members"], ["new-members", "/app/orgs/new/members"],
    ];
    const orders = (rest: [string, string][]): [string, string][][] => rest.length <= 1 ? [rest]
      : rest.flatMap((first, i) => orders(rest.filter((_, j) => j !== i)).map((tail) => [first, ...tail]));
    for (const order of orders(patterns)) {
      const shuffled = table(Object.fromEntries(order));
      expect(matchIn(shuffled, "/app/orgs/new")).toEqual({ page: "new-org" });
      expect(matchIn(shuffled, "/app/orgs/acme")).toEqual({ page: "org", org: "acme" });
      expect(matchIn(shuffled, "/app/orgs/new/members")).toEqual({ page: "new-members" });
      expect(matchIn(shuffled, "/app/orgs/acme/members")).toEqual({ page: "members", org: "acme" });
    }
  });

  test("parameters are decoded, and a dot is part of one", () => {
    expect(matchIn(entries, "/app/benchmarks/acme/eval_ifbench_2026.06.1")).toEqual(
      { page: "benchmark", org: "acme", id: "eval_ifbench_2026.06.1" });
    expect(matchIn(entries, "/app/orgs/a%2Fb%20c")).toEqual({ page: "org", org: "a/b c" });
    expect(matchIn(entries, "/app/orgs/%E0%A4%A")).toBeNull();
  });

  test("a record's link is built once, its id encoded", () => {
    expect(orgLink({ org_id: "org_1", slug: "acme" })).toBe("/app/orgs/acme");
    expect(orgLink({ org_id: "org_1", slug: "new" })).toBe("/app/orgs/org_1");
    expect(orgLink({ org_id: "org_1", slug: "acme" }, "/devices?status=pending")).toBe("/app/orgs/acme/devices?status=pending");
    expect(orgLink({ org_id: "org_1", slug: "acme" }, "/vault")).toBe("/app/orgs/acme/vault");
    expect(campaignLink("cmp_a/b")).toBe("/app/campaigns/cmp_a%2Fb");
    expect(userLink("usr_a/b")).toBe("/app/users/usr_a%2Fb");
    expect(clientLink("ev1_x")).toBe("/app/clients/ev1_x");
    expect(vaultLink("vault_1")).toBe("/app/vault/vault_1");
    expect(planLink("plan-1")).toBe("/app/plans/plan-1");
    expect(planLink("plan-1", "/results")).toBe("/app/plans/plan-1/results");
    expect(campaignPlanLink("cmp_1", "plan-1")).toBe("/app/campaigns/cmp_1/plans/plan-1");
  });

  test("the vault pages are the app's, the upload form ahead of a resource", () => {
    expect(match("/app/vault/new")).toEqual({ page: "vault-new" });
    expect(match("/app/vault")).toEqual({ page: "vault" });
    expect(match("/app/vault/vault_1/")).toEqual({ page: "vault-resource", resource_id: "vault_1" });
    expect(match("/app/me/vault")).toEqual({ page: "me-vault" });
    expect(match("/app/orgs/new/vault")).toEqual({ page: "org-vault", org: "new" });
    expect(drawn("/app/orgs/acme/vault?cursor=vault_9")).not.toBe("missing");
    expect(match("/app/vault/vault_1/content")).toEqual({ page: "missing", path: "/app/vault/vault_1/content" });
  });

  test("an old owner filter lands on its branch, its limit kept", () => {
    const acme = { org_slug: "acme", org_id: "org_1" };
    const pages = { mine: "me-vault", org: "org-vault", all: "vault" } as const;
    const filter = (search: string, reader: { org_id?: string | null; org_slug?: string | null } = acme) =>
      legacyOwnerFilter(new URLSearchParams(search), pages, reader);
    expect(filter("")).toBeNull();
    expect(filter("owner=&org_id=&limit=5")).toBeNull();
    expect(filter("owner=mine&org_id=org_2&limit=5&offset=50")).toBe("/app/me/vault?limit=5");
    expect(filter("org_id=org_2")).toBe("/app/orgs/org_2/vault");
    expect(filter("owner=org&org_id=org_2")).toBe("/app/orgs/org_2/vault");
    expect(filter("owner=org")).toBe("/app/orgs/acme/vault");
    expect(filter("owner=org", { org_slug: "new", org_id: "org_1" })).toBe("/app/orgs/org_1/vault");
    expect(filter("owner=org", { org_id: "org_1" })).toBe("/app/orgs/org_1/vault");
    expect(filter("owner=org", {})).toBe("/app/vault");
    expect(filter("owner=org&limit=5", { org_id: null })).toBe("/app/vault?limit=5");
  });

  test("the device listings and the device page are the app's", () => {
    expect(drawn("/app/clients?status=pending")).not.toBe("missing");
    expect(drawn("/app/me/devices")).not.toBe("missing");
    expect(drawn("/app/orgs/acme/devices?status=pending#device-keys")).not.toBe("missing");
    expect(match("/app/orgs/org_1/devices/")).toEqual({ page: "org-devices", org: "org_1" });
    expect(match("/app/clients/ev1_x/")).toEqual({ page: "client", client_id: "ev1_x" });
    expect(match("/app/clients/ev1_x/owners")).toEqual({ page: "missing", path: "/app/clients/ev1_x/owners" });
  });

  test("the account pages are the app's, the create form ahead of an account", () => {
    expect(match("/app/users/new")).toEqual({ page: "user-new" });
    expect(match("/app/users/user_x/")).toEqual({ page: "user", user_id: "user_x" });
    expect(match("/app/orgs/new/members")).toEqual({ page: "org-members", org: "new" });
    expect(drawn("/app/users?status=pending")).not.toBe("missing");
    expect(drawn("/app/orgs/acme/members?status=pending")).not.toBe("missing");
    expect(match("/app/users/user_x/keys")).toEqual({ page: "user-keys", user_id: "user_x" });
    expect(match("/app/users/user_x/owners")).toEqual({ page: "missing", path: "/app/users/user_x/owners" });
  });

  test("the campaign pages are the app's, the create form ahead of a campaign", () => {
    expect(match("/app/campaigns/new/")).toEqual({ page: "campaign-new" });
    expect(match("/app/orgs/acme/campaigns")).toEqual({ page: "org-campaigns", org: "acme" });
    expect(drawn("/app/campaigns/new?owner=org_1")).not.toBe("missing");
    expect(drawn("/app/me/campaigns?limit=5")).not.toBe("missing");
    expect(match("/app/campaigns/cmp_1")).toEqual({ page: "campaign", campaign_id: "cmp_1" });
    expect(match("/app/campaigns/cmp_1/plans/plan-1/")).toEqual(
      { page: "campaign-plan", campaign_id: "cmp_1", plan_id: "plan-1" });
    expect(match("/app/campaigns/cmp_1/document")).toEqual({ page: "campaign-document", campaign_id: "cmp_1" });
    expect(match("/app/campaigns/cmp_1/plans")).toEqual({ page: "missing", path: "/app/campaigns/cmp_1/plans" });
    expect(match("/app/campaigns/cmp_1/plans/plan-1/analysis/vl_spec_decode/")).toEqual(
      { page: "campaign-plan-analysis-report", campaign_id: "cmp_1", plan_id: "plan-1", view: "vl_spec_decode" });
    expect(drawn("/app/campaigns/cmp_1/plans/plan-1/results")).toBe("missing");
    expect(match("/app/campaigns/cmp_1/plans/plan-1/revisions")).toEqual(
      { page: "campaign-plan-revisions", campaign_id: "cmp_1", plan_id: "plan-1" });
    expect(match("/app/campaigns/cmp_1/plans/plan-1/cells/")).toEqual(
      { page: "campaign-plan-cells", campaign_id: "cmp_1", plan_id: "plan-1" });
    expect(drawn("/app/campaigns/cmp_1?offset=5")).not.toBe("missing");
    // The retired variant edit has no page; its plan's document is edited whole.
    expect(drawn("/app/campaigns/cmp_1/variants")).toBe("missing");
    expect(campaignPlanLink("cmp_1", "plan-1", "?offset=3#job-a")).toBe("/app/campaigns/cmp_1/plans/plan-1?offset=3#job-a");
  });

  test("the organization pages are the app's, the create form ahead of an organization", () => {
    expect(match("/app/orgs/new")).toEqual({ page: "org-new" });
    expect(match("/app/orgs/acme/")).toEqual({ page: "org", org: "acme" });
    expect(match("/app/orgs/org_1")).toEqual({ page: "org", org: "org_1" });
    expect(drawn("/app/orgs/new")).not.toBe("missing");
    expect(drawn("/app/orgs/acme/campaigns")).not.toBe("missing");
  });

  test("the benchmark pages are the app's, the create form ahead of an entry", () => {
    expect(match("/app/benchmarks")).toEqual({ page: "benchmarks" });
    expect(match("/app/benchmarks/new")).toEqual({ page: "benchmark-new" });
    expect(match("/app/benchmarks/new/eval_x")).toEqual({ page: "benchmark", org: "new", benchmark_id: "eval_x" });
    expect(match("/app/orgs/acme/benchmarks")).toEqual({ page: "org-benchmarks", org: "acme" });
    expect(drawn("/app/orgs/acme/benchmarks?offset=50")).not.toBe("missing");
    expect(drawn("/app/benchmarks/new?org=acme")).not.toBe("missing");
    expect(benchmarkLink({ slug: "acme", org_id: "org_1" }, "eval_ifbench_2026.06.1"))
      .toBe("/app/benchmarks/acme/eval_ifbench_2026.06.1");
  });

  test("a plan's tabs are the app's: cells, analysis, one report, revisions and settings; results and reports are old addresses", () => {
    expect(match("/app/plans/plan-1/results")).toEqual({ page: "plan-results", plan_id: "plan-1" });
    expect(match("/app/plans/plan-1/cells")).toEqual({ page: "plan-cells", plan_id: "plan-1" });
    expect(drawn("/app/plans/plan-1/cells?offset=50&limit=10#job-a")).not.toBe("missing");
    expect(match("/app/plans/plan-1/analysis/")).toEqual({ page: "plan-analysis", plan_id: "plan-1" });
    expect(match("/app/plans/plan-1/analysis/decode_throughput")).toEqual(
      { page: "plan-analysis-report", plan_id: "plan-1", view: "decode_throughput" });
    expect(match("/app/plans/plan-1/analysis/a%2Fb")).toEqual({ page: "plan-analysis-report", plan_id: "plan-1", view: "a/b" });
    expect(match("/app/plans/plan-1/reports/")).toEqual({ page: "plan-reports", plan_id: "plan-1" });
    expect(match("/app/plans/plan-1/reports/eval")).toEqual({ page: "plan-report", plan_id: "plan-1", view: "eval" });
    expect(match("/app/plans/plan-1/revisions")).toEqual({ page: "plan-revisions", plan_id: "plan-1" });
    expect(match("/app/plans/plan-1/settings")).toEqual({ page: "plan-settings", plan_id: "plan-1" });
    expect(drawn("/app/plans/plan-1/reports/rows?benchmark=eval&status=failed&offset=50")).not.toBe("missing");
    // The tabs before these were never served.
    expect(drawn("/app/plans/plan-1/results/eval")).toBe("missing");
    expect(drawn("/app/plans/plan-1/rows")).toBe("missing");
  });

  test("a plan's tabs are one screen, so a step between them keeps the page; any other page is its address", () => {
    expect(screenOf("/app/plans/plan-1/analysis/eval")).toBe("/app/plans/plan-1");
    expect(screenOf("/app/plans/plan-1/")).toBe(screenOf("/app/plans/plan-1/revisions"));
    expect(screenOf("/app/plans/plan-1/settings")).toBe("/app/plans/plan-1");
    expect(screenOf("/app/plans/plan-1/cells")).toBe("/app/plans/plan-1");
    expect(screenOf("/app/campaigns/cmp_1/plans/plan-1/cells")).toBe("/app/campaigns/cmp_1/plans/plan-1");
    expect(screenOf("/app/plans/plan-2/analysis")).not.toBe(screenOf("/app/plans/plan-1/analysis"));
    expect(screenOf("/app/campaigns/cmp_1/plans/plan-1/analysis/eval")).toBe("/app/campaigns/cmp_1/plans/plan-1");
    expect(screenOf("/app/campaigns/cmp_1/plans/plan-1")).not.toBe(screenOf("/app/plans/plan-1"));
    // The old results and reports addresses redirect, so each is a screen of its own; so is a campaign's own address.
    expect(screenOf("/app/plans/plan-1/results")).toBe("/app/plans/plan-1/results");
    expect(screenOf("/app/plans/plan-1/reports/eval")).toBe("/app/plans/plan-1/reports/eval");
    expect(screenOf("/app/campaigns/cmp_1")).toBe("/app/campaigns/cmp_1");
    expect(screenOf("/app/x")).toBe("/app/x");
    // So are an account's, and the reader's own.
    expect(screenOf("/app/users/user_1/settings")).toBe("/app/users/user_1");
    expect(screenOf("/app/users/user_1/plans")).not.toBe(screenOf("/app/users/user_2/plans"));
    expect(screenOf("/app/account/keys")).toBe("/app/account");
  });

  test("an address with its query is built once, the empty pairs dropped unless kept", () => {
    expect(href("/app/plans", [["status", ""], ["limit", "5"]])).toBe("/app/plans?limit=5");
    expect(href("/app/plans", [["status", ""]], ["status"])).toBe("/app/plans?status=");
    expect(href("/app/plans", [])).toBe("/app/plans");
  });

  test("an address out of the app is not the app's", () => {
    expect(inApp("/health")).toBe(false);
    expect(inApp("/apple")).toBe(false);
    expect(inApp("/app/assets/index.js")).toBe(false);
    expect(inApp("/app/orgs")).toBe(true);
  });
});
