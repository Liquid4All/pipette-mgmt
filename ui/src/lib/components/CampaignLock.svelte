<script lang="ts">
  // A campaign's lock, a General row of its Settings: a locked campaign keeps its plan until it is unlocked, and the
  // lock needs a note saying why.
  import { updateCampaign } from "../api";
  import { stateOf } from "../campaigns";
  import { session } from "../session.svelte";
  import type { NamedCampaignOverview } from "../api-types";
  import SettingRow from "./SettingRow.svelte";
  import { Busy } from "./work.svelte";

  let { detail, onchange }: {
    detail: NamedCampaignOverview;
    /** Read the campaign again, after a write. */
    onchange: () => Promise<void>;
  } = $props();

  const locked = $derived(stateOf(detail) === "locked");
  let note = $state("");
  const saving = new Busy();
  /** What the last write did. */
  let done = $state("");

  async function lock(event: SubmitEvent) {
    event.preventDefault();
    if (saving.busy || !note.trim()) return;
    await saving.attempt(async () => {
      await updateCampaign(session.signer, detail.campaign_id, { state: "locked", state_note: note.trim() });
      await onchange();
      note = "";
      done = "Locked: its plan stays as it is.";
    });
  }

  async function unlock() {
    if (saving.busy) return;
    await saving.attempt(async () => {
      await updateCampaign(session.signer, detail.campaign_id, { state: "active" });
      await onchange();
      done = "Unlocked.";
    });
  }
</script>

<SettingRow title="Lock" status={done}
            text={locked
              ? `This campaign is locked: ${detail.state_note ?? ""}. No plan is filed into it, and its plan is not cancelled, until it is unlocked.`
              : "Keep the plan this campaign runs: no revision, re-run, revert, cancel or delete until it is unlocked. Rename, labels and hand-off still work."}>
  {#snippet children(ids)}
    {#if locked}
      <button type="button" aria-describedby={ids.text} aria-disabled={saving.busy} onclick={unlock}>{
        saving.busy ? "Saving…" : "Unlock campaign"}</button>
    {:else}
      <form novalidate onsubmit={lock}>
        <div class="bar">
          <input type="text" bind:value={note} autocomplete="off" placeholder="why it is locked"
                 aria-labelledby={ids.title} aria-describedby={ids.text} oninput={() => (done = "")}>
          <button type="submit" aria-disabled={saving.busy || !note.trim()}>{
            saving.busy ? "Saving…" : "Lock campaign"}</button>
        </div>
      </form>
    {/if}
    {#if saving.error}<p class="err" role="alert">{saving.error}</p>{/if}
  {/snippet}
</SettingRow>
