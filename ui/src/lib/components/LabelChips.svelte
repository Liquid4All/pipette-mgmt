<script lang="ts">
  // A record's labels as chips, a key then its value, in key order. Each links to `listing` narrowed to its
  // label, where one is given, and a remove beside each, where `remove` is.
  import { labelLink, labelPairs } from "../campaigns";

  let { labels, listing = null, remove }: {
    labels: Record<string, string> | undefined;
    /** The listing a chip opens, filtered by its label. */
    listing?: string | null;
    /** Take a label off the record named `from`; its buttons wait while `busy`. */
    remove?: { from: string; busy: boolean; drop: (key: string) => void };
  } = $props();
</script>

{#snippet text([key, value]: [string, string])}
  <span class="k">{key}</span>{#if value}<span class="v">{value}</span>{/if}
{/snippet}

<!-- Two elements, not one `svelte:element`: its class goes through clsx, which the bundle would then carry. -->
<span class="chips">
  {#each labelPairs(labels) as pair (pair[0])}
    {#if listing}<a class="chip" href={labelLink(listing, pair)}>{@render text(pair)}</a>
    {:else}<span class="chip">{@render text(pair)}</span>{/if}
    {#if remove}<button type="button" class="drop" aria-label="Remove label {pair[0]} from {remove.from}"
                        aria-disabled={remove.busy} onclick={() => remove.drop(pair[0])}>×</button>{/if}
  {/each}
</span>

<style>
  .chips { display: inline-flex; flex-wrap: wrap; gap: 4px; }
  /* The plan cells' knob: a name, then its value as written. */
  .chip { display: inline-flex; align-items: baseline; gap: 5px; padding: 1px 8px; border: 1px solid var(--border-strong);
          border-radius: 10px; font-size: 12px; line-height: 1.5; white-space: nowrap; background: var(--bg); }
  .k { color: var(--text-muted); }
  .v { color: var(--text); font-weight: 500; }
  a.chip:hover { border-color: var(--accent); background: var(--accent-soft); }
  /* Snug to its chip: the chips' gap is between pairs. */
  .drop { margin-left: -3px; padding: 0 2px; border: none; background: none; font-size: 12px; line-height: 1.5;
          color: var(--text-muted); cursor: pointer; }
  .drop:hover { color: var(--red); }
</style>
