import { render } from "svelte/server";
import { describe, expect, test, vi } from "vitest";
import schema from "../src/lib/api-schema.json";
import {
  FAILURE_CODES, MAX_RERUNS, changeText, checkLines, checkText, codesProblem, filingsTable, gapsTable, groupTabs,
  groupsCrumb, listingTabs, mayAdminister, mayFile, memberOffers, memberRows, outcomeText, paramsGaps, paramsWrite,
  percentProblem, policyBody, policyPreviewTable, policyText, policyTitle, previewLacks, recentFilings,
  rerunSummaryText, rerunTitle, rerunValue, rerunsCsv, rerunsProblem, rerunsTable, shortProblem, smallerFailedRows,
  smallerFailedTable, stateTitle, togetherText, turnOffText, turnOnText,
} from "../src/lib/campaign-groups";
import { CAMPAIGN_PAGES } from "../src/lib/campaigns";
import CampaignGroupBody from "../src/lib/components/CampaignGroupBody.svelte";
import PolicyPreview from "../src/lib/components/PolicyPreview.svelte";
import PolicyRow from "../src/lib/components/PolicyRow.svelte";
import RerunResults from "../src/lib/components/RerunResults.svelte";
import { Loader } from "../src/lib/components/work.svelte";
import { ownerBranch } from "../src/lib/owned";
import { match, pathOf } from "../src/lib/routes";
import { tableView, unsorted } from "../src/lib/table";
import type {
  CampaignGroupPolicy, CampaignGroupPolicyPreview, CampaignGroupRerunResults, CampaignView, FailureCode,
  MemberPolicyPreview, ModelParamsGap, OwnUserView, PolicyCheckCounts, PolicyFiling, PolicyKind, PolicyMemberState,
  PolicyCheck, PolicyRerun, PolicyRerunSummary, PrincipalOwner, SmallerFailure, UserPermission,
} from "../src/lib/api-types";
import { abilitiesOf } from "./abilities";

// The table imports it, and it reads `location` as it loads; the re-runs' table reads an empty address.
vi.mock("../src/lib/route.svelte", async () => ({
  here: { query: new URLSearchParams() }, href: (await import("../src/lib/routes")).href,
}));

const reader = (permissions: UserPermission[], org_id?: string): OwnUserView => ({
  user_id: "user_me", contact_email: "me@acme.example", account_type: "human", status: "active", created_at: "2026-10-01T00:00:00Z",
  permissions, keys: [], may: abilitiesOf(permissions, org_id), ...(org_id ? { org_id, org_slug: "acme" } : {}),
});

const [mine, theirs]: PrincipalOwner[] = [{ type: "user", user_id: "user_me" }, { type: "user", user_id: "user_x" }];
const [acme, rival]: PrincipalOwner[] = [{ type: "org", org_id: "org_acme" }, { type: "org", org_id: "org_rival" }];

const campaign = (campaign_id: string, owner: PrincipalOwner, fields: Partial<CampaignView> = {}): CampaignView =>
  ({ campaign_id, name: campaign_id, owner, created_at: "2026-11-01T00:00:00Z", updated_at: "2026-11-01T00:00:00Z", ...fields });

describe("the group pages' tabs", () => {
  test("an owner's campaign listing opens on its campaigns, its groups a tab beside them", () => {
    const listings = [["/app/me/campaigns", "me-campaigns", "me-campaign-groups"],
                      ["/app/orgs/acme/campaigns", "org-campaigns", "org-campaign-groups"]] as const;
    for (const [base, ...pages] of listings) {
      expect(listingTabs(base, false).map((tab) => [tab.label, tab.href, tab.current])).toEqual([
        ["Campaigns", base, true], ["Groups", `${base}/groups`, false],
      ]);
      const tabs = listingTabs(base, true);
      expect(tabs.map((tab) => tab.current)).toEqual([false, true]);
      expect(tabs.map((tab) => match(tab.href).page)).toEqual(pages);
      for (const tab of tabs) expect(pathOf(match(tab.href))).toBe(tab.href);
    }
  });

  test("a group's Policies and Activity follow its Campaigns, and its Settings are set apart, offered with the right and listed whenever open", () => {
    expect(groupTabs("group_1", "campaigns", true)).toEqual([
      { key: "campaigns", label: "Campaigns", href: "/app/campaign-groups/group_1", current: true, start: false },
      { key: "policies", label: "Policies", href: "/app/campaign-groups/group_1/policies", current: false, start: false },
      { key: "activity", label: "Activity", href: "/app/campaign-groups/group_1/activity", current: false, start: false },
      { key: "settings", label: "Settings", href: "/app/campaign-groups/group_1/settings", current: false, start: true },
    ]);
    expect(groupTabs("group_1", "policies", false).map((tab) => [tab.key, tab.current]))
      .toEqual([["campaigns", false], ["policies", true], ["activity", false]]);
    expect(groupTabs("group_1", "activity", false).map((tab) => [tab.key, tab.current]))
      .toEqual([["campaigns", false], ["policies", false], ["activity", true]]);
    // A link to it still says where the reader is.
    expect(groupTabs("group_1", "settings", false).map((tab) => [tab.key, tab.current]))
      .toEqual([["campaigns", false], ["policies", false], ["activity", false], ["settings", true]]);
    expect(groupTabs("group_1", "settings", true).map((tab) => match(tab.href).page))
      .toEqual(["campaign-group", "campaign-group-policies", "campaign-group-activity", "campaign-group-settings"]);
  });

  test("a group's page sits under its owner's Groups tab, which the listing of every campaign has none of", () => {
    const crumb = (owner: PrincipalOwner) =>
      groupsCrumb(ownerBranch({ user_id: "user_me" }, owner, [{ org_id: "org_acme", slug: "acme" }], CAMPAIGN_PAGES));
    expect(crumb(mine)).toEqual({ label: "Groups", href: "/app/me/campaigns/groups" });
    expect(crumb(acme)).toEqual({ label: "Groups", href: "/app/orgs/acme/campaigns/groups" });
    expect([rival, theirs].map(crumb)).toEqual([null, null]);
  });
});

describe("who the group pages offer a write to", () => {
  test("a create and a member change go to a reader who files into the owner's campaigns", () => {
    const owners = [mine, theirs, acme, rival];
    expect(owners.map((owner) => mayFile(reader(["submit_plans"], "org_acme"), owner))).toEqual([true, false, true, false]);
    expect(owners.map((owner) => mayFile(reader(["submit_plans", "manage_any_orgs"]), owner))).toEqual([true, true, true, true]);
    // The routes' gate is submit_plans, which administering an organization does not give.
    expect([mine, acme].map((owner) => mayFile(reader(["manage_orgs", "read_plans"], "org_acme"), owner))).toEqual([false, false]);
    expect(mayFile(reader(["manage_any_orgs"]), rival)).toBe(false);
  });

  test("a rename and a delete go to whoever renames the owner's campaigns, and to an administrator on another account's group", () => {
    expect([mine, theirs, acme].map((owner) => mayAdminister(reader(["submit_plans"], "org_acme"), owner)))
      .toEqual([true, false, false]);
    // The other account may be a disabled member, whose group its administrator winds down.
    expect([acme, rival, theirs, mine].map((owner) => mayAdminister(reader(["manage_orgs"], "org_acme"), owner)))
      .toEqual([true, false, true, false]);
    expect([rival, theirs].map((owner) => mayAdminister(reader(["manage_any_orgs"]), owner))).toEqual([true, true]);
    expect(mayAdminister(reader(["read_any_plans"]), theirs)).toBe(false);
    // Reading every plan, it opens any account's group, not only a disabled member's.
    expect(mayAdminister(reader(["manage_orgs", "read_any_plans"], "org_acme"), theirs)).toBe(false);
  });
});

