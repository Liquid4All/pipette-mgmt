<script lang="ts">
  // One chart of the results page under its section: a line per runtime, whose dot a click draws or drops, then its
  // charts and a link to the rows they draw on Jobs. A legend entry's swatch hides or shows its series; a catalog
  // name beside a dot or a swatch opens its descriptor.
  import { SvelteSet } from "svelte/reactivity";
  import { drawnRows, labelText, partKey, type ReportView } from "../report";
  import type { ResultRow } from "../api-types";
  import BarChart from "./BarChart.svelte";
  import ChartLabel from "./ChartLabel.svelte";
  import LineChart from "./LineChart.svelte";
  import Swatch from "./Swatch.svelte";

  let { view, names, failed, runtime, jobs, job, checked, rerunnable }: {
    view: ReportView;
    /** The read's names: the vault resources the reader may read. */
    names: Record<string, string>;
    /** The family's failed rows, which a family with nothing measured links to. */
    failed: string;
    /** Draw a runtime, or drop it. */
    runtime: (variant: string) => void;
    /** The Jobs tab under the filters that keep the result rows drawn. */
    jobs: (rows: ReadonlySet<ResultRow>) => string;
    /** A job's page on Jobs. */
    job: (jobId: string) => string;
    /** The jobs checked for one re-run, over every chart. */
    checked: SvelteSet<string>;
    /** Whether the reader may re-run a job, which has answered. */
    rerunnable: (jobId: string) => boolean;
  } = $props();

  const runtimes = $derived(view.runtimes);

  const family = $derived(view.family!);

  // Series a legend turned off, by key: each is in one chart, a group's keyed apart, so one set serves them all.
  const hidden = new SvelteSet<string>();
  const toggle = (key: string) => (hidden.has(key) ? hidden.delete(key) : hidden.add(key));
  // Over every chart of the section, without a series a legend hides.
  const rows = $derived(drawnRows(view.charts, hidden));
</script>

{#if view.caption.length}<p class="muted"><ChartLabel label={view.caption} {names} /></p>{/if}
{#if runtimes.options.length}
  <!-- Every runtime is drawn until one is dropped; pressed is drawn. -->
  <div class="runtimes" role="group" aria-label="Runtimes, each with its runtime flags">
    {#each runtimes.options as variant (variant)}
      {@const lines = view.runtimeLines[variant] ?? [variant]}
      {@const drawn = !runtimes.dropped.includes(variant)}
      <div class="entry" class:off={!drawn}>
        <button type="button" class="toggle" aria-pressed={drawn} aria-label={labelText(lines)}
                onclick={() => runtime(variant)}><span class="dot"></span></button>
        <span>{#each lines as line, i (i)}<span class="line"><ChartLabel label={[line]} {names} /></span>{/each}</span>
      </div>
    {/each}
  </div>
{/if}
{#each view.notes as note, i (i)}<p class="muted">{note}</p>{/each}

{#if view.unmeasured !== null}
  <p>No measured results: <a href={failed}>{view.unmeasured} failed {view.unmeasured === 1 ? "job" : "jobs"}</a>.</p>
{:else if view.empty}
  <p class="muted">{view.empty}</p>
{/if}

{#each view.charts as chart, k (k)}
  {#if chart.group && chart.group !== view.charts[k - 1]?.group}<h3 class="group">{chart.group}</h3>{/if}
  <section>
    {#if chart.form === "line"}
      {#if chart.title && chart.group}<h4>{chart.title}</h4>{:else if chart.title}<h3>{chart.title}</h3>{/if}
      <!-- Above the panels, and for a lone series too: the last chart of a split reads as the others do. -->
      <div class="legend">
        {#each chart.legend as s (s.key)}
          <div class="entry" class:off={hidden.has(s.key)}>
            <button type="button" class="toggle" aria-pressed={!hidden.has(s.key)} aria-label={s.label}
                    onclick={() => toggle(s.key)}><Swatch hue={s.hue} shape={s.shape} /></button>
            <span><ChartLabel label={s.parts} {names} /></span>
          </div>
        {/each}
      </div>
      <!-- One chart's panels share its series and legend, so they compare across: a device type each. -->
      <div class="panels">
        {#each chart.panels as panel (`${panel.title}\n${panel.runtime.map(partKey).join("\n")}`)}
          {@const shown = panel.series.filter((s) => !hidden.has(s.key))}
          <div class="panel">
            <LineChart {panel} series={shown} rungs={chart.rungs} x={chart.x} {names} {job} {checked} {rerunnable} />
          </div>
        {/each}
      </div>
    {:else}
      <BarChart {chart} {names} />
    {/if}
  </section>
{/each}

{#if rows.size}
  <p><a href={jobs(rows)}>See the filtered jobs</a></p>
{/if}

<style>
  section { margin: 0 0 22px; }
  .group { margin: 4px 0 8px; font: 600 12px/1.3 var(--mono); }
  h4 { font-size: 13px; font-weight: 500; margin: 20px 0 8px; }
  .runtimes { display: flex; flex-direction: column; align-items: flex-start; margin: 6px 0 12px; }
  /* A runtime or a legend entry: its toggle, the dot or the swatch, then its label, which a catalog name in it opens. */
  .entry { display: flex; gap: 1px; align-items: flex-start; font: 10px/1.3 var(--mono); text-align: left;
           word-break: break-all; }
  /* An entry that is off shows its label in grey. The label stays readable: muted is 4.8:1 on white. */
  .entry.off { color: var(--text-muted); }
  .toggle { display: flex; flex: none; padding: 1px 4px; border: 0; background: none; color: inherit; }
  .toggle:hover { background: var(--hover); }
  .dot { width: 7.5px; height: 7.5px; margin: 2.25px 1.25px 0; border-radius: 50%; background: currentColor; }
  .off .dot { opacity: .3; }
  .runtimes .line { display: block; padding: 1px 0; }
  .panels { display: grid; grid-template-columns: repeat(auto-fit, minmax(min(100%, 520px), 1fr)); gap: 0 28px;
            align-items: start; }
  .panel { min-width: 0; }
  /* A legend names a model by its catalog name or its descriptor as stored, an entry a line. One longer than the
     line fills it and breaks anywhere: left to break at a hyphen, the browser would cut the line short at the last one. */
  .legend { display: flex; flex-direction: column; margin: 4px 0 8px; }
  .legend .entry > span { padding: 1px 0; }
  .legend .toggle :global(.swatch) { width: 10px; height: 10px; margin-top: 1px; flex: none; }
  .legend .off .toggle :global(.swatch) { opacity: .3; }
</style>
