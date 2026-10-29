import { describe, expect, test } from "vitest";
import {
  amendment, backlogDisagrees, backlogRows, capabilityFlags, deviceBranch, deviceTrail, handOver, listingName, listingPath, mayMintFor, mintRequest, namesOwner, usage,
} from "../src/lib/devices";
import { exactChoice, ownerOf } from "../src/lib/owners";
import { days } from "../src/lib/preauth";
import { match, orgOf } from "../src/lib/routes";
import type { ClientDetail, OwnUserView, OwnerMatch, UserPermission } from "../src/lib/api-types";
import { abilitiesOf } from "./abilities";

const reader = (permissions: UserPermission[], org_id?: string): OwnUserView => ({
  user_id: "user_me", contact_email: "me@acme.example", account_type: "human", status: "active", created_at: "2026-10-01T00:00:00Z",
  permissions, keys: [], may: abilitiesOf(permissions, org_id), ...(org_id ? { org_id, org_slug: "acme" } : {}),
});

const device = (fields: Partial<ClientDetail>): ClientDetail => ({
  client_id: "ev1_x", client_details: "", contact_email: "", organization: "", public_key: "",
  registered_at: "2026-10-01T00:00:00Z", status: "approved", derived_capabilities: [], permissions: [], tags: [],
  suspended: false, reindex_pending: false, last_seen_at: null, backlog: null, backlog_by_campaign: null, may_administer: false, ...fields,
});

const acme = { org_id: "org_acme", slug: "acme" };

describe("the branch a device's page sits under", () => {
  test("nobody's is the server's, and the reader's own is its", () => {
    expect(deviceBranch(device({}), reader(["read_orgs"]), null)).toEqual({ kind: "server" });
    const mine = device({ owner: { type: "user", user_id: "user_me" }, owning_org_id: "org_acme" });
    expect(deviceBranch(mine, reader(["read_orgs"], "org_acme"), acme)).toEqual({ kind: "me" });
  });

  test("an organization's for a reader of its roster the tree holds it for, else the fleet", () => {
    const held = device({ owner: { type: "user", user_id: "user_member" }, owning_org_id: "org_acme" });
    expect(deviceBranch(held, reader(["read_orgs"], "org_acme"), acme))
      .toEqual({ kind: "org", org_id: "org_acme", slug: "acme", count: null });
    // `owning_org_id` without the roster read: a scoped device administrator's.
    expect(deviceBranch(held, reader(["manage_clients"], "org_acme"), acme)).toEqual({ kind: "all" });
    expect(deviceBranch(held, reader(["read_orgs"]), null)).toEqual({ kind: "all" });
    const unnamed = device({ owner: { type: "org", org_id: "org_acme" } });
    expect(deviceBranch(unnamed, reader(["read_orgs"]), acme)).toEqual({ kind: "all" });
  });

  test("each branch goes back to its listing", () => {
    expect([listingPath({ kind: "all" }), listingName({ kind: "all" })]).toEqual(["/app/clients", "Fleet"]);
    expect([listingPath({ kind: "me" }), listingName({ kind: "me" })]).toEqual(["/app/me/devices", "Devices"]);
    expect([listingPath({ kind: "server" }), listingName({ kind: "server" })])
      .toEqual(["/app/server/devices", "Unclaimed devices"]);
    expect(listingPath({ kind: "org", ...acme, count: null })).toBe("/app/orgs/acme/devices");
    expect(listingPath({ kind: "org", org_id: "org_new", slug: "new", count: null })).toBe("/app/orgs/org_new/devices");
  });

  test("a device's trail is its branch's section, then its listing", () => {
    expect(deviceTrail({ kind: "all" })).toEqual([{ label: "Fleet", href: "/app/clients" }]);
    expect(deviceTrail({ kind: "me" })).toEqual([{ label: "Me" }, { label: "Devices", href: "/app/me/devices" }]);
    expect(deviceTrail({ kind: "server" }))
      .toEqual([{ label: "Server" }, { label: "Unclaimed devices", href: "/app/server/devices" }]);
    expect(deviceTrail({ kind: "org", ...acme, count: null }))
      .toEqual([...[{ label: "Organizations", href: "/app/orgs" }, { label: "acme", href: "/app/orgs/acme" }], { label: "Devices", href: "/app/orgs/acme/devices" }]);
  });

  test("an organization's listing names the organization the tree opens", () => {
    expect(orgOf(match(listingPath({ kind: "org", ...acme, count: null })))).toBe("acme");
    expect(orgOf(match(listingPath({ kind: "all" })))).toBeNull();
  });
});

