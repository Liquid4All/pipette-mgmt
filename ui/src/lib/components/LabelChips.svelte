<script lang="ts">
  // A record's labels as chips, a key then its value, in key order. Each links to `listing` narrowed to its
  // label, where one is given, and a remove beside each, where `remove` is. On `listing` itself, a chip switches
  // its label on or off in the labels filter. A chip whose label the address filters by is marked current.
  import { labelHue, labelLink, labelPairs, labelText, labelToggled } from "../campaigns";
  import { here } from "../route.svelte";

  let { labels, listing = null, remove }: {
    labels: Record<string, string> | undefined;
    /** The listing a chip opens, filtered by its label. */
    listing?: string | null;
    /** Take a label off the record named `from`; its buttons wait while `busy`. */
    remove?: { from: string; busy: boolean; drop: (key: string) => void };
  } = $props();

  const filtered = $derived(new Set(here.query.getAll("label")));
</script>

{#snippet text([key, value]: [string, string])}
  <span class="k">{key}</span>{#if value}<span class="v">{value}</span>{/if}
{/snippet}

<!-- The chip is a span around its text, linked where `listing` is, and its remove: a button cannot sit in a link. -->
<span class="chips">
  {#each labelPairs(labels) as pair (pair[0])}
    <span class="chip" style:--hue={labelHue(pair)}>
      {#if listing}<a aria-current={filtered.has(labelText(pair)) || undefined}
                      href={listing === here.path ? labelToggled(listing, here.query, pair) : labelLink(listing, pair)}>{@render text(pair)}</a>
      {:else}{@render text(pair)}{/if}
      {#if remove}<button type="button" class="drop" aria-label="Remove label {pair[0]} from {remove.from}"
                          aria-disabled={remove.busy} onclick={() => remove.drop(pair[0])}>×</button>{/if}
    </span>
  {/each}
</span>

<style>
  .chips { display: inline-flex; flex-wrap: wrap; gap: 4px; }
  /* The plan cells' knob: a name, then its value as written, tinted by the label's `--hue` as light as a status pill. */
  .chip { display: inline-flex; align-items: baseline; gap: 5px; padding: 1px 8px;
          border: 1px solid oklch(0.87 0.06 var(--hue)); border-radius: 10px; font-size: 12px; line-height: 1.5;
          white-space: nowrap; background: oklch(0.955 0.03 var(--hue)); }
  .k { color: oklch(0.52 0.07 var(--hue)); }
  .v { color: oklch(0.36 0.09 var(--hue)); font-weight: 500; }
  .chip:has(.drop) { padding-right: 3px; }
  a { display: inline-flex; align-items: baseline; gap: 5px; border-radius: 8px; color: inherit; }
  a:hover { text-decoration: none; }
  .chip:has(> a:hover) { border-color: oklch(0.62 0.12 var(--hue)); }
  /* An inset ring thickens the border without moving the chips beside it. */
  .chip:has(> a[aria-current]) { border-color: oklch(0.55 0.12 var(--hue));
                                 box-shadow: inset 0 0 0 1px oklch(0.55 0.12 var(--hue));
                                 background: oklch(0.91 0.05 var(--hue)); }
  /* A circle a shade deeper than the chip, so it reads apart from the label's text. */
  .drop { align-self: center; display: inline-grid; place-items: center; width: 15px; height: 15px; padding: 0;
          border: none; border-radius: 50%; font-size: 11px; line-height: 1; cursor: pointer;
          background: oklch(0.88 0.05 var(--hue)); color: oklch(0.36 0.09 var(--hue)); }
  .drop:hover { background: oklch(0.55 0.12 var(--hue)); color: var(--bg); }
</style>
