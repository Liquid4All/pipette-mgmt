<script module lang="ts">
  /** The keys of the check and action columns among the widths kept, which no column's key takes. */
  const CHECK = "#check";
  const ACTIONS = "#actions";
</script>

<script lang="ts">
  // The Jobs table (`jobs.ts`), shared by Jobs and Results' Swap: sorted, paged by job and exported here, with
  // each job's re-run or recalculation and a box to check it for one re-run of several, each confirmed first.
  import { untrack, type Snippet } from "svelte";
  import { SvelteSet } from "svelte/reactivity";
  import { failedText, keepTail } from "../plans";
  import { boxOver, parentLink, rerunIds, rerunQuestion, rowOffers, REVISION, type Filing } from "../rerun";
  import { Filer } from "../rerun.svelte";
  import {
    byJob, cellText, changeOf, clientOf, columnNameOf, earlierRows, issuesOf, linesCsv, sortLines, spreadOf, swapped,
    tierOf, type Column, type Context, type Line,
  } from "../jobs";
  import { offsetPaging } from "../pager";
  import { here } from "../route.svelte";
  import { clientLink } from "../routes";
  import { PickedColumns } from "../picked.svelte";
  import { ColumnWidths, headerWidths } from "../widths.svelte";
  import { drawnColumns } from "../table";
  import type { TabBase } from "../tabs";
  import type { PlanJobWithResult, PlanJobsWithResults } from "../api";
  import type { NamedPlanView, ParentJob } from "../api-types";
  import ColumnPicker from "./ColumnPicker.svelte";
  import ConfirmDialog, { type Asked } from "./ConfirmDialog.svelte";
  import Pager from "./Pager.svelte";
  import SortTh from "./SortTh.svelte";
  import StatusPill from "./StatusPill.svelte";
  import CatalogName from "./CatalogName.svelte";
  import DescriptorNames from "./DescriptorNames.svelte";

  let { base, plan, jobs, lines, ctx, kind, columns: every, sort, link, empty, count, onchange }: {
    /** The plan, and the campaign whose pages this is drawn on. */
    base: TabBase;
    plan: NamedPlanView;
    /** Every job of the plan, which the screen reads once for all its tabs. */
    jobs: PlanJobsWithResults;
    /** The lines listed, in the plan's order. */
    lines: Line<PlanJobWithResult>[];
    ctx: Context;
    /** The listing, which names its file and keeps its own pick of columns in the browser. */
    kind: "cells" | "swap";
    /** Every column the reader may pick, in the table's order. */
    columns: Column[];
    sort: string;
    /** The listing's address under a sort, from job `offset`. */
    link: (sort: string, offset?: number) => string;
    /** What the table says with no line to list. */
    empty: string;
    /** How many it lists, before the download of them, and what the listing offers after it. */
    count: Snippet<[{ jobs: number; lines: number }]>;
    /** Read the plan again, after a write. */
    onchange: () => Promise<void>;
  } = $props();

  const names = $derived(ctx.names);
  // The campaign this plan heads, where the reader may re-run its jobs (`campaigns::may_rerun`).
  const rerunIn = $derived(jobs.rerun_campaign_id ?? null);
  const filer = new Filer();
  let confirm = $state<ReturnType<typeof ConfirmDialog>>();
  /** A filing, which opens the revision's Jobs once filed. */
  const filingOf = (which: string, filing: Filing, question: Omit<Asked, "run" | "pending">): Asked => ({
    ...question, pending: "Filing…",
    run: () => (rerunIn ? filer.revise(which, rerunIn, plan.plan_id, filing, onchange) : Promise.resolve(null)),
  });
  // Every job of the plan a re-run takes: the danger zone's re-run, where all of them are checked.
  const answered = $derived(rerunIds(jobs.jobs.map((job) => ({ job }))).length);
  const rerunOne = (jobId: string) => filingOf(`rerun:${jobId}`, { verb: "rerun", body: { job_ids: [jobId] } },
    rerunQuestion([jobId], answered));
  const recalculateOne = (jobId: string) => filingOf(`recalculate:${jobId}`,
    { verb: "recalculate", body: { job_ids: [jobId] } }, {
      title: `Recalculate ${jobId}?`, action: "Recalculate",
      text: `Its recorded completions are scored again under a new job, which no device runs. ${REVISION}`,
    });
  /** Open a confirmation, unless a filing is under way: behind `main.ts`, which drops a locked control's click. */
  const ask = (question: Asked) => filer.busy || void confirm?.ask(question);
  // Kept over every page of jobs, so the columns do not come and go with the filters.
  const actionable = $derived(!!rerunIn && jobs.jobs.some((job) => {
    const offers = rowOffers(job);
    return offers.rerun || offers.recalculate;
  }));
  const checkable = $derived(!!rerunIn && answered > 0);
  /** A value cell's text, or why it has none. */
  const valueText = (column: Column, line: Line<PlanJobWithResult>) =>
    (line.row?.failed ? "failed" : cellText(column, line, ctx) || "—");
  /** A row control's accessible name, which names its job. */
  const rowLabel = (which: string, text: string, jobId: string) =>
    (filer.which === which ? "Filing…" : `${text} ${jobId}`);

  const listed = $derived(sortLines(lines, sort, ctx, every));
  const groups = $derived(byJob(listed));
  const paging = $derived(offsetPaging(here.query));
  const listedJobs = $derived(new Set(listed.map((line) => line.job)).size);
  const shown = $derived(groups.slice(paging.offset, paging.offset + paging.limit));

  // The jobs checked for one re-run. Only those listed are filed: a job the filters drop stays out, checked or not.
  const checked = new SvelteSet<string>();
  // A revision carries its plan's jobs over under their ids, so another plan starts with none checked.
  $effect.pre(() => {
    void plan.plan_id;
    untrack(() => checked.clear());
  });
  /** Every listed job a re-run takes, on this page of them and the others. */
  const offered = $derived(checkable ? rerunIds(listed) : []);
  const chosen = $derived(offered.filter((id) => checked.has(id)));
  const box = $derived(boxOver(offered, checked));
  const check = (jobId: string, on: boolean) => (on ? checked.add(jobId) : checked.delete(jobId));
  const checkAll = (on: boolean) => offered.forEach((id) => check(id, on));
  /** The jobs the open confirmation files, which it lists. */
  let filing = $state.raw<string[]>([]);
  function rerunChecked() {
    if (!chosen.length) return;
    filing = chosen;
    ask(filingOf("checked", { verb: "rerun", body: { job_ids: filing } },
      { ...rerunQuestion(filing, answered), fields: filing.length > 1 ? filed : undefined }));
  }

  // The columns the reader picks, kept in this browser for every plan.
  // `v2`: a pick from before the columns were reworked names keys gone, so it is let go of.
  const picked = untrack(() => new PickedColumns(`pipette.${kind}.columns.v2`, every));
  const columns = $derived(drawnColumns(every, picked.picks));
  // The widths the reader drags the headers to, kept beside the pick.
  const widths = untrack(() => new ColumnWidths(`pipette.${kind}.columns.v2.widths`));
  let head = $state<HTMLTableRowElement>();
  const at = (offset: number) => link(sort, offset);

  // The lines listed, in the columns picked, as one file.
  function exportCsv() {
    const file = document.createElement("a");
    file.href = URL.createObjectURL(new Blob([linesCsv(listed, columns, ctx)], { type: "text/csv" }));
    file.download = `${plan.plan_id}-${kind}.csv`;
    file.click();
    setTimeout(() => URL.revokeObjectURL(file.href));
  }
