<script lang="ts">
  // A revision's dropped jobs, as the document page's confirmation lists them.
  let { ids }: { /** The current plan's jobs the revision stops. */ ids: string[] } = $props();
</script>

{#if ids.length}
  <details>
    <summary>Jobs it stops ({ids.length})</summary>
    <pre>{ids.join("\n")}</pre>
  </details>
{/if}