describe("a group's members", () => {
  test("are the group's records, in its order, with the plans the owner's listing reads", () => {
    const plan = { plan_id: "plan-1", status: "active", cancel_requested: false, counts: null, created_at: "2026-11-01T00:00:00Z" } as const;
    const listed = [campaign("campaign_2", acme, { name: "old name", plan }), campaign("campaign_1", acme)];
    expect(memberRows([campaign("campaign_1", acme), campaign("campaign_2", acme), campaign("campaign_3", acme)], listed)
      .map((row) => [row.name, row.plan?.plan_id ?? null]))
      .toEqual([["campaign_1", null], ["campaign_2", "plan-1"], ["campaign_3", null]]);
  });

  test("the add offers the owner's campaigns that are not members, an archived one marked, found by its labels", () => {
    const listed = [
      campaign("campaign_3", acme, { name: "aa-perf S26", labels: { device: "S26 Ultra" } }),
      campaign("campaign_2", acme, { name: "aa-perf iPhone", state: "archived" }),
      campaign("campaign_1", acme, { name: "member" }),
      campaign("campaign_0", rival, { name: "elsewhere" }),
    ];
    expect(memberOffers(listed, { owner: acme, members: [{ campaign_id: "campaign_1" }] })).toEqual([
      { value: "campaign_3", label: "aa-perf S26", searchText: "device=S26 Ultra" },
      { value: "campaign_2", label: "aa-perf iPhone (archived)", searchText: "" },
    ]);
    expect(memberOffers(listed, { owner: theirs, members: [] })).toEqual([]);
  });
});

describe("the group page", () => {
  const draw = (who: OwnUserView, owner: PrincipalOwner, listed?: CampaignView[]) => render(CampaignGroupBody, {
    props: {
      group: {
        group_id: "group_1", name: "S26 study", owner, created_at: "2026-11-01T00:00:00Z", updated_at: "2026-11-01T00:00:00Z",
        members: listed ?? [campaign("campaign_1", owner, { name: "listed member" }), campaign("campaign_2", owner, { name: "unlisted member" })],
      },
      listed: listed ?? [campaign("campaign_1", owner, { name: "listed member" }), campaign("campaign_3", owner, { name: "outsider" })],
      reader: who, names: {}, add: async () => {}, remove: async () => {},
    },
  }).body;

  test("marks each policy that is on cleared or triggered per member, and counts the members every one cleared", () => {
    const checks = (triggered: boolean): PolicyCheck[] => [{ kind: "swap", triggered: false }, { kind: "spread", triggered }];
    const body = draw(reader([]), mine, [
      campaign("campaign_1", mine, { name: "flagged", policy_checks: checks(true) }),
      campaign("campaign_2", mine, { name: "clean", policy_checks: checks(false) }),
      campaign("campaign_3", mine, { name: "not read yet" }),
    ]).replace(/<!--.*?-->/g, "");
    expect(body).toContain("1 of 2 campaigns cleared on every policy that is on.");
    expect(body).toContain('<span data-state="cleared">swap cleared</span> <span data-state="triggered">spread triggered</span>');
    expect(body).toContain('<span data-state="cleared">swap cleared</span> <span data-state="cleared">spread cleared</span>');
  });

  test("counts nothing while queue-maintenance has read no member", () => {
    expect(draw(reader([]), mine)).not.toContain("cleared on every policy");
  });

  test("links each member to its campaign and offers only the owner's other campaigns", () => {
    const body = draw(reader(["submit_plans"]), mine);
    expect(body).toContain('<a href="/app/campaigns/campaign_1">listed member</a>');
    expect(body).toContain('<a href="/app/campaigns/campaign_2">unlisted member</a>');
    expect(body).toMatch(/class="name[^"]*">outsider</);
    expect(body).not.toMatch(/class="name[^"]*">listed member</);
  });

  test("names the progress counts once, in the header, outside its sort control", () => {
    expect(draw(reader([]), mine).replace(/<!--.*?-->/g, "")).toContain('</button> <span class="key">finished · '
      + '<i class="swatch" data-of="failed"></i>failed · <i class="swatch" data-of="running"></i>running / total</span>');
  });

  test("offers the member writes to a filer, not to an administrator who files no plans", () => {
    const filer = draw(reader(["submit_plans"], "org_acme"), acme);
    expect(filer).toContain("campaigns to add");
    expect(filer).toContain('aria-label="Remove listed member from the group"');

    const admin = draw(reader(["manage_orgs"], "org_acme"), acme);
    expect(admin).not.toContain("campaigns to add");
    expect(admin).not.toContain(">Remove</button>");
    expect(admin).toContain("Adding or removing its campaigns takes <code>submit_plans</code> for its owner.");
  });
});

/** Failed's default codes, as `PolicyKind::defaults` sets them, each at 2 re-runs. */
const CODES = ["runtime_error", "device_error", "unspecified"] as const;
const LIMITS = Object.fromEntries(CODES.map((code) => [code, 2]));

const policy = (kind: PolicyKind, max_reruns: number,
                fields: Partial<CampaignGroupPolicy> = {}): CampaignGroupPolicy => ({
  group_id: "group_1", kind, enabled: false, max_reruns,
  settings: kind === "spread" ? { percent: 5 } : kind === "failed" ? { codes: { ...LIMITS } } : {},
  enabled_by: null, enabled_at: null, ...fields,
});

