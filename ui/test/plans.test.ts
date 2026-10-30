import { describe, expect, test } from "vitest";
import {
  MAX_DRAWN_REASON_BYTES, cancelOffered, changeText, planWrites, describeClauses, documentNote, expiresText, failedText, filedEmpty, filedQuery,
  keepTail,
  doneShare, leaseText, measured, parametersText, planFilters, planQuery, planTable, resultsFilename, shownStatus, statusChoices,
  starvedLine, takenScope, withheldNotice,
} from "../src/lib/plans";
import { PickedColumns } from "../src/lib/picked.svelte";
import type { Counts, OwnUserView, PlanView, UserPermission } from "../src/lib/api-types";
import { abilitiesOf } from "./abilities";
import { drawnKeys, stubStorage } from "./picks";

const reader = (permissions: UserPermission[]): OwnUserView => ({
  user_id: "user_me", contact_email: "me@example.test", account_type: "human", status: "active", created_at: "2026-10-01T00:00:00Z",
  updated_at: "2026-10-01T00:00:00Z", permissions, keys: [], may: abilitiesOf(permissions),
}) as OwnUserView;

const none = { scope: "", status: "", submitted_by: "", campaign_id: "" };

describe("the plan listing", () => {
  test("asks newest first with the submitters named, carrying only the filters set", () => {
    expect(planQuery(none, null, "")).toBe("order=newest&names=true");
    expect(planQuery({ ...none, status: "active", campaign_id: "campaign_1" }, "plan-9", "5"))
      .toBe("order=newest&names=true&campaign_id=campaign_1&status=active&limit=5&cursor=plan-9");
    expect(filedQuery("user_x", null, "")).toBe("order=newest&names=true&submitted_by=user_x");
  });

  test("reads its filters off the address, an absent one empty", () => {
    expect(planFilters(new URLSearchParams("scope=mine&submitted_by=user_x"))).toEqual(
      { scope: "mine", status: "", submitted_by: "user_x", campaign_id: "" });
  });

  test("echoes the scope it was taken at, as PlanScope::label", () => {
    expect(takenScope(reader(["read_any_plans"]), "")).toBe("all");
    expect(takenScope(reader(["read_any_plans"]), "mine")).toBe("mine");
    expect(takenScope(reader(["read_plans"]), "")).toBe("mine");
    expect(takenScope(reader(["read_plans"]), "all")).toBe("all");
  });

  const counted = (failed: number): Counts => ({ total: 4, finished: 2, failed, running: 0, available: 0, cancelled: 0 });

  test("a complete plan says where any of its jobs failed, and no other status does", () => {
    expect(shownStatus("complete", counted(2))).toBe("complete_with_failures");
    expect(shownStatus("complete", counted(0))).toBe("complete");
    expect(shownStatus("complete", null)).toBe("complete");
    expect(shownStatus("active", counted(2))).toBe("active");
    expect(shownStatus("cancelled", counted(2))).toBe("cancelled");
  });

  test("a failed job counts toward progress, and the status column and its filter read the drawn status", () => {
    expect(doneShare(counted(2))).toBe(1);
    expect(doneShare({ ...counted(0), total: 0 })).toBeNull();
    expect(doneShare(null)).toBeNull();
    const plan = { status: "complete", progress_snapshot: { counts: counted(1) } } as PlanView;
    const spec = planTable({}, false);
    const raw = (key: string) => spec.columns.find((column) => column.key === key)?.raw(plan);
    expect([raw("status"), raw("progress")]).toEqual(["complete_with_failures", 0.75]);
    // `complete` keeps it too: a filter on complete plans does not lose those with failures.
    expect(spec.choices?.find((choice) => choice.key === "status")?.values(plan))
      .toEqual(["complete", "complete_with_failures"]);
    expect(statusChoices("complete", counted(0))).toEqual(["complete"]);
    expect(statusChoices("active", counted(2))).toEqual(["active"]);
  });

  test("a pick on a listing without submitted_by leaves that column drawn where it is, unless let go of there", () => {
    stubStorage();
    const KEY = "pipette.plans.columns";
    const without = planTable({}, false).columns;
    const named = planTable({}, true).columns;
    new PickedColumns(KEY, without).pick("note", false);
    expect(drawnKeys(KEY, named)).toEqual(["plan_id", "status", "created_at", "submitted_by", "progress"]);
    new PickedColumns(KEY, named).pick("submitted_by", false);
    new PickedColumns(KEY, without).pick("terminal_at", true);
    expect(drawnKeys(KEY, named)).toEqual(["plan_id", "status", "created_at", "progress", "terminal_at"]);
  });

  test("an account's empty plans say which answer it is", () => {
    expect(filedEmpty("mine")).toBe(
      "No plans that this account filed into a campaign you may see. Its other plans need read_any_plans.");
    expect(filedEmpty("all")).toBe("No plans filed by this account.");
  });
});

