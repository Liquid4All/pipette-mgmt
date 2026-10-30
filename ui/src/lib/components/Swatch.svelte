<script lang="ts">
  // A series' mark as legends and tooltips show it: its hue and marker shape.
  import { markPath } from "../chart";

  let { hue, shape }: { hue: number; shape: number } = $props();
</script>

<svg class="swatch" viewBox="0 0 12 12" aria-hidden="true"><path d={markPath(shape, 6, 6, 4.5)} fill="var(--series-{hue})" /></svg>

<style>
  .swatch { width: 12px; height: 12px; flex: none; overflow: visible; }
</style>
