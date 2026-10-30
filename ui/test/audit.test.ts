import { describe, expect, test } from "vitest";
import {
  ACTOR_KINDS, DEFAULT_ACTORS, OUTCOMES, actionShown, actorShown, auditAction, auditFilters, auditLink, auditQuery,
  effectiveActors, offered, targetHref, targetId, targetShown, toggled,
} from "../src/lib/audit";
import type { AuditTarget, UserPermission, OwnUserView } from "../src/lib/api-types";
import { abilitiesOf } from "./abilities";

const PATH = "/app/orgs/acme/audit";
const read = (search: string) => auditFilters(new URLSearchParams(search));
const params = (query: string) => Object.fromEntries(new URLSearchParams(query));

describe("the filters", () => {
  test("no actor in the address reads the default kinds, devices and the host left out", () => {
    const filters = read("");
    expect(filters).toEqual({ outcome: [], actor: null, target: "", limit: "" });
    expect(effectiveActors(filters)).toEqual(["user", "unverified"]);
    expect(DEFAULT_ACTORS).not.toContain("client");
    expect(DEFAULT_ACTORS).not.toContain("host");
    expect(params(auditQuery(filters, null))).toEqual({ actor: "user,unverified", names: "true" });
  });

  test("an empty actor is every kind, which the route reads from no actor at all", () => {
    const filters = read("actor=");
    expect(filters.actor).toEqual([]);
    expect(effectiveActors(filters)).toEqual([]);
    expect(params(auditQuery(filters, null))).toEqual({ names: "true" });
  });

  test("the address's keys are the route's, lists comma-separated, each value once", () => {
    const filters = read("outcome=refused,%20failed,refused&actor=client&target=%20ev1_a%20&limit=50");
    expect(filters).toEqual({ outcome: ["refused", "failed"], actor: ["client"], target: "ev1_a", limit: "50" });
    expect(params(auditQuery(filters, "audit_9"))).toEqual({
      outcome: "refused,failed", actor: "client", target: "ev1_a", limit: "50", cursor: "audit_9", names: "true",
    });
  });

  test("a link keeps every filter, an empty actor apart from none, and the cursor only where given", () => {
    expect(auditLink(PATH, read(""))).toBe(PATH);
    expect(auditLink(PATH, read("actor="))).toBe(`${PATH}?actor=`);
    const filters = read("outcome=refused&actor=user,host&target=campaign_1&limit=50");
    const next = new URL(auditLink(PATH, filters, "audit_9"), "https://mgmt.example");
    expect(next.pathname).toBe(PATH);
    expect(Object.fromEntries(next.searchParams)).toEqual({
      outcome: "refused", actor: "user,host", target: "campaign_1", limit: "50", cursor: "audit_9",
    });
    expect(auditLink(PATH, filters)).not.toContain("cursor");
    // What the link names reads back as the same filters.
    expect(auditFilters(next.searchParams)).toEqual(filters);
  });

  test("a choice toggles in the select's order; back at the default kinds the address names none", () => {
    const shown = toggled(read(""), "actor", "client", true);
    expect(shown.actor).toEqual(["user", "client", "unverified"]);
    expect(toggled(shown, "actor", "client", false).actor).toBeNull();
    expect(toggled(read(""), "actor", "host", true).actor).toEqual(["user", "unverified", "host"]);
    expect(toggled(read(""), "actor", "unverified", false).actor).toEqual(["user"]);
    // Letting go of every kind is every kind, as the select's "any" says.
    expect(toggled(read("actor=user"), "actor", "user", false).actor).toEqual([]);
    expect(toggled(read("outcome=failed"), "outcome", "succeeded", true).outcome).toEqual(["succeeded", "failed"]);
    expect(toggled(read("outcome=failed"), "outcome", "failed", false).outcome).toEqual([]);
    // The rest of the filters are kept.
    expect(toggled(read("target=org_1&limit=5"), "outcome", "refused", true))
      .toEqual({ outcome: ["refused"], actor: null, target: "org_1", limit: "5" });
  });

  test("the selects offer a value the address carries that they do not know, so it can be let go of", () => {
    expect(offered(OUTCOMES, ["refused"])).toEqual(OUTCOMES);
    expect(offered(OUTCOMES, ["refused", "lost"]).at(-1)).toEqual({ value: "lost", label: "lost" });
    expect(ACTOR_KINDS.map((kind) => kind.label)).toEqual(["people", "devices", "unverified", "host operator"]);
  });
});

