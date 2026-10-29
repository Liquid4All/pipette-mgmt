// The state of a plan page's re-run and recalculation controls.

import { recalculateJobs, rerunJobs } from "./api";
import { filingFailure, filingName, filingStatus, type Filing } from "./rerun";
import { here, navigate } from "./route.svelte";
import { screenOf } from "./routes";
import { session } from "./session.svelte";
import { tabLink } from "./tabs";

/**
 * A re-run or recalculation being filed, which takes seconds: every such
 * control locks, and the status line counts the wait (`console-rerun.js`).
 */
export class Filer {
  busy = $state(false);
  /** The control that is filing, which reads "Filing…". */
  which = $state("");
  status = $state("");
  failed = $state(false);

  /** Run `work`, filing the `what` asked by the control `which`. */
  async file(which: string, what: string, work: () => Promise<void>) {
    this.busy = true;
    this.which = which;
    this.failed = false;
    const started = Date.now();
    const tick = () => { this.status = filingStatus(what, Date.now() - started); };
    tick();
    const timer = setInterval(tick, 1000);
    try {
      await work();
    } catch (e) {
      this.status = filingFailure(what, e);
      this.failed = true;
    } finally {
      clearInterval(timer);
      this.busy = false;
      this.which = "";
    }
  }

  /**
   * File `filing` off `planId` into `campaign`, then open the revision's Jobs
   * under its campaign as the classic redirect did. Resolves to why it failed, or `null`.
   */
  async revise(which: string, campaign: string, planId: string, filing: Filing,
               reread: () => Promise<void>): Promise<string | null> {
    const from = here.path;
    await this.file(which, filingName(filing), async () => {
      const revised = filing.verb === "rerun"
        ? await rerunJobs(session.signer, campaign, planId, filing.body)
        : await recalculateJobs(session.signer, campaign, planId, filing.body);
      if (here.path === from) navigate(tabLink({ plan_id: revised.plan.plan_id, campaign_id: campaign }, "cells"));
      // A reader moved to another of this plan's tabs, which drop this one, stays and sees it cancelled.
      else if (screenOf(here.path) === screenOf(from)) await reread();
    });
    return this.failed ? this.status : null;
  }
}