describe("the plan page", () => {
  // The same sentences as `RevisionChange`'s `Display` in pipette-plan-types.
  test("a revision's change reads as plans status prints it", () => {
    expect(changeText({ type: "rerun", job_count: 1 })).toBe("Re-run of 1 job.");
    expect(changeText({ type: "rerun", answered: "failed", job_count: 12 })).toBe("Re-run of all failed jobs (12).");
    expect(changeText({ type: "rerun", answered: "all", job_count: 3 })).toBe("Re-run of all finished or failed jobs (3).");
    expect(changeText({ type: "recalculate", job_count: 2 })).toBe("Recalculation of 2 evals.");
    expect(changeText({ type: "recalculate", all_evals: true, job_count: 5 }))
      .toBe("Recalculation of all finished evals (5).");
    expect(changeText({ type: "document" })).toBe("Document revised.");
    expect(changeText({ type: "revert", to_plan_id: "plan-a" })).toBe("Reverted to plan-a.");
    expect(changeText({
      type: "variants", removed_variant_count: 1, added_variant_count: 2, inherit_from: ["plan-a"],
      restored: [{ plan_id: "plan-a", variant_index: 2 }, { plan_id: "plan-a", variant_index: 0 }],
    })).toBe("Variant edit: 1 removed, 2 added. Restored variant 2 of plan-a. Restored variant 0 of plan-a. "
      + "Inherits runs of plan-a.");
    expect(changeText({
      type: "variants", removed_variant_count: 1, added_variant_count: 0, carry_earlier: true,
      inherit_from: ["plan-a", "plan-b"],
    }))
      .toBe("Variant edit: 1 removed, 0 added. Carries runs of earlier plans. Inherits runs of plan-a, plan-b.");
  });

  test("a reason keeps its last bytes behind an ellipsis, on a character boundary", () => {
    expect(keepTail("short")).toBe("short");
    expect(keepTail("abcdefghij", 7)).toBe("…ghij");
    // `é` is two bytes: a cut through it moves on to the next character.
    expect(keepTail("aaéééé", 8)).toBe("…éé");
    const long = "x".repeat(MAX_DRAWN_REASON_BYTES + 10);
    expect(new TextEncoder().encode(keepTail(long)).length).toBe(MAX_DRAWN_REASON_BYTES);
  });

  test("a starved group reads as plans status prints it, its clauses withheld below the full tier", () => {
    const group = { job_ids: ["job-1", "job-2"], requires: ["os:macos"], any_of: [["gpu:m3", "gpu:m4"]],
                    excludes: ["arch:x86"], clients: [] };
    expect(describeClauses(group))
      .toBe("requiring os:macos excluding flags arch:x86 with any-of [gpu:m3, gpu:m4]");
    expect(describeClauses({ job_ids: [] })).toBe("with no eligibility");
    expect(starvedLine(group, "full")).toBe("2 job(s) requiring os:macos excluding flags arch:x86 with any-of [gpu:m3, gpu:m4]");
    expect(starvedLine({ job_ids: ["job-1"] }, "withheld")).toBe("1 job(s) requirement set withheld");
  });

  test("the document draws its text, a note or nothing", () => {
    expect(documentNote("present", "full")).toBeNull();
    expect(documentNote("present", "withheld")?.text).toBe(
      "The plan document is withheld: reading it needs read_clients or read_any_clients.");
    expect(documentNote("collected", "full")).toEqual({ text: "The plan document was collected.", fault: false });
    expect(documentNote("mismatch", "full")).toEqual(
      { text: "The stored plan document does not match its digest.", fault: true });
    expect(documentNote("none", "withheld")).toBeNull();
  });

  test("the withheld notice names devices and failure reasons only where results are read", () => {
    expect(withheldNotice(false)).toBe(
      "Fleet detail is withheld: reading clause sets, lease holders needs read_clients or read_any_clients.");
    expect(withheldNotice(true)).toContain("lease holders, devices and failure reasons needs");
  });

  test("a cancel is offered on a live plan to a holder of its grant alone", () => {
    expect(cancelOffered("active", true)).toBe(true);
    expect(cancelOffered("active", false)).toBe(false);
    for (const status of ["complete", "cancelled", "cancelling"] as const) expect(cancelOffered(status, true)).toBe(false);
  });

  test("Settings have a write for the reader: its note, a re-run, or a cancel not yet requested", () => {
    const active = { status: "active", cancel_requested: false } as const;
    const none = { rerun_campaign_id: null, may_cancel: false, may_edit_note: false };
    expect(planWrites(active, { ...none, rerun_campaign_id: "cmp_1" })).toBe(true);
    expect(planWrites(active, { ...none, may_cancel: true })).toBe(true);
    expect(planWrites(active, none)).toBe(false);
    expect(planWrites({ ...active, cancel_requested: true }, { ...none, may_cancel: true })).toBe(false);
    for (const status of ["complete", "cancelled", "cancelling"] as const) {
      expect(planWrites({ status, cancel_requested: false }, { ...none, may_cancel: true })).toBe(false);
      expect(planWrites({ status, cancel_requested: false }, { ...none, may_edit_note: true })).toBe(true);
    }
  });

  test("a job's cells read as the classic table draws them", () => {
    const now = Date.parse("2026-04-09T12:00:00Z");
    const at = (iso: string) => `at ${iso}`;
    expect(expiresText({ expires_at: "2026-04-09T11:59:59Z" }, at, now)).toBe("at 2026-04-09T11:59:59Z (expired)");
    // Claimable through its deadline's second, as the queue sweep expires it.
    expect(expiresText({ expires_at: "2026-04-09T12:00:00Z" }, at, now)).toBe("at 2026-04-09T12:00:00Z");
    expect(expiresText({ expires_at: null }, at, now)).toBe("never");
    expect(leaseText({ expires_at: "2026-04-09T12:00:00Z" }, at, now)).toBe("until at 2026-04-09T12:00:00Z (lapsed)");
    expect(leaseText({ expires_at: "2026-04-09T12:00:01Z" }, at, now)).toBe("until at 2026-04-09T12:00:01Z");
    expect(leaseText(null, at)).toBe("—");
  });
});

