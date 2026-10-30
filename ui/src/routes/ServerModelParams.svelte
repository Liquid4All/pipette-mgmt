<script lang="ts">
  // `/api/v1/model-params`: one row of parameter counts per model, each edited on
  // its own. A result row the warehouse holds no count for reads it from here.
  import { createModelParams, deleteModelParams, modelParams, updateModelParams } from "../lib/api";
  import ConfirmDialog from "../lib/components/ConfirmDialog.svelte";
  import DataTable from "../lib/components/DataTable.svelte";
  import Loaded from "../lib/components/Loaded.svelte";
  import PageHead from "../lib/components/PageHead.svelte";
  import { Busy, Loader } from "../lib/components/work.svelte";
  import { SERVER } from "../lib/crumbs";
  import { DELETE_ROW, MODEL_PARAMS_TABLE, counts, noted } from "../lib/model-params";
  import { pathOf, type RouteOf } from "../lib/routes";
  import { session } from "../lib/session.svelte";
  import type { ModelParams, ModelParamsPage } from "../lib/api-types";

  let { route }: { route: RouteOf<"server-model-params"> } = $props();

  const read = new Loader<ModelParamsPage>();
  const list = (keep = false) => read.run(() => modelParams(session.signer), keep);
  list();

  // The gate of the writes is the one creating an organization takes.
  const manages = $derived(!!session.account?.may.create_orgs);
  const work = new Busy();
  let confirm = $state<ReturnType<typeof ConfirmDialog>>();

  let modelKey = $state("");
  let total = $state<number | null>(null);
  let active = $state<number | null>(null);
  let note = $state("");

  async function add(event: SubmitEvent) {
    event.preventDefault();
    await work.attempt(async () => {
      await createModelParams(session.signer, { model_key: modelKey.trim(), ...counts(total, active), ...noted(note) });
      [modelKey, total, active, note] = ["", null, null, ""];
      await list(true);
    });
  }

  // The one row being edited, and its counts and note as typed.
  let editing = $state<string | null>(null);
  let editTotal = $state<number | null>(null);
  let editActive = $state<number | null>(null);
  let editNote = $state("");

  const edit = (row: ModelParams) => {
    editing = row.model_key;
    editTotal = row.total_millions;
    // Left empty for a dense model, so a new total stays the active count too.
    editActive = row.active_millions === row.total_millions ? null : row.active_millions;
    editNote = row.note ?? "";
  };

  const save = (model_key: string) => work.attempt(async () => {
    await updateModelParams(session.signer, model_key, { ...counts(editTotal, editActive), ...noted(editNote) });
    editing = null;
    await list(true);
  }, model_key);

  const remove = (model_key: string) => work.busy || void confirm?.ask({
    ...DELETE_ROW(model_key),
    run: async () => {
      await work.attempt(async () => {
        await deleteModelParams(session.signer, model_key);
        await list(true);
      });
      return work.error || null;
    },
  });
</script>

{#snippet total_millions(row: ModelParams)}
  {#if editing === row.model_key}
    <input class="count" type="number" min="1" required bind:value={editTotal} aria-label="total_millions of {row.model_key}">
  {:else}{row.total_millions}{/if}
{/snippet}
{#snippet active_millions(row: ModelParams)}
  {#if editing === row.model_key}
    <input class="count" type="number" min="1" bind:value={editActive} placeholder="total" aria-label="active_millions of {row.model_key}">
  {:else}{row.active_millions}{/if}
{/snippet}
{#snippet noteCell(row: ModelParams)}
  {#if editing === row.model_key}
    <input class="note" type="text" maxlength="500" bind:value={editNote} aria-label="note of {row.model_key}">
  {:else if row.note}<span class="wrapped">{row.note}</span>{:else}—{/if}
{/snippet}
{#snippet rowActions(row: ModelParams)}
  {#if editing === row.model_key}
    <button type="button" class="primary" aria-disabled={work.busy} onclick={() => save(row.model_key)}>Save</button>
    <button type="button" onclick={() => (editing = null)}>Cancel</button>
  {:else}
    <button type="button" aria-disabled={work.busy} aria-label="Edit {row.model_key}" onclick={() => edit(row)}>Edit</button>
    <button type="button" class="danger" aria-haspopup="dialog" aria-disabled={work.busy}
            aria-label="Delete {row.model_key}" onclick={() => remove(row.model_key)}>Delete</button>
  {/if}
{/snippet}

<PageHead trail={SERVER} title="Model params" />
<p class="muted">
  The model parameter table: parameter counts in millions, one row per model. A scoring
  pass stores a model's counts on the rows it writes, matched on their
  descriptor, and a result row the warehouse holds no count for reads them
  here. The <code>model_key</code> is the model's name as the table normalizes it:
  no organization, no <code>-GGUF</code> or quantization suffix, or a vault id.
</p>
{#if work.error}<p class="err" role="alert">{work.error}</p>{/if}

{#if manages}
  <form class="add" onsubmit={add}>
    <label><span>model_key</span>
      <input type="text" bind:value={modelKey} required autocomplete="off" spellcheck="false"
             placeholder="LiquidAI/LFM2-700M or vault_…">
    </label>
    <label><span>total_millions</span><input type="number" min="1" bind:value={total} required></label>
    <label><span>active_millions, the total when left empty</span><input type="number" min="1" bind:value={active}></label>
    <label><span>note, where the counts come from</span>
      <input type="text" maxlength="500" bind:value={note} autocomplete="off"
             placeholder="MoE: card states 1.5B active">
    </label>
    <button type="submit" class="primary" aria-disabled={work.busy}>Add row</button>
  </form>
{/if}

<Loaded of={read}>
  {#snippet children(listed)}
    {#if listed.unreadable}
      <div class="bar"><span class="err">{listed.unreadable} unreadable model-params row(s)</span></div>
    {/if}
    <DataTable spec={MODEL_PARAMS_TABLE} rows={listed.items} id={(row) => row.model_key} noun="rows" path={pathOf(route)}
               empty="No model has a row." cells={{ total_millions, active_millions, note: noteCell }}
               action={manages ? rowActions : undefined} />
  {/snippet}
</Loaded>
<ConfirmDialog bind:this={confirm} />

<style>
  .add { margin: 12px 0 20px; }
  .count { width: 8em; }
  .note { width: 28em; }
  /* A fixed width wraps a long note; the row stays as wide as the others. */
  .wrapped { display: block; width: 28em; white-space: normal; }
</style>
