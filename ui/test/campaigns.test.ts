import { describe, expect, test } from "vitest";
import { ApiError } from "../src/lib/api";
import {
  CAMPAIGN_PAGES, LABELS_MAX, NAME_MAX_CHARS, NOTE_MAX_CHARS, campaignPlan, campaignQuery, campaignTable, chosenOwner, createQuery,
  deviceTimeText, handedAway, stateOf, headAddress, knownLabels, labelLink, labelPairs, labelText, labelVocabulary, labelsBody,
  labelScope, labelsProblem, mayLabel, nameProblem, newCampaignAction, newCampaignOwners, newCampaignRefusal, noteProblem,
  parseLabel, planTrail, progressText, renameBody, unlessWithheld, inheritOffers, namedOffer, planSourceOf, sourceText,
  repeatedText, restoreEdit, revertOffered, revisionQuery, revisionSummary, importKey, importLine, importText, withLabel, withoutLabel, withVariantsOf,
} from "../src/lib/campaigns";
import { ownedEmpty, ownerBranch, underTrail } from "../src/lib/owned";
import { legacyOwnerFilter } from "../src/lib/routes";
import { PickedColumns } from "../src/lib/picked.svelte";
import { choicesOf, drawnColumns, type Picks } from "../src/lib/table";
import type { CampaignView, NamedCampaignOverview, OwnUserView, UserPermission } from "../src/lib/api-types";
import { abilitiesOf } from "./abilities";
import { drawnKeys, stubStorage } from "./picks";

const reader = (permissions: UserPermission[], org_id?: string): OwnUserView => ({
  user_id: "user_me", contact_email: "me@example.test", account_type: "human", status: "active", created_at: "2026-10-01T00:00:00Z",
  updated_at: "2026-10-01T00:00:00Z", permissions, keys: [], may: abilitiesOf(permissions, org_id), ...(org_id ? { org_id, org_slug: "acme" } : {}),
}) as OwnUserView;

const acme = { kind: "org", org_id: "org_acme", slug: "acme" } as const;