describe("a group's policies", () => {
  test("a policy reads as the jobs its check finds, its re-runs, then the check's own outcome", () => {
    expect([policy("swap", 5), policy("spread", 3), policy("failed", 0), policy("ttft_over_e2e", 3)].map(policyText))
      .toEqual([
        "Re-runs a job whose device swapped memory during the run, up to 5 times. If it still swaps, the job is "
          + "marked failed.",
        "Re-runs a job whose standard deviation is over 5% of its result, up to 3 times. If it is still over, the job "
          + "is marked failed.",
        "Re-runs a job whose device reported a failure coded unspecified up to 2 times, runtime_error up to 2 times or "
          + "device_error up to 2 times. Its runs in a row that failed with any of these codes count toward the limit "
          + "of its last one. If it still fails, the job keeps the failure.",
        "Re-runs a job whose time to first token is over its end-to-end latency, up to 3 times. If it still is, the "
          + "job keeps its last result.",
      ]);
    expect(policyText(policy("swap", 0))).toBe(
      "A job whose device swapped memory during the run is marked failed, with no re-run.");
    // It only flags, so it takes no re-run.
    expect(policyText(policy("model_params", 0))).toBe("Flags the result rows whose model has no entry in the model "
      + "params table, so they show no parameter counts. It re-runs and marks nothing: the rows it flags, and the form "
      + "to add their counts, are on the Activity tab.");
    expect(policyText(policy("smaller_failed", 0))).toBe("Flags the result rows whose job failed while a job of the "
      + "same cell at larger sizes, such as a longer context, ran with no failure. It re-runs and marks nothing, and "
      + "queue-maintenance does not read it: the jobs it flags are on the Activity tab.");
  });

  test("a policy is titled by its check, spread at its stored percent, and marked where the Activity tab finds it off", () => {
    expect([policy("swap", 5), policy("spread", 3, { settings: { percent: 7.5 } }), policy("failed", 0),
            policy("ttft_over_e2e", 3), policy("model_params", 0), policy("smaller_failed", 0)].map(policyTitle))
      .toEqual(["Swap", "Stddev over 7.5%", "Failures by type", "TTFT over E2E latency", "Missing model params",
                "Failed below a clean larger run"]);
    expect([policy("model_params", 0), policy("model_params", 0, { enabled: true })].map(stateTitle))
      .toEqual(["Missing model params (off)", "Missing model params"]);
  });

  test("spread reads its short repetition rule, where it has one, in its title and text", () => {
    const rule = { percent: 5, short_repetition_ms: 1000, short_repetition_percent: 10 };
    const spread = policy("spread", 3, { settings: rule });
    expect(policyTitle(spread)).toBe("Stddev over 5% (10% under 1000 ms)");
    expect(policyText(spread)).toBe("Re-runs a job whose standard deviation is over 5% of its result, or 10% where its "
      + "repetitions average under 1000 ms, up to 3 times. If it is still over, the job is marked failed.");
  });

  test("failed reads the codes it re-runs, each with its re-runs, in its text, in the boxes' order", () => {
    const failed = (codes: Partial<Record<FailureCode, number>>) => policyText(policy("failed", 0, { settings: { codes } }));
    const counting = " Its runs in a row that failed with any of these codes count toward the limit of its last one. "
      + "If it still fails, the job keeps the failure.";
    expect([failed({ harness_error: 1 }), failed({ harness_error: 3, out_of_memory: 1 })]).toEqual([
      `Re-runs a job whose device reported a failure coded harness_error once.${counting}`,
      `Re-runs a job whose device reported a failure coded out_of_memory once or harness_error up to 3 times.${counting}`,
    ]);
  });

  test("offers every failure code the server takes", () => {
    expect(FAILURE_CODES).toEqual(schema.$defs.FailureCode.oneOf.map((code) => code.const));
  });

  const typed = (max_reruns: string, percent: string, short_repetition_ms = "", short_repetition_percent = "",
                 codes: Partial<Record<FailureCode, string>> = {}) =>
    ({ max_reruns, percent, short_repetition_ms, short_repetition_percent, codes });

  test("Save changes sends every value as typed, a percent for spread alone, over the stored policy as its base", () => {
    const on = policy("swap", 5, { enabled: true, enabled_by: "user_me", enabled_at: "2026-11-09T22:30:00Z" });
    expect(policyBody(on, typed("3", "")))
      .toEqual({ enabled: true, max_reruns: 3, settings: {}, base: { enabled: true, max_reruns: 5, settings: {} } });
    expect(policyBody(policy("spread", 3), typed("4", "8"))).toEqual({
      enabled: false, max_reruns: 4, settings: { percent: 8 },
      base: { enabled: false, max_reruns: 3, settings: { percent: 5 } },
    });
    expect(policyBody(policy("spread", 3), typed("3", "5", "1000", "10")).settings)
      .toEqual({ percent: 5, short_repetition_ms: 1000, short_repetition_percent: 10 });
    // Failed sends each code typed above 0 with its re-runs.
    const limits = { device_error: "2", harness_error: "1", runtime_error: "0", unspecified: "" };
    expect(policyBody(policy("failed", 0), typed("0", "", "", "", limits))).toEqual({
      enabled: false, max_reruns: 0, settings: { codes: { device_error: 2, harness_error: 1 } },
      base: { enabled: false, max_reruns: 0, settings: { codes: LIMITS } },
    });
    expect((["swap", "ttft_over_e2e"] as const)
      .map((kind) => policyBody(policy(kind, 2), typed("2", "5", "1000", "10", { device_error: "2" })).settings))
      .toEqual([{}, {}]);
  });

  test("Turn off sends the stored values, whatever was typed, over them as its base", () => {
    // Stored before the route took at least 1 re-run for spread.
    const stored = { enabled: true, max_reruns: 0, settings: { percent: 7.5 } };
    expect(policyBody(policy("spread", 0, stored), "off")).toEqual({ ...stored, enabled: false, base: stored });
  });

  test("Turn on sends the stored values the preview counted, whatever was typed, over them as its base", () => {
    const stored = { enabled: false, max_reruns: 4, settings: { percent: 7.5 } };
    expect(policyBody(policy("spread", 4, stored), "on")).toEqual({ ...stored, enabled: true, base: stored });
  });

  test("the model params policy is turned on and off with no re-run and no setting", () => {
    const off = { enabled: false, max_reruns: 0, settings: {} };
    expect(policyBody(policy("model_params", 0), "on")).toEqual({ ...off, enabled: true, base: off });
    const on = { ...off, enabled: true };
    expect(policyBody(policy("model_params", 0, on), "off")).toEqual({ ...off, base: on });
  });

  test("a field is checked as typed, as the route checks it", () => {
    expect(["0", "5", "15"].map((typed) => rerunsProblem("swap", typed))).toEqual(["", "", ""]);
    expect(["", "-1", "1.5", "16"].map((typed) => rerunsProblem("swap", typed)))
      .toEqual(Array(4).fill("Enter a whole number of re-runs from 0 to 15"));
    // A check that keeps the result needs a re-run to do anything.
    expect((["spread", "ttft_over_e2e"] as const)
      .map((kind) => [rerunsProblem(kind, "0"), rerunsProblem(kind, "1")]))
      .toEqual(Array(2).fill(["Enter a whole number of re-runs from 1 to 15", ""]));
    // Model params re-runs nothing, and failed takes its re-runs per code, so each takes 0 alone.
    expect((["model_params", "failed"] as const).map((kind) => ["0", "1"].map((typed) => rerunsProblem(kind, typed))))
      .toEqual(Array(2).fill(["", "Enter a whole number of re-runs from 0 to 0"]));
    expect(["", "0", "-2"].map((typed) => percentProblem("spread", typed)))
      .toEqual(Array(3).fill("Enter a percent above 0"));
    expect(percentProblem("spread", "7.5")).toBe("");
    expect((["swap", "failed", "ttft_over_e2e", "model_params"] as const).map((kind) => percentProblem(kind, "")))
      .toEqual(["", "", "", ""]);
    // Spread takes a short repetition's time and percent together, or neither.
    expect([["", ""], ["1000", "10"], ["0.5", "7.5"]].map(([ms, percent]) => shortProblem("spread", ms, percent)))
      .toEqual(["", "", ""]);
    expect([["1000", ""], ["", "10"], ["0", "10"], ["1000", "-1"]].map(([ms, percent]) => shortProblem("spread", ms, percent)))
      .toEqual(Array(4).fill("Enter a repetition time and its percent above 0, or neither"));
    expect(shortProblem("swap", "1000", "")).toBe("");
    // Failed takes at least one code, each with 1 to 15 re-runs; empty or 0 leaves a code out.
    expect([{}, { harness_error: "0", device_error: "" }].map((codes) => codesProblem("failed", codes)))
      .toEqual(Array(2).fill("Enter re-runs for at least one failure code"));
    expect([{ harness_error: "16" }, { harness_error: "1.5" }, { harness_error: "2", device_error: "-1" }]
      .map((codes) => codesProblem("failed", codes)))
      .toEqual(Array(3).fill("Enter a whole number of re-runs from 0 to 15 for each failure code"));
    expect([codesProblem("failed", { harness_error: "15", device_error: "0" }), codesProblem("swap", {})])
      .toEqual(["", ""]);
  });

  test("caps re-runs where the server's schema does", () => {
    expect(MAX_RERUNS).toBe(schema.$defs.SetCampaignGroupPolicyRequest.properties.max_reruns.maximum);
  });

  const row = (shown: CampaignGroupPolicy, writes: boolean, counts?: string[]) => render(PolicyRow, {
    props: { policy: shown, save: writes ? async () => true : undefined, counts },
  }).body;

  test("offers a filer the re-runs, spread's threshold and no outcome, and no Turn on until the preview loads", () => {
    const swap = row(policy("swap", 5), true);
    expect(swap).toContain('data-state="off"');
    expect(swap).toContain("<span>Re-runs</span>");
    expect(swap).toMatch(/<input type="number" min="0" max="15"/);
    expect(swap).not.toContain("<span>Threshold (%)</span>");
    expect(swap).not.toMatch(/<select\b|Turn on/);
    const spread = row(policy("spread", 3), true);
    expect(spread).toContain("<span>Threshold (%)</span>");
    expect(spread).toContain("<span>Repetitions under (ms)</span>");
    expect(spread).toContain("<span>Their threshold (%)</span>");
    expect(spread).toMatch(/<input type="number" min="1" max="15"/);
    expect(swap).not.toContain("<span>Repetitions under (ms)</span>");
    // One re-runs box per failure code, the stored ones filled, and no other re-runs box.
    const failed = row(policy("failed", 0), true);
    const legend = "<legend>Re-runs by failure code (empty or 0: not re-run)</legend>";
    expect(failed).toContain(legend);
    expect(failed.match(/<code>\w+<\/code>(<!--.*?-->|<\/span>|\s)*<input [^>]*>/g)
      ?.map((box) => [box.match(/<code>(\w+)</)?.[1], box.match(/ value="(\d*)"/)?.[1] ?? ""]))
      .toEqual(FAILURE_CODES.map((code) => [code, (CODES as readonly string[]).includes(code) ? "2" : ""]));
    expect(failed).not.toContain("<span>Re-runs</span>");
    expect(swap).not.toContain(legend);
  });

  test("offers a filer Turn on, which asks first, once the preview loaded, and never on a policy that is on", () => {
    const off = row(policy("swap", 5), true, ["S26 CPU: 0 flagged"]);
    expect(off).toMatch(/<button type="button" id="[^"]+-switch" [^>]*aria-haspopup="dialog"[^>]*>Turn on<\/button>/);
    expect(off).toContain("<dialog");
    // The preview shows a reader without the write what turning it on would do.
    expect(row(policy("swap", 5), false, ["S26 CPU: 0 flagged"])).not.toContain("Turn on");
    const on = row(policy("swap", 5, { enabled: true, enabled_by: "user_me", enabled_at: "2026-11-09T22:30:00Z" }), true,
                   ["S26 CPU: 0 flagged"]);
    expect(on).toContain(">Turn off</button>");
    expect(on).not.toContain(">Turn on</button>");
  });

  test("shows a reader without the write the state and the rule alone", () => {
    const body = row(policy("swap", 5), false);
    expect(body).toContain('data-state="off"');
    expect(body).toContain("Re-runs a job whose device swapped memory during the run, up to 5 times.");
    expect(body).not.toMatch(/<(button|input|select)\b/);
    expect(body).not.toContain('class="body"');
  });

  test("marks the field the route would refuse", () => {
    // A stored value stands in for a typed one, which a server render cannot type.
    const inputs = (shown: CampaignGroupPolicy) =>
      row(shown, true).match(/<input [^>]*>/g)?.map((input) => input.includes('aria-invalid="true"'));
    // Spread's percent, its short repetition's time and percent, then the re-runs.
    expect(inputs(policy("spread", 3))).toEqual([false, false, false, false]);
    expect(inputs(policy("spread", 3, { settings: {} }))).toEqual([true, false, false, false]);
    expect(inputs(policy("spread", 3, { settings: { percent: 5, short_repetition_ms: 1000 } })))
      .toEqual([false, true, true, false]);
    expect(inputs(policy("spread", 0))).toEqual([false, false, false, true]);
    // Failed's re-runs box per code.
    expect(inputs(policy("failed", 0, { settings: { codes: {} } }))).toEqual(FAILURE_CODES.map(() => true));
    expect(inputs(policy("failed", 0))).toEqual(FAILURE_CODES.map(() => false));
  });

  test("shows a policy that is on, and offers a filer its turn-off", () => {
    const on = policy("failed", 2, { enabled: true, enabled_by: "user_me", enabled_at: "2026-11-09T22:30:00Z" });
    const body = row(on, false);
    expect(body).toContain('data-state="on"');
    expect(body).not.toMatch(/<button\b/);

    const filer = row(on, true);
    expect(filer).toContain(">Turn off</button>");
    // Each of the four rows may have one, so its name carries the row's title.
    const [, text, hint] = filer.match(
      /<button type="button" id="([^"]+)" aria-labelledby="\1 [^"]+-title" aria-describedby="([^" ]+) ([^"]+)"/)
      ?.slice(1) ?? [];
    expect(filer).toContain(`id="${text}">${policyText(on)}`);
    expect(filer).toContain(`<div id="${hint}" class="hint">Turning it off files no new re-runs or marks from the next `
                            + "queue-maintenance run; re-runs it already filed still run.</div>");
  });

  test("offers Turn off while a value has a problem", () => {
    // A stored value stands in for a typed one, which a server render cannot type.
    const body = row(policy("spread", 0, { enabled: true, enabled_by: "user_me", enabled_at: "2026-11-09T22:30:00Z" }), true);
    expect(body).toContain('aria-invalid="true"');
    expect(body).toMatch(/<button type="button" id="[^"]+-switch" [^>]*aria-disabled="false"[^>]*>Turn off<\/button>/);
  });

  test("offers a filer the model params policy's Turn on and Turn off alone, with what turning it off stops", () => {
    const off = row(policy("model_params", 0), true, ["S26 CPU: 2 jobs whose rows lack model params"]);
    expect(off).toContain('data-state="off"');
    expect(off).toMatch(/<button type="button" id="[^"]+-switch" [^>]*aria-haspopup="dialog"[^>]*>Turn on<\/button>/);
    expect(off).not.toMatch(/<(form|input)\b|Save changes|class="body"/);
    const on = row(policy("model_params", 0, { enabled: true, enabled_by: "user_me", enabled_at: "2026-11-11T12:00:00Z" }),
                   true);
    expect(on).toContain(">Turn off</button>");
    expect(on).toContain('class="hint">Turning it off stops queue-maintenance logging these rows from its next run; the '
                         + "Activity tab still shows them.</div>");
    expect(on).not.toMatch(/<(form|input)\b/);
  });

  test("the smaller failed policy takes no re-run, and turning it on or off changes only its state", () => {
    expect(["0", "1"].map((typed) => rerunsProblem("smaller_failed", typed)))
      .toEqual(["", "Enter a whole number of re-runs from 0 to 0"]);
    expect(turnOnText(policy("smaller_failed", 0))).toBe(policyText(policy("smaller_failed", 0)));
    expect(turnOffText("smaller_failed"))
      .toBe("Turning it off only marks it off; the Activity tab still shows these jobs.");
    const on = row(policy("smaller_failed", 0, { enabled: true, enabled_by: "user_me",
                                                  enabled_at: "2026-11-12T12:00:00Z" }), true);
    expect(on).toContain(`class="hint">${turnOffText("smaller_failed")}</div>`);
    expect(on).not.toMatch(/<(form|input)\b/);
  });
});

