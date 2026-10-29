<script lang="ts">
  // The Overview's Progress (`GET /api/v1/plans/{id}/jobs?all=true`): the cells as
  // `cells.ts` groups them, each row with its jobs by state and their results,
  // under a header that totals them. On a campaign's page, a reader who may name its runtimes and models in the
  // campaign's organization gets a button under each runtime and model that the organization does not name yet.
  import { untrack, type Snippet } from "svelte";
  import { descriptorNamesByDigest } from "../api";
  import { digestBatches, displayOf, entryOffer, offerReason, type CatalogOrg, type DigestNames } from "../catalog-entry";
  import { doneOf } from "../format";
  import { cellsWith } from "../jobs";
  import { shortLabel } from "../report";
  import { here } from "../route.svelte";
  import { session } from "../session.svelte";
  import { sharedQuery, tabLink, type TabBase } from "../tabs";
  import {
    idLabels, jobParts, knobLabels, type Cells, type Edge, type FlagKind, type JobCounts, type ResultCounts,
    type Spanned,
  } from "../cells";
  import type { DescriptorDisplay, DescriptorKind, JobState } from "../api-types";
  import type { BarTab } from "../bar";
  import type { Results } from "../results.svelte";
  import CatalogEntryAction from "./CatalogEntryAction.svelte";
  import CatalogName from "./CatalogName.svelte";
  import DescriptorNames from "./DescriptorNames.svelte";
  import TabBar from "./TabBar.svelte";

  let { base, cells, results, again, counted = null, devices = [], ranOn = null, catalogOrg = null }: {
    base: TabBase;
    /** The cells to draw: the plan's, or those of the device type picked. */
    cells: { data: Cells | null; error: string };
    /** The plan's results once read: a benchmark type links to its report where they have one. */
    results: Results | null;
    /** Read again: after a failed read, and beside the results count while results can still change. */
    again: Snippet;
    /** queue-maintenance's count of the jobs, drawn while the cells are read. */
    counted?: string | null;
    /** The device types to narrow the cells to, as a bar under the heading; none where there is no type to pick. */
    devices?: BarTab[];
    /** What the device types picked ran on: their devices and device time. */
    ranOn?: string | null;
    /** The organization where the reader may name the runtimes and models (`catalog_org_id`), with its label. */
    catalogOrg?: CatalogOrg | null;
  } = $props();

  const drawn = $derived(cells.data);
  // The live names of the drawn runtimes and models, read for the buttons: what the organization names already, and
  // the names of an entry just created. Read again with each read of the cells; null until read, and after a failed
  // read.
  const digests = $derived(catalogOrg && drawn
    ? [...new Set([...drawn.runtimeDigests.values(), ...drawn.modelDigests.values()])].sort() : []);
  let digestNames = $state.raw<DigestNames | null>(null);
  let namesFailed = $state(false);
  // What the last read was for, and a count of the reads: only the latest read applies, so a slow read cannot undo a
  // newer one.
  let readFor = "";
  let latest = 0;
  async function readNames(org: string, digests: string[]) {
    const key = [org, ...digests].join(",");
    if (readFor !== key) digestNames = null;
    readFor = key;
    const seq = ++latest;
    if (!digests.length) {
      digestNames = null;
      namesFailed = false;
      return;
    }
    const read = await Promise.all(digestBatches(digests).map((batch) => descriptorNamesByDigest(session.signer, batch)))
      .then((reads) => new Map(reads.flatMap((one) => Object.entries(one.names))), () => null);
    if (seq !== latest) return;
    digestNames = read;
    namesFailed = !read;
  }
  $effect(() => {
    const org = catalogOrg?.id ?? "";
    const wanted = digests;
    untrack(() => void readNames(org, wanted));
  });
  let entries = $state<ReturnType<typeof CatalogEntryAction>>();
  // The type picked, which the failed jobs' link carries to Jobs, so it lists what the count counts.
  const picked = $derived(devices.find((tab) => tab.current)?.key ?? "");
  // Read again stays once offered: unpublished rows, or jobs still to run, may have results by the next republish,
  // and a read that settles them must not pull the button from under the focus.
  let reread = $state(false);
  $effect.pre(() => {
    const totals = drawn?.totals;
    if (totals?.results?.pending || totals?.jobs.available || totals?.jobs.running || totals?.jobs.unknown) reread = true;
  });
  // A benchmark type's report, the first of its tabs where it has several (an eval's).
  const reportOf = (type: string) => results?.index.benchmarks.find((f) => f.types.includes(type))?.key ?? null;
  const AWAITING = "Finished, but pipette-duckdb has not republished their rows yet; it republishes every few minutes.";
  const plural = (n: number, noun: string) => `${noun}${n === 1 ? "" : "s"}`;
  const count = (n: number, noun: string) => `${n} ${plural(n, noun)}`;
  /** Each job state's tone, the status pills' colours: its bar segment and the dot beside its count. */
  const TONE: Record<JobState, string> = {
    finished: "good", failed: "bad", running: "active", available: "idle", unknown: "waiting", cancelled: "off",
  };
