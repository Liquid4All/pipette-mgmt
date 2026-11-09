<script lang="ts">
  // A descriptor under its catalog name (`descriptor_names`), the member key quieter. A click shows the descriptor
  // in a balloon by the name. The tooltip lists the other names. A descriptor with no name shows as it is. Under its
  // entry's name, the button shows the member key alone.
  import type { Snippet } from "svelte";
  import type { DescriptorDisplay } from "../api-types";
  import { memberKeyOf, synonymsTitle } from "../descriptor-names";
  import Balloon from "./Balloon.svelte";

  let { name, children, member = false }: {
    /** The descriptor's catalog names, or null for a descriptor with none. */
    name: DescriptorDisplay | null;
    /** Show the member key alone, for a member drawn under its entry's name. */
    member?: boolean;
    /** The descriptor, as drawn without a name. */
    children: Snippet;
  } = $props();
</script>

{#if name}
  <Balloon class="catalog-name" title={synonymsTitle(name) || undefined}
           label={member ? name.display : undefined} {children}>
    {#snippet face()}{#if member && memberKeyOf(name)}{memberKeyOf(name)}{:else}{name.name}<span class="member">{
      memberKeyOf(name)}</span>{/if}{/snippet}
  </Balloon>
{:else}{@render children()}{/if}

<style>
  :global(.again.catalog-name) { font-family: var(--mono); font-size: 12px; text-align: left; white-space: nowrap; }
  .member { color: var(--text-muted); }
</style>
