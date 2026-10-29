<script lang="ts">
  // A campaign's labels, a row of its Settings: free `key = value` pairs its readers filter the listings by. A key
  // and a value offer what the owner's other campaigns already use, so one organization's labels stay alike.
  import { untrack } from "svelte";
  import { campaign as campaignOf, updateCampaign } from "../api";
  import { LABELS_MAX, labelPairs, labelsBody, labelsProblem } from "../campaigns";
  import { session } from "../session.svelte";
  import type { NamedCampaignOverview } from "../api-types";
  import SettingRow from "./SettingRow.svelte";
  import { Busy } from "./work.svelte";

  let { detail, vocabulary, onopen, onchange }: {
    detail: NamedCampaignOverview;
    /** The labels the owner's campaigns use: each key, with the values it takes. */
    vocabulary: ReadonlyMap<string, readonly string[]>;
    /** Told once the row is drawn, for a page that reads `vocabulary` only then. */
    onopen?: () => void;
    /** Read the campaign again, after a write. */
    onchange: () => Promise<void>;
  } = $props();

  type Row = { key: string; value: string };
  const uid = $props.id();
  const stored = $derived(labelPairs(detail.labels));
  // Taken again from each read, as a write reads the campaign again.
  const asRows = (of: [string, string][]): Row[] => of.map(([key, value]) => ({ key, value }));
  let rows = $derived(asRows(stored));
  const pairs = $derived(rows.map((row): [string, string] => [row.key, row.value]));
  const changed = $derived(JSON.stringify(pairs.map(([key, value]) => [key.trim(), value.trim()]))
    !== JSON.stringify(stored));
  const problem = $derived(changed ? labelsProblem(pairs) : "");
  const saving = new Busy();
  /** What the last save did, until a label is typed again. */
  let done = $state("");

  $effect(() => untrack(() => onopen?.()));

  /** The rows are replaced, not written into: a derived's value is not deeply reactive. */
  const edit = (next: Row[]) => { rows = next; done = ""; };
  const typed = (i: number, field: keyof Row, text: string) =>
    edit(rows.map((row, at) => (at === i ? { ...row, [field]: text } : row)));

  async function save(event: SubmitEvent) {
    event.preventDefault();
    if (saving.busy || problem || !changed) return;
    const body = labelsBody(pairs);
    await saving.attempt(async () => {
      // The save is the whole set, so one over labels another reader changed meanwhile would undo theirs.
      const fresh = await campaignOf(session.signer, detail.campaign_id);
      if (JSON.stringify(labelPairs(fresh.labels)) !== JSON.stringify(stored)) {
        await onchange();
        throw new Error("The labels changed since this page read them; they are shown as they are now.");
      }
      await updateCampaign(session.signer, detail.campaign_id, body);
      await onchange();
      done = "Labels saved.";
    });
  }
</script>

<SettingRow title="Labels"
            text={`Pairs to group and filter campaigns by, such as study = nightly; up to ${LABELS_MAX}. A value may be left empty.`}>
  {#snippet children(ids)}
    <form novalidate onsubmit={save} aria-labelledby={ids.title} aria-describedby="{ids.text} {uid}-note">
      {#each rows as row, i (i)}
        <div class="bar">
          <input type="text" class="key" value={row.key} placeholder="key" autocomplete="off" list="{uid}-keys"
                 aria-label="Key of label {i + 1}" oninput={(e) => typed(i, "key", e.currentTarget.value)}>
          <input type="text" value={row.value} placeholder="value" autocomplete="off" list="{uid}-values-{i}"
                 aria-label="Value of label {i + 1}" oninput={(e) => typed(i, "value", e.currentTarget.value)}>
          <datalist id="{uid}-values-{i}">
            {#each vocabulary.get(row.key.trim()) ?? [] as value (value)}<option {value}></option>{/each}
          </datalist>
          <button type="button" aria-label="Remove label {i + 1}"
                  onclick={() => edit(rows.filter((_, at) => at !== i))}>Remove</button>
        </div>
      {/each}
      <datalist id="{uid}-keys">{#each vocabulary.keys() as key (key)}<option value={key}></option>{/each}</datalist>
      <div class="bar">
        <button type="button" aria-disabled={rows.length >= LABELS_MAX}
                onclick={() => { if (rows.length < LABELS_MAX) edit([...rows, { key: "", value: "" }]); }}>
          Add label</button>
        <button type="submit" class="primary" aria-disabled={saving.busy || !changed || !!problem}>{
          saving.busy ? "Saving…" : "Save changes"}</button>
        {#if changed}<button type="button" onclick={() => edit(asRows(stored))}>Reset</button>{/if}
      </div>
      <div id="{uid}-note" class="hint" class:err={!!problem} class:ok={!problem && !!done} role="status"
           hidden={!problem && !done}>{problem || done}</div>
      {#if saving.error}<p class="err" role="alert">{saving.error}</p>{/if}
    </form>
  {/snippet}
</SettingRow>

<style>
  .key { max-width: 14em; }
</style>