describe("a row", () => {
  const names = { user_1: "ana@acme.example", ev1_a: "bos-17pro-1", org_1: "acme", campaign_1: "nightly" };

  test("an actor reads by its name, linked to its page, the id on hover", () => {
    expect(actorShown({ type: "user", user_id: "user_1", key_id: "userkey_1" }, names)).toEqual({
      text: "ana@acme.example", href: "/app/users/user_1", title: "user_1, key userkey_1", id: false,
    });
    expect(actorShown({ type: "client", client_id: "ev1_a" }, names)).toEqual({
      text: "bos-17pro-1", href: "/app/clients/ev1_a", title: "ev1_a", id: false,
    });
  });

  test("an unnamed actor reads as its bare id, linked nowhere, one with no record as what it is", () => {
    // The server names what the reader may see, so an unnamed page would refuse them.
    expect(actorShown({ type: "user", user_id: "user_2", key_id: "userkey_2" }, {}))
      .toEqual({ text: "user_2", href: null, title: "user_2, key userkey_2", id: true });
    expect(actorShown({ type: "client", client_id: "ev1_b" }, {}))
      .toEqual({ text: "ev1_b", href: null, title: undefined, id: true });
    expect(actorShown({ type: "unverified", claimed_id: "user_2" }, names))
      .toEqual({ text: "unverified (claimed user_2)", href: null, title: undefined, id: false });
    expect(actorShown({ type: "unverified", claimed_id: null }, names).text).toBe("unverified");
    expect(actorShown({ type: "host", os_user: "ops", hostname: "mgmt-1" }, names))
      .toEqual({ text: "host operator ops@mgmt-1", href: null, title: undefined, id: false });
  });

  test("an action reads as the route's summary, else the request; the status only where it did not succeed", () => {
    const action = { type: "http", method: "PATCH", route: "/api/v1/orgs/{org_id}", path: "/api/v1/orgs/acme",
                     status: 403 } as const;
    expect(actionShown({ action, outcome: "refused", summary: "Rename an organization, or change its status" }))
      .toEqual({ text: "Rename an organization, or change its status", title: "PATCH /api/v1/orgs/acme", status: 403 });
    expect(actionShown({ action: { ...action, status: 200 }, outcome: "succeeded" }))
      .toEqual({ text: "PATCH /api/v1/orgs/{org_id}", title: "PATCH /api/v1/orgs/acme", status: null });
    expect(actionShown({ action: { ...action, status: 500 }, outcome: "failed", summary: null }).status).toBe(500);
  });

  test("a command reads as itself, its arguments on hover", () => {
    expect(actionShown({ action: { type: "cli", command: "orgs create", args: ["orgs", "create", "acme"] },
                         outcome: "succeeded" }))
      .toEqual({ text: "pipette-mgmt orgs create", title: "pipette-mgmt orgs create acme", status: null });
    expect(actionShown({ action: { type: "cli", command: "audit prune", args: ["audit", "prune"] },
                         outcome: "failed" }).status).toBeNull();
    // An organization's log clears the arguments; the hover falls back to the command.
    expect(actionShown({ action: { type: "cli", command: "orgs create", args: [] }, outcome: "succeeded" }).title)
      .toBe("pipette-mgmt orgs create");
  });

  test("a first Clerk sign-in reads as the keys the account held, each on hover", () => {
    const key = (key_id: string, key_details: string) =>
      ({ key_id, key_details, public_key: "00", added_at: "2026-10-01T00:00:00Z" });
    expect(actionShown({ action: { type: "first_clerk_sign_in",
                                   prior_keys: [key("userkey_a", "laptop"), key("userkey_b", "build box")] },
                         outcome: "succeeded" }))
      .toEqual({ text: "First Clerk sign-in, to an account holding 2 keys",
                 title: "userkey_a (laptop)\nuserkey_b (build box)", status: null });
    expect(actionShown({ action: { type: "first_clerk_sign_in", prior_keys: [key("userkey_a", "laptop")] },
                         outcome: "succeeded" }).text)
      .toBe("First Clerk sign-in, to an account holding 1 key");
  });

  test("a target's id sits under its kind's key, a vault resource's as every vault body names it", () => {
    const targets: [AuditTarget, string][] = [
      [{ type: "org", org_id: "org_1" }, "org_1"],
      [{ type: "user", user_id: "user_1" }, "user_1"],
      [{ type: "client", client_id: "ev1_a" }, "ev1_a"],
      [{ type: "campaign", campaign_id: "campaign_1" }, "campaign_1"],
      [{ type: "plan", plan_id: "plan-1" }, "plan-1"],
      [{ type: "job", job_id: "job-1" }, "job-1"],
      [{ type: "vault_resource", resource_id: "vault_1" }, "vault_1"],
    ];
    for (const [target, id] of targets) expect(targetId(target)).toBe(id);
  });

  test("a target reads by its name, links to its page, a job to none", () => {
    const named = { ...names, vault_1: "weights.gguf" };
    expect(targetShown({ type: "campaign", campaign_id: "campaign_1" }, named))
      .toEqual({ text: "nightly", href: "/app/campaigns/campaign_1", title: "campaign_1", id: false });
    expect(targetShown({ type: "job", job_id: "job-1" }, named))
      .toEqual({ text: "job-1", href: null, title: undefined, id: true });
    expect(targetHref({ type: "org", org_id: "org_1" }, named)).toBe("/app/orgs/acme");
    expect(targetHref({ type: "vault_resource", resource_id: "vault_1" }, named)).toBe("/app/vault/vault_1");
    expect(targetHref({ type: "user", user_id: "user_1" }, named)).toBe("/app/users/user_1");
    expect(targetHref({ type: "client", client_id: "ev1_a" }, named)).toBe("/app/clients/ev1_a");
  });

  test("an unnamed target is out of reach or gone, so links nowhere; a plan, which names never cover, still links", () => {
    expect(targetShown({ type: "org", org_id: "org_2" }, names))
      .toEqual({ text: "org_2", href: null, title: undefined, id: true });
    const unnamed: AuditTarget[] = [
      { type: "user", user_id: "user_9" }, { type: "client", client_id: "client_9" },
      { type: "campaign", campaign_id: "campaign_9" }, { type: "vault_resource", resource_id: "vault_resource_9" },
    ];
    for (const target of unnamed) expect(targetHref(target, names)).toBeNull();
    expect(targetShown({ type: "plan", plan_id: "plan-1" }, names))
      .toEqual({ text: "plan-1", href: "/app/plans/plan-1", title: undefined, id: true });
  });
});