</script>

<!-- A row's controls as icons, named by their labels and titles: a circling arrow, and a calculator. -->
{#snippet rerunIcon()}
  <svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true" fill="none" stroke="currentColor"
       stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
    <path d="M13.2 10a5.5 5.5 0 1 1-1.3-5.7L14 6" /><polyline points="14 2.5 14 6 10.5 6" />
  </svg>
{/snippet}

{#snippet recalculateIcon()}
  <svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true" fill="none" stroke="currentColor"
       stroke-width="1.3">
    <rect x="3" y="1.5" width="10" height="13" rx="1.5" /><rect x="5.25" y="3.75" width="5.5" height="2.5" rx=".5" />
    <g fill="currentColor" stroke="none">
      <circle cx="5.9" cy="9.2" r=".85" /><circle cx="8" cy="9.2" r=".85" /><circle cx="10.1" cy="9.2" r=".85" />
      <circle cx="5.9" cy="11.9" r=".85" /><circle cx="8" cy="11.9" r=".85" /><circle cx="10.1" cy="11.9" r=".85" />
    </g>
  </svg>
{/snippet}

<!-- `short` keeps only an id's tail, which tells a job's few runs apart, and names the whole id on hover. -->
{#snippet parentRow({ job_id, plan_id }: ParentJob, short = false)}
  {@const text = short ? `…${job_id.slice(-6)}` : job_id}
  {#if plan_id}<a class="id" href={parentLink(base.campaign_id, plan_id, job_id)} title={short ? job_id : undefined}
    >{text}</a>{:else}<span class="id" title={short ? job_id : undefined}>{text}</span>{/if}
{/snippet}

<!-- The checked jobs, as the document page's confirmation lists the ones a revision stops. -->
{#snippet filed()}
  <details>
    <summary>Jobs it re-runs ({filing.length})</summary>
    <pre>{filing.join("\n")}</pre>
  </details>
{/snippet}

<div class="bar summary">
  <ColumnPicker columns={every} drawn={columns.map((column) => column.key)} {picked} {widths} />
  {#if checkable}
    <button type="button" aria-haspopup="dialog" aria-disabled={filer.busy || !chosen.length}
            title={chosen.length ? undefined : "Check the jobs to run again"} onclick={rerunChecked}>{
      filer.which === "checked" ? "Filing…"
        : chosen.length ? `Re-run checked jobs (${chosen.length})` : "Re-run checked jobs"}</button>
  {/if}
  <span class="muted count">{@render count({ jobs: listedJobs, lines: listed.length })}
    {#if listed.length}<button type="button" class="primary" title="The rows listed, in the columns shown"
                               onclick={exportCsv}>Download CSV</button>{/if}</span>
</div>
<!-- Columns with their own markup first; the rest as text. -->
{#snippet cell(column: Column, line: Line<PlanJobWithResult>)}
  {@const job = line.job}
  {@const client = clientOf(line)}
  {#if column.key === "job"}
    <!-- The jobs it derives from, newest first, folded under its id as a value folds its earlier runs. -->
    {@const lineage = [["recalculated from", job.result.status === "scored" ? job.result.rescored_from : null,
                        job.result.status === "scored" ? job.result.earlier_runs.slice(1).map((run) => run.job) : []],
                       ["re-run of", job.plan.rerun_of, job.plan.rerun_history]] as const}
    <!-- Its id opens its plan's Jobs narrowed to it. -->
    {@const own = { job_id: job.job_id, plan_id: plan.plan_id }}
    {#if lineage.some(([, from]) => from)}
      <details class="history"><summary>{@render parentRow(own)}</summary>
        {#each lineage as [what, from, earlier]}{#if from}
          <ol class="lineage"><li>{what} {@render parentRow(from)}</li>
            {#each earlier as run}<li>{@render parentRow(run)}</li>{/each}</ol>
        {/if}{/each}</details>
    {:else}{@render parentRow(own)}{/if}
  {:else if column.key === "state"}<StatusPill status={job.state} />
  {:else if column.key === "device" && client && names[client]}
    <a href={clientLink(client)}>{names[client]}</a>
  {:else if column.json}
    {@const text = cellText(column, line, ctx)}
    <!-- A model or a runtime with a catalog name shows the name; its descriptor opens in a balloon by it. -->
    {#if text}<CatalogName name={columnNameOf(column.key, line, ctx)}>
      <DescriptorNames descriptor={text} {names} /><code class="descriptor">{text}</code></CatalogName>{/if}
  {:else if column.key === "value" && earlierRows(line).length}
    {@const earlier = earlierRows(line)}
    {@const change = changeOf(line)}
    <!-- The value, and how far it moved from the newest earlier run's; opened, every earlier run's. -->
    <details class="history"><summary title="Earlier runs ({earlier.length})">{valueText(column, line)}<span
      class="change" data-tone={change?.tone ?? "flat"} title="vs {valueText(column, { job, row: earlier[0]!.row })} in {
        earlier[0]!.run.job_id}">{change?.text ?? `${earlier.length} earlier`}</span></summary>
      <ol>{#each earlier as { run, row }}<li><span class="was">{valueText(column, { job, row })}</span>
        {@render parentRow(run, true)}</li>{/each}</ol></details>
  {:else if column.key === "spread" || column.key === "stddev"}
    <span class="spread" data-tier={tierOf(spreadOf(line))}>{cellText(column, line, ctx)}</span>
  {:else if column.key === "swap" && swapped(line)}
    <span class="swapped">{cellText(column, line, ctx)}</span>
  {:else if column.key === "issues"}
    {#each issuesOf(line) as issue (issue.key)}<span class="issue" class:bad={issue.key === "failed"}
                                                  class:swap={issue.key === "swap"}>{issue.text}</span>{/each}
  {:else if column.key === "failure" && line.row?.failed}
    <span class="err">{failedText(line.row.failure_source)}</span>
    {#if line.row.failed_reason && !columns.some((c) => c.key === "failed_reason")}
      <details class="why"><summary>why</summary><div class="reason">{keepTail(line.row.failed_reason)}</div></details>
    {/if}
  {:else if column.key === "failed_reason" && line.row?.failed_reason}
    <div class="reason">{keepTail(line.row.failed_reason)}</div>
  {:else}{cellText(column, line, ctx)}{/if}
{/snippet}

<div class="wide">
  <table class:sized={widths.sized}>
    {#if widths.sized}
      <colgroup>
        {#if checkable}<col style:width="{widths.of(CHECK)}px" />{/if}
        {#if actionable}<col style:width="{widths.of(ACTIONS)}px" />{/if}
        {#each columns as column (column.key)}<col style:width="{widths.of(column.key)}px" />{/each}
      </colgroup>
    {/if}
    <thead><tr bind:this={head}>
      {#if checkable}
        <!-- Over every page of the listing, as the button's count says. -->
        <th class="check" data-key={CHECK}><input type="checkbox" checked={box.checked} indeterminate={box.mixed}
                                 disabled={filer.busy || !offered.length}
                                 aria-label="Check every listed job a re-run takes"
                                 title="Check every listed job a re-run takes ({offered.length})"
                                 onchange={(e) => checkAll(e.currentTarget.checked)} /></th>
      {/if}
      {#if actionable}<th aria-label="actions" data-key={ACTIONS}></th>{/if}
      {#each columns as column (column.key)}
        <SortTh key={column.key} label={column.label} {sort} num={column.num} link={(to) => link(to)}
                resize={widths.grip(column.key, () => headerWidths(head))} />
      {/each}
    </tr></thead>
    <tbody>
      {#each shown as group (group)}
        {@const job = group.job}
        {@const offers = rowOffers(job)}
        {@const span = group.lines.length}
        {#each group.lines as line, i (i)}
          <!-- A job's own cells span its lines; a line is one row of its run. -->
          <tr class:start={!i}>
            {#if checkable && !i}
              <td class="check" rowspan={span}>{#if offers.rerun}<input
                type="checkbox" checked={checked.has(job.job_id)} disabled={filer.busy}
                aria-label="Check {job.job_id} for a re-run"
                onchange={(e) => check(job.job_id, e.currentTarget.checked)} />{/if}</td>
            {/if}
            {#if actionable && !i}
              <!-- Two fixed slots, an unoffered one left empty, so each icon keeps its place down the column. -->
              <td class="actions" rowspan={span}><div class="slots">
                {#if offers.rerun}
                  <button type="button" class="icon" aria-disabled={filer.busy} aria-haspopup="dialog"
                          aria-busy={filer.which === `rerun:${job.job_id}`} title="Re-run"
                          aria-label={rowLabel(`rerun:${job.job_id}`, "Re-run", job.job_id)}
                          onclick={() => ask(rerunOne(job.job_id))}>{@render rerunIcon()}</button>
                {:else}<span></span>{/if}
                {#if offers.recalculate}
                  <button type="button" class="icon" aria-disabled={filer.busy} aria-haspopup="dialog"
                          aria-busy={filer.which === `recalculate:${job.job_id}`} title="Recalculate"
                          aria-label={rowLabel(`recalculate:${job.job_id}`, "Recalculate", job.job_id)}
                          onclick={() => ask(recalculateOne(job.job_id))}>{@render recalculateIcon()}</button>
                {:else}<span></span>{/if}
              </div></td>
            {/if}
            {#each columns as column (column.key)}
              {#if !column.job || !i}
                <td rowspan={column.job ? span : undefined} class:id={column.key === "job" || column.key === "client_id"}
                    class:num={column.num}>{@render cell(column, line)}</td>
              {/if}
            {/each}
          </tr>
        {/each}
      {:else}
        <tr><td colspan={columns.length + Number(actionable) + Number(checkable)} class="muted">{empty}</td></tr>
      {/each}
    </tbody>
  </table>
</div>
<Pager offset={paging.offset} shown={shown.length} size={paging.limit} total={groups.length} {at} />

<ConfirmDialog bind:this={confirm} status={filer.busy ? filer.status : ""} />

<style>
  .summary { padding: 0 0 10px; border-bottom: 1px solid var(--border); align-items: end; }
  /* As tall as the column picker's box (31px), so their text centres on it and not on its label. */
  .summary > :global(button), .count { box-sizing: border-box; min-height: 31px; }
  .count { display: inline-flex; align-items: center; margin-left: auto; padding-bottom: 0; }
  /* The listing's own links come from its page. */
  .count :global(a), .count :global(button) { margin-left: 8px; }
  .check { width: 1%; }
  .check input { margin: 0; vertical-align: middle; }
  .slots { display: grid; grid-template-columns: 24px 24px; gap: 4px; }
  button.icon { display: inline-flex; align-items: center; justify-content: center; width: 24px; height: 24px;
                padding: 0; color: var(--text-muted); vertical-align: middle; }
  button.icon:hover { color: var(--text); }
  button.icon[aria-busy="true"] { opacity: .5; }
  /* Short flags keep a narrow column; a descriptor wraps at a width it reads whole in, on fewer lines. */
  code.descriptor { width: max-content; max-width: 44em; }
  /* At a width the reader set, a descriptor wraps at its column's width. */
  .sized code.descriptor { width: auto; max-width: none; }
  /* A job's first line opens with a firmer rule, so its lines read as one. */
  tr.start > td { border-top: 1px solid var(--border-strong); }
  tbody tr:first-child > td { border-top: 0; }
  tbody tr:hover > td { background: none; }
  /* Warmer over 3%, 5% and 10% of the value; up to 3% is noise. */
  .spread, .swapped { padding: 1px 4px; border-radius: 3px; }
  .spread[data-tier=notable] { background: linear-gradient(90deg, #fefce8, #fef08a); }
  .spread[data-tier=significant] { background: linear-gradient(90deg, #fff7ed, #fed7aa); }
  .spread[data-tier=unreliable] { background: linear-gradient(90deg, #fff1f2, #fecdd3); }
  .swapped { background: linear-gradient(90deg, #f5f3ff, #ddd6fe); }
  .issue { display: inline-block; margin: 0 4px 2px 0; padding: 0 6px; border-radius: 9px; font-size: 11px;
           white-space: nowrap; background: var(--amber-soft); color: var(--amber); }
  .issue.bad { background: var(--red-soft); color: var(--red); }
  .issue.swap { background: linear-gradient(90deg, #f5f3ff, #ddd6fe); color: #6d28d9; }
  details.why { margin: 2px 0 0; }
  details.why summary { font-size: 11px; color: var(--text-muted); cursor: pointer; }
  /* Each earlier job of a chain, which the one above it derives from. */
  .lineage li + li::before { content: "← "; }
  details.history { margin: 0; }
  /* One line, the value first: the pill marks the cell as one to open, not a disclosure triangle. */
  details.history summary { cursor: pointer; list-style: none; white-space: nowrap; }
  details.history summary::-webkit-details-marker { display: none; }
  .change { display: inline-block; margin-left: 6px; padding: 0 5px; border-radius: 9px; font-size: 11px;
            background: var(--surface-2); color: var(--text-muted); }
  .change[data-tone=better] { background: var(--green-soft); color: var(--green); }
  .change[data-tone=worse] { background: var(--red-soft); color: var(--red); }
  .history ol { margin: 2px 0 0; padding: 0; list-style: none; font-size: 11px; color: var(--text-muted); }
  .history li { white-space: nowrap; }
  .history .was { color: var(--text); }
</style>
