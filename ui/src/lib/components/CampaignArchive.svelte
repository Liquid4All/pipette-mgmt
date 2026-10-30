<script lang="ts">
  // A campaign's own state, a General row of its Settings: an archived campaign is hidden from the listings until
  // unarchived.
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

  const archived = $derived(stateOf(detail) === "archived");
  const saving = new Busy();
  /** What the last write did. */
  let done = $state("");

  async function flip() {
    if (saving.busy) return;
    const to = archived ? "active" : "archived";
    await saving.attempt(async () => {
      await updateCampaign(session.signer, detail.campaign_id, { state: to });
      await onchange();
      done = to === "archived" ? "Archived: the listings hide it." : "Unarchived: back in the listings.";
    });
  }
</script>

<SettingRow title="Archive" status={done}
            text={archived
              ? "This campaign is archived: the listings hide it, and it takes no new plan until it is unarchived."
              : "Hide this campaign from the listings and stop new plans being filed into it. The plan it runs keeps running, and it can be unarchived."}>
  {#snippet control(ids)}
    <button type="button" aria-describedby={ids.text} aria-disabled={saving.busy} onclick={flip}>{
      saving.busy ? "Saving…" : archived ? "Unarchive campaign" : "Archive campaign"}</button>
    {#if saving.error}<p class="err" role="alert">{saving.error}</p>{/if}
  {/snippet}
</SettingRow>
