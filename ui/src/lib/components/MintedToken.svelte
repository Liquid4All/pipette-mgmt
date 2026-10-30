<script lang="ts">
  // A pre-auth key just minted: its token, shown this once from the answer the
  // page holds, the facts about it, and how to hand it over. `ondone` hides it
  // inside a section; without one it is the page.
  import type { Snippet } from "svelte";

  let { token, facts, handover, ondone }: {
    token: string;
    /** The `<dt>`/`<dd>` pairs about the key. */
    facts: Snippet;
    /** Who it goes to and how. */
    handover: Snippet;
    ondone?: () => void;
  } = $props();
</script>

<svelte:element this={ondone ? "h3" : "h1"}>Pre-auth key minted</svelte:element>
<p class="notice">
  This is the only time the token is shown: the server keeps its hash and
  nothing else. Copy it now.
</p>
<pre><code>{token}</code></pre>
<dl>{@render facts()}</dl>
<svelte:element this={ondone ? "h4" : "h2"}>Hand it over</svelte:element>
{@render handover()}
{#if ondone}
  <div class="bar"><button type="button" onclick={ondone}>Done: hide the token</button></div>
{/if}
