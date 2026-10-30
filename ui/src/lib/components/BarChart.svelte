<script lang="ts">
  // One bar chart of the results report, in one hue, after pipette-duckdb's export report.
  import { barAxis, barPath, scale, tickText, withUnit } from "../chart";
  import { BEYOND_LIMIT, cellText, medianText, type BarChart } from "../report";
  import ChartLabel from "./ChartLabel.svelte";
  import ChartTip from "./ChartTip.svelte";

  let { chart, names }: {
    chart: BarChart;
    /** The read's names: the vault resources the reader may read. */
    names: Record<string, string>;
  } = $props();

  // The name gutter, then the bars, then room for the value at a full bar's tip.
  const MIN = 480, R = 84, T = 4, BAR = 14, LINE = 13, GLYPH = 6;
  // Drawn at the figure's own width, so 11px text stays 11px; narrower than MIN it scrolls instead.
  let width = $state(0);
  const W = $derived(Math.max(MIN, Math.min(1600, width || 900)));
  const NAME = $derived(Math.min(280, Math.max(120, Math.round(W * 0.32))));
  const L = $derived(NAME + 8);
  // A name wraps as a legend entry does, anywhere, and is drawn whole: counted a glyph short a line, so a font
  // wider than GLYPH wraps within the row rather than clipping its last line.
  const lines = $derived(Math.max(1, ...chart.bars.map((b) => Math.ceil(b.name.length / (Math.floor(NAME / GLYPH) - 1)))));
  const ROW = $derived(Math.max(22, LINE * lines + 8));

  const ax = $derived(barAxis(chart.bars.flatMap((b) => b.cell.values), chart.atLeast));
  const px = $derived(scale(0, ax.hi, L, W - R));
  const bottom = $derived(T + chart.bars.length * ROW);
  const H = $derived(bottom + 24);

  let at = $state<number | null>(null);
  let pointer = $state({ x: 0, y: 0 });
  const move = (event: PointerEvent) => (pointer = { x: event.clientX, y: event.clientY });
  const leave = () => (at = null);
</script>

<figure bind:clientWidth={width} class:scroll={width > 0 && width < MIN}>
  <!-- HTML, not SVG text, so a long title wraps at a narrow width. -->
  <figcaption>{withUnit(chart.title, chart.unit)}</figcaption>
  <div class="plot" role="presentation" style:min-width="{MIN}px" onpointermove={move} onpointerleave={leave}>
    <svg viewBox="0 0 {W} {H}" role="img" aria-label={chart.title}>
      {#each ax.ticks as v (v)}
        <line class="grid" x1={px(v)} x2={px(v)} y1={T} y2={bottom} />
        <text class="tick" x={px(v)} y={bottom + 14} text-anchor="middle">{tickText(v, ax.step)}</text>
      {/each}
      {#each chart.bars as bar, i (bar.key)}
        {@const y = T + i * ROW + (ROW - BAR) / 2}
        <path class="fill" class:lit={at === i} d={barPath(L, y, Math.max(0, px(bar.cell.median) - L), BAR, 4)} />
        {#if bar.cell.beyond}
          <path class="beyond" d={barPath(L, y, Math.max(0, px(bar.cell.median) - L), BAR, 4)}><title>{BEYOND_LIMIT}</title></path>
        {/if}
        {#if bar.cell.n > 1}
          {#each bar.cell.values as v, j (j)}
            <circle class="value" cx={px(v)} cy={y + BAR / 2} r="3" />
          {/each}
        {/if}
        <!-- Clear of the outermost run's ring, so the ring never reads as a glyph of the value. -->
        <text class="tip-value"
              x={Math.max(px(bar.cell.median), ...bar.cell.values.map(px)) + (bar.cell.n > 1 ? 10 : 6)} y={y + BAR - 3}>{
          medianText(bar.cell)}{#if bar.cell.n > 1}<tspan class="n">{` (n=${bar.cell.n})`}</tspan>{/if}{#if bar.cell.beyond}<tspan
          class="n">{` · ${BEYOND_LIMIT}`}</tspan>{/if}</text>
        <rect class="hit" aria-hidden="true" x="0" y={T + i * ROW} width={W} height={ROW}
              onpointerenter={() => (at = i)} onpointerleave={() => (at = null)} />
      {/each}
    </svg>
    <!-- The names are HTML over the svg's gutter, so a catalog name's balloon anchors to its name. Each name sits over
         its row's hit area, placed in fractions of the svg, and lights and clears the row as the hit area does. -->
    {#each chart.bars as bar, i (bar.key)}
      <div class="cell" role="presentation" style:top="{((T + i * ROW) / H) * 100}%" style:height="{(ROW / H) * 100}%"
           style:width="{(NAME / W) * 100}%" onpointerenter={() => (at = i)} onpointerleave={() => (at = null)}><div
        class="name"><ChartLabel label={bar.nameParts} {names} /></div></div>
    {/each}
  </div>
  <!-- The values for a screen reader, which the svg's marks do not reach. -->
  <table class="sr">
    <caption>{withUnit(chart.title, chart.unit)}</caption>
    <tbody>{#each chart.bars as bar (bar.key)}<tr><th>{bar.label}</th><td>{cellText(bar.cell)}{bar.cell.beyond ? ` (${BEYOND_LIMIT})` : ""}</td></tr>{/each}</tbody>
  </table>
  {#if at !== null && chart.bars[at]}
    <ChartTip x={pointer.x} y={pointer.y} head={chart.bars[at].label}>
      <div><b>{cellText(chart.bars[at].cell)}</b> {chart.unit}</div>
      {#if chart.bars[at].cell.beyond}<span class="limit">{BEYOND_LIMIT}</span>{/if}
    </ChartTip>
  {/if}
</figure>

<style>
  figure { margin: 6px 0 0; max-width: 1600px; }
  .scroll { overflow-x: auto; }
  .plot { position: relative; }
  svg { display: block; width: 100%; height: auto; overflow: visible; }
  .grid { stroke: var(--grid); stroke-width: 1; }
  .tick { font-size: 11px; fill: var(--text-muted); }
  figcaption { margin-bottom: 4px; font-size: 12px; font-weight: 500; color: var(--text); }
  .cell { position: absolute; left: 0; display: flex; align-items: center; justify-content: flex-end; }
  .name { font: 10px/13px var(--mono); color: var(--text-muted); text-align: right; word-break: break-all;
          line-break: anywhere; }
  .name :global(.catalog-name) { line-height: inherit; text-align: right; }
  .fill { fill: var(--series-1); }
  .fill.lit { opacity: .8; }
  /* The bar is in the series hue, so a value on it is ringed in the text tone. */
  .value { fill: none; stroke: var(--text); stroke-width: 1.25; }
  .tip-value { font-size: 11px; fill: var(--text); }
  .n { fill: var(--text-muted); }
  .hit { fill: transparent; }
  .beyond { fill: none; stroke: var(--text); stroke-width: 1.25; stroke-dasharray: 3 2; }
  /* The same label the Jobs table's issue chip prints. */
  .limit { display: inline-block; margin-top: 2px; padding: 0 6px; border-radius: 9px; font-size: 11px;
           white-space: nowrap; background: var(--amber-soft); color: var(--amber); }
</style>
