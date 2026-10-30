<script lang="ts">
  // One line panel of the results report, after pipette-duckdb's export report:
  // a series per member over the x rungs, and a crosshair listing every
  // visible series at the rung nearest the pointer.
  import type { SvelteSet } from "svelte/reactivity";
  import { axis, labelled, markPath, scale, tickText, withUnit, xScale } from "../chart";
  import { when } from "../format";
  import { BEYOND_LIMIT, cellText, type LinePanel, type LineSeries, type Rung } from "../report";
  import ChartLabel from "./ChartLabel.svelte";
  import ChartTip from "./ChartTip.svelte";
  import Swatch from "./Swatch.svelte";

  let { panel, series, rungs, x: param, names, job, checked, rerunnable }: {
    panel: LinePanel;
    /** The series the legend leaves on. */
    series: LineSeries[];
    rungs: Rung[];
    /** The x parameter, the axis title. */
    x: string;
    /** The read's names: the vault resources the reader may read. */
    names: Record<string, string>;
    /** A job's page on Jobs. */
    job: (jobId: string) => string;
    /** The jobs checked for one re-run, over every chart. */
    checked: SvelteSet<string>;
    /** Whether the reader may re-run a job, which has answered. */
    rerunnable: (jobId: string) => boolean;
  } = $props();

  const MIN = 480, MAX = 1600, L = 74, R = 18, T = 24, B = 46;
  // Drawn at the figure's own width, so 11px text stays 11px; narrower than MIN it scrolls instead.
  let width = $state(0);
  const W = $derived(Math.max(MIN, Math.min(MAX, width || 900)));
  // A panel that fills a wide page stands taller, so its lines keep their slope.
  const H = $derived(W > 1100 ? 314 : 258);
  // A screen reader hears the unit after the title, so a layer whose title names a type adds its metric first.
  const named = $derived(withUnit([panel.title, panel.metric?.label].filter(Boolean).join(" · "), panel.unit));

  const ax = $derived(axis(series.flatMap((s) => s.cells.flatMap((c) => (c ? [c.min, c.max] : []))),
                           { zero: panel.zero, band: panel.band,
                             include: panel.reference === null ? [] : [panel.reference] }));
  const px = $derived(xScale(rungs.map((r) => r.x), L, W - R));
  const py = $derived(scale(ax.lo, ax.hi, H - B, T));
  // Short rungs (an image count) read upright; a lone turned `1` reads as a stroke.
  const upright = $derived(rungs.every((r) => String(r.x).length <= 3));
  // A label crowding the one before is left to the tooltip; an upright one needs its own width, 11px digits ~7px.
  const shown = $derived(labelled(rungs.map((r) => px(r.x)),
    upright ? 7 * Math.max(...rungs.map((r) => String(r.x).length)) + 4 : 14));

  /** The median line, broken at a gap: a segment across one asserts a measurement nobody took. */
  function line(s: LineSeries): string {
    let d = "";
    let open = false;
    s.cells.forEach((c, i) => {
      if (!c) {
        open = false;
        return;
      }
      d += `${open ? "L" : "M"}${px(rungs[i]!.x).toFixed(1)} ${py(c.median).toFixed(1)}`;
      open = true;
    });
    return d;
  }

  let at = $state<number | null>(null);
  let pointer = $state({ x: 0, y: 0 });
  // The pointer in the chart's own units, and how many of them a screen pixel spans.
  let hover = $state<{ u: number; v: number } | null>(null);
  let unit = $state(1);

  function place(event: MouseEvent): { u: number; v: number } {
    const box = (event.currentTarget as SVGSVGElement).getBoundingClientRect();
    unit = W / box.width;
    return { u: ((event.clientX - box.left) / box.width) * W, v: ((event.clientY - box.top) / box.height) * H };
  }
  function move(event: PointerEvent) {
    hover = place(event);
    const { u } = hover;
    let best = 0;
    rungs.forEach((r, i) => {
      if (Math.abs(px(r.x) - u) < Math.abs(px(rungs[best]!.x) - u)) best = i;
    });
    at = rungs.length ? best : null;
    pointer = { x: event.clientX, y: event.clientY };
  }
  const leave = () => {
    at = null;
    hover = null;
  };
  // A filter typed while the pointer rests here can take the rung away.
  const rung = $derived(at === null ? null : (rungs[at] ?? null));
  const listed = $derived(rung ? series.filter((s) => s.cells[at!]) : []);

  // A click takes every point within REACH screen pixels of it, a ring that follows the pointer showing how far.
  const REACH = 14;
  const near = (u: number, v: number) => series.flatMap((s) => s.cells.flatMap((c, i) =>
    c && Math.hypot(px(rungs[i]!.x) - u, py(c.median) - v) / unit <= REACH ? [{ s, i, c }] : []))
    .sort((a, b) => b.c.median - a.c.median);
  let figure = $state<HTMLElement>();
  let pinned = $state<{ u: number; v: number; left: number; top: number } | null>(null);
  const caught = $derived(pinned ? near(pinned.u, pinned.v) : []);
  // The card's width, and its gap from the click: it opens to the left where the right would leave the panel.
  const CARD = 360, GAP = 18;
  // From the click itself, as a tap moves no pointer first.
  function pick(event: MouseEvent) {
    const at = place(event);
    const frame = figure!.getBoundingClientRect();
    // In the figure's own coordinates, which a narrow panel scrolls.
    const shift = figure!.scrollLeft;
    const [x, y] = [event.clientX - frame.left + shift, event.clientY - frame.top];
    const left = x + GAP + CARD <= shift + figure!.clientWidth ? x + GAP : Math.max(shift, x - GAP - CARD);
    pinned = near(at.u, at.v).length ? { ...at, left, top: Math.max(0, y - 24) } : null;
  }
  // A point's jobs, one entry a job: a point is one job, a re-run kept beside it another.
  const jobsOf = (rows: LineSeries["points"][number]) => [...new Map(rows.map((row) => [row.job_id, row])).values()];
  // The jobs here a check may take, for the card's check-all.
  const offered = $derived([...new Set(caught.flatMap(({ s, i }) => (s.points[i] ?? []).map((row) => row.job_id)))]
    .filter(rerunnable));
