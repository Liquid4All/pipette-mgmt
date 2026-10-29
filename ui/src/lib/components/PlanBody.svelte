<script lang="ts">
  // One plan's Jobs tab (`plan_body.html`'s jobs): a line per result row of its
  // jobs (`jobs.ts`), filtered here and listed by `JobLines`, on the plan's own
  // pages and its campaign's, whose links it keeps. Writes over the whole plan
  // are Settings'.
  import { readFailure } from "../plans";
  import { cellsLink, columnsOf, deviceTypeNames, deviceTypes, filtering, jobChoices, jobFilters, keeps, linesOf, listedText } from "../jobs";
  import { offsetPaging } from "../pager";
  import { here } from "../route.svelte";
  import type { TabBase } from "../tabs";
  import type { PlanJobsWithResults } from "../api";
  import type { NamedPlanView } from "../api-types";
  import CellFilters from "./CellFilters.svelte";
  import JobLines from "./JobLines.svelte";
  import { Busy } from "./work.svelte";

  let { base, plan, jobs, onchange }: {
    /** The plan, and the campaign whose pages this is drawn on. */
    base: TabBase;
    plan: NamedPlanView;
    /** Every job of the plan, which the screen reads once for all its tabs. */
    jobs: PlanJobsWithResults;
    /** Read the plan again, after a write. */
    onchange: () => Promise<void>;
  } = $props();

  const names = $derived({ ...plan.names, ...jobs.names });
  const drawn = $derived(jobs.results_read === "ok");
  const work = new Busy();

  const lines = $derived(linesOf(jobs.jobs));
  const ctx = $derived({ names, types: deviceTypes(lines), typeNames: deviceTypeNames(lines),
                         descriptorNames: jobs.descriptor_names ?? {} });
  const filters = $derived(jobFilters(here.query));
  const filtered = $derived(filtering(filters));
  const kept = $derived(lines.filter((line) => keeps(line, filters, ctx)));
  /** The page size as the address gave it, which every link keeps. */
  const limit = $derived(offsetPaging(here.query).asked);
  const choices = $derived(jobChoices(lines, ctx, filters));
</script>

{#if work.error}<p class="err" role="alert">{work.error}</p>{/if}

{#if jobs.results_read === "timed_out" || jobs.results_read === "failed"}
  <p class="notice">The results could not be read from pipette-duckdb ({readFailure(jobs.results_read)}), so the jobs
    are drawn without them.
    <button type="button" class="again" aria-disabled={work.busy}
            onclick={() => work.busy || work.attempt(onchange)}>{work.busy ? "Reading…" : "Read again"}</button></p>
{/if}
<CellFilters {filters} {choices} rows={drawn} link={(to) => cellsLink(base, to, limit)} />
<JobLines {base} {plan} {jobs} lines={kept} {ctx} kind="cells" columns={columnsOf(lines)} sort={filters.sort}
          link={(sort, offset) => cellsLink(base, { ...filters, sort }, limit, offset)}
          empty={filtered ? "No job matches these filters." : "This plan holds no jobs."} {onchange}>
  {#snippet count(listed)}{listedText(listed.jobs, listed.lines, filtered ? jobs.jobs.length : null)}{/snippet}
</JobLines>
