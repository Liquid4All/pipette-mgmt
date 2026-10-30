// A page's reads and writes: what it is waiting on, and what failed.

import { untrack } from "svelte";
import { ApiError } from "../api";
import { validationScheduler } from "../document";
import { reveal } from "../route.svelte";
import type { PlanValidation } from "../api-types";
import { describe, latest } from "./guard";

/** One read a page draws from, read again when its address changes. */
export class Loader<T> {
  data = $state<T | null>(null);
  error = $state("");
  /** The refusal's HTTP status; `null` for a failure the server did not answer. */
  status = $state<number | null>(null);
  #start = latest();

  /** Read with `read`; `keep` leaves the drawn data up until the answer replaces it. */
  async run(read: () => Promise<T>, keep = false) {
    const mine = this.#start();
    if (!keep) this.data = null;
    this.error = "";
    this.status = null;
    try {
      const answered = await read();
      if (mine()) this.data = answered;
    } catch (e) {
      if (mine()) {
        this.error = describe(e);
        this.status = e instanceof ApiError ? e.status : null;
      }
    }
    if (mine()) await reveal();
  }
}

/** A write in progress, which marks its buttons busy, and the last one's error. */
export class Busy {
  busy = $state(false);
  error = $state("");

  /** Run `work`; a failure is shown prefixed with `what` when it names a row. */
  async attempt(work: () => Promise<void>, what = "") {
    this.busy = true;
    this.error = "";
    try {
      await work();
    } catch (e) {
      this.error = what ? `${what}: ${describe(e)}` : describe(e);
    } finally {
      this.busy = false;
    }
  }
}

/** A plan document checked as it is typed (`document.ts`'s scheduler), and the last answer. */
export class DocumentValidator {
  verdict = $state.raw<PlanValidation | null>(null);
  /** The text the verdict or error answers. */
  on = $state<string | null>(null);
  pending = $state(false);
  error = $state("");
  #checks;

  constructor(ask: (text: string, signal: AbortSignal) => Promise<PlanValidation>) {
    this.#checks = validationScheduler({
      ask,
      onChange: (state) => {
        this.verdict = state.verdict;
        this.on = state.on;
        this.pending = state.pending;
        this.error = state.error;
      },
    });
  }

  isFor = (text: string) => this.on === text;
  /** Called from an effect, which must not come to depend on what it writes. */
  schedule = (text: string) => untrack(() => this.#checks.schedule(text));
  now = (text: string) => this.#checks.now(text);
  clear = () => untrack(() => this.#checks.cancel());
  stop = () => this.#checks.stop();
}