describe("the results", () => {
  test("a value carries its unit, then its stddev in the unit it shows in and relative to it", () => {
    expect(measured(0.40833333, null, "ratio")).toBe("40.83%");
    expect(measured(0.408, 0.0123, "ratio")).toBe("40.80% ±1.23 pp (±3%)");
    expect(measured(4385.673, 7.8393607, "tokens/sec")).toBe("4,385.67 tokens/sec ±7.84 tokens/sec (±0.2%)");
    expect(measured(58.371887, 0.104339354, "ms")).toBe("0.058 secs ±0.000104 secs (±0.2%)");
    expect(measured(1_572_864_000, null, "bytes")).toBe("1,500 MB");
    expect(measured(812.555, 3.25, "tokens")).toBe("812.56 tokens ±3.25 tokens (±0.4%)");
    expect(measured(0, 1, "x")).toBe("0 x ±1 x");
    expect(measured(2_000_000, 1_572_864, "bytes")).toBe("2 MB ±1.5 MB (±78.6%)");
    expect(measured(5, 0, "ms")).toBe("0.005 secs ±0 secs (±0%)");
    expect(measured(41, null, "")).toBe("41");
    expect(measured(null, null, "ms")).toBe("—");
  });

  test("parameters read name=value, a string unquoted", () => {
    expect(parametersText({ prefill_tokens: 256, mode: "fast", flags: [1] })).toBe(
      "prefill_tokens=256, mode=fast, flags=[1]");
    expect(parametersText({ sizes: [1, 2], shape: { a: 1 }, seed: null })).toBe(
      'sizes=[1,2], shape={"a":1}, seed=null');
    expect(parametersText({})).toBe("");
  });

  test.each([
    ["lease_lost", "failed: retired for losing its leases"],
    ["client", "failed: reported by its client"],
    ["all_clients_denied", "failed: every client denied it"],
    ["expired", "failed: expired"],
    ["unmatchable", "failed: no client can run it"],
    [null, "failed"],
  ] as const)("a %s failure names the pass that declared it", (source, label) => {
    expect(failedText(source)).toBe(label);
  });

  test.each([
    ["attachment; filename=\"Nightly-plan-3.csv\"; filename*=UTF-8''Nightly-plan-3.csv", "Nightly-plan-3.csv"],
    ["attachment; filename=\"N_chtlich-plan-3.csv\"; filename*=UTF-8''N%C3%A4chtlich-plan-3.csv", "Nächtlich-plan-3.csv"],
    ["attachment; filename=\"100% done-plan-3.csv\"; filename*=UTF-8''100%25%20done-plan-3.csv", "100% done-plan-3.csv"],
    ["attachment; filename=\"bad-plan-3.csv\"; filename*=UTF-8''%E0%A4-plan-3.csv", "plan-1-results.csv"],
    ["attachment", "plan-1-results.csv"],
    [null, "plan-1-results.csv"],
  ] as const)("the file takes the name in %j", (disposition, name) => {
    expect(resultsFilename("plan-1", disposition)).toBe(name);
  });
});