</script>

<!-- Models or runtimes as stored, every one shown: the table ends the page, so it runs its length. What a
     descriptor reads as goes over it. A model or a runtime with a catalog name shows the name; its descriptor opens
     in a balloon by it. A runtime or a model that the reader may name has its button under it. -->
{#snippet labels(list: string[], kind: DescriptorKind, catalog: ReadonlyMap<string, DescriptorDisplay> | undefined,
                 digestOf: ReadonlyMap<string, string>)}
  {#each list as descriptor (descriptor)}
    {@const digest = digestOf.get(descriptor)}
    {@const live = digest ? digestNames?.get(digest) : undefined}
    {@const offer = catalogOrg ? entryOffer(descriptor, kind, digest, digestNames, catalogOrg.id) : null}
    <div><CatalogName name={(live && displayOf(live)) || (catalog?.get(descriptor) ?? null)}><DescriptorNames
      {descriptor} names={drawn?.names ?? {}} /><code class="descriptor">{descriptor}</code></CatalogName>
      {#if offer === "create"}
        <div class="entry"><button type="button" aria-haspopup="dialog"
                                   onclick={() => entries?.create(descriptor, kind)}>Create a catalog entry</button></div>
      {:else if offer}
        <div class="entry"><button type="button" disabled>Create a catalog entry</button>
          <span class="muted">{offerReason(offer, kind)}</span></div>
      {/if}
    </div>
  {/each}
{/snippet}

{#snippet named(cell: Spanned<string[]>, kind: DescriptorKind, catalog: ReadonlyMap<string, DescriptorDisplay> | undefined,
                digestOf: ReadonlyMap<string, string>)}
  <td rowspan={cell.span} class="named" class:model-end={cell.end === "model"} class:table-end={cell.end === "table"}>
    <div class="list">{@render labels(cell.value, kind, catalog, digestOf)}</div>
  </td>
{/snippet}

<!-- A bar of the jobs' shares by state, over a count per state. -->
{#snippet bar(parts: ReturnType<typeof jobParts>)}
  <div class="bar" role="img" aria-label={parts.map((part) => part.text).join(", ")}>
    <!-- The done line counts the failed too, so the finished segment names its own jobs. -->
    {#each parts as part (part.state)}{#if part.n}<span
      data-tone={TONE[part.state]} style:flex-grow={part.n}
      title={part.state === "finished" ? `${part.n} finished` : part.text}></span>{/if}{/each}
  </div>
{/snippet}

<!-- A count per state. The failed count links to the failed jobs of benchmarks `ids` on Jobs, every benchmark's
     without any, with why under it or, where `brief`, on hovering it. -->
{#snippet states(parts: ReturnType<typeof jobParts>, why: [string, number][], ids: readonly string[], brief = false)}
  {#each parts as part (part.state)}
    {#if part.state === "failed"}
      <div title={brief ? why.map(([cause, n]) => `${cause}: ${n}`).join("\n") || undefined : undefined}>{@render dot(
        TONE.failed)}<a href={cellsWith(base, { benchmark: ids, state: ["failed"], device: picked ? [picked] : [] })}>{
        part.text}</a></div>
      {#if !brief}{@render causes(why)}{/if}
    {:else}
      <div>{@render dot(TONE[part.state])}{part.text}</div>
    {/if}
  {/each}
{/snippet}

{#snippet jobs(counts: JobCounts, why: [string, number][], ids: readonly string[], end: Edge)}
  {@const parts = jobParts(counts)}
  <td class="counts jobs" class:alone={!drawn?.totals.results} class:model-end={end === "model"}
      class:table-end={end === "table"}>
    {#if counts.total}{@render bar(parts)}{/if}
    {@render states(parts, why, ids)}
  </td>
{/snippet}

{#snippet awaiting(counts: ResultCounts)}
  {#if counts.pending}<div title={AWAITING}>{@render dot("waiting")}{counts.pending} awaiting rows</div>{/if}
{/snippet}

{#snippet scored(counts: ResultCounts, end: Edge)}
  <td class="counts results" class:model-end={end === "model"} class:table-end={end === "table"}>
    <div>{@render dot("good")}{counts.measured} measured</div>
    {@render awaiting(counts)}
  </td>
{/snippet}

<!-- Each flag set the cell merges, one under another, as `key: value` labels; the knobs that tell rows apart
     first, those every set of the column shares after and quieter. -->
{#snippet flags(cell: Spanned<[string, string][][]>, kind: FlagKind, shared: Cells["shared"])}
  <td rowspan={cell.span} class:model-end={cell.end === "model"} class:table-end={cell.end === "table"}>
    {#each cell.value as entries, i (i)}
      <div class="set">
        {#if entries.length}
          <div class="knobs">
            {#each knobLabels(entries, kind, shared) as label, j (j)}
              <span class="knob" class:quiet={label.shared}><span class="k">{label.key}:</span><code
                class="v">{label.value}</code></span>
            {/each}
          </div>
        {:else}—{/if}
      </div>
    {/each}
  </td>
{/snippet}

{#snippet dot(tone: string)}<span class="dot" data-tone={tone}></span>{/snippet}

<!-- Failure causes under the count they explain, a count each only where there are several. -->
{#snippet causes(list: [string, number][])}
  {#each list as [cause, n] (cause)}<div class="muted cause">{list.length > 1 ? `${cause}: ${n}` : cause}</div>{/each}
{/snippet}

<!-- Not "Cells": Jobs lists them one by one. -->
<h2>Progress</h2>
{#if catalogOrg}<CatalogEntryAction org={catalogOrg} onnamed={() => readNames(catalogOrg.id, digests)}
                                    bind:this={entries} />{/if}
{#if devices.length}<TabBar label="Device type" items={devices} sub />{/if}
{#if ranOn}<p class="muted">{ranOn}</p>{/if}
{#if cells.error}
  <p class="err" role="alert">{cells.error}</p>
  <p>{@render again()}</p>
{:else if !drawn}
  <!-- The plan's own count comes with it; the jobs and their results take longer. -->
  <p class="muted">{#if counted}Jobs {counted}; reading their results…{:else}Reading progress…{/if}</p>
{:else}
  {#if drawn.rows}
    {@const totals = drawn.totals}
    {@const parts = jobParts(totals.jobs)}
    {#if drawn.unplaced}<p class="muted">{count(drawn.unplaced, "job")} with no stored spec, not drawn.</p>{/if}
    {#if namesFailed}<p class="muted">The catalog names did not read, so no runtime or model offers an entry. {@render again()}</p>{/if}
    <div class="wide">
      <table>
        <!-- The header is the table's summary: each column named over what it holds in all. -->
        <thead><tr>
          <th class="counts jobs" class:alone={!totals.results}>
            <span class="label">jobs</span><span class="total">{doneOf(totals.jobs)}<span
              class="of">/{totals.jobs.total} done</span></span>
            {#if totals.jobs.total}{@render bar(parts)}{/if}
            {@render states(parts.slice(1), totals.results?.causes ?? [], [], true)}
          </th>
          {#if totals.results}
            <th class="counts results">
              <span class="label">results</span><span class="total">{totals.results.measured}
                <span class="of">measured</span></span>
              {@render awaiting(totals.results)}
              {#if reread}<div class="reread">{@render again()}</div>{/if}
            </th>
          {/if}
          <th><span class="label">models</span><span class="total">{totals.models}</span></th>
          <th><span class="label">runtimes</span><span class="total">{totals.runtimes}</span></th>
          {#each drawn.kinds as kind (kind)}
            <th><span class="label">{kind} flags</span><span class="total">{drawn.sets[kind]}
              <span class="of">{plural(drawn.sets[kind], "set")}</span></span></th>
          {/each}
          <th><span class="label">benchmarks</span><span class="total">{totals.benchmarks}
            <span class="of">in {count(totals.types, "type")}</span></span></th>
        </tr></thead>
        <tbody>
          {#each drawn.rows as row (row.key)}
            {@const ided = idLabels(row.benchmark_type, row.benchmark_ids)}
            <tr>
              <!-- What a row came to first, so it stays in sight as the table scrolls sideways to what it asks. -->
              {@render jobs(row.jobs, row.results?.causes ?? [], row.benchmark_ids, row.end)}
              {#if row.results}{@render scored(row.results, row.end)}{/if}
              {#if row.model}{@render named(row.model, "model", drawn.modelNames, drawn.modelDigests)}{/if}
              {#if row.runtime}{@render named(row.runtime, "runtime", drawn.runtimeNames, drawn.runtimeDigests)}{/if}
              {#each drawn.kinds as kind (kind)}
                {@const cell = row.flags[kind]}
                {#if cell}{@render flags(cell, kind, drawn.shared)}{/if}
              {/each}
              <td class:model-end={row.end === "model"} class:table-end={row.end === "table"}>
                {#if row.benchmark_type}
                  {@const type = row.benchmark_type}
                  {@const report = reportOf(type)}
                  {#if report}<a href={tabLink(base, "analysis", report, sharedQuery(here.query))}
                                 title={shortLabel(type)}>{type}</a>{:else}{type}{/if}
                {:else}<span class="muted">untyped</span>{/if}
                <span class="count">{row.benchmark_ids.length}</span>
                {#if ided.shared}<code class="shared" title="every id of this row starts so">{ided.shared}…</code>{/if}
                <!-- Its ids under it as labels, the prefixes they share dropped: `256` under `prefill_throughput`. -->
                <div class="ids">{#each ided.labels as id, i (i)}<span class="knob quiet"
                  title={row.benchmark_ids[i]}><code class="v">{id}</code></span>{/each}</div>
              </td>
            </tr>
          {/each}
        </tbody>
      </table>
    </div>
  {:else}
    <p class="muted">No job of this plan names a spec this server reads, so its cells are not drawn.</p>
  {/if}
{/if}

<style>
  /* Values and ids wrap at spaces, never mid-word. */
  .wide td { white-space: normal; }
  /* Each column as wide as what it holds, the table scrolling sideways. */
  table { width: max-content; min-width: 100%; }
  /* One whole line per descriptor, so a list of them reads one per line. A named descriptor's balloon wraps it. */
  .named .list > div > code.descriptor { width: auto; white-space: nowrap; word-break: normal; }
  .named .list > div + div { margin-top: 2px; }
  /* A compact button, so it fits the cell under its runtime or model. */
  .named .entry { margin: 4px 0 2px; font-size: 12px; }
  .named .entry button { padding: 2px 8px; font-size: 11px; }
  /* A named descriptor is two lines, so a gap parts it from its neighbours. */
  .named .list > div:has(:global(.named-as)):not(:first-child) { margin-top: 8px; }
  .named .list > div:has(:global(.named-as)):not(:last-child) { margin-bottom: 8px; }
  .set + .set { margin-top: 6px; padding-top: 6px; border-top: 1px dashed var(--border); }
  /* A set's labels wrap within the cell, each whole: a knob's name, then its value as written. */
  .knobs { display: flex; flex-wrap: wrap; gap: 4px; max-width: 30em; }
  .knob { display: inline-flex; align-items: baseline; gap: 4px; padding: 1px 8px; border: 1px solid var(--border-strong);
          border-radius: 10px; font-size: 12px; white-space: nowrap; }
  .knob .k { color: var(--text-muted); }
  .knob .v { font-size: 11px; color: var(--text); }
  .knob.quiet { border-color: var(--border); background: var(--surface); }
  .knob.quiet .v { color: var(--text-muted); }
  /* A cell spans its group, so a line marks where a group ends, the stronger one a model's. */
  td.model-end { border-bottom-color: var(--border-strong); }
  td.table-end { border-bottom: 0; }
  tbody tr:hover > td { background: none; }
  td.counts { white-space: nowrap; }
  .cause { padding-left: 14px; font-size: 11px; white-space: normal; min-width: 8em; }
  /* The status pills' colours; a 2px gap between segments, text left in ink beside its dot. */
  [data-tone=good] { --tone: var(--green); }
  [data-tone=bad] { --tone: var(--red); }
  [data-tone=active] { --tone: var(--accent); }
  [data-tone=waiting] { --tone: var(--amber); }
  [data-tone=idle] { --tone: var(--band); }
  [data-tone=off] { --tone: var(--grid); }
  .bar { display: flex; gap: 2px; height: 6px; min-width: 12em; margin: 4px 0 6px; }
  .bar span { flex: 0 1 0; min-width: 3px; border-radius: 3px; background: var(--tone); }
  .dot { display: inline-block; width: 8px; height: 8px; margin-right: 6px; border-radius: 50%;
         background: var(--tone); }
  /* A type's ids as labels, as the flags beside them, wrapping so a long sweep stays a few lines. */
  .ids { display: flex; flex-wrap: wrap; gap: 4px; max-width: 30em; margin-top: 4px; }
  .count, .shared { margin-left: 6px; color: var(--text-muted); font-size: 11px; }
  /* What the cells came to, set apart from what they ask for. */
  .counts.results, .counts.jobs.alone { border-right: 1px solid var(--border-strong); }
  /* The summary header: on the page's own ground, a quiet name over a figure, a firm rule under it. */
  th { padding: 8px 8px 10px; background: var(--bg); font-size: 12px; font-weight: 400; white-space: normal;
       border-right: 0; border-bottom: 2px solid var(--border-strong); }
  th .label { display: block; color: var(--text-muted); font-size: 11px; }
  th .total { display: block; margin: 1px 0 2px; font-size: 16px; font-weight: 600; white-space: nowrap; }
  th .of { color: var(--text-muted); font-size: 12px; font-weight: 400; }
  .reread { margin-top: 4px; }
</style>