describe("the campaign listings", () => {
  test("each branch asks the newest first with each plan, and only every campaign's names owners", () => {
    expect(campaignQuery({ kind: "all" }, null, "")).toBe("order=newest&plan=true&state=active&names=true");
    expect(campaignQuery({ kind: "me" }, "campaign_9", "")).toBe("order=newest&plan=true&state=active&owner=mine&cursor=campaign_9");
    expect(campaignQuery(acme, null, "5")).toBe("order=newest&plan=true&state=active&owner=org&org_id=org_acme&limit=5");
    expect(campaignQuery({ kind: "me" }, null, "", "plan-1", "archived")).toBe("order=newest&plan=true&plan_id=plan-1&owner=mine");
    expect([campaignQuery({ kind: "me" }, null, "", "", "archived"), campaignQuery({ kind: "me" }, null, "", "", "all")])
      .toEqual(["order=newest&plan=true&state=archived&owner=mine", "order=newest&plan=true&owner=mine"]);
  });

  test("an empty branch says so in the classic words", () => {
    expect(ownedEmpty({ kind: "me" }, "campaigns")).toBe("You own no campaigns.");
    expect(ownedEmpty(acme, "campaigns")).toBe("acme owns no campaigns you may see.");
    expect(ownedEmpty({ kind: "all" }, "campaigns")).toBe("No campaigns you may see.");
  });

  test("an organization's New campaign opens the form with it as the owner", () => {
    expect(newCampaignAction({ kind: "me" })).toEqual({ href: "/app/campaigns/new", label: "New campaign" });
    expect(newCampaignAction(acme).href).toBe("/app/campaigns/new?owner=org_acme");
  });

  test("a campaign's page sits under its owner's listing, in its section", () => {
    const known = [{ org_id: "org_acme", slug: "acme" }];
    const mine = { type: "user", user_id: "user_me" } as const;
    const trail = (owner: Parameters<typeof ownerBranch>[1]) =>
      underTrail(ownerBranch({ user_id: "user_me" }, owner, known, CAMPAIGN_PAGES), "Campaigns");
    expect(trail(mine)).toEqual([{ label: "Me" }, { label: "Campaigns", href: "/app/me/campaigns" }]);
    expect(trail({ type: "org", org_id: "org_acme" }))
      .toEqual([...[{ label: "Organizations", href: "/app/orgs" }, { label: "acme", href: "/app/orgs/acme" }], { label: "Campaigns", href: "/app/orgs/acme/campaigns" }]);
    expect(trail({ type: "org", org_id: "org_rival" })).toEqual([{ label: "Campaigns", href: "/app/campaigns" }]);
  });

  test("a plan's trail is Plans, or its campaign under that campaign's listing once read", () => {
    const known = [{ org_id: "org_acme", slug: "acme" }];
    const detail = { name: "nightly", owner: { type: "org", org_id: "org_acme" } } as unknown as NamedCampaignOverview;
    const atCampaign = { label: "nightly", href: "/app/campaigns/campaign_1/plans/plan_1" };
    const cases: [string | null, NamedCampaignOverview | null, unknown][] = [
      [null, null, [{ label: "Plans", href: "/app/plans" }]],
      // Unread, or refused the reader: the campaign alone, unnamed.
      ["campaign_1", null, [{ ...atCampaign, label: "Campaign" }]],
      ["campaign_1", detail, [{ label: "Organizations", href: "/app/orgs" }, { label: "acme", href: "/app/orgs/acme" },
                               { label: "Campaigns", href: "/app/orgs/acme/campaigns" }, atCampaign]],
    ];
    for (const [campaign, read, trail] of cases) {
      expect(planTrail("plan_1", campaign, read, { user_id: "user_me" }, known)).toEqual(trail);
    }
  });

  test("the listing hides a campaign's id until the reader picks that column", () => {
    const keys = (picks: Picks | null) => drawnColumns(campaignTable({}, false).columns, picks).map((c) => c.key);
    expect(keys(null)).toEqual(["name", "status", "progress", "labels", "created_at", "updated_at"]);
    expect(keys({ campaign_id: true, labels: false }))
      .toEqual(["campaign_id", "name", "status", "progress", "created_at", "updated_at"]);
  });

  test("a pick on a branch without the owner column leaves that column drawn on every campaign's branch", () => {
    stubStorage();
    new PickedColumns("pipette.campaigns.columns", campaignTable({}, false).columns).pick("labels", false);
    expect(drawnKeys("pipette.campaigns.columns", campaignTable({}, true).columns))
      .toEqual(["name", "owner", "status", "progress", "created_at", "updated_at"]);
  });

  test("a list of keys kept on a branch without the owner column hides that column on every campaign's branch", () => {
    stubStorage().set("pipette.campaigns.columns", JSON.stringify(["name", "status"]));
    expect(drawnKeys("pipette.campaigns.columns", campaignTable({}, true).columns)).toEqual(["name", "status"]);
  });

  test("a row's progress counts its plan's failed jobs as done, and its status says they failed", () => {
    const counts = { total: 4, finished: 2, failed: 1, running: 1, available: 0, cancelled: 0 };
    const row = (status: "active" | "complete") =>
      ({ campaign_id: "campaign_1", plan: { status, cancel_requested: false, counts } }) as CampaignView;
    const spec = campaignTable({}, false);
    const raw = (key: string, of: CampaignView) => spec.columns.find((column) => column.key === key)?.raw(of);
    expect([progressText(row("active")), raw("progress", row("active")), raw("status", row("active"))])
      .toEqual(["3/4", 0.75, "active"]);
    expect(raw("status", row("complete"))).toBe("complete_with_failures");
  });

  test("a row's labels are a column, a filter and searched, each as key=value, a bare key for an empty value", () => {
    const row = { campaign_id: "campaign_1", name: "Nightly",
                  labels: { study: "acmelm-text", Device: "S26 Ultra", canary: "" } } as unknown as CampaignView;
    const bare = { campaign_id: "campaign_2", name: "Plain" } as CampaignView;
    expect(labelPairs(row.labels).map(labelText)).toEqual(["Device=S26 Ultra", "canary", "study=acmelm-text"]);
    expect(labelPairs(bare.labels)).toEqual([]);
    const spec = campaignTable({}, false);
    const column = spec.columns.find((each) => each.key === "labels")!;
    expect([column.raw(row), column.raw(bare)]).toEqual(["Device=S26 Ultra, canary, study=acmelm-text", null]);
    const filter = spec.choices!.find((each) => each.key === "label")!;
    expect([filter.values(row), filter.values(bare)]).toEqual([["Device=S26 Ultra", "canary", "study=acmelm-text"], []]);
    expect(spec.texts![0]!.texts(row)).toContain("study=acmelm-text");
    // A chip opens its listing under that filter.
    expect(labelLink("/app/orgs/acme/campaigns", ["Device", "S26 Ultra"]))
      .toBe("/app/orgs/acme/campaigns?label=Device%3DS26+Ultra");
    expect(labelLink("/app/campaigns", ["canary", ""])).toBe("/app/campaigns?label=canary");
  });

  test("an archived campaign's status is archived, whatever its plan is doing", () => {
    const plan = { status: "active", cancel_requested: false, counts: null } as unknown as CampaignView["plan"];
    const active = { campaign_id: "campaign_1", plan } as CampaignView;
    const archived = { campaign_id: "campaign_2", state: "archived", plan } as CampaignView;
    expect([stateOf(active), stateOf(archived)]).toEqual(["active", "archived"]);
    const spec = campaignTable({}, false);
    expect([active, archived].map((row) => spec.columns.find((column) => column.key === "status")?.raw(row)))
      .toEqual(["active", "archived"]);
    expect(choicesOf(spec, [active, archived], {}).status!.map((c) => c.label)).toEqual(["active", "archived"]);
  });

  test("labels are refused in the browser where the route would refuse them, and sent trimmed as the whole set", () => {
    expect(labelsProblem([["study", "acmelm-text"], ["Device", "S26 Ultra"], ["canary", ""]])).toBe("");
    expect(labelsProblem([["", "x"]])).toBe("Name every label, or remove the empty row");
    expect(labelsProblem([["study", "a"], [" study ", "b"]])).toBe("“study” is set twice");
    expect(labelsProblem([["a,b", "x"]])).toBe("“a,b” may hold letters, digits, spaces and _ . : / = + - @");
    expect(labelsProblem([["k", "tab\there"]])).toContain("may hold");
    expect(labelsProblem([["к".repeat(128), "x".repeat(256)], ["a:b/c=d+e-f@g_h.i", "ü 9"]])).toBe("");
    expect(labelsProblem([["k".repeat(129), ""]])).toBe("Keep a key to 128 characters");
    expect(labelsProblem([["k", "v".repeat(257)]])).toBe("Keep the value of “k” to 256 characters");
    const many = Array.from({ length: LABELS_MAX + 1 }, (_, i): [string, string] => [`k${i}`, ""]);
    expect(labelsProblem(many)).toBe("Keep to 50 labels");
    expect(labelsProblem(many.slice(1))).toBe("");
    expect(labelsBody([[" study ", " acmelm "], ["canary", ""]])).toEqual({ labels: { study: "acmelm", canary: "" } });
    expect(labelsBody([])).toEqual({ labels: {} });
    expect(() => labelsBody([["", ""]])).toThrow("Name every label");
  });

  test("a label's inputs offer what the owner's campaigns use, an organization's shared by its members", () => {
    const rows: Pick<CampaignView, "labels">[] =
      [{ labels: { study: "acmelm-text", device: "s26" } }, { labels: { study: "24b", canary: "" } }, {}];
    expect([...labelVocabulary(rows)]).toEqual([["canary", []], ["device", ["s26"]], ["study", ["24b", "acmelm-text"]]]);
    const me = { user_id: "user_me" };
    expect(labelScope(me, { type: "org", org_id: "org_acme" })).toEqual({ kind: "org", org_id: "org_acme" });
    expect(labelScope(me, { type: "user", user_id: "user_me" })).toEqual({ kind: "me" });
    expect(labelScope(me, { type: "user", user_id: "user_other" })).toBeNull();
  });

  test("one box adds a label typed key=value, split at the first =, in place of its key's value", () => {
    expect(parseLabel(" study = acmelm-text ")).toEqual(["study", "acmelm-text"]);
    expect(parseLabel("canary")).toEqual(["canary", ""]);
    expect(parseLabel("ratio=a=b")).toEqual(["ratio", "a=b"]);
    const labels = { study: "acmelm", device: "s26" };
    expect(withLabel(labels, ["study", "acmelm-text"])).toEqual([["device", "s26"], ["study", "acmelm-text"]]);
    expect(withLabel(undefined, ["canary", ""])).toEqual([["canary", ""]]);
    expect(withoutLabel(labels, "study")).toEqual([["device", "s26"]]);
    expect(withoutLabel(labels, "kind")).toEqual([["device", "s26"], ["study", "acmelm"]]);
    expect(withoutLabel(undefined, "study")).toEqual([]);
    expect(labelsBody(withoutLabel({ study: "acmelm" }, "study"))).toEqual({ labels: {} });
    expect(labelsBody(withLabel(labels, parseLabel("kind=canary"))))
      .toEqual({ labels: { device: "s26", study: "acmelm", kind: "canary" } });
    expect(labelsProblem(withLabel(labels, parseLabel("=x")))).toBe("Name every label, or remove the empty row");
    expect(knownLabels(new Map([["canary", []], ["study", ["24b", "acmelm-text"]]])))
      .toEqual(["canary", "study=24b", "study=acmelm-text"]);
  });

  test("the add is offered where the route would take it: the reader's own campaign, or one it administers", () => {
    const as = (permissions: UserPermission[], org?: string) => reader(permissions, org);
    const [mine, theirs] = [{ type: "user", user_id: "user_me" }, { type: "user", user_id: "user_x" }] as const;
    const [acme, rival] = [{ type: "org", org_id: "org_acme" }, { type: "org", org_id: "org_rival" }] as const;
    expect([mine, theirs, acme].map((owner) => mayLabel(as(["submit_plans"], "org_acme"), owner)))
      .toEqual([true, false, false]);
    expect(mayLabel(as(["read_plans"]), mine)).toBe(false);
    expect([acme, rival, theirs].map((owner) => mayLabel(as(["manage_orgs"], "org_acme"), owner)))
      .toEqual([true, false, false]);
    expect([rival, theirs].map((owner) => mayLabel(as(["manage_any_orgs"]), owner))).toEqual([true, true]);
  });

  test("a link with the old owner filter lands on its branch", () => {
    const member = reader(["read_plans"], "org_acme");
    expect(legacyOwnerFilter(new URLSearchParams("owner=mine&limit=3"), CAMPAIGN_PAGES, member))
      .toBe("/app/me/campaigns?limit=3");
    expect(legacyOwnerFilter(new URLSearchParams("owner=org"), CAMPAIGN_PAGES, member)).toBe("/app/orgs/acme/campaigns");
    expect(legacyOwnerFilter(new URLSearchParams("owner=org"), CAMPAIGN_PAGES, reader([]))).toBe("/app/campaigns");
  });
});

