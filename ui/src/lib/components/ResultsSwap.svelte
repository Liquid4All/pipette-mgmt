<script lang="ts">
  // Results' Swap: the Jobs table over the lines whose run swapped, with Jobs' filters, kept in the address.
  import {
    SWAP_COLUMNS, columnsOf, deviceTypeNames, listedText, deviceTypes, filterPairs, filtering, jobChoices, jobFilters, keeps, linesOf, swapped, type JobFilters,
  } from "../jobs";
  import { offsetPaging } from "../pager";
  import { SWAP } from "../report";
  import { here } from "../route.svelte";
  import { sharedQuery, tabLink, type TabBase } from "../tabs";
  import type { PlanJobsWithResults } from "../api";
  import type { NamedPlanView } from "../api-types";
  import CellFilters from "./CellFilters.svelte";
  import JobLines from "./JobLines.svelte";

  let { base, plan, jobs, onchange }: {
    base: TabBase;
    plan: NamedPlanView;
    /** Every job of the plan, whose lines that swapped are listed. */
    jobs: PlanJobsWithResults;
    /** Read the plan again, after a write. */
    onchange: () => Promise<void>;
  } = $props();

  const all = $derived(linesOf(jobs.jobs));
  const ctx = $derived({ names: { ...plan.names, ...jobs.names }, types: deviceTypes(all), typeNames: deviceTypeNames(all),
                         descriptorNames: jobs.descriptor_names ?? {} });
  const lines = $derived(all.filter(swapped));
  const filters = $derived(jobFilters(here.query));
  const filtered = $derived(filtering(filters));
  const kept = $derived(lines.filter((line) => keeps(line, filters, ctx)));
  const choices = $derived(jobChoices(lines, ctx, filters));
  const limit = $derived(offsetPaging(here.query).asked);

  /** Swap under `to`, from job `offset`; the chart tabs' own keys ride along for the way back, `device` in Jobs' pairs. */
  const link = (to: JobFilters, offset = 0) => tabLink(base, "analysis", SWAP, [
    ...sharedQuery(here.query).filter(([key]) => key !== "model" && key !== "device"),
    ...filterPairs(to, limit, offset),
  ]);
</script>

<CellFilters {filters} {choices} rows reported link={(to) => link(to)} />
<JobLines {base} {plan} {jobs} lines={kept} {ctx} kind="swap" columns={columnsOf(lines, SWAP_COLUMNS)} sort={filters.sort}
          link={(sort, offset) => link({ ...filters, sort }, offset)} empty="No line matches these filters."
          {onchange}>
  {#snippet count(listed)}
    {listedText(listed.jobs, listed.lines, filtered ? new Set(lines.map((line) => line.job)).size : null)}
  {/snippet}
</JobLines>
