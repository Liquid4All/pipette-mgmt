<script lang="ts">
  // The Results tab's charts: the device-type bar, then a section per chart, the one the address names open (none
  // where it names none). A section draws only once open; the filters are in the address.
  import { tick, untrack } from "svelte";
  import { SvelteSet } from "svelte/reactivity";
  import { cellsWith, drawnLink } from "../jobs";
  import {
    chartRefs, familyOf, pageControls, reportFilters, reportPairs, toggledRuntime, view, withFilter,
    type ChartRef, type ReportFilters,
  } from "../report";
  import { rerunIds, rerunQuestion, rowOffers } from "../rerun";
  import { Filer } from "../rerun.svelte";
  import { here, redirect } from "../route.svelte";
  import { deviceBar, tabLink, type TabBase } from "../tabs";
  import type { Results } from "../results.svelte";
  import type { PlanJobsWithResults } from "../api";
  import type { NamedPlanView } from "../api-types";
  import ConfirmDialog from "./ConfirmDialog.svelte";
  import FilterBar from "./FilterBar.svelte";
  import ReportFamily from "./ReportFamily.svelte";
  import TabBar from "./TabBar.svelte";

  let { base, plan, jobs, onchange, results, family, segment, opened }: {
    base: TabBase;
    plan: NamedPlanView;
    /** Every job of the plan, whose states say which a point may re-run. */
    jobs: PlanJobsWithResults;
    /** Read the plan again, after a re-run. */
    onchange: () => Promise<void>;
    results: Results;
    /** The family the address names, whose chart is open; `""` at the bare address. */
    family: string;
    /** The address's own segment, which a filter keeps. */
    segment: string;
    /** The open sections' keys, the screen's own, so they hold while the reader is on another tab. */
    opened: SvelteSet<string>;
  } = $props();

  // As asked, not as one chart resolves them: a pin another chart holds stays in the address.
  const asked = $derived(reportFilters(here.query, family));
  const to = (filters: ReportFilters) => tabLink(base, "analysis", segment, reportPairs(filters));
  const change = (key: string, value: string) => redirect(to(withFilter(asked, key, value)));
  /** A chart's runtime, clicked: the drop is the page's, so every chart that ran the runtime follows it. */
  const runtime = (filters: ReportFilters) => (variant: string) =>
    redirect(to({ ...asked, drop: toggledRuntime(results.index, filters, variant) }));

  const page = $derived(pageControls(results.index, asked));
  const devices = $derived(deviceBar(base, segment, asked, page.device, results.index.deviceNames));
  const charts = $derived(chartRefs(results.index));
  const named = $derived(charts.find((c) => c.family.key === family && c.metric?.key === (asked.metric || c.family.metric))
    ?? charts.find((c) => c.family.key === family));
  const open = (chart: ChartRef) => opened.has(chart.key);
  const toggle = (chart: ChartRef, to: boolean) => (to ? opened.add(chart.key) : opened.delete(chart.key));
  const sections: Record<string, HTMLDetailsElement> = {};
  // A link naming a section opens it alone, before the sections draw, and brings it into view; the bare address
  // leaves them as the reader left them. By key, so a re-read of the plan neither closes nor scrolls.
  const namedKey = $derived(named?.key);
  $effect.pre(() => {
    const key = namedKey;
    if (key === undefined) return;
    untrack(() => {
      opened.clear();
      opened.add(key);
    });
    void tick().then(() => {
      const top = sections[key]?.getBoundingClientRect().top;
      if (top !== undefined && (top < 0 || top > innerHeight)) sections[key]!.scrollIntoView({ block: "start" });
    });
  });

  // Jobs checked from the charts' points, over every section, re-run in one filing as Jobs' checked rows are,
  // confirmed first; a job is offered where the reader may re-run and it has answered.
  const rerunIn = $derived(jobs.rerun_campaign_id ?? null);
  const states = $derived(new Map(jobs.jobs.map((job) => [job.job_id, job])));
  const answered = $derived(rerunIds(jobs.jobs.map((job) => ({ job }))).length);
  const rerunnable = (jobId: string) => {
    const job = states.get(jobId);
    return !!rerunIn && !!job && rowOffers(job).rerun;
  };
  const checked = new SvelteSet<string>();
  const filer = new Filer();
  let confirm = $state<ReturnType<typeof ConfirmDialog>>();
  /** The jobs the open confirmation files, which it lists. */
  let filing = $state.raw<string[]>([]);
  function rerunChecked() {
    if (!rerunIn || filer.busy || !checked.size) return;
    filing = [...checked];
    void confirm?.ask({
      ...rerunQuestion(filing, answered), pending: "Filing…", fields: filing.length > 1 ? filed : undefined,
      run: () => filer.revise("checked", rerunIn, plan.plan_id, { verb: "rerun", body: { job_ids: filing } }, onchange),
    });
  }

  // Jobs filters by benchmark id, so a type's failed jobs are those of every id of its rows.
  const ids = (chart: ChartRef) =>
    [...new Set(results.view.results.filter((row) => familyOf(row) === chart.family.key).map((row) => row.benchmark_id))];