describe("the new-campaign form", () => {
  test("is refused without submit_plans, as the route's gate refuses it", () => {
    const refused = "Creating a campaign needs the submit_plans permission.";
    expect(newCampaignRefusal(reader(["read_plans"]))).toBe(refused);
    expect(newCampaignRefusal(reader(["manage_orgs"], "org_acme"))).toBe(refused);
    expect(newCampaignRefusal(reader(["submit_plans", "read_plans"]))).toBe("");
    expect(newCampaignRefusal(reader(["submit_any_plans", "submit_plans", "read_plans"]))).toBe("");
  });

  test("offers its organization to a member that files plans", () => {
    const me = [{ value: "", label: "me" }];
    const org = { value: "org_acme", label: "acme (organization)" };
    expect(newCampaignOwners(reader(["submit_plans"], "org_acme"))).toEqual([...me, org]);
    expect(newCampaignOwners(reader(["manage_orgs"], "org_acme"))).toEqual(me);
    expect(newCampaignOwners(reader(["submit_any_plans", "submit_plans"], "org_acme"))).toEqual([...me, org]);
    expect(newCampaignOwners(reader(["submit_any_plans", "submit_plans"]))).toEqual(me);
  });

  test("chooses the owner an address names only when it offers it", () => {
    const offered = newCampaignOwners(reader(["submit_plans", "manage_orgs"], "org_acme"));
    expect(chosenOwner(offered, "org_acme")).toBe("org_acme");
    expect(chosenOwner(offered, "org_rival")).toBe("");
    expect(chosenOwner(offered, null)).toBe("");
  });

  test("leaves an empty field out of the query, and refuses a blank name", () => {
    expect(createQuery({ name: "Nightly LFM2", owner: "", note: "" })).toBe("name=Nightly+LFM2");
    expect(createQuery({ name: "N", owner: "org_acme", note: "nightly-1" }))
      .toBe("name=N&owner=org_acme&note=nightly-1");
    expect(createQuery({ name: "N", owner: "", note: "  nightly-1 " })).toBe("name=N&note=nightly-1");
    expect(createQuery({ name: "N", owner: "", note: "   " })).toBe("name=N");
    expect(() => createQuery({ name: "  ", owner: "", note: "" })).toThrow("Name the campaign");
    expect(() => createQuery({ name: "N", owner: "", note: "p".repeat(NOTE_MAX_CHARS + 1) }))
      .toThrow("Keep the note to 4,096 characters");
  });

  test("refuses a note past the route's ceiling, counting characters", () => {
    expect(noteProblem("")).toBe("");
    expect(noteProblem(` ${"🙂".repeat(NOTE_MAX_CHARS)} `)).toBe("");
    expect(noteProblem("p".repeat(NOTE_MAX_CHARS + 1))).toBe("Keep the note to 4,096 characters");
    expect(noteProblem("a\0b")).toBe("Remove the NUL character from the note");
  });
});

