<script lang="ts">
  // A page's heading: the pages above it, its title, and the one action it
  // opens, such as a create form.
  import type { Crumb } from "../crumbs";
  import Crumbs from "./Crumbs.svelte";

  let { trail = [], title, action = null, inline = false }: {
    trail?: readonly Crumb[];
    title: string;
    action?: { href: string; label: string } | null;
    /** The title is the trail's last step, with no line of its own: a page whose tabs or table say the rest. */
    inline?: boolean;
  } = $props();
</script>

{#if inline}
  <div class:head={!!action}>
    <Crumbs {trail} here={title} />
    {#if action}<a class="button" href={action.href}>{action.label}</a>{/if}
  </div>
{:else}
  <Crumbs {trail} />
  <!-- One heading whether or not an action joins it, so one arriving late never remounts the focused h1. -->
  <div class:head={!!action}>
    <h1>{title}</h1>
    {#if action}<a class="button" href={action.href}>{action.label}</a>{/if}
  </div>
{/if}
