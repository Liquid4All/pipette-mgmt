<script lang="ts">
  // A plan's screen, drawn alike on its own pages and its campaign's; on a campaign's page its records and the
  // revise links sit on History, every write on Settings. Every job, with its results, is read once per screen.
  import { untrack, type Snippet } from "svelte";
  import { SvelteSet } from "svelte/reactivity";
  import type { CatalogOrg } from "../catalog-entry";
  import { Cells } from "../cells";
  import { progress } from "../format";
  import { planWrites, readFailure } from "../plans";
  import { DERIVED_FAMILIES, MISSING, shortLabel, SWAP } from "../report";
  import { here } from "../route.svelte";
  import { PlanRead } from "../results.svelte";
  import { NO_DEVICE, allRuns, deviceTypeRuns, devicesText, jobsOn, jobTypes, scorersText } from "../summary";
  import { pickedDevice, progressBar, reportOf, sharedQuery, tabLink, topBar, type Tab, type TabBase } from "../tabs";
  import type { NamedPlanView } from "../api-types";
  import PlanBody from "./PlanBody.svelte";
  import PlanCells from "./PlanCells.svelte";
  import PlanFacts from "./PlanFacts.svelte";
  import PlanHead from "./PlanHead.svelte";
  import PlanSettings from "./PlanSettings.svelte";
  import PlanSummary from "./PlanSummary.svelte";
  import ResultsCharts from "./ResultsCharts.svelte";
  import ResultsMissing from "./ResultsMissing.svelte";
  import ResultsSwap from "./ResultsSwap.svelte";
  import TabBar from "./TabBar.svelte";

  let { base, tab, plan, onchange, record, revisions, campaign, catalogOrg = null }: {
    base: TabBase;
    tab: Tab;
    plan: NamedPlanView;
    /** Read the plan again, after a write. */
    onchange: () => Promise<void>;
    /** On a campaign's page, the heading over the plan's record on History: which of its plans this is. */
    record?: string;
    /** The History tab, where the plan has a campaign whose history the reader may read. */
    revisions?: Snippet;
    /** On a campaign's page whose reader administers it, its rename and transfer, on Settings. */
    campaign?: { general: Snippet; transfer: Snippet };
    /** On a campaign's page, the organization where the reader may name the plan's runtimes, with its label. */
    catalogOrg?: CatalogOrg | null;
  } = $props();

  const results = new PlanRead();
  const planId = $derived(base.plan_id);
  // Once per screen; untracked, as the read's own state changing must not start it again.
  $effect(() => {
    const id = planId;
    untrack(() => results.read(id));
  });
  const answer = $derived(results.answer);
  // When the plan was last read, kept across tabs: the header judges its count's freshness by it, not by a remount.
  let seen = $state(Date.now());
  $effect.pre(() => {
    void plan;
    seen = Date.now();
  });
  const read = $derived(answer.state === "read" ? answer.results : null);
  const cells = $derived(results.cells);
  const jobs = $derived(results.jobs);
  // Progress by device type, as the address's one `device` names it: the jobs that ran on it, a job not run on none.
  const types = $derived(read?.view.fleet_detail === "full" ? jobTypes(read.view.results) : null);
  const runs = $derived(read && types ? deviceTypeRuns(read.view.results, types) : []);
  const typeList = $derived(runs.map((run) => run.type).filter((type) => type !== NO_DEVICE));
  const picked = $derived(pickedDevice(here.query, typeList));
  const devices = $derived(progressBar(base, typeList, read?.index.deviceNames ?? new Map(), picked));
  // What the pick ran on, which the Progress header's job counts do not say.
  const ranOn = $derived(typeList.length && read
    ? devicesText(runs.find((run) => run.type === picked) ?? allRuns(runs, read.view.results)) : null);
  const scorers = $derived(scorersText(read?.view.results ?? []));
  const progressCells = $derived(picked && types && jobs.data
    ? { data: new Cells({ ...jobs.data, jobs: jobsOn(jobs.data.jobs, types, picked) }), error: jobs.error }
    : cells);
  /** A write on Jobs or Settings, after which the plan and its jobs are read again. */
  const changed = async () => {
    results.read(planId);
    await onchange();
  };

  const settings = $derived(!!campaign || (!!jobs.data && planWrites(plan, jobs.data)));
  const top = $derived(topBar(base, tab, { analysis: answer.state !== "absent", revisions: !!revisions, settings },
                              here.query));
  // With no rows the Results tab says so as the Overview does, and has nothing to add.
  const reports = $derived(tab.name === "analysis" && read?.view.results.length ? read : null);
  const report = $derived(reports ? reportOf(tab.view, reports.index) : null);
  // The Results sections the reader has open, kept across tabs as the read is.
  const opened = new SvelteSet<string>();
