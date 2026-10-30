// The re-run and recalculation controls of a plan's page (the classic
// `plan_body.html` and `console-rerun.js`), over `POST /api/v1/campaigns/{id}/rerun`
// and `…/recalculate`.

import { ApiError, type PlanJobWithResult } from "./api";
import { describe } from "./components/guard";
import { PHRASES } from "./danger";
import { cellsWith } from "./jobs";
import type {
  PlanJob, PlanStatus, RecalculateJobsRequest, RerunJobsRequest, ResultsRead,
} from "./api-types";

/** One filing a control asks for: its route, and the body it sends. */
export type Filing =
  | { verb: "rerun"; body: RerunJobsRequest }
  | { verb: "recalculate"; body: RecalculateJobsRequest };

/** What every filing does besides its own jobs, said in each confirmation. */
export const REVISION =
  "A revision of this plan goes into its campaign with every other job carried over, and this plan is cancelled.";

/** What the status line calls a filing, as the classic form's `data-rerun`. */
export const filingName = (filing: Filing): string => (filing.verb === "rerun" ? "re-run" : "recalculation");

/** The controls a row offers: a re-run on an answered row, a recalculation where the jobs read says so. */
export const rowOffers = (job: Pick<PlanJobWithResult, "state" | "result">) => ({
  rerun: job.state === "finished" || job.state === "failed",
  recalculate: job.result.status === "scored" && job.result.recalculable,
});

/** The jobs of `lines` a re-run takes, each once, in the lines' order. */
export function rerunIds(lines: readonly { job: Pick<PlanJobWithResult, "job_id" | "state" | "result"> }[]): string[] {
  const ids = new Set<string>();
  for (const { job } of lines) if (rowOffers(job).rerun) ids.add(job.job_id);
  return [...ids];
}

/** The box over a listing's `offered` jobs: checked where every one is checked, mixed where only some are. */
export function boxOver(offered: readonly string[], checked: ReadonlySet<string>): { checked: boolean; mixed: boolean } {
  const n = offered.filter((id) => checked.has(id)).length;
  return { checked: n > 0 && n === offered.length, mixed: n > 0 && n < offered.length };
}

/**
 * What a re-run of the checked `ids` asks: one job by its id, more by their count. Every one of the plan's
 * `answered` jobs is the danger zone's re-run, and takes its phrase.
 */
export function rerunQuestion(ids: readonly string[], answered: number):
  { title: string; action: string; text: string; phrase?: string } {
  if (ids.length === 1) {
    return { title: `Re-run ${ids[0]}?`, action: "Re-run", text: `This job runs again on a device. ${REVISION}` };
  }
  return {
    title: `Re-run ${ids.length} jobs?`, action: `Re-run ${ids.length} jobs`,
    text: `Each of the ${ids.length} checked jobs runs again on a device. ${REVISION}`,
    ...(ids.length === answered ? { phrase: PHRASES.all } : {}),
  };
}

/**
 * How many jobs read `failed`, or `null` where that may miss a recorded failure
 * `answered: failed` would take: one pipette-duckdb has not republished reads
 * `unknown`, or `cancelled` on a plan torn down. Without pipette-duckdb the
 * states come from the records themselves.
 */
export function failedCount(jobs: Pick<PlanJob, "state">[], results: ResultsRead,
                            status: PlanStatus): number | null {
  const tornDown = status === "complete" || status === "cancelling" || status === "cancelled";
  const settled = results === "off"
    || (results === "ok" && !tornDown && !jobs.some((job) => job.state === "unknown"));
  return settled ? jobs.filter((job) => job.state === "failed").length : null;
}

/** The status line while a filing waits, in whole seconds. */
export const filingStatus = (what: string, elapsedMs: number): string =>
  `Filing the ${what}… ${Math.round(elapsedMs / 1000)}s`;

/**
 * What a failed filing says. A `4xx` refuses before anything takes effect; a
 * `5xx`, or no answer at all, may come after the revision was filed.
 */
export function filingFailure(what: string, e: unknown): string {
  if (e instanceof ApiError && e.status >= 400 && e.status < 500) return `The ${what} was refused: ${e.message}`;
  const maybe = `Reload the campaign to see whether the ${what} was filed.`;
  if (e instanceof ApiError) return `The server failed (${e.message}). ${maybe}`;
  return `No answer from the server (${describe(e)}). ${maybe}`;
}

/** A recalculated or re-run row's parent: its plan's Jobs tab searched for its id, on `campaign`'s pages. */
export const parentLink = (campaign: string | null, plan_id: string, job_id: string): string =>
  cellsWith({ plan_id, campaign_id: campaign }, { q: job_id });
