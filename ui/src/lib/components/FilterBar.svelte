<script lang="ts">
  // A results tab's filter row: its selects. The tab applies a change, as it owns
  // its address.
  import type { Control } from "../report";

  let { controls, change }: {
    controls: (Control | null)[];
    /** Apply a select's choice. */
    change: (key: string, value: string) => void;
  } = $props();

  const shown = $derived(controls.some(Boolean));
</script>

{#snippet select(control: Control)}
  <label><span>{control.label}</span>
    <select value={control.value} disabled={control.disabled}
            onchange={(e) => change(control.key, e.currentTarget.value)}>
      {#each control.options as option (option.value)}
        <option value={option.value} disabled={option.disabled}>{option.label}</option>
      {/each}
    </select>
  </label>
{/snippet}

<!-- With no choice to offer, the row would be an empty band between two rules. -->
{#if shown}
  <div class="bar filters">
    {#each controls as control, i (i)}{#if control}{@render select(control)}{/if}{/each}
  </div>
{/if}

<style>
  .filters { padding: 10px 0; border-block: 1px solid var(--border); }
  .filters select { max-width: 24em; }
</style>