const counts = (kind: PolicyKind, flagged: number, rerun = 0, marked = 0, kept = 0): PolicyCheckCounts =>
  ({ kind, on: true, flagged, rerun, marked, kept });

const member = (campaign_id: string, fields: Partial<MemberPolicyPreview> = {}): MemberPolicyPreview => ({
  campaign_id, plan_id: null, state: "evaluated", answered_job_count: 0, waiting_job_count: 0, earliest_deadline: null,
  deadline_passed: false, checks: [], rerun_job_ids: [], marked_job_ids: [], kept_job_count: 0, skipped: {},
  filing_plan_id: null, unclaimed_job_count: 0, missing_model_params: [], smaller_failed: [], last_filing: null,
  ...fields,
});

const filing = (plan_id: string, filed_at: string): PolicyFiling => ({ plan_id, filed_at, queued_job_count: 3 });

const gap = (key: string, row_count: number, job_count: number): ModelParamsGap => ({ key, row_count, job_count });

describe("a group's policy preview", () => {
  // Spread and model_params are off: their columns still count what they would do, and the policies on together leave
  // their jobs kept.
  const policies = [policy("swap", 3, { enabled: true }), policy("spread", 3), policy("failed", 2, { enabled: true }),
                    policy("ttft_over_e2e", 3, { enabled: true }), policy("model_params", 0)];
  const checked = member("campaign_1", {
    plan_id: "plan-1", answered_job_count: 4, waiting_job_count: 1, earliest_deadline: "2026-11-09T00:00:00Z",
    deadline_passed: true,
    checks: [counts("swap", 1, 0, 1), counts("spread", 1, 1), counts("failed", 1, 1), counts("ttft_over_e2e", 0),
             counts("model_params", 1, 0, 0, 1)],
    rerun_job_ids: ["job-2"], marked_job_ids: ["job-1"], kept_job_count: 1,
    missing_model_params: [gap("granite-4.2-8b", 3, 1)],
  });

  test("a check reads the jobs its policy alone flags, then what it would do with each", () => {
    expect(checkText(counts("swap", 4, 1, 2, 1))).toBe("4 flagged: 1 re-run, 2 marked failed, 1 kept");
    expect(checkText(counts("spread", 0))).toBe("0 flagged");
    // A skipped job counts as flagged alone.
    expect(checkText(counts("failed", 3, 1, 0, 1))).toBe("3 flagged: 1 re-run, 0 marked failed, 1 kept, 1 skipped");
    // It only flags, and keeps every job it flags.
    expect([counts("model_params", 0), counts("model_params", 1, 0, 0, 1), counts("model_params", 2, 0, 0, 2)]
      .map(checkText))
      .toEqual(["0 jobs whose rows lack model params", "1 job whose rows lack model params",
                "2 jobs whose rows lack model params"]);
    expect([counts("smaller_failed", 1, 0, 0, 1), counts("smaller_failed", 2, 0, 0, 2)].map(checkText))
      .toEqual(["1 failed job below a clean larger run", "2 failed jobs below a clean larger run"]);
  });

  test("the policies that are on read what they would re-run and mark failed together", () => {
    expect(togetherText(checked)).toBe("1 re-run, 1 marked failed");
    expect(togetherText(member("campaign_1", { skipped: { "job-4": "chain_read_failed" } })))
      .toBe("0 re-run, 0 marked failed, 1 skipped");
  });

  test("a row reads a checked member's counts, only the state of one left as it is, and an unreadable one's jobs", () => {
    const spec = policyPreviewTable(policies, { campaign_1: "S26 CPU" });
    expect(spec.columns.map((column) => column.label)).toEqual([
      "Campaign", "State", "Answered jobs", "Waiting for rows", "Earliest deadline", "Swap", "Stddev over 5% (off)",
      "Failures by type", "TTFT over E2E latency", "Missing model params (off)", "Policies on",
    ]);
    const row = (shown: MemberPolicyPreview) => spec.columns.map((column) => column.raw(shown));
    const flagged = (rerun: number, marked: number) => `1 flagged: ${rerun} re-run, ${marked} marked failed, 0 kept`;
    expect(row(checked)).toEqual([
      "S26 CPU", "checked", 4, 1, "2026-11-09T00:00:00Z", flagged(0, 1), flagged(1, 0), flagged(1, 0), "0 flagged",
      "1 job whose rows lack model params", "1 re-run, 1 marked failed",
    ]);
    expect(row(member("campaign_2", { state: "other_owner" })))
      .toEqual(["campaign_2", "another owner: left as it is", ...Array(9).fill(null)]);
    expect(row(member("campaign_3", { state: "unreadable", answered_job_count: 7, earliest_deadline: "2099-01-01T00:00:00Z" })))
      .toEqual(["campaign_3", "not read: pipette-duckdb failed or timed out", 7, null, "2099-01-01T00:00:00Z",
                ...Array(6).fill(null)]);
    // A member the policy pass holds names the plan that holds it, and how many of its jobs wait.
    const held = (state: PolicyMemberState, unclaimed_job_count = 0) =>
      row(member("campaign_4", { state, plan_id: "plan-4", filing_plan_id: "plan-5", unclaimed_job_count }));
    expect([held("waiting_for_devices", 1), held("waiting_for_devices", 3), held("filing_not_advanced"), held("refused")])
      .toEqual([
        ["campaign_4", "waiting for devices to take 1 job of its last group filing, plan-5", ...Array(9).fill(null)],
        ["campaign_4", "waiting for devices to take 3 jobs of its last group filing, plan-5", ...Array(9).fill(null)],
        ["campaign_4", "its last group filing, plan-5, did not advance: held", ...Array(9).fill(null)],
        ["campaign_4", "plan plan-4 cannot be filed again: left as it is", ...Array(9).fill(null)],
      ]);
  });

  test("Turn on says the rule, when it acts, and what the check alone would do with each campaign read", () => {
    expect(turnOnText(policy("swap", 3))).toBe(
      "Re-runs a job whose device swapped memory during the run, up to 3 times. If it still swaps, the job is marked "
      + "failed. From the next queue-maintenance run on, it acts on each campaign's current plan, jobs that finished "
      + "before it was turned on included.");
    expect(turnOnText(policy("model_params", 0))).toBe(`${policyText(policy("model_params", 0))} From the next `
      + "queue-maintenance run on, it logs the rows that lack model params in each campaign's current plan, jobs that "
      + "finished before it was turned on included.");
    const rows = [checked, member("campaign_2", { state: "other_owner" }), member("campaign_3", { state: "not_ready" })];
    const members = [campaign("campaign_1", acme, { name: "S26 CPU" }), campaign("campaign_2", acme, { name: "Pixel" })];
    // Spread is off, and counts all the same.
    expect(checkLines("spread", rows, members)).toEqual([
      "S26 CPU: 1 flagged: 1 re-run, 0 marked failed, 0 kept", "Pixel: another owner: left as it is",
      "campaign_3: plan being created: not checked yet",
    ]);
    expect(checkLines("swap", [checked], members)).toEqual(["S26 CPU: 1 flagged: 0 re-run, 1 marked failed, 0 kept"]);
    expect(checkLines("model_params", rows.slice(0, 2), members))
      .toEqual(["S26 CPU: 1 job whose rows lack model params", "Pixel: another owner: left as it is"]);
  });

  test("the recent revisions are the members the policies revised, newest first, each plan on its campaign's page", () => {
    const recent = recentFilings([
      member("campaign_1", { last_filing: filing("plan-1", "2026-11-11T09:00:00Z") }),
      member("campaign_2"),
      // Half a second later, though its text sorts first.
      member("campaign_3", { last_filing: filing("plan-3", "2026-11-11T09:00:00.5Z") }),
    ]);
    expect(recent.map((row) => row.campaign_id)).toEqual(["campaign_3", "campaign_1"]);
    const spec = filingsTable({ campaign_1: "S26 CPU" });
    expect(spec.columns.map((column) => column.label)).toEqual(["Campaign", "Plan", "Filed", "Jobs queued"]);
    expect(recent.map((row) => spec.columns.map((column) => column.raw(row)))).toEqual([
      ["campaign_3", "plan-3", "2026-11-11T09:00:00.5Z", 3],
      ["S26 CPU", "plan-1", "2026-11-11T09:00:00Z", 3],
    ]);
    expect(recent.map((row) => spec.columns[1]!.href?.(row)))
      .toEqual(["/app/campaigns/campaign_3/plans/plan-3", "/app/campaigns/campaign_1/plans/plan-1"]);
  });

  test("the rows that lack model params sum each key over the members, in key order", () => {
    const gaps = paramsGaps([
      member("campaign_1", { missing_model_params: [gap("granite-4.2-8b", 3, 2), gap("vault_0195c3e2-aaaa", 1, 1)] }),
      member("campaign_2", { missing_model_params: [gap("granite-4.2-8b", 2, 1)] }),
      member("campaign_3", { missing_model_params: [gap("bolt-1b", 4, 2)] }),
    ]);
    expect(gaps).toEqual([
      { key: "bolt-1b", row_count: 4, job_count: 2, campaign_ids: ["campaign_3"] },
      { key: "granite-4.2-8b", row_count: 5, job_count: 3, campaign_ids: ["campaign_1", "campaign_2"] },
      { key: "vault_0195c3e2-aaaa", row_count: 1, job_count: 1, campaign_ids: ["campaign_1"] },
    ]);
    const spec = gapsTable({ campaign_1: "iPhone 17 Pro", campaign_2: "Pixel" });
    expect(spec.columns.map((column) => column.label)).toEqual(["Key", "Result rows", "Jobs", "Campaigns"]);
    expect(gaps.map((sum) => spec.columns.map((column) => column.raw(sum)))).toEqual([
      ["bolt-1b", 4, 2, "campaign_3"], ["granite-4.2-8b", 5, 3, "iPhone 17 Pro, Pixel"],
      ["vault_0195c3e2-aaaa", 1, 1, "iPhone 17 Pro"],
    ]);
    expect(paramsGaps([member("campaign_1")])).toEqual([]);
  });

  const smaller = (job_id: string, failure_source: SmallerFailure["failure_source"]): SmallerFailure => ({
    job_id, benchmark_id: "decode_throughput_512_100", failure_source, failure_code: "out_of_memory",
    model_key: "lfm2-350m", clean_job_id: "job-clean", clean_benchmark_id: "decode_throughput_1024_100",
  });

  test("the failed jobs below a clean larger run list each member's in member order", () => {
    const found = smallerFailedRows([
      member("campaign_2", { smaller_failed: [smaller("job-2", "client")] }),
      member("campaign_1"),
      member("campaign_3", { smaller_failed: [smaller("job-3", null)] }),
    ]);
    const spec = smallerFailedTable({ campaign_2: "Pixel" });
    expect(spec.columns.map((column) => column.label))
      .toEqual(["Campaign", "Model", "Failed", "Failure", "Failed job", "Clean larger run", "Clean job"]);
    expect(found.map((row) => spec.columns.map((column) => column.raw(row)))).toEqual([
      ["Pixel", "lfm2-350m", "decode_throughput_512_100", "client: out_of_memory", "job-2", "decode_throughput_1024_100",
       "job-clean"],
      ["campaign_3", "lfm2-350m", "decode_throughput_512_100", "out_of_memory", "job-3", "decode_throughput_1024_100",
       "job-clean"],
    ]);
  });

  test("draws the failed jobs below a clean larger run under the policy's state, after the model params gaps", () => {
    const rows = [member("campaign_1", { smaller_failed: [smaller("job-2", "marked")] })];
    const body = render(PolicyPreview, { props: {
      of: read(rows), lacks: [], members: [campaign("campaign_1", acme, { name: "S26 CPU" })],
      policies: [...policies, policy("smaller_failed", 0)], reader: reader(["read_plans", "read_clients"], "org_acme"),
      add: async () => {},
    } }).body;
    expect(body).toMatch(/<h2 id="[^"]+">Missing model params \(off\)<\/h2>[\s\S]*<h2 id="[^"]+">Failed below a clean larger run \(off\)<\/h2>/);
    expect(body).toContain('<a href="/app/campaigns/campaign_1">S26 CPU</a>');
    expect(body).toContain("marked: out_of_memory");
    expect(draw(read(rows))).not.toContain("Failed below a clean larger run");
  });

  test("an entry is added in place with manage_any_orgs, and a vault model's on its resource page by an administrator", () => {
    const keys = ["granite-4.2-8b", "vault_0195c3e2-aaaa"];
    expect(keys.map((key) => paramsWrite(reader(["manage_any_orgs"]), key))).toEqual(["form", "vault"]);
    expect(keys.map((key) => paramsWrite(reader(["manage_orgs"], "org_acme"), key))).toEqual([null, "vault"]);
    expect(keys.map((key) => paramsWrite(reader(["submit_plans", "read_clients"], "org_acme"), key)))
      .toEqual([null, null]);
  });

  const read = (members: MemberPolicyPreview[] | null) => {
    const of = new Loader<CampaignGroupPolicyPreview>();
    of.data = members && { members };
    return of;
  };
  const draw = (of: Loader<CampaignGroupPolicyPreview>, lacks: UserPermission[] = [],
                members = [campaign("campaign_1", acme, { name: "S26 CPU" })],
                who = reader(["read_plans", "read_clients"], "org_acme")) =>
    render(PolicyPreview, { props: { of, lacks, members, policies, reader: who, add: async () => {} } }).body;

  test("draws the revisions and the rows that lack model params, offering each key's write to who may make it", () => {
    const rows = [member("campaign_1", {
      last_filing: filing("plan-1", "2026-11-11T09:00:00Z"),
      missing_model_params: [gap("granite-4.2-8b", 3, 2), gap("vault_0195c3e2-aaaa", 1, 1)],
    })];
    const operator = draw(read(rows), [], undefined, reader(["manage_any_orgs", "read_clients"]));
    expect(operator).toContain('<a class="id" href="/app/campaigns/campaign_1/plans/plan-1">plan-1</a>');
    // The policy is off, and the rows still show.
    expect(operator).toMatch(/<h2 id="[^"]+">Missing model params \(off\)<\/h2>/);
    expect(operator).toMatch(
      /<button type="button" id="[^"]+" aria-label="Add params for granite-4\.2-8b" [^>]*>Add params<\/button>/);
    expect(operator).toContain(
      '<a href="/app/vault/vault_0195c3e2-aaaa" aria-label="Add params for vault_0195c3e2-aaaa">Add params</a>');
    const admin = draw(read(rows), [], undefined, reader(["manage_orgs", "read_plans", "read_clients"], "org_acme"));
    expect(admin).not.toContain('aria-label="Add params for granite-4.2-8b"');
    expect(admin).toContain('aria-label="Add params for vault_0195c3e2-aaaa"');
    expect(draw(read(rows))).not.toContain(">Add params<");
    const quiet = draw(read([member("campaign_1")]));
    expect(quiet).toContain("The policies have revised none of these campaigns.");
    expect(quiet).toContain("No row read lacks model params.");
  });

  test("draws a member a row, linked to its campaign, with an expired deadline marked and what it stops", () => {
    const body = draw(read([checked, member("campaign_2", { state: "other_owner" })]));
    expect(body).toContain('<a href="/app/campaigns/campaign_1">S26 CPU</a>');
    expect(body).toContain("another owner: left as it is");
    expect(body).toMatch(/<span data-state="expired">expired<\/span>(<!--.*?-->)* (<!--.*?-->)*2026-11-09 00:00:00Z/);
    expect(body).toContain("The server refuses a re-run of a plan whose document sets a deadline that has passed.");
    expect(draw(read([member("campaign_1")]))).not.toContain("The server refuses");
  });

  test("says it is loading until read, and why it lists no member", () => {
    expect(draw(read(null))).toContain("Loading…");
    // The route leaves out the members whose plans the reader may not read.
    expect(draw(read([]))).toContain("You may not read the plans of this group's campaigns.");
    expect(draw(read([]), [], [])).toContain("This group holds no campaign.");
  });

  test("is not read for a reader without read_plans or read_clients, and names what it lacks", () => {
    expect(previewLacks(reader(["submit_plans", "read_clients"], "org_acme"))).toEqual([]);
    expect(previewLacks(reader(["submit_plans"], "org_acme"))).toEqual(["read_clients"]);
    expect(previewLacks(reader(["manage_orgs", "read_clients"], "org_acme"))).toEqual(["read_plans"]);
    expect(previewLacks(reader(["manage_orgs"], "org_acme"))).toEqual(["read_plans", "read_clients"]);
    const shown = (lacks: UserPermission[]) => draw(read(null), lacks).replace(/<!--.*?-->/g, "");
    expect(shown(["read_clients"])).toContain("Previewing the policies takes <code>read_clients</code>.");
    const body = shown(["read_plans", "read_clients"]);
    expect(body).toContain("Previewing the policies takes <code>read_plans</code> and <code>read_clients</code>.");
    expect(body).not.toMatch(/<table|Loading/);
  });
});

