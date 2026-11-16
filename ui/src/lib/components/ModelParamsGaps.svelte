<script lang="ts">
  // A campaign group's result rows that lack model params, summed over its member campaigns by the key their entry
  // takes, each with the write that adds that entry for a reader who may make it.
  import { tick } from "svelte";
  import { gapsTable, paramsGaps, paramsWrite, stateTitle, type ParamsGap } from "../campaign-groups";
  import { counts, noted } from "../model-params";
  import { vaultLink } from "../routes";
  import type { CampaignGroupPolicy, CreateModelParamsRequest, MemberPolicyPreview, OwnUserView } from "../api-types";
  import DataTable from "./DataTable.svelte";
  import { Busy } from "./work.svelte";

  let { rows, policy, names, reader, add }: {
    /** The preview's member campaigns. */
    rows: MemberPolicyPreview[];
    /** The model_params policy, whose state the heading says. */
    policy: CampaignGroupPolicy;
    names: Record<string, string>;
    reader: OwnUserView;
    /** Write the entry, then read the preview again. */
    add: (body: CreateModelParamsRequest) => Promise<void>;
  } = $props();

  const uid = $props.id();
  const gaps = $derived(paramsGaps(rows));
  const spec = $derived(gapsTable(names));
  const work = new Busy();
  /** The key the form adds an entry for; `null` while it is closed. */
  let adding = $state<string | null>(null);
  let total = $state<number | null>(null);
  let active = $state<number | null>(null);
  let note = $state("");
  let said = $state("");
  let first = $state<HTMLInputElement>();
  let status = $state<HTMLElement>();

  async function open(key: string) {
    if (work.busy) return;
    [adding, total, active, note, said, work.error] = [key, null, null, "", "", ""];
    await tick();
    first?.focus();
  }

  // The form's buttons go with it, so the focus moves to its key's button, or once that row is gone, to what was done.
  async function close() {
    const key = adding;
    adding = null;
    await tick();
    (document.getElementById(`${uid}-${key}`) ?? status)?.focus();
  }

  async function submit(event: SubmitEvent) {
    event.preventDefault();
    const model_key = adding;
    if (!model_key || work.busy) return;
    await work.attempt(async () => {
      await add({ model_key, ...counts(total, active), ...noted(note) });
      said = `Added the model params of ${model_key}.`;
      await close();
    });
  }
</script>

{#snippet write(gap: ParamsGap)}
  {@const where = paramsWrite(reader, gap.key)}
  {#if where === "vault"}
    <a href={vaultLink(gap.key)} aria-label="Add params for {gap.key}">Add params</a>
  {:else if where === "form"}
    <button type="button" id="{uid}-{gap.key}" aria-label="Add params for {gap.key}" aria-expanded={adding === gap.key}
            aria-disabled={work.busy} onclick={() => open(gap.key)}>Add params</button>
  {/if}
{/snippet}

<section aria-labelledby="{uid}-title">
  <h2 id="{uid}-title">{stateTitle(policy)}</h2>
  <p class="muted">The result rows of each campaign's answered jobs whose model has no entry in the model params table,
    so the plan pages show them without parameter counts, by the key that entry takes; a row that names no model is
    left out. The pages show an entry's counts on those rows once it is added.</p>
  <DataTable {spec} rows={gaps} id={(gap) => gap.key} noun="keys" empty="No row read lacks model params."
             action={gaps.some((gap) => paramsWrite(reader, gap.key)) ? write : undefined} />
  <div class="hint ok" role="status" tabindex="-1" hidden={!said} bind:this={status}>{said}</div>
  {#if adding}
    <form onsubmit={submit} aria-label="Add params for {adding}">
      <label><span>model_key</span><input type="text" value={adding} readonly></label>
      <label><span>total_millions</span><input type="number" min="1" bind:value={total} required bind:this={first}
                                               aria-describedby="{uid}-source"></label>
      <label><span>active_millions, the total when left empty</span>
        <input type="number" min="1" bind:value={active} aria-describedby="{uid}-source"></label>
      <label><span>note, where the counts come from</span>
        <input type="text" maxlength="500" bind:value={note} autocomplete="off" aria-describedby="{uid}-source"></label>
      <div id="{uid}-source" class="hint">An external model takes its publisher's reported counts, from its model card;
        only an internal LiquidAI model with no published figure is counted from its tensors.</div>
      <div class="bar">
        <button type="submit" class="primary" aria-disabled={work.busy}>Add params</button>
        <button type="button" onclick={close}>Cancel</button>
      </div>
    </form>
  {/if}
  {#if work.error}<p class="err" role="alert">{work.error}</p>{/if}
</section>