describe("the owner's name", () => {
  const held = device({ owner: { type: "user", user_id: "user_member" }, owning_org_id: "org_acme" });

  test("is asked for by a reader whose roster read covers the device's organization", () => {
    expect(namesOwner(held, reader(["read_orgs"], "org_acme"))).toBe(true);
    expect(namesOwner(device({ owner: { type: "user", user_id: "user_loner" } }), reader(["read_any_orgs"]))).toBe(true);
  });

  test("is not asked for past that roster, or without it", () => {
    expect(namesOwner(held, reader(["read_orgs"], "org_globex"))).toBe(false);
    expect(namesOwner(device({ owner: { type: "user", user_id: "user_loner" } }), reader(["read_orgs"], "org_acme")))
      .toBe(false);
    // `owning_org_id` sent to a scoped device administrator, who reads no roster.
    expect(namesOwner(held, reader(["manage_clients"], "org_acme"))).toBe(false);
    expect(namesOwner(device({ owner: { type: "org", org_id: "org_acme" } }), reader(["read_clients"], "org_acme")))
      .toBe(false);
  });
});

describe("the permission form", () => {
  test("grants what is ticked, and revokes what is held and unticked", () => {
    expect(amendment(["submit_results"], new Set(["perform_work"])))
      .toEqual({ add: ["perform_work"], remove: ["submit_results"] });
    expect(amendment([], new Set(["submit_results", "perform_work"])))
      .toEqual({ add: ["submit_results", "perform_work"], remove: [] });
  });

  test("grants a held box again, so a revoke made since the page was drawn does not stand", () => {
    expect(amendment(["perform_work"], new Set(["perform_work"]))).toEqual({ add: ["perform_work"], remove: [] });
  });

  test("asks for nothing for a device holding nothing, saved with nothing ticked", () => {
    expect(amendment([], new Set())).toBeNull();
  });

  test("leaves a name this build does not know alone", () => {
    expect(amendment(["rescore_plans", "perform_work"], new Set())).toEqual({ add: [], remove: ["perform_work"] });
  });
});

describe("the owner field", () => {
  const choices: OwnerMatch[] = [
    { value: "org_acme", name: "acme", label: "acme", type: "org" },
    { value: "user_m", name: "member@acme.example", label: "member@acme.example (acme)", type: "user" },
  ];

  test("typed text names a choice by its id, handle or address, case aside", () => {
    expect(exactChoice(choices, " ACME ")?.value).toBe("org_acme");
    expect(exactChoice(choices, "Member@Acme.example")?.value).toBe("user_m");
    expect(exactChoice(choices, "member@acme.example (acme)")?.value).toBe("user_m");
    expect(exactChoice(choices, "user_m")?.value).toBe("user_m");
    expect(exactChoice(choices, "acm")).toBeNull();
    expect(exactChoice(choices, "  ")).toBeNull();
  });

  test("a pick posts the owner its id names", () => {
    expect(choices.map(ownerOf)).toEqual([
      { type: "org", org_id: "org_acme" }, { type: "user", user_id: "user_m" },
    ]);
  });
});

