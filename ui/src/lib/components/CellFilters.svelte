<script lang="ts">
  // The Jobs table's filters, kept in the address: a text box applies once the typing rests, a select at once,
  // each from the first job.
  import { REPORTED, filtering, jobFilters, toggled, type ChoiceKey, type JobChoice, type JobFilters, type TextKey } from "../jobs";
  import { redirect } from "../route.svelte";
  import { Typing } from "../typing";
  import MultiSelect from "./MultiSelect.svelte";

  let { filters, choices, rows, reported = false, link }: {
    filters: JobFilters;
    choices: Record<ChoiceKey, JobChoice[]>;
    /** The results were read, so a row's selects have something to choose. */
    rows: boolean;
    /** The model and benchmark flags selects shown, not only one the address chose. */
    reported?: boolean;
    /** The listing's address under `filters`, from its first job. */
    link: (filters: JobFilters) => string;
  } = $props();

  const change = (key: TextKey, value: string) => redirect(link({ ...filters, [key]: value }));
  const typing = new Typing();
  $effect(() => () => typing.clear());
  const toggle = (key: ChoiceKey) => (value: string, on: boolean) => redirect(link(toggled(filters, key, value, on)));
</script>

{#snippet text(key: TextKey, label: string, placeholder: string)}
  <label><span>{label}</span>
    <input type="text" class="search" value={filters[key]} {placeholder} autocomplete="off" spellcheck="false"
           oninput={(e) => { const value = e.currentTarget.value; typing.later(key, () => change(key, value)); }}
           onchange={() => typing.flush(key)} /></label>
{/snippet}

{#snippet choice(key: ChoiceKey, label: string, options: JobChoice[], searchable = false)}
  <MultiSelect {label} {options} chosen={filters[key]} toggle={toggle(key)} {searchable} />
{/snippet}

<div class="bar filters">
  {@render text("q", "search", "job id, model, runtime, runtime flags")}
  {@render choice("state", "state", choices.state)}
  {@render choice("benchmark_type", "benchmark type", choices.benchmark_type)}
  {@render choice("benchmark", "benchmark", choices.benchmark)}
  <!-- Model or benchmark flags only where asked for, or where the address chose one, so it can be let go of. -->
  {#each REPORTED as [key, label] (key)}
    {#if key === "runtime_flags" || !key.endsWith("_flags") || reported || filters[key].length}
      {@render choice(key, label, choices[key], true)}
    {/if}
  {/each}
  {#if choices.device.length}{@render choice("device", "device", choices.device)}{/if}
  {#if rows}
    {@render choice("result", "result", choices.result)}
    {@render choice("metric", "metric", choices.metric)}
    {@render choice("spread", "stddev %", choices.spread)}
    {@render choice("issue", "issues", choices.issue)}
  {/if}
  {#if filtering(filters)}
    <button type="button" onclick={() => redirect(link({ ...jobFilters(new URLSearchParams()), sort: filters.sort }))}>
      Clear filters</button>
  {/if}
</div>

<style>
  .filters { align-items: end; }
  .filters input.search { width: 18em; }
  /* As tall as the selects' boxes, beside them. */
  .filters > button { box-sizing: border-box; min-height: 31px; }
</style>
