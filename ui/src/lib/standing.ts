// What a plan's Overview header reads off it: how fresh its count is, what
// blocks it, and when its jobs expire. Pure, so vitest imports it;
// `PlanHead.svelte` draws it.

import { relative, when } from "./format";
import type { JobExpiry, PlanJob, PlanView, StarvedGroup } from "./api-types";

/** How old queue-maintenance's count may grow before the header says it may have stopped: ten missed passes. */
export const STALE_MS = 10 * 60_000;

/**
 * When queue-maintenance last counted the plan, and whether that is too long
 * before the plan was read; null where no count is due, as creating or done.
 */
export function counted(plan: Pick<PlanView, "status" | "progress_snapshot" | "created_at">,
                        seen: number): { at: string | null; stale: boolean } | null {
  if (plan.status === "creating" || plan.status === "complete" || plan.status === "cancelled") return null;
  const at = plan.progress_snapshot?.computed_at ?? null;
  return { at, stale: seen - Date.parse(at ?? plan.created_at) > STALE_MS };
}

/**
 * The starved groups of the last count, cut to the jobs the live read still has
 * queued, as starvation is counted over queued jobs alone; none where the plan
 * no longer waits on a device.
 */
export function starvedNow(plan: Pick<PlanView, "status" | "cancel_requested" | "progress_snapshot">,
                           jobs: readonly Pick<PlanJob, "job_id" | "state">[] | null): StarvedGroup[] {
  if ((plan.status !== "active" && plan.status !== "pending_clients") || plan.cancel_requested) return [];
  const groups = plan.progress_snapshot?.starved_groups ?? [];
  if (!jobs) return groups;
  const queued = new Set(jobs.filter((job) => job.state === "available").map((job) => job.job_id));
  return groups.map((group) => ({ ...group, job_ids: group.job_ids.filter((id) => queued.has(id)) }))
    .filter((group) => group.job_ids.length);
}

/** When the plan's jobs stop being handed out, against `now`: one time, a range, or never; the exact ends as its title. */
export function expiryText(expiry: JobExpiry, now: number): { text: string; title: string } {
  const read = (at: string | null) => (at === null ? { text: "never", title: "never" }
    : { text: relative(Date.parse(at), now), title: when(at) });
  const [first, last] = [read(expiry.earliest_expires_at), read(expiry.latest_expires_at)];
  if (expiry.earliest_expires_at === expiry.latest_expires_at) return first;
  // Ends a few hours apart read alike at a day's grain: once, the exact range in the title.
  const text = first.text === last.text ? first.text : `${first.text} – ${last.text}`;
  return { text, title: `${first.title} → ${last.title}` };
}