</script>

<!-- The page's own choices on one line: the device types, then how many series a chart holds. -->
{#if devices.length || page.per}
  <div class="top">
    {#if devices.length}<TabBar label="Device type" items={devices} sub />{/if}
    {#if page.per}
      {@const per = page.per}
      <label class="per">{per.label}
        <select value={per.value} disabled={per.disabled} onchange={(e) => change(per.key, e.currentTarget.value)}>
          {#each per.options as option (option.value)}
            <option value={option.value} disabled={option.disabled}>{option.label}</option>
          {/each}
        </select>
      </label>
    {/if}
  </div>
{/if}

{#each charts as chart (chart.key)}
  <details class="chart" bind:this={sections[chart.key]} open={open(chart)} ontoggle={(e) => toggle(chart, e.currentTarget.open)}>
    <summary><h2>{chart.name}</h2></summary>
    {#if open(chart)}
      {@const filters = { ...asked, benchmark: chart.family.key, metric: chart.metric?.key ?? "" }}
      {@const drawn = view(results.index, filters)}
      <div class="body">
        {#if drawn.pins.length}<FilterBar controls={drawn.pins} {change} />{/if}
        <ReportFamily view={drawn} names={results.names} failed={cellsWith(base, { benchmark: ids(chart), result: ["failed"] })}
                      runtime={runtime(filters)} jobs={(rows) => drawnLink(base, rows, results.view.results)}
                      job={(jobId) => cellsWith(base, { q: jobId })} {checked} {rerunnable} />
      </div>
    {/if}
  </details>
{/each}

{#if checked.size}
  <div class="picked" role="region" aria-label="Checked jobs">
    <b>{checked.size} {checked.size === 1 ? "job" : "jobs"} checked</b>
    <button type="button" class="primary" onclick={rerunChecked}>Re-run {checked.size}</button>
    <button type="button" onclick={() => checked.clear()}>Clear</button>
  </div>
{/if}

{#snippet filed()}
  <details>
    <summary>Jobs it re-runs ({filing.length})</summary>
    <pre>{filing.join("\n")}</pre>
  </details>
{/snippet}

<ConfirmDialog bind:this={confirm} status={filer.busy ? filer.status : ""} />

<style>
  .chart { margin: 10px 0; border: 1px solid var(--border); border-radius: var(--radius); }
  /* The browser's own disclosure arrow, as every other collapsed block of the app has. */
  .chart > summary { padding: 9px 14px; color: var(--text); }
  .chart > summary::marker { color: var(--text-muted); }
  h2 { display: inline; margin: 0 0 0 2px; padding: 0; border: 0; }
  .body { padding: 0 14px 10px; }
  .top { display: flex; flex-wrap: wrap; gap: 8px 16px; align-items: center; justify-content: space-between;
         margin: 14px 0 6px; padding-bottom: 10px; border-bottom: 1px solid var(--border); }
  .top :global(nav.tabs) { flex: 1 1 auto; margin: 0; }
  .per { display: inline-flex; flex: none; gap: 6px; align-items: center; margin: 0 0 0 auto; font-size: 12px;
         color: var(--text-muted); white-space: nowrap; }
  /* Held at the foot of the window while jobs are checked, so it is in reach from any section. */
  .picked { position: fixed; right: 24px; bottom: 16px; z-index: 6; display: flex; gap: 10px; align-items: center;
            padding: 8px 12px; border: 1px solid var(--border-strong); border-radius: 8px; background: var(--bg);
            box-shadow: 0 8px 28px var(--shadow); }
</style>