describe("device pre-auth keys", () => {
  test("are minted by the fleet's administrator anywhere, and a scoped one in its own organization", () => {
    expect(mayMintFor(reader(["manage_clients", "manage_any_clients"]), "org_acme")).toBe(true);
    expect(mayMintFor(reader(["manage_clients"], "org_acme"), "org_acme")).toBe(true);
    expect(mayMintFor(reader(["manage_clients"], "org_acme"), "org_globex")).toBe(false);
    expect(mayMintFor(reader(["manage_clients"]), "org_acme")).toBe(false);
    expect(mayMintFor(reader(["read_clients", "read_orgs"], "org_acme"), "org_acme")).toBe(false);
  });

  test("send their expiry and usage always, and no blank note", () => {
    expect(mintRequest(7, false, "  ")).toEqual({ expires_in_days: 7, usage: "single_use" });
    expect(mintRequest(90, true, " rack 3 ")).toEqual({ expires_in_days: 90, usage: "multi_use", note: "rack 3" });
  });

  test("read as the classic page words them", () => {
    expect([days(1), days(7)]).toEqual(["1 day", "7 days"]);
    expect([usage("single_use"), usage("multi_use")]).toEqual(["single-use", "multi-use"]);
    expect(handOver("multi_use")).toMatch(/^It registers any number of devices/);
    expect(handOver("single_use")).toMatch(/^It registers one device and is gone/);
  });
});

describe("the capability flags a device's page lists", () => {
  test.each<[string, Partial<ClientDetail>, string[]]>([
    ["the declared and the derived, merged and sorted",
     { capabilities: ["runtime:llama_cpp"], derived_capabilities: ["os:ios", "gpu:a17pro"] },
     ["gpu:a17pro", "os:ios", "runtime:llama_cpp"]],
    ["a flag in both sets, once", { capabilities: ["os:ios"], derived_capabilities: ["os:ios"] }, ["os:ios"]],
    ["the derived alone, when the device declares nothing", { derived_capabilities: ["os:ios"] }, ["os:ios"]],
    ["nothing, when neither set holds a flag", {}, []],
  ])("%s", (_, fields, flags) => {
    expect(capabilityFlags(device(fields))).toEqual(flags);
  });
});

describe("the backlog by campaign a device's page lists", () => {
  const counted_at = "2026-10-01T00:00:00Z";
  const mine = { campaign_id: "campaign_mine", name: "Mine", job_count: 2 };

  test.each<[string, ClientDetail["backlog_by_campaign"], { label: string; jobs: number; href?: string }[]]>([
    ["nothing before the first count", null, []],
    ["nothing for a device with no claimable job",
     { campaigns: [], outside_campaign_job_count: 0, unreadable_job_count: 0, counted_at }, []],
    ["each readable campaign with its link, then the two sums",
     { campaigns: [mine], outside_campaign_job_count: 1, unreadable_job_count: 3, counted_at },
     [{ label: "Mine", jobs: 2, href: "/app/campaigns/campaign_mine" },
      { label: "not in a campaign", jobs: 1 },
      { label: "in campaigns you cannot read", jobs: 3 }]],
    ["no sum row with no jobs", { campaigns: [mine], outside_campaign_job_count: 0, unreadable_job_count: 0, counted_at },
     [{ label: "Mine", jobs: 2, href: "/app/campaigns/campaign_mine" }]],
  ])("%s", (_, backlog_by_campaign, rows) => {
    expect(backlogRows(device({ backlog_by_campaign })).map(({ label, jobs, href }) => ({ label, jobs, ...(href ? { href } : {}) })))
      .toEqual(rows);
  });
});

describe("whether a device's breakdown disagrees with its backlog count", () => {
  const breakdown = { campaigns: [], outside_campaign_job_count: 1, unreadable_job_count: 0, counted_at: "2026-10-01T00:05:00Z" };

  test.each<[string, Partial<ClientDetail>, boolean]>([
    ["no, with no breakdown", { backlog: { job_count: 1, counted_at: "2026-10-01T00:05:00Z" } }, false],
    ["no, when one run counted both",
     { backlog: { job_count: 1, counted_at: "2026-10-01T00:05:00Z" }, backlog_by_campaign: breakdown }, false],
    ["yes, when another run counted the total",
     { backlog: { job_count: 4, counted_at: "2026-10-01T00:10:00Z" }, backlog_by_campaign: breakdown }, true],
    ["yes, when the total is missing", { backlog_by_campaign: breakdown }, true],
  ])("%s", (_, fields, disagrees) => {
    expect(backlogDisagrees(device(fields))).toBe(disagrees);
  });
});
