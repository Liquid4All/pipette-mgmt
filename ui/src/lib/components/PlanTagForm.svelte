<script lang="ts">
  // Tags one plan of a campaign: a name and a message, made once and never moved.
  import { createPlanTag } from "../api";
  import { session } from "../session.svelte";
  import { Busy } from "./work.svelte";

  let { campaignId, planId, onclose, onchange }: {
    campaignId: string;
    planId: string;
    /** Close the form without a write. */
    onclose: () => void;
    /** Read the page again, after the tag is made. */
    onchange?: () => Promise<void>;
  } = $props();

  const uid = $props.id();
  let name = $state("");
  let message = $state("");
  const saving = new Busy();

  async function save(event: SubmitEvent) {
    event.preventDefault();
    if (saving.busy || !name.trim() || !message.trim()) return;
    await saving.attempt(async () => {
      await createPlanTag(session.signer, campaignId, { plan_id: planId, name: name.trim(), message: message.trim() });
      await onchange?.();
      onclose();
    });
  }
</script>

<form class="bar" novalidate onsubmit={save} aria-label="Tag plan {planId}">
  <label for="{uid}-name">Tag <span class="id">{planId}</span> as</label>
  <input id="{uid}-name" type="text" bind:value={name} autocomplete="off" placeholder="release/2026.11">
  <input type="text" bind:value={message} autocomplete="off" placeholder="message" aria-label="Tag message">
  <button type="submit" class="primary" aria-disabled={saving.busy || !name.trim() || !message.trim()}>{
    saving.busy ? "Saving…" : "Save tag"}</button>
  <button type="button" onclick={onclose}>Cancel</button>
  {#if saving.error}<p class="err" role="alert">{saving.error}</p>{/if}
</form>