describe("a campaign's page", () => {
  const detail = (history: string[], history_truncated = false) => ({
    campaign_id: "campaign_1", history_truncated,
    history: history.map((plan_id) => ({ plan_id, plan: null })),
  });
  const read = (campaign_id: string | null) => ({ plan: { campaign_id } });

  test("draws a plan of its walked history, withheld or not", () => {
    expect(campaignPlan(detail(["plan-2", "plan-1"]), "plan-1", read("campaign_1"))).toEqual(read("campaign_1"));
    expect(campaignPlan(detail(["plan-2", "plan-1"]), "plan-2", null)).toBeNull();
  });

  test("past the walk's ceiling, only a plan that names the campaign", () => {
    expect(campaignPlan(detail(["plan-2"], true), "plan-0", read("campaign_1"))).toEqual(read("campaign_1"));
    expect(() => campaignPlan(detail(["plan-2"], true), "plan-0", read("campaign_9"))).toThrow(
      "404: campaign campaign_1 has run no plan plan-0");
    expect(() => campaignPlan(detail(["plan-2"], true), "plan-0", null)).toThrow(ApiError);
  });

  test("never one it did not run, though the plan names it", () => {
    // A revision that lost the race to advance names the campaign and was never run.
    expect(() => campaignPlan(detail(["plan-2", "plan-1"]), "plan-9", read("campaign_1"))).toThrow(
      "404: campaign campaign_1 has run no plan plan-9");
  });

  test("draws a plan the scope refuses as withheld, and fails on any other error", async () => {
    expect(await unlessWithheld(Promise.resolve(1))).toBe(1);
    expect(await unlessWithheld(Promise.reject(new ApiError(404, "no plan")))).toBeNull();
    expect(await unlessWithheld(Promise.reject(new ApiError(403, "needs read_plans")))).toBeNull();
    await expect(unlessWithheld(Promise.reject(new ApiError(500, "store")))).rejects.toThrow("500: store");
    await expect(unlessWithheld(Promise.reject(new Error("offline")))).rejects.toThrow("offline");
  });

  test("its own address opens on the Overview of the plan it runs", () => {
    expect(headAddress("cmp_1", "plan-1")).toBe("/app/campaigns/cmp_1/plans/plan-1");
    expect(headAddress("cmp 1", "plan/1")).toBe("/app/campaigns/cmp%201/plans/plan%2F1");
  });


  test("a hand-over whose re-read is refused says where the campaign went", () => {
    const to = { label: "member@acme.example (acme)" };
    expect(handedAway(new ApiError(403, "this campaign belongs to another owner"), to)).toBe(
      "This campaign was handed over to member@acme.example (acme), which leaves it outside your reach.",
    );
    // Any other failure is the page's to draw, as it would after any write.
    expect(handedAway(new ApiError(404, "campaign not found"), to)).toBeNull();
    expect(handedAway(new ApiError(500, "store"), to)).toBeNull();
    expect(handedAway(new Error("offline"), to)).toBeNull();
  });

  test("a rename refuses a blank name in the classic words", () => {
    expect(renameBody("Nightly v2")).toEqual({ name: "Nightly v2" });
    expect(() => renameBody("  ")).toThrow("Name the campaign");
  });

  test("a name is checked as it is typed, as the route counts it: trimmed, in characters", () => {
    expect(nameProblem("Nightly v2")).toBe("");
    expect(nameProblem(" \t")).toBe("Name the campaign");
    expect(nameProblem(`  ${"é".repeat(NAME_MAX_CHARS)}  `)).toBe("");
    expect(nameProblem("😀".repeat(NAME_MAX_CHARS))).toBe("");
    expect(nameProblem("a".repeat(NAME_MAX_CHARS + 1))).toBe("Keep the name to 4,096 characters");
    expect(() => renameBody("a".repeat(NAME_MAX_CHARS + 1))).toThrow("Keep the name to 4,096 characters");
  });
});