describe("a record page's audit link", () => {
  const reader = (permissions: UserPermission[], org_id?: string): Pick<OwnUserView, "may" | "org_id" | "org_slug"> => ({
    may: abilitiesOf(permissions, org_id), ...(org_id ? { org_id, org_slug: "acme" } : {}),
  });

  test("a server admin's opens the whole log on the record, every actor kind", () => {
    expect(auditAction(reader(["manage_any_orgs"]), "ev1_a"))
      .toEqual({ href: "/app/server/audit?target=ev1_a&actor=", label: "Audit" });
  });

  test("an organization's administrator's opens that organization's log", () => {
    expect(auditAction(reader(["manage_orgs"], "org_acme"), "campaign_1"))
      .toEqual({ href: "/app/orgs/acme/audit?target=campaign_1&actor=", label: "Audit" });
  });

  test("a reader administering no roster has none", () => {
    expect(auditAction(reader(["read_orgs", "read_plans"], "org_acme"), "user_1")).toBeNull();
    // Nor an administrator whose organization the session cannot name.
    expect(auditAction({ may: abilitiesOf(["manage_orgs"], "org_acme") }, "user_1")).toBeNull();
  });

  test("the link reads back as the record's filter over every kind", () => {
    const link = auditAction(reader(["manage_any_orgs"]), "user_1")!.href;
    const filters = auditFilters(new URL(link, "https://mgmt.example").searchParams);
    expect(filters).toEqual({ outcome: [], actor: [], target: "user_1", limit: "" });
    expect(params(auditQuery(filters, null))).toEqual({ target: "user_1", names: "true" });
  });
});
