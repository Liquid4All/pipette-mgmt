<script lang="ts" generics="R">
  // A listing's filters over the rows it read, as the Jobs tab draws them: a
  // text box per text filter, applied once the typing rests, the server's
  // filters, applied on change, and a multi-select per select, applied at once,
  // each from the first row, then how many rows they keep and the way back to all.
  // They live in the address, or where `onchange` is given, in the page.
  import { redirect } from "../route.svelte";
  import { cleared, filtering, toggledIn, type Choice, type TableSpec, type TableState } from "../table";
  import { Typing } from "../typing";
  import MultiSelect from "./MultiSelect.svelte";

  let { spec, state, choices, kept, total, noun, link, onchange }: {
    spec: TableSpec<R>;
    state: TableState;
    choices: Record<string, Choice[]>;
    kept: number;
    total: number;
    /** What a row is, plural: "campaigns". */
    noun: string;
    /** The listing's address under a state, from its first row. */
    link?: (state: TableState) => string;
    /** A state the page keeps, for a table that shares its address with another. */
    onchange?: (state: TableState) => void;
  } = $props();

  const apply = (to: TableState) => (onchange ? onchange(to) : redirect(link!(to)));
  // Read when it applies, so two boxes typed in turn both hold.
  const typed = (key: string, value: string) => apply({ ...state, texts: { ...state.texts, [key]: value } });
  const typing = new Typing();
  $effect(() => () => typing.clear());
  const toggle = (key: string) => (value: string, on: boolean) => apply(toggledIn(state, key, value, on));
</script>

<div class="bar filters">
  {#each spec.texts ?? [] as { key, label, placeholder, wide } (key)}
    <label><span>{label}</span>
      <input type="text" class:long={wide} value={state.texts[key]} placeholder={placeholder ?? "any"} autocomplete="off"
             spellcheck="false"
             oninput={(e) => { const value = e.currentTarget.value; typing.later(key, () => typed(key, value)); }}
             onchange={() => typing.flush(key)} /></label>
  {/each}
  {#each spec.server ?? [] as { key, label, placeholder, wide, options } (key)}
    <label><span>{label}</span>
      {#if options}
        <select value={state.texts[key]} onchange={(e) => typed(key, e.currentTarget.value)}>
          {#each options as option (option.value)}<option value={option.value}>{option.label}</option>{/each}
        </select>
      {:else}
        <input type="text" class:long={wide} value={state.texts[key]} placeholder={placeholder ?? "any"}
               autocomplete="off" spellcheck="false" onchange={(e) => typed(key, e.currentTarget.value.trim())} />
      {/if}</label>
  {/each}
  {#each spec.choices ?? [] as { key, label } (key)}
    <!-- A select with nothing to offer is left out; a choice the address holds is always offered. -->
    {#if choices[key]?.length}
      <MultiSelect {label} options={choices[key] ?? []} chosen={state.choices[key] ?? []} toggle={toggle(key)} />
    {/if}
  {/each}
  <span class="muted count">{kept < total ? `${kept} of ${total}` : total} {noun}
    {#if filtering(state)}{#if link}<a href={link(cleared(state))}>Clear filters</a>{:else}<button type="button"
      class="again" onclick={() => apply(cleared(state))}>Clear filters</button>{/if}{/if}</span>
</div>

<style>
  .filters input { width: 10em; }
  /* Not `wide`: that global class is a table wrapper's, with margins of its own. */
  .filters input.long { width: 26em; }
  .count { margin-left: auto; align-self: center; }
  .count a, .count button { margin-left: 8px; }
</style>