describe("a document revision", () => {
  test("names the plan it was read from, and an empty name not at all", () => {
    expect(revisionQuery("plan-1", "")).toBe("base_plan_id=plan-1");
    expect(revisionQuery("plan-1", " ")).toBe("base_plan_id=plan-1");
    expect(revisionQuery("plan-1", "rev 2")).toBe("base_plan_id=plan-1&note=rev+2");
    expect(revisionQuery("plan-1", "", true)).toBe("base_plan_id=plan-1&carry_earlier=true");
    expect(revisionQuery("plan-1", "", false, ["plan-2", "plan-3"])).toBe("base_plan_id=plan-1&inherit_from=plan-2%2Cplan-3");
  });
});

describe("the plans a revision inherits from", () => {
  const row = (campaign_id: string, name: string, current_plan_id: string | null, finished: number | null) => ({
    campaign_id, name, current_plan_id, owner: { type: "user" as const, user_id: "user_a" },
    created_at: "2026-10-01T00:00:00Z", updated_at: "2026-10-01T00:00:00Z",
    plan: current_plan_id ? { plan_id: current_plan_id, status: "active" as const, created_at: "2026-10-01T00:00:00Z",
      cancel_requested: false,
      counts: finished === null ? null : { available: 0, cancelled: 0, failed: 0, finished, running: 0, total: finished } } : null,
  });
  const rows = [row("campaign_self", "Nightly", "plan-self", 3), row("campaign_b", "East", "plan-b", 1503),
    row("campaign_idle", "Idle", null, null), row("campaign_c", "East", "plan-c", null)];
  const offers = inheritOffers(rows, "campaign_self");

  test("offers the plan each other campaign runs, where the reader reads it", () => {
    expect(offers.map((offer) => offer.plan_id)).toEqual(["plan-b", "plan-c"]);
    expect(offers[0]).toEqual({ plan_id: "plan-b", campaign_id: "campaign_b", campaign: "East", finished: 1503 });
    expect(inheritOffers([{ ...row("campaign_d", "Unread", "plan-d", 1), plan: null }], "x")).toEqual([]);
  });

  test("finds an offer by plan id, campaign id, or the newest campaign of the name", () => {
    expect(namedOffer(offers, " plan-c ")?.campaign_id).toBe("campaign_c");
    expect(namedOffer(offers, "campaign_c")?.plan_id).toBe("plan-c");
    expect(namedOffer(offers, "East")?.plan_id).toBe("plan-b");
    expect(namedOffer(offers, "plan-elsewhere")).toBeUndefined();
  });

  test("names a plan read by id by its campaign where the reader lists it", () => {
    const snapshot = (finished: number) => ({ computed_at: "2026-10-01T00:00:00Z",
      counts: { available: 0, cancelled: 0, failed: 0, finished, running: 0, total: finished }, starved_groups: [] });
    expect(planSourceOf({ plan_id: "plan-old", campaign_id: "campaign_b", progress_snapshot: snapshot(7) }, offers))
      .toEqual({ plan_id: "plan-old", campaign_id: "campaign_b", campaign: "East", finished: 7 });
    expect(planSourceOf({ plan_id: "plan-loose", campaign_id: null, progress_snapshot: null }, offers))
      .toEqual({ plan_id: "plan-loose", campaign_id: null, campaign: null, finished: null });
  });

  test("says whose plan it is and what it finished", () => {
    expect(sourceText(offers[0]!)).toBe("East · 1,503 finished");
    expect(sourceText({ plan_id: "plan-x", campaign_id: "campaign_x", campaign: null, finished: 0 }))
      .toBe("campaign_x · 0 finished");
    expect(sourceText({ plan_id: "plan-y", campaign_id: null, campaign: null, finished: null }))
      .toBe("no campaign");
  });
});

