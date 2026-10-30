<script lang="ts">
  // The pages above this one, each a link where it has a page.
  import type { Crumb } from "../crumbs";

  let { trail, here = "" }: {
    trail: readonly Crumb[];
    /** This page as the trail's last step and the page's heading, where the title has no line of its own. */
    here?: string;
  } = $props();
</script>

{#if trail.length || here}
  <nav class="crumbs" aria-label="Breadcrumb">
    <ol>
      {#each trail as crumb, index (index)}
        <li>{#if crumb.href}<a href={crumb.href}>{crumb.label}</a>{:else}{crumb.label}{/if}</li>
      {/each}
      {#if here}<li aria-current="page"><h1>{here}</h1></li>{/if}
    </ol>
  </nav>
{/if}
