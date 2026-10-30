<script lang="ts">
  // The top of a plan's Overview: the plan record as a box of fields, as Settings lays out its rows, and the file
  // of its results. Progress has its own section; the queue's row warns only where its count may be stale or jobs
  // still queued are starved (`standing.ts`).
  import { when } from "../format";
  import { changeText, shownStatus, starvedLine } from "../plans";
  import { userLink } from "../routes";
  import { counted, expiryText, starvedNow } from "../standing";
  import type { PlanJobsWithResults } from "../api";
  import type { NamedPlanView } from "../api-types";
  import Clamped from "./Clamped.svelte";
  import ResultsDownload from "./ResultsDownload.svelte";
  import StatusPill from "./StatusPill.svelte";

  let { plan, seen, jobs, rows = null, scorers = "" }: {
    plan: NamedPlanView;
    /** When the plan was read, which its count's freshness is judged against. */
    seen: number;
    /** Every job, once read: the starved groups are cut to the jobs still queued. */
    jobs: PlanJobsWithResults | null;
    /** How many result rows the read holds; null before the results are read. */
    rows?: number | null;
    /** The scorer versions of the results, each with its job count, as `scorersText` writes them. Empty if no row has one. */
    scorers?: string;
  } = $props();

  const names = $derived(plan.names ?? {});
  const done = $derived(plan.status === "complete" || plan.status === "cancelled");
  const starved = $derived(starvedNow(plan, jobs?.jobs ?? null));
  const count = $derived(counted(plan, seen));
  const expiry = $derived(!done && plan.job_expiry ? expiryText(plan.job_expiry, seen) : null);
</script>

<dl class="facts" aria-label="Plan">
  <div><dt>Status</dt>
    <dd><StatusPill status={shownStatus(plan.status, plan.progress_snapshot?.counts)} />{#if plan.cancel_requested}{" "}cancel requested{/if}</dd></div>
  {#if plan.note}<div><dt>Note</dt><dd class="note">{plan.note}</dd></div>{/if}
  {#if plan.change}<div><dt>Change</dt><dd>{changeText(plan.change)}</dd></div>{/if}
  <div><dt>ID</dt><dd class="id">{plan.plan_id}</dd></div>
  <div><dt>Submitted by</dt>
    <dd>{#if plan.submitted_by}<a href={userLink(plan.submitted_by)}>{
      names[plan.submitted_by] ?? plan.submitted_by}</a>{:else}operator{/if}</dd></div>
  <div><dt>Created</dt><dd>{when(plan.created_at)}</dd></div>
  {#if plan.terminal_at}<div><dt>Ended</dt><dd>{when(plan.terminal_at)}</dd></div>{/if}
  {#if expiry}<div><dt>Jobs expire</dt><dd>{expiry.title}</dd></div>{/if}
  {#if count?.stale || starved.length}
    <div><dt>Queue</dt>
      <dd>
        {#if count?.stale}<div class="stale">queue-maintenance may have stopped, so the status and the starved jobs may be out of date.</div>{/if}
        {#if starved.length}
          <ul class="starved" aria-label="Starved jobs">
            {#each starved as group, i (i)}<li><Clamped text={starvedLine(group, plan.fleet_detail)} /></li>{/each}
          </ul>
        {/if}
      </dd></div>
  {/if}
  {#if rows}
    <div><dt>Results</dt>
      <dd class="results">{rows.toLocaleString("en-US")} rows <ResultsDownload planId={plan.plan_id} /></dd></div>
  {/if}
  {#if scorers}<div><dt>Scorer</dt><dd>{scorers}</dd></div>{/if}
</dl>

<style>
  .stale { color: var(--amber); font-size: 12px; }
  .starved { margin: 4px 0 0; padding: 0 0 0 10px; list-style: none; border-left: 2px solid var(--amber-border);
             font-size: 12px; color: var(--text-muted); }
  .note { white-space: pre-wrap; }
  .results { display: flex; gap: 12px; align-items: baseline; flex-wrap: wrap; }
</style>