describe("another plan's variants", () => {
  test("are appended under a line that names the plan", () => {
    expect(withVariantsOf("benchmarks = [\"a\"]\n\n[[variants]]\nmodels = []\n\n\n", "plan-x", "[[variants]]\nmodels = [1]\n"))
      .toBe("benchmarks = [\"a\"]\n\n[[variants]]\nmodels = []\n\n# The variants of plan-x.\n[[variants]]\nmodels = [1]\n");
  });

  test("that repeat earlier ones are named", () => {
    expect(repeatedText([])).toBe("");
    expect(repeatedText([3])).toBe("Variant 3 repeats an earlier variant, so it adds no job.");
    expect(repeatedText([3, 4, 6])).toBe("Variants 3, 4 and 6 repeat earlier variants, so they add no job.");
  });
});

describe("a revert", () => {
  const detail = { may_edit_variants: true, current_plan_id: "plan-b" };

  test("is offered on an earlier plan to a reader who may revise", () => {
    expect(revertOffered(detail, "plan-a")).toBe(true);
    expect(revertOffered(detail, "plan-b")).toBe(false);
    expect(revertOffered({ ...detail, may_edit_variants: false }, "plan-a")).toBe(false);
    expect(revertOffered({ ...detail, current_plan_id: null }, "plan-a")).toBe(false);
  });
});

