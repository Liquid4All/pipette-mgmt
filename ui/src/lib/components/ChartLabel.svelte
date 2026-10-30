<script lang="ts">
  // A chart's label (`Label`), with its parts joined by ` · `. A text part shows as plain text. A descriptor part shows
  // under its catalog name. A click on the name opens the descriptor (`CatalogName`).
  import type { Label } from "../report";
  import CatalogName from "./CatalogName.svelte";
  import DescriptorNames from "./DescriptorNames.svelte";

  let { label, names }: {
    label: Label;
    /** The read's names: the vault resources the reader may read. */
    names: Record<string, string>;
  } = $props();
</script>

<span class="chart-label">{#each label as part, i (i)}{#if i}{" · "}{/if}{#if typeof part === "string"}{part}{:else}<!--
  --><CatalogName name={part.name}><DescriptorNames descriptor={part.descriptor} {names} /><code
  class="descriptor">{part.descriptor}</code></CatalogName>{/if}{/each}</span>

<style>
  /* A catalog name takes the size of the label around it, and wraps as the label does. */
  .chart-label :global(.catalog-name) { font-size: inherit; white-space: normal; }
</style>
