<script lang="ts">
  // A campaign's labels as chips and, for a reader who may label it, an add beside them: one `key=value` box that
  // sets the key on the campaign's set, and a remove beside each chip. Renaming one is Settings' row.
  import { campaign as campaignOf, updateCampaign } from "../api";
  import { knownLabels, labelsBody, labelsProblem, parseLabel, withLabel, withoutLabel } from "../campaigns";
  import { session } from "../session.svelte";
  import type { CampaignView } from "../api-types";
  import LabelChips from "./LabelChips.svelte";
  import { Busy } from "./work.svelte";

  let { campaign, listing = null, editable, vocabulary, onopen, onchange }: {
    campaign: Pick<CampaignView, "campaign_id" | "name" | "labels">;
    /** The listing a chip opens, filtered by its label. */
    listing?: string | null;
    editable: boolean;
    /** The labels the owner's campaigns use, which the box offers. */
    vocabulary: ReadonlyMap<string, readonly string[]>;
    /** Told when the box opens, for a page that reads `vocabulary` only then. */
    onopen?: () => void;
    /** Read the campaign again, after a write. */
    onchange: () => Promise<void>;
  } = $props();

  const uid = $props.id();
  let open = $state(false);
  let text = $state("");
  const adding = new Busy();
  const removing = new Busy();
  const rows = $derived(withLabel(campaign.labels, parseLabel(text)));
  const problem = $derived(text.trim() ? labelsProblem(rows) : "");

  const close = () => { open = false; text = ""; adding.error = ""; };

  async function add(event: SubmitEvent) {
    event.preventDefault();
    if (adding.busy || problem || !text.trim()) return;
    const label = parseLabel(text);
    await adding.attempt(async () => {
      // Onto the set as it stands, not as this page read it, so a label added meanwhile is kept.
      const fresh = await campaignOf(session.signer, campaign.campaign_id);
      await updateCampaign(session.signer, campaign.campaign_id, labelsBody(withLabel(fresh.labels, label)));
      await onchange();
      close();
    });
  }

  async function remove(key: string) {
    if (removing.busy) return;
    await removing.attempt(async () => {
      // Off the set as it stands, as an add is, so a label added meanwhile is kept.
      const fresh = await campaignOf(session.signer, campaign.campaign_id);
      await updateCampaign(session.signer, campaign.campaign_id, labelsBody(withoutLabel(fresh.labels, key)));
      await onchange();
    }, `Removing ${key}`);
  }
</script>

<span class="labels">
  <LabelChips labels={campaign.labels} {listing} remove={editable ? { from: campaign.name, busy: removing.busy, drop: remove } : undefined} />
  {#if removing.error}<span class="hint err" role="alert">{removing.error}</span>{/if}
  {#if editable}
    {#if open}
      <form novalidate onsubmit={add}>
        <input type="text" bind:value={text} placeholder="key=value" autocomplete="off" list="{uid}-known"
               aria-label="A label for {campaign.name}, as key=value" aria-invalid={!!problem || undefined}
               aria-describedby="{uid}-note" onkeydown={(e) => { if (e.key === "Escape") close(); }}
               {@attach (node) => node.focus()}>
        <datalist id="{uid}-known">{#each knownLabels(vocabulary) as known (known)}<option value={known}></option>{/each}</datalist>
        <button type="submit" class="primary" aria-disabled={adding.busy || !!problem || !text.trim()}>{
          adding.busy ? "Adding…" : "Add"}</button>
        <button type="button" onclick={close}>Cancel</button>
      </form>
      <span id="{uid}-note" class="hint err" role="alert" hidden={!problem && !adding.error}>{problem || adding.error}</span>
    {:else}
      <button type="button" class="add" aria-label="Add a label to {campaign.name}" onclick={() => { open = true; onopen?.(); }}>+ label</button>
    {/if}
  {/if}
</span>

<style>
  .labels { display: inline-flex; flex-wrap: wrap; align-items: center; gap: 4px 6px; }
  form { display: inline-flex; gap: 4px; align-items: center; }
  input { width: 14em; padding: 2px 8px; font-size: 12px; }
  form button { padding: 2px 8px; font-size: 12px; }
  /* A chip not yet there: the chips' shape, dashed. */
  .add { padding: 1px 8px; border: 1px dashed var(--border-strong); border-radius: 10px; background: none;
         font-size: 12px; line-height: 1.5; color: var(--text-muted); white-space: nowrap; cursor: pointer; }
  .add:hover { border-color: var(--accent); color: var(--accent); }
  .hint { margin: 0; }
</style>
