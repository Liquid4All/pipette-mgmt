<script lang="ts">
  // A header that sorts its table by its column: up, down, then back to the
  // listing's order, kept in the address, or where `onsort` is given, in the page.
  // Where `resize` is given, a grip on its right edge drags the column's width.
  import { redirect } from "../route.svelte";
  import { ariaSort, nextSort } from "../table";
  import type { Resize } from "../widths.svelte";

  let { key, label, sort, link, onsort, num = false, resize }: {
    key: string;
    label: string;
    sort: string;
    /** The table's address under a sort. */
    link?: (sort: string) => string;
    /** A sort the page keeps, for a table that shares its address with another. */
    onsort?: (sort: string) => void;
    /** Right-aligned, as figures are. */
    num?: boolean;
    resize?: Resize;
  } = $props();

  const dir = $derived(ariaSort(sort, key));
  const to = $derived(link?.(nextSort(sort, key)));

  // In place, as a filter applies, so the page does not jump to its top.
  function click(event: MouseEvent) {
    if (event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
    event.preventDefault();
    redirect(to!);
  }

  let th = $state<HTMLTableCellElement>();
  /** Where the pointer and the width were as a drag started; `null` between drags. */
  let drag: { x: number; width: number } | null = null;

  function grab(event: PointerEvent) {
    if (!resize || event.button !== 0) return;
    event.preventDefault();
    (event.currentTarget as HTMLElement).setPointerCapture(event.pointerId);
    resize.start();
    drag = { x: event.clientX, width: resize.width ?? th!.offsetWidth };
  }

  function move(event: PointerEvent) {
    if (drag) resize!.to(drag.width + event.clientX - drag.x);
  }

  function release() {
    if (!drag) return;
    drag = null;
    resize!.done();
  }

  /** The step an arrow key moves the grip. */
  const STEP = 16;

  function nudge(event: KeyboardEvent) {
    const sign = event.key === "ArrowRight" ? 1 : event.key === "ArrowLeft" ? -1 : 0;
    if (!resize || !sign) return;
    event.preventDefault();
    resize.start();
    resize.to((resize.width ?? th!.offsetWidth) + sign * STEP);
    resize.done();
  }
</script>

<!-- An unsorted header shows both chevrons, so it reads as sortable before it is clicked. -->
{#snippet arrow()}<svg class="arrow" viewBox="0 0 12 12" aria-hidden="true">{#if dir === "ascending"}<path
  d="M3 7.5 6 4.5 9 7.5" />{:else if dir === "descending"}<path d="M3 4.5 6 7.5 9 4.5" />{:else}<path
  d="M3.5 4.5 6 2 8.5 4.5M3.5 7.5 6 10 8.5 7.5" />{/if}</svg>{/snippet}

<th bind:this={th} class:num aria-sort={dir} data-key={key}>{#if onsort}<button type="button" class="sort" class:on={dir}
  onclick={() => onsort(nextSort(sort, key))}>{label}{@render arrow()}</button>{:else}<a class="sort" class:on={dir}
  href={to} onclick={click}>{label}{@render arrow()}</a>{/if}{#if resize}
  <!-- A focusable separator is a control, as ARIA's window splitter is, though Svelte counts the role as static. -->
  <!-- svelte-ignore a11y_no_noninteractive_tabindex, a11y_no_noninteractive_element_interactions -->
  <div class="grip" role="separator" aria-orientation="vertical" tabindex="0" aria-label="Width of {label}"
       aria-valuenow={resize.width} onpointerdown={grab} onpointermove={move} onpointerup={release}
       onpointercancel={release} onkeydown={nudge}></div>{/if}</th>

<style>
  th { position: relative; }
  th.num { text-align: right; }
  .sort { color: inherit; text-decoration: none; }
  button.sort { padding: 0; border: 0; background: none; font: inherit; text-align: inherit; cursor: pointer; }
  .sort:hover, .sort.on { color: var(--accent); }
  .arrow { width: 12px; height: 12px; margin-left: 3px; vertical-align: -2px; fill: none; stroke: currentColor;
           stroke-width: 1.25; stroke-linecap: round; stroke-linejoin: round; }
  .sort:not(.on) .arrow { opacity: .4; }
  .sort:hover .arrow { opacity: 1; }
  /* Over the column's right rule, which it marks while the pointer or the focus is on it. */
  .grip { position: absolute; top: 0; right: 0; bottom: 0; width: 6px; cursor: col-resize; touch-action: none; }
  .grip:hover, .grip:focus-visible { background: color-mix(in srgb, var(--accent) 45%, transparent); outline: none; }
  /* Two faint dots beside the rule tell the reader that the rule moves. */
  .grip::before { content: ""; position: absolute; top: 50%; right: 2px; width: 2px; height: 2px;
                  margin-top: -3px; border-radius: 50%; background: var(--text-muted);
                  box-shadow: 0 4px var(--text-muted); opacity: .45; }
  th:hover .grip::before { opacity: .8; }
  .grip:hover::before, .grip:focus-visible::before { background: #fff; box-shadow: 0 4px #fff; opacity: 1; }
</style>