</script>

<svelte:window onkeydown={(e) => e.key === "Escape" && (pinned = null)} />

<figure bind:this={figure} bind:clientWidth={width} class:scroll={width > 0 && width < MIN}>
  <!-- HTML, not SVG text, so a long title wraps at a narrow width. -->
  <figcaption style:padding-left="{L}px">{panel.title}
    {#each panel.runtime as line, i (i)}<span class="runtime"><ChartLabel label={[line]} {names} /></span>{/each}</figcaption>
  <!-- Jobs lists the same jobs by keyboard; a click here is a shortcut to them. -->
  <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_noninteractive_element_interactions -->
  <svg viewBox="0 0 {W} {H}" style:min-width="{MIN}px" role="img" aria-label={named}
       onpointermove={move} onpointerleave={leave} onclick={pick}>
    {#if panel.band}
      <rect class="band" x={L} y={py(panel.band)} width={W - R - L} height={Math.abs(py(-panel.band) - py(panel.band))} />
    {/if}
    {#each ax.ticks as v (v)}
      <line class="grid" x1={L} x2={W - R} y1={py(v)} y2={py(v)} />
      <text class="tick" x={L - 8} y={py(v) + 4} text-anchor="end">{tickText(v, ax.step)}</text>
    {/each}
    {#if panel.reference !== null && panel.reference >= ax.lo && panel.reference <= ax.hi}
      <line class="reference" x1={L} x2={W - R} y1={py(panel.reference)} y2={py(panel.reference)} />
    {/if}
    {#each rungs as r, i (r.x)}
      <line class="grid" x1={px(r.x)} x2={px(r.x)} y1={H - B} y2={H - B + 4} />
      {#if shown[i]}
        <text class="tick" x={px(r.x)} y={H - B + 14} text-anchor={upright ? "middle" : "end"}
              transform={upright ? undefined : `rotate(-45 ${px(r.x)} ${H - B + 14})`}>{r.x}</text>
      {/if}
    {/each}
    <text class="tick" x={W - R} y={H - 4} text-anchor="end">{param}</text>
    <!-- The unit tops the y axis: the title names a device type. -->
    {#if panel.unit}<text class="tick" x={L - 8} y={T - 12} text-anchor="end">{panel.unit}</text>{/if}

    {#each series as s (s.key)}
      <path class="line" d={line(s)} stroke="var(--series-{s.hue})" />
    {/each}
    {#each series as s (s.key)}
      {#each s.cells as c, i (i)}
        {#if c && c.n > 1}
          {#each c.values as v, j (j)}
            <circle class="value" cx={px(rungs[i]!.x)} cy={py(v)} r="3" stroke="var(--series-{s.hue})" />
          {/each}
        {/if}
      {/each}
    {/each}
    {#each series as s (s.key)}
      {#each s.cells as c, i (i)}
        {#if c?.beyond}<circle class="beyond" cx={px(rungs[i]!.x)} cy={py(c.median)} r="8"><title>{BEYOND_LIMIT}</title></circle>{/if}
      {/each}
    {/each}
    {#each series as s (s.key)}
      {#each s.cells as c, i (i)}
        {#if c}<path class="mark" d={markPath(s.shape, px(rungs[i]!.x), py(c.median), 4)} fill="var(--series-{s.hue})" />{/if}
      {/each}
    {/each}
    {#if pinned}
      <circle class="reach pinned" cx={pinned.u} cy={pinned.v} r={REACH * unit} />
    {:else if hover}
      <circle class="reach" cx={hover.u} cy={hover.v} r={REACH * unit} />
    {/if}
    {#if rung}
      <line class="cross" x1={px(rung.x)} x2={px(rung.x)} y1={T} y2={H - B} />
    {/if}
  </svg>
  <!-- The values for a screen reader, which the svg's marks do not reach. -->
  <table class="sr">
    <caption>{named}</caption>
    <thead><tr><th>{param}</th>{#each rungs as r (r.x)}<th>{r.text}</th>{/each}</tr></thead>
    <tbody>
      {#each series as s (s.key)}
        <tr><th>{s.label}</th>{#each s.cells as c, i (i)}<td>{c ? cellText(c) : "—"}{c?.beyond ? ` (${BEYOND_LIMIT})` : ""}</td>{/each}</tr>
      {/each}
    </tbody>
  </table>
  {#if pinned}
    <div class="card" style:left="{pinned.left}px" style:top="{pinned.top}px" style:width="{CARD}px" role="dialog"
         aria-label="The jobs under the click">
      <div class="head"><b>{caught.length} {caught.length === 1 ? "point" : "points"} here</b>
        {#if offered.length > 1}
          <label><input type="checkbox" checked={offered.every((id) => checked.has(id))}
                        onchange={(e) => offered.forEach((id) => (e.currentTarget.checked ? checked.add(id) : checked.delete(id)))} />
            check all {offered.length}</label>
        {/if}
        <button type="button" class="close" aria-label="Close" onclick={() => (pinned = null)}>✕</button></div>
      {#each caught as { s, i, c } (`${s.key}\n${i}`)}
        {@const jobs = jobsOf(s.points[i] ?? [])}
        <!-- A point once, then every job behind it: the E2E check draws one from three benchmarks' jobs. -->
        <div class="point">
          <div class="value"><Swatch hue={s.hue} shape={s.shape} /><b>{cellText(c)}</b> <span class="muted">{panel.unit}
            · {param}={rungs[i]!.text}{jobs.length > 1 ? ` · ${jobs.length} jobs` : ""}</span></div>
          {#if c.beyond}<span class="limit">{BEYOND_LIMIT}</span>{/if}
          <div class="model"><ChartLabel label={s.parts} {names} /></div>
          {#each jobs as row (row.job_id)}
            <div class="job">
              {#if rerunnable(row.job_id)}
                <input type="checkbox" aria-label="Check {row.job_id} to re-run" checked={checked.has(row.job_id)}
                       onchange={(e) => (e.currentTarget.checked ? checked.add(row.job_id) : checked.delete(row.job_id))} />
              {/if}
              <span><a href={job(row.job_id)}>{row.job_id}</a><br /><span class="muted">{row.benchmark_id}
                · {row.device_name ?? "—"} · {when(row.scored_at ?? row.submitted_at)}</span></span>
            </div>
          {/each}
        </div>
      {/each}
    </div>
  {/if}
  {#if rung && !pinned}
    <ChartTip x={pointer.x} y={pointer.y} head="{param}={rung.text}">
      {#each listed as s (s.key)}
        <div class="row">
          <Swatch hue={s.hue} shape={s.shape} />
          <span><b>{cellText(s.cells[at!]!)}</b> {panel.unit} <span class="muted">{s.label}</span>
            {#if s.cells[at!]!.beyond}<span class="limit">{BEYOND_LIMIT}</span>{/if}</span>
        </div>
      {:else}
        <div class="muted">no value here</div>
      {/each}
    </ChartTip>
  {/if}
</figure>

<style>
  figure { position: relative; margin: 6px 0 0; max-width: 1600px; }
  .scroll { overflow-x: auto; }
  svg { display: block; width: 100%; height: auto; overflow: visible; }
  .grid { stroke: var(--grid); stroke-width: 1; }
  .band { fill: var(--band); opacity: .15; }
  .reference { stroke: var(--text); stroke-width: 1; stroke-dasharray: 3 3; }
  .tick { font-size: 11px; fill: var(--text-muted); }
  figcaption { margin-bottom: 4px; font-size: 12px; font-weight: 500; color: var(--text); }
  /* The column's runtime, by its catalog name or as stored, on a line of its own under the device type. */
  .runtime { display: block; font: 400 10px/1.3 var(--mono); color: var(--text-muted); word-break: break-all; }
  .line { fill: none; stroke-width: 2; stroke-linejoin: round; stroke-linecap: round; }
  .value { fill: none; stroke-width: 1.25; }
  .mark { stroke: var(--bg); stroke-width: 2; }
  .beyond { fill: none; stroke: var(--text); stroke-width: 1.25; stroke-dasharray: 2 2; }
  /* The same label the Jobs table's issue chip prints. */
  .limit { display: inline-block; margin-top: 2px; padding: 0 6px; border-radius: 9px; font-size: 11px;
           white-space: nowrap; background: var(--amber-soft); color: var(--amber); }
  svg { cursor: crosshair; }
  .reach { fill: var(--text); fill-opacity: .04; stroke: var(--text-muted); stroke-width: 1; stroke-dasharray: 2 2;
           pointer-events: none; }
  .reach.pinned { fill-opacity: .08; stroke: var(--text); stroke-dasharray: none; }
  .card { position: absolute; z-index: 5; max-height: 360px; overflow-y: auto; padding: 8px 11px;
          border: 1px solid var(--border-strong); border-radius: 8px; background: var(--bg);
          box-shadow: 0 8px 28px var(--shadow); font-size: 12px; }
  .card .head { display: flex; justify-content: space-between; align-items: baseline; margin-bottom: 2px; }
  .card .close { padding: 0 4px; border: 0; background: none; color: var(--text-muted); }
  .card .point { padding: 6px 0; border-top: 1px solid var(--border); }
  .card .value { display: flex; gap: 6px; align-items: center; }
  .card .model { margin: 2px 0; font: 10px/1.3 var(--mono); color: var(--text-muted); word-break: break-all;
                 display: -webkit-box; -webkit-line-clamp: 2; line-clamp: 2; -webkit-box-orient: vertical; overflow: hidden; }
  .card .head label { display: inline-flex; gap: 4px; align-items: center; margin: 0 6px 0 auto; font-size: 12px;
                     color: var(--text-muted); }
  .card .job { display: flex; gap: 8px; align-items: flex-start; margin-top: 4px; }
  .card .job input { flex: none; margin: 3px 0 0; }
  .card .job > span { min-width: 0; font: 11px/1.4 var(--mono); word-break: break-all; }
  .cross { stroke: var(--text-muted); stroke-width: 1; stroke-dasharray: 2 3; }
  .row { display: flex; gap: 6px; align-items: flex-start; }
  .row :global(.swatch) { margin-top: 3px; }
</style>
