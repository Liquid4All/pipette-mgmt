// The header's jump box: the record a pasted ID names, and the address of its
// page. The reads arrive as arguments, so vitest imports it.

import { ApiError } from "./api";
import type { PlanSummary } from "./api-types";
import { parentLink } from "./rerun";
import { pathOf, type Route } from "./routes";

/** What a pasted ID names, by its form alone. */
export type Pasted =
  | { kind: "page"; route: Route }
  | { kind: "job"; job_id: string }
  | { kind: "job-or-plan"; id: string }
  | { kind: "unknown" };

/** The ID prefixes the server mints (`pipette-plan-types` `ids.rs`, `principal.rs`), each with its page. */
const PREFIXED: [string, (id: string) => Route][] = [
  ["campaign_", (campaign_id) => ({ page: "campaign", campaign_id })],
  ["plan-", (plan_id) => ({ page: "plan", plan_id })],
  ["ev1_", (client_id) => ({ page: "client", client_id })],
  ["vault_", (resource_id) => ({ page: "vault-resource", resource_id })],
  ["user_", (user_id) => ({ page: "user", user_id })],
  ["org_", (org) => ({ page: "org", org })],
];

/**
 * What `text` names. A job or plan ID may lack its mint prefix (a bare UUID,
 * or an ID a client chose), so such a string names either one.
 */
export function classify(text: string): Pasted {
  const id = text.trim();
  if (!/^[A-Za-z0-9_-]+$/.test(id)) return { kind: "unknown" };
  if (id.startsWith("job-")) return { kind: "job", job_id: id };
  const prefixed = PREFIXED.find(([prefix]) => id.startsWith(prefix) && id.length > prefix.length);
  if (prefixed) return { kind: "page", route: prefixed[1](id) };
  // Job and plan IDs hold letters, digits, and `-` only.
  if (!id.includes("_")) return { kind: "job-or-plan", id };
  return { kind: "unknown" };
}

/** The Jobs tab of the newest of `plans` (oldest first), filtered to `job_id`; `null` for no plan. */
export function jobAddress(job_id: string, plans: PlanSummary[]): string | null {
  const newest = plans.at(-1);
  if (!newest) return null;
  return parentLink(null, newest.plan_id, job_id);
}

/** The reads that resolve a job ID or an unprefixed one. */
export interface Lookups {
  /** The plans the reader reads that hold the job, oldest first. */
  jobPlans: (job_id: string) => Promise<PlanSummary[]>;
  /** Answers if the reader reads the plan. */
  plan: (plan_id: string) => Promise<unknown>;
}

export const UNKNOWN = "Paste the ID of a job, plan, campaign, device, vault resource, user, or organization.";
export const NO_JOB = "No plan that you can read holds this job.";
export const NO_JOB_OR_PLAN = "No job or plan that you can read has this ID.";

const missing = (e: unknown) => e instanceof ApiError && e.status === 404;

/** The address of the page `text` names; a refusal throws, with its reason as the message. */
export async function resolve(text: string, look: Lookups): Promise<string> {
  const pasted = classify(text);
  switch (pasted.kind) {
    case "unknown":
      throw new Error(UNKNOWN);
    case "page":
      return pathOf(pasted.route);
    case "job": {
      const plans = await look.jobPlans(pasted.job_id).catch((e: unknown) => {
        if (missing(e)) return [];
        throw e;
      });
      const address = jobAddress(pasted.job_id, plans);
      if (!address) throw new Error(NO_JOB);
      return address;
    }
    case "job-or-plan": {
      const job = Promise.allSettled([look.jobPlans(pasted.id)]).then(([result]) => result);
      const [plan] = await Promise.allSettled([look.plan(pasted.id)]);
      if (plan.status === "fulfilled") return pathOf({ page: "plan", plan_id: pasted.id });
      const jobResult = await job;
      const address = jobResult.status === "fulfilled" ? jobAddress(pasted.id, jobResult.value) : null;
      if (address) return address;
      const failed = [plan, jobResult].find((read) => read.status === "rejected" && !missing(read.reason));
      if (failed?.status === "rejected") throw failed.reason;
      throw new Error(NO_JOB_OR_PLAN);
    }
  }
}
