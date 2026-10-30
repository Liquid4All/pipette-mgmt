<script lang="ts">
  // The fleet's filter form (`clients.html`, `console-filters.js`): a status and
  // repeatable tag and capability clauses, edited as a draft until Apply hands
  // the page its pairs for the address bar.
  import { CLAUSES, effectiveStatus, filtering, pagePairs, type ClauseKey, type FleetFilters } from "../fleet";

  let { filters, statuses, onapply, note = "" }: {
    filters: FleetFilters;
    statuses: readonly string[];
    onapply: (pairs: [string, string][]) => void;
    /** Drawn beside Apply, such as the branch's count. */
    note?: string;
  } = $props();

  // Each clause keeps one blank field to type into.
  const draft = (read: FleetFilters) => Object.fromEntries(CLAUSES.map(({ key }) => {
    const typed = read.clauses[key].filter((value) => value.trim());
    return [key, typed.length ? typed : [""]];
  })) as Record<ClauseKey, string[]>;

  let status = $state("");
  let clauses = $state(Object.fromEntries(CLAUSES.map(({ key }) => [key, [""]])) as Record<ClauseKey, string[]>);
  const drafted = $derived(pagePairs({ status, clauses, limit: filters.limit }));
  const dirty = $derived(JSON.stringify(drafted) !== JSON.stringify(pagePairs(filters)));

  // The draft starts over from the address whenever the address changes.
  $effect.pre(() => {
    status = effectiveStatus(filters);
    clauses = draft(filters);
  });

  function apply(event: SubmitEvent) {
    event.preventDefault();
    onapply(drafted);
  }

  function remove(key: ClauseKey, index: number) {
    clauses[key].splice(index, 1);
    if (!clauses[key].length) clauses[key].push("");
  }
</script>

<form onsubmit={apply}>
  <div class="bar">
    <label><span>status</span>
      <select bind:value={status}>
        <option value="">any status</option>
        {#each statuses as each (each)}<option value={each}>{each}</option>{/each}
      </select>
    </label>
    <button type="submit">Apply</button>
    {#if dirty}<span class="dirty">Not applied yet — press Apply</span>{/if}
    {#if note}<span class="muted">{note}</span>{/if}
  </div>

  <!-- Folded until one is in use: most reads want the status alone. -->
  <details open={filtering(filters)}>
    <summary>Tag and capability filters</summary>
    <div class="clauses">
      {#each CLAUSES as clause (clause.key)}
        <fieldset class="clause">
          <legend>{clause.label}</legend>
          <p class="muted" id="help-{clause.key}">{clause.help}</p>
          {#each clauses[clause.key] as _, index (index)}
            <div class="entry">
              <input type="text" bind:value={clauses[clause.key][index]} placeholder={clause.placeholder}
                     aria-label="{clause.label} {index + 1}" aria-describedby="help-{clause.key}"
                     autocomplete="off" spellcheck="false">
              <button type="button" aria-label="Remove {clause.label} {index + 1}"
                      onclick={() => remove(clause.key, index)}>Remove</button>
            </div>
          {/each}
          <button type="button" onclick={() => clauses[clause.key].push("")}>Add</button>
        </fieldset>
      {/each}
    </div>
  </details>
</form>
