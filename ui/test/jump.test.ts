import { describe, expect, test } from "vitest";
import { ApiError } from "../src/lib/api";
import type { PlanSummary } from "../src/lib/api-types";
import { NO_JOB, NO_JOB_OR_PLAN, UNKNOWN, classify, jobAddress, resolve, type Lookups } from "../src/lib/jump";

const CAMPAIGN = "campaign_0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b";
const PLAN = "plan-0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b";
const JOB = "job-0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b";
const BARE = "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b";
const CLIENT = `ev1_${"a3".repeat(32)}`;

const summary = (plan_id: string, campaign_id: string | null = null): PlanSummary => ({
  plan_id, campaign_id, cancel_requested: false, counts: null, created_at: "2026-10-29T00:00:00Z", status: "active",
});

/** Reads that answer from `plans` (a job's plans) and `readable` (the plan IDs the reader reads), else `404`. */
const lookups = (plans: Record<string, PlanSummary[]>, readable: string[] = []): Lookups => ({
  jobPlans: async (job_id) => {
    const held = plans[job_id];
    if (!held) throw new ApiError(404, "no readable plan holds this job");
    return held;
  },
  plan: async (plan_id) => {
    if (!readable.includes(plan_id)) throw new ApiError(404, "plan not found");
    return {};
  },
});

describe("the jump box", () => {
  test.each([
    [CAMPAIGN, `/app/campaigns/${CAMPAIGN}`],
    [PLAN, `/app/plans/${PLAN}`],
    [CLIENT, `/app/clients/${CLIENT}`],
    ["vault_0199a1b2", "/app/vault/vault_0199a1b2"],
    ["user_0199a1b2", "/app/users/user_0199a1b2"],
    ["org_0199a1b2", "/app/orgs/org_0199a1b2"],
    [`  ${CAMPAIGN}\n`, `/app/campaigns/${CAMPAIGN}`],
  ])("opens %j at its page without a read", async (text, address) => {
    expect(await resolve(text, lookups({}))).toBe(address);
  });

  test.each([
    [JOB, { kind: "job", job_id: JOB }],
    [BARE, { kind: "job-or-plan", id: BARE }],
    ["mlx-preserve-job", { kind: "job-or-plan", id: "mlx-preserve-job" }],
    ["", { kind: "unknown" }],
    ["campaign_", { kind: "unknown" }],
    ["audit_0199a1b2", { kind: "unknown" }],
    ["two words", { kind: "unknown" }],
    [`/app/plans/${PLAN}`, { kind: "unknown" }],
  ])("reads %j as %j", (text, pasted) => {
    expect(classify(text)).toEqual(pasted);
  });

  test.each([
    ["a campaign's plan", [summary(PLAN, CAMPAIGN)], `/app/plans/${PLAN}/cells?q=${JOB}`],
    ["a plan of no campaign", [summary(PLAN)], `/app/plans/${PLAN}/cells?q=${JOB}`],
    ["the newest of several plans", [summary("plan-old", CAMPAIGN), summary(PLAN)], `/app/plans/${PLAN}/cells?q=${JOB}`],
  ])("opens a job in %s, filtered to it", async (_, plans, address) => {
    expect(jobAddress(JOB, plans)).toBe(address);
    expect(await resolve(JOB, lookups({ [JOB]: plans }))).toBe(address);
  });

  test("opens an unprefixed ID as the plan the reader reads, else as a job", async () => {
    expect(await resolve(BARE, lookups({}, [BARE]))).toBe(`/app/plans/${BARE}`);
    expect(await resolve(BARE, lookups({ [BARE]: [summary(PLAN)] }))).toBe(`/app/plans/${PLAN}/cells?q=${BARE}`);
  });

  test("opens a readable plan without waiting for the job lookup", async () => {
    const look: Lookups = {
      plan: async () => ({}),
      jobPlans: () => new Promise<PlanSummary[]>(() => {}),
    };
    await expect(resolve(BARE, look)).resolves.toBe(`/app/plans/${BARE}`);
  });

  test.each([
    ["garbage", "two words", lookups({}), UNKNOWN],
    ["a job no readable plan holds", JOB, lookups({}), NO_JOB],
    ["a job with no plans", JOB, lookups({ [JOB]: [] }), NO_JOB],
    ["an unprefixed ID nothing answers for", BARE, lookups({}), NO_JOB_OR_PLAN],
  ])("refuses %s", async (_, text, look, reason) => {
    await expect(resolve(text, look)).rejects.toThrow(reason);
  });

  test("passes on a failure other than a 404", async () => {
    const down: Lookups = {
      jobPlans: async () => { throw new ApiError(500, "store failed"); },
      plan: async () => { throw new ApiError(404, "plan not found"); },
    };
    await expect(resolve(JOB, down)).rejects.toThrow("500: store failed");
    await expect(resolve(BARE, down)).rejects.toThrow("500: store failed");
  });
});
