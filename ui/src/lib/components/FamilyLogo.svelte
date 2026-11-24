<script lang="ts">
  // A model family's logo, read by its digest when a page first draws it.
  import { familyLogo } from "../family-logos";

  let { sha256 }: { sha256: string | null | undefined } = $props();
  const logo = $derived(sha256 ? familyLogo(sha256) : Promise.resolve(null));
</script>

{#await logo then src}{#if src}<img class="logo" {src} alt="" />{:else}—{/if}{/await}

<style>
  .logo { height: 20px; width: auto; vertical-align: middle; }
</style>
