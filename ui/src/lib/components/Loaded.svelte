<script lang="ts" generics="T">
  // A read's answer, or that it is still on its way, or why it failed.
  import type { Snippet } from "svelte";
  import { unsized } from "../pager";
  import { here } from "../route.svelte";
  import type { Loader } from "./work.svelte";

  let { of, children }: { of: Loader<T>; children: Snippet<[T]> } = $props();

  // No listing form has a `limit` field, so this link is the only way off a refused one.
  const way = $derived(of.error ? unsized(here.path, here.query, of.status) : null);
</script>

<!-- In place, as the route words it: the server decides, and the page shows that decision. -->
{#if of.error}
  <p class="err" role="alert">{of.error}</p>
  {#if way}<p><a href={way}>Read this listing at the default page size</a></p>{/if}
{:else if of.data === null}
  <p class="muted">Loading…</p>
{:else}
  {@render children(of.data)}
{/if}