describe("a restore of one variant", () => {
  test("appends the variant, or replaces one variant of the current plan", () => {
    expect(restoreEdit("plan-head", "plan-a", 2, null)).toEqual({
      query: "base_plan_id=plan-head&restore=plan-a%3A2", toml: "",
    });
    expect(restoreEdit("plan-head", "plan-a", 0, 1)).toEqual({
      query: "base_plan_id=plan-head&restore=plan-a%3A0", toml: "remove = [1]\n",
    });
  });
});

describe("a revision's preview", () => {
  const preview = { bodies: [], plan_warnings: [], imports: [], document: "", unchanged: false, carried_job_count: 3, new_job_count: 0,
    requirements_rerun_job_count: 0, satisfied_job_count: 0, restored_job_count: 0, inherited_job_count: 0, rerun_job_count: 0, recalculated_job_count: 0, dropped_job_ids: [] as string[],
    held_jobs: [], repeated_variants: [] as number[] };

  test("says what filing it does to the current plan's jobs", () => {
    expect(revisionSummary(preview)).toBe("Filing it changes no job: every job carries over as it is.");
    expect(revisionSummary({ ...preview, unchanged: true, carried_job_count: 0 }))
      .toBe("Filing it files nothing: this is the document the campaign runs.");
    expect(revisionSummary({ ...preview, new_job_count: 2, dropped_job_ids: ["job-1"] }))
      .toBe("Filing it keeps 3 of the current plan's jobs, finished ones included, queues 2 new, stops 1.");
    expect(revisionSummary({ ...preview, rerun_job_count: 1 }))
      .toBe("Filing it keeps 3 of the current plan's jobs, finished ones included, runs 1 again, stops 0.");
    expect(revisionSummary({ ...preview, requirements_rerun_job_count: 2, dropped_job_ids: ["job-1", "job-2"] }))
      .toBe("Filing it keeps 3 of the current plan's jobs, finished ones included, "
        + "runs 2 finished again (failed, or their device or build is not known to meet the requirements), stops 2.");
    expect(revisionSummary({ ...preview, restored_job_count: 2, dropped_job_ids: ["job-1", "job-2"] }))
      .toBe("Filing it keeps 3 of the current plan's jobs, finished ones included, "
        + "brings back 2 finished from earlier plans, stops 2.");
    expect(revisionSummary({ ...preview, inherited_job_count: 1, dropped_job_ids: ["job-1"] }))
      .toBe("Filing it keeps 3 of the current plan's jobs, finished ones included, "
        + "takes 1 finished from the plans it inherits from, stops 1.");
    expect(revisionSummary({ ...preview, satisfied_job_count: 2, dropped_job_ids: ["job-1"] }))
      .toBe("Filing it keeps 3 of the current plan's jobs, finished ones included, "
        + "takes 2 finished from earlier runs of the same cell, stops 1.");
  });

  test("says what filing does to each gated model", () => {
    expect(importText({ hf_ref: "LiquidAI/LFM2-GGUF/m.gguf", action: "fetch", bytes: 3 * 1024 * 1024 }))
      .toBe("LiquidAI/LFM2-GGUF/m.gguf: downloads 3.0 MB");
    expect(importText({ hf_ref: "LiquidAI/LFM2-GGUF/m.gguf", action: "reuse", resource_ids: ["vault_1", "vault_2"] }))
      .toBe("LiquidAI/LFM2-GGUF/m.gguf: reuses vault_1, vault_2");
  });

  test("gives each downloaded file one progress line", () => {
    const hf_ref = `LiquidAI/LFM2-GGUF@${"3f1c2a9".padEnd(40, "0")}`;
    const line = (file: string, bytes_done: number) =>
      ({ event: "progress" as const, hf_ref, file, bytes_done, bytes_total: 200 });
    expect(importLine(line("m.gguf", 50))).toBe("m.gguf (LiquidAI/LFM2-GGUF@3f1c2a9): downloading 25%");
    expect(importLine(line("m.gguf", 200))).toBe("m.gguf (LiquidAI/LFM2-GGUF@3f1c2a9): downloaded");
    expect(importKey(line("m.gguf", 50))).toBe(importKey(line("m.gguf", 200)));
    expect(importKey(line("m.gguf", 50))).not.toBe(importKey(line("mmproj.gguf", 50)));
  });
});

describe("the campaign's device time", () => {
  test("a total prints its duration and the jobs it leaves out", () => {
    expect(deviceTimeText({ device_time_ms: 7_500_000, unreported_job_count: 0 })).toBe("2h 05m");
    expect(deviceTimeText({ device_time_ms: 184_250, unreported_job_count: 1 })).toBe("3m 04s (1 job reported none)");
    expect(deviceTimeText({ device_time_ms: null, unreported_job_count: 3 })).toBe("— (3 jobs reported none)");
    expect(deviceTimeText({ device_time_ms: null, unreported_job_count: 0 })).toBe("—");
  });
});