const GB = 2 ** 30;

const rerun = (kind: PolicyKind, fields: Partial<PolicyRerun> = {}): PolicyRerun => ({
  campaign_id: "campaign_1", kind, rerun_job_id: "job-2", rerun_of: "job-1", plan_id: "plan-2",
  filed_at: "2026-11-11T09:00:00Z", attempt: 1, benchmark_id: "llama-bench-pp512", model_key: "lfm2-1.2b",
  before: 1.5 * GB, threshold: 0, short_repetition_ms: null, short_repetition_percent: null, codes: null,
  before_code: null, after: null, after_code: null, outcome: null, judged_at: null, ...fields,
});

const judged = { judged_at: "2026-11-11T09:10:00Z" };

const summary = (kind: PolicyKind, fields: Partial<PolicyRerunSummary> = {}): PolicyRerunSummary =>
  ({ kind, reruns: 0, waiting: 0, cleared: 0, flagged: 0, failed: 0, dropped: 0, median_before: null, median_after: null,
     ...fields });

describe("a group's re-run results", () => {
  test("a check's value reads as a size for swap, a percent for spread and TTFT, and a failure by its code", () => {
    expect([rerunValue("swap", 1.5 * GB), rerunValue("swap", 0)]).toEqual(["1.5 GB", "0 B"]);
    expect([rerunValue("spread", 12.345), rerunValue("spread", 5)]).toEqual(["±12.3%", "±5%"]);
    expect([rerunValue("ttft_over_e2e", 104.2), rerunValue("ttft_over_e2e", 100)]).toEqual(["104.2%", "100.0%"]);
    // A re-run that failed with a code the policy does not re-run reads 0, and still failed.
    expect([rerunValue("failed", 1, "runtime_error"), rerunValue("failed", 0, "out_of_memory")])
      .toEqual(["failed: runtime_error", "failed: out_of_memory"]);
    // A 0 with no code: the run may still have failed with a code the policy does not re-run.
    expect([1, 0].map((value) => rerunValue("failed", value))).toEqual(["failed", "no failure it re-runs"]);
    expect([rerunValue("swap", null), rerunValue("failed", null)]).toEqual(["none", "none"]);
  });

  test("a re-run reads its value before, and once its rows are read the value after, with its policy as it stood and its outcome", () => {
    expect(changeText(rerun("swap"))).toBe("1.5 GB");
    expect(changeText(rerun("swap", { after: 0, outcome: "cleared", ...judged }))).toBe("1.5 GB → 0 B");
    // Its rows hold no value: a swap re-run that failed outright measured none.
    expect(changeText(rerun("swap", { outcome: "failed", ...judged }))).toBe("1.5 GB → none");
    // Dropped before it had rows, so it has no value after.
    expect(changeText(rerun("swap", { outcome: "dropped", ...judged }))).toBe("1.5 GB");
    const failed = { before: 1, threshold: null, codes: [...CODES], before_code: "runtime_error" };
    expect(changeText(rerun("failed", { ...failed, after: 1, after_code: "device_error", outcome: "flagged", ...judged })))
      .toBe("failed: runtime_error → failed: device_error");
    expect(changeText(rerun("failed", { ...failed, after: 0, outcome: "cleared", ...judged })))
      .toBe("failed: runtime_error → no failure it re-runs");
    expect(changeText(rerun("failed", { ...failed, after: 0, after_code: "out_of_memory", outcome: "failed", ...judged })))
      .toBe("failed: runtime_error → failed: out_of_memory");
    const short = { threshold: 5, short_repetition_ms: 1000, short_repetition_percent: 10 };
    // One set of codes, however its filing ordered them, makes one title.
    const defaults = "Failures by type (unspecified, runtime_error or device_error)";
    expect([rerun("spread", { threshold: 7.5 }), rerun("spread", short), rerun("swap"), rerun("failed", failed),
            rerun("failed", { ...failed, codes: [...CODES].reverse() }),
            rerun("failed", { ...failed, codes: ["harness_error"] })].map(rerunTitle))
      .toEqual(["Stddev over 7.5%", "Stddev over 5% (10% under 1000 ms)", "Swap", defaults, defaults,
                "Failures by type (harness_error)"]);
    expect(([null, "cleared", "flagged", "failed", "dropped"] as const).map((outcome) => outcomeText(rerun("swap", { outcome }))))
      .toEqual(["waiting", "cleared", "still flagged", "failed", "dropped"]);
  });

  test("a policy's summary counts its re-runs by outcome, then the share of those read that cleared and their medians", () => {
    expect(rerunSummaryText(summary("swap", {
      reruns: 12, waiting: 2, dropped: 1, cleared: 6, flagged: 2, failed: 1, median_before: 1.5 * GB, median_after: 0,
    }))).toBe("12 re-runs: 2 waiting, 1 dropped; 6 cleared, 2 still flagged, 1 failed; 66.7% cleared, median 1.5 GB → 0 B");
    expect(rerunSummaryText(summary("ttft_over_e2e", {
      reruns: 3, cleared: 2, flagged: 1, median_before: 104.2, median_after: 98.5,
    }))).toBe("3 re-runs: 0 waiting, 0 dropped; 2 cleared, 1 still flagged, 0 failed; 66.7% cleared, median 104.2% → 98.5%");
    // Failed's medians say no more than its counts.
    expect(rerunSummaryText(summary("failed", {
      reruns: 3, cleared: 1, flagged: 1, failed: 1, median_before: 1, median_after: 0,
    }))).toBe("3 re-runs: 0 waiting, 0 dropped; 1 cleared, 1 still flagged, 1 failed; 33.3% cleared");
    // None read: no share and no median.
    expect(rerunSummaryText(summary("spread", { reruns: 2, waiting: 1, dropped: 1 })))
      .toBe("2 re-runs: 1 waiting, 1 dropped; 0 cleared, 0 still flagged, 0 failed");
    // A failure is no improvement: a swap re-run that failed outright holds no value after.
    expect(rerunSummaryText(summary("swap", { reruns: 1, failed: 1, median_before: 1024 })))
      .toBe("1 re-run: 0 waiting, 0 dropped; 0 cleared, 0 still flagged, 1 failed; 0% cleared, median 1.0 KB → none");
  });

  const rows = [
    rerun("swap", { after: 0, outcome: "cleared", ...judged }),
    rerun("spread", { campaign_id: "campaign_2", rerun_job_id: "job-4", rerun_of: "job-3", attempt: 2, model_key: null,
                      threshold: 7.5, before: 12.345, after: 9.5, outcome: "flagged", ...judged }),
    rerun("failed", { rerun_job_id: "job-6", rerun_of: "job-5", before: 1, threshold: null, codes: ["runtime_error"],
                      before_code: "runtime_error" }),
  ];

  test("the table reads a re-run a row, sorts the change by its value before, and filters by campaign, policy and outcome", () => {
    const spec = rerunsTable({ campaign_1: "S26 CPU" });
    expect(spec.columns.map((column) => column.label)).toEqual([
      "Campaign", "Model key", "Benchmark", "Policy", "Attempt", "Before → after", "Outcome", "Filed", "Judged",
    ]);
    expect(rows.map((row) => spec.columns.map((column) => column.raw(row)))).toEqual([
      ["S26 CPU", "lfm2-1.2b", "llama-bench-pp512", "Swap", 1, 1.5 * GB, "cleared", "2026-11-11T09:00:00Z",
       "2026-11-11T09:10:00Z"],
      ["campaign_2", null, "llama-bench-pp512", "Stddev over 7.5%", 2, 12.345, "still flagged", "2026-11-11T09:00:00Z",
       "2026-11-11T09:10:00Z"],
      ["S26 CPU", "lfm2-1.2b", "llama-bench-pp512", "Failures by type (runtime_error)", 1, 1, "waiting",
       "2026-11-11T09:00:00Z", null],
    ]);
    const shown = (choices: Record<string, string[]>, sort = "") =>
      tableView(spec, { ...unsorted(spec), choices: { ...unsorted(spec).choices, ...choices }, sort }, rows)
        .map((row) => row.rerun_job_id);
    expect(shown({}, "change")).toEqual(["job-6", "job-4", "job-2"]);
    expect(shown({ campaign: ["campaign_1"] })).toEqual(["job-2", "job-6"]);
    expect(shown({ policy: ["Stddev over 7.5%"], outcome: ["still flagged"] })).toEqual(["job-4"]);
    expect(spec.choices?.find((choice) => choice.key === "campaign")?.name?.("campaign_1")).toBe("S26 CPU");
  });

  test("the CSV holds every re-run, its campaign named, then its fields as the route sends them", () => {
    expect(rerunsCsv(rows.slice(1), { campaign_2: "S26, NPU" }).split("\r\n")).toEqual([
      "campaign,campaign_id,kind,rerun_job_id,rerun_of,plan_id,filed_at,attempt,benchmark_id,model_key,before,threshold,"
        + "short_repetition_ms,short_repetition_percent,codes,before_code,after,after_code,outcome,judged_at",
      "\"S26, NPU\",campaign_2,spread,job-4,job-3,plan-2,2026-11-11T09:00:00Z,2,llama-bench-pp512,,12.345,7.5,,,,,9.5,,"
        + "flagged,2026-11-11T09:10:00Z",
      "campaign_1,campaign_1,failed,job-6,job-5,plan-2,2026-11-11T09:00:00Z,1,llama-bench-pp512,lfm2-1.2b,1,,,,"
        + "runtime_error,runtime_error,,,,",
      "",
    ]);
    expect(rerunsCsv([rerun("failed", { codes: ["device_error", "harness_error"] })], {}).split("\r\n")[1])
      .toContain(",device_error harness_error,");
  });

  const policies = [policy("swap", 3, { enabled: true }), policy("spread", 3), policy("failed", 2, { enabled: true })];
  const read = (shown: PolicyRerun[] | null, kinds: PolicyRerunSummary[] = []) => {
    const of = new Loader<CampaignGroupRerunResults>();
    of.data = shown && { summary: kinds, rows: shown, total: shown.length, offset: 0, limit: shown.length };
    return of;
  };
  const draw = (of: Loader<CampaignGroupRerunResults>, lacks: UserPermission[] = [],
                members = [campaign("campaign_1", acme, { name: "S26 CPU" })]) =>
    render(RerunResults, { props: { of, lacks, groupId: "group_1", members, policies } }).body.replace(/<!--.*?-->/g, "");

  test("draws each policy's summary, titled as its re-runs were filed, and the re-runs, each linked to its campaign, to download", () => {
    const body = draw(read(rows, [summary("swap", { reruns: 1, cleared: 1, median_before: 1.5 * GB, median_after: 0 }),
                                  summary("spread", { reruns: 1, flagged: 1, median_before: 12.345, median_after: 9.5 })]));
    expect(body).toMatch(/<h2 id="[^"]+">Re-run results<\/h2>/);
    expect(body).toContain("<dt>Swap</dt><dd>1 re-run: 0 waiting, 0 dropped; 1 cleared, 0 still flagged, 0 failed; 100% "
                           + "cleared, median 1.5 GB → 0 B</dd>");
    // At the percent its re-run was filed at, not the 5% stored, and off now.
    expect(body).toContain("<dt>Stddev over 7.5% (off)</dt><dd>1 re-run: 0 waiting, 0 dropped; 0 cleared, 1 still "
                           + "flagged, 0 failed; 0% cleared, median ±12.3% → ±9.5%</dd>");
    expect(body).toContain('<a href="/app/campaigns/campaign_1">S26 CPU</a>');
    expect(body).toContain('<a href="/app/campaigns/campaign_2">campaign_2</a>');
    expect(body).toContain("1.5 GB → 0 B");
    expect(body).toContain(">Download CSV</button>");
    // Sorted, filtered and paged in the Activity tab's address.
    expect(body).toContain('href="/app/campaign-groups/group_1/activity?sort=');
  });

  test("says it is loading until read, and why it lists no re-run", () => {
    expect(draw(read(null))).toContain("Loading…");
    const none = draw(read([]));
    expect(none).toContain("The policies have filed no re-run in the campaigns whose plans you may read.");
    expect(none).not.toMatch(/<dl|Download CSV/);
    expect(draw(read([]), [], [])).toContain("This group holds no campaign.");
  });

  test("is not read for a reader without the preview's grants, and names what it lacks", () => {
    const body = draw(read(null), ["read_plans", "read_clients"]);
    expect(body).toContain("Reading the re-run results takes <code>read_plans</code> and <code>read_clients</code>.");
    expect(body).not.toMatch(/<table|Loading/);
  });
});
