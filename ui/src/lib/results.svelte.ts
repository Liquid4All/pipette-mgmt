// A plan's jobs as one screen of its tabs shares them
// (`GET /api/v1/plans/{id}/jobs?results=true&all=true`): the Overview's cells, the
// Jobs tab's rows, and each scored job's rows, which the Reports index. Read once and kept while the
// reader moves between the tabs, dropped with the screen, read again after a
// write.

import { planJobsWithResults, type PlanJobsWithResults } from "./api";
import { Cells } from "./cells";
import { Loader } from "./components/work.svelte";
import { index, type ReportIndex } from "./report";
import { session } from "./session.svelte";
import type { FleetDetail, ResultRow, ResultsRead } from "./api-types";

/** Every row of a plan's results, indexed; a class, so the Loader's `$state` does not proxy the rows. */
export class Results {
  readonly view: { fleet_detail: FleetDetail; jobs_reported: number; total: number; results: ResultRow[] };
  readonly index: ReportIndex;
  /** The read's names: a vault resource's id to its record's name. */
  readonly names: Record<string, string>;

  constructor({ jobs, fleet_detail, names, descriptor_names }:
    Pick<PlanJobsWithResults, "jobs" | "fleet_detail" | "names" | "descriptor_names">) {
    const results = jobs.flatMap(({ result }) => (result.status === "scored" ? result.rows : []));
    const reported = jobs.filter(({ result }) => result.status === "scored").length;
    this.view = { fleet_detail, results, total: results.length, jobs_reported: reported };
    this.names = names ?? {};
    this.index = index(results, fleet_detail, this.names, descriptor_names ?? {});
  }
}

/** One read of every job: the Overview's cells, and the results where the server read them. */
class Read {
  readonly jobs: PlanJobsWithResults;
  readonly cells: Cells;
  readonly status: ResultsRead;
  readonly results: Results | null;

  constructor(body: PlanJobsWithResults) {
    this.jobs = body;
    this.cells = new Cells(body);
    this.status = body.results_read;
    this.results = body.results_read === "ok" ? new Results(body) : null;
  }
}

/**
 * What a plan's screen has of its results; `absent` where the server reads none (no `[duckdb]`),
 * `unavailable` where pipette-duckdb failed or outlasted the read.
 */
export type Answer =
  | { state: "loading" }
  | { state: "absent" }
  | { state: "unavailable"; status: "timed_out" | "failed" }
  | { state: "failed"; error: string }
  | { state: "read"; results: Results };

/** One screen's read of its plan's jobs. */
export class PlanRead {
  #pending = $state(0);
  readonly #read = new Loader<Read>();

  /** Read `planId`'s jobs, what is drawn kept until they come. */
  read(planId: string) {
    this.#pending++;
    void this.#read.run(async () => new Read(await planJobsWithResults(session.signer, planId, "all=true")), true)
      .finally(() => this.#pending--);
  }

  /** A read is on its way, over what is drawn. */
  get reading(): boolean {
    return this.#pending > 0;
  }

  /** Every job, and why there are none yet. */
  get jobs(): { data: PlanJobsWithResults | null; error: string } {
    return { data: this.#read.data?.jobs ?? null, error: this.#read.error };
  }

  /** The cells, and why there are none yet. */
  get cells(): { data: Cells | null; error: string } {
    return { data: this.#read.data?.cells ?? null, error: this.#read.error };
  }

  get answer(): Answer {
    if (this.#read.error) return { state: "failed", error: this.#read.error };
    const read = this.#read.data;
    if (!read) return { state: "loading" };
    if (read.results) return { state: "read", results: read.results };
    if (read.status === "timed_out" || read.status === "failed") return { state: "unavailable", status: read.status };
    return { state: "absent" };
  }
}