</script>

{#snippet again()}
  <button type="button" class="again" aria-disabled={results.reading}
          onclick={() => results.reading || results.read(planId)}>{results.reading ? "Reading…" : "Read again"}</button>
{/snippet}

<!-- The results' state where there are no rows to draw: on the Overview, and on Results in its place. -->
{#snippet unread()}
  {#if answer.state === "loading"}
    <p class="muted">Reading results…</p>
  {:else if answer.state === "unavailable"}
    <p class="notice">The results could not be read from pipette-duckdb ({readFailure(answer.status)}).
      {@render again()}</p>
  {:else if answer.state === "failed"}
    <p class="err" role="alert">{answer.error}</p>
    <p>{@render again()}</p>
  {:else if answer.state === "absent"}
    <p class="muted">This server reads no results: it has no pipette-duckdb configured.</p>
  {:else}
    <p class="muted">No results yet.</p>
  {/if}
{/snippet}

<TabBar label="Plan" items={top} />

<!-- A check's own page, reached from the Overview: the way back to the charts. -->
{#snippet back()}
  <p><a href={tabLink(base, "analysis", "", sharedQuery(here.query))}>← All results</a></p>
{/snippet}

{#snippet facts()}<PlanFacts {plan} document={cells.data?.document ?? null} results={answer.state !== "absent"} />{/snippet}

{#if tab.name === "overview"}
  <PlanHead {plan} {seen} jobs={jobs.data} rows={read?.view.results.length ?? null} {scorers} />
  {#if answer.state === "unavailable" || answer.state === "absent"}
    <!-- Loading and a failed read are the cells', which come in the same read. -->
    {@render unread()}
  {/if}
  {#if read && jobs.data}<PlanSummary {base} results={read} jobs={jobs.data} />{/if}
  <PlanCells {base} cells={progressCells} results={read} {again} {devices} {ranOn} {catalogOrg}
             counted={plan.progress_snapshot ? progress(plan) : null} />
  {#if !record}
    <h2>Plan</h2>
    {@render facts()}
  {/if}
{:else if tab.name === "cells"}
  {#if jobs.data}
    <PlanBody {base} {plan} jobs={jobs.data} onchange={changed} />
  {:else if jobs.error}
    <p class="err" role="alert">{jobs.error}</p>
    <p>{@render again()}</p>
  {:else}
    <p class="muted">Reading the jobs…</p>
  {/if}
{:else if tab.name === "revisions"}
  {#if revisions}
    {@render revisions()}
  {:else}
    <p class="muted">No revisions to list: this plan was filed into no campaign you may read.</p>
  {/if}
  {#if record}
    <h2>{record}</h2>
    {@render facts()}
  {/if}
{:else if tab.name === "settings"}
  <PlanSettings {base} {plan} {jobs} {again} onchange={changed} general={campaign?.general}
                transfer={campaign?.transfer} />
{:else if reports && report}
  {#if report.kind === "charts"}
    <ResultsCharts {base} {plan} jobs={jobs.data!} onchange={changed} results={reports} family={report.family}
                   segment={tab.view ?? ""} {opened} />
  {:else if report.kind === "swap"}
    {@render back()}
    <!-- The results are the jobs' own, so one read holds both. -->
    <ResultsSwap {base} {plan} jobs={jobs.data!} onchange={changed} />
  {:else if report.kind === "missing"}
    {@render back()}
    <ResultsMissing {base} missing={report.missing} names={reports.names} />
  {:else}
    <p class="notice">
      {#if DERIVED_FAMILIES.includes(report.view)}{shortLabel(report.view)} is not offered for this plan.{:else if report.view === SWAP}Swap is not
        offered for this plan: none of its jobs reports swap or resident memory.{:else if report.view === MISSING}Every
        measured row of this plan has its model's parameter count.{:else}This plan has no results of
        benchmark type <code>{report.view}</code>.{/if}
      <a href={tabLink(base, "analysis", "", sharedQuery(here.query))}>Back to Results</a>
    </p>
  {/if}
{:else}
  {@render unread()}
{/if}

