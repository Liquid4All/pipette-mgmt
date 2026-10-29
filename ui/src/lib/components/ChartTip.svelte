<script lang="ts">
  // A chart's tooltip, fixed beside the pointer and flipped where the viewport ends.
  import type { Snippet } from "svelte";
  import { tipPlace } from "../chart";

  let { x, y, head, children }: {
    /** The pointer, in viewport pixels. */
    x: number;
    y: number;
    head: string;
    children: Snippet;
  } = $props();

  let width = $state(0);
  let height = $state(0);
  const place = $derived(tipPlace(x, y, width, height, window.innerWidth, window.innerHeight));
</script>

<div class="tip" bind:offsetWidth={width} bind:offsetHeight={height} style:left="{place.left}px" style:top="{place.top}px">
  <div class="title">{head}</div>
  {@render children()}
</div>

<style>
  .tip { position: fixed; z-index: 20; pointer-events: none; max-width: 36em; padding: 6px 9px;
         background: var(--bg); border: 1px solid var(--border-strong); border-radius: var(--radius);
         box-shadow: 0 2px 8px #0000001f; font-size: 12px; }
  .title { font-weight: 500; margin-bottom: 2px; }
</style>
