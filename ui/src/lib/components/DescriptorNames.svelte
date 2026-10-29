<script lang="ts">
  // What a descriptor's vault sources read as (`namedAs`), over the descriptor itself, each linking to its
  // resource's page. Nothing for a descriptor that names no vault resource, or none the reader may read.
  import { vaultLink } from "../routes";
  import { namedAs } from "../vault";

  let { descriptor, names }: {
    /** A model or runtime descriptor, as stored. */
    descriptor: unknown;
    /** The read's names: the vault resources the reader may read. */
    names: Record<string, string>;
  } = $props();

  const named = $derived(namedAs(descriptor, names));
</script>

{#each named as { id, name } (id)}
  <div class="named-as"><a href={vaultLink(id)}>{name}</a></div>
{/each}

<style>
  .named-as { margin-bottom: 2px; white-space: normal; }
</style>
