<script lang="ts">
  // Handing a campaign to another owner, the last row of its danger zone,
  // confirmed in a dialog that takes the new owner and the campaign's name.
  import { campaign, campaignOwners, setCampaignOwner } from "../api";
  import { handedAway } from "../campaigns";
  import { ownerOf } from "../owners";
  import { session } from "../session.svelte";
  import type { NamedCampaignOverview, OwnerMatch } from "../api-types";
  import ConfirmDialog from "./ConfirmDialog.svelte";
  import { describe } from "./guard";
  import OwnerCell from "./OwnerCell.svelte";
  import OwnerPicker from "./OwnerPicker.svelte";
  import SettingRow from "./SettingRow.svelte";
  import { Busy } from "./work.svelte";

  let { detail, onchange, onhanded, ondone }: {
    detail: NamedCampaignOverview;
    /** Read the campaign again, after a write. */
    onchange: () => Promise<void>;
    /** The transfer took the campaign out of the reader's reach: why, for the page to draw in its place. */
    onhanded: (why: string) => void;
    /** Whom it went to, for the page to say: a reader who no longer administers it loses this row. */
    ondone: (said: string) => void;
  } = $props();

  const TEXT = "Moves the campaign and every plan, running ones included, to another person or organization. "
    + "Afterwards you keep only the access the new owner gives you.";
  const uid = $props.id();
  const id = $derived(detail.campaign_id);
  let confirm = $state<ReturnType<typeof ConfirmDialog>>();
  let picker = $state<ReturnType<typeof OwnerPicker>>();
  /** The owner picked from the list, which the dialog names. */
  let to = $state<OwnerMatch | null>(null);
  /** Why the owner field does not name one the campaign may go to. */
  let ownerError = $state("");
  const handing = new Busy();
  /** What the last transfer did, said by the page once the dialog has closed. */
  let said = "";

  function open() {
    if (handing.busy) return;
    to = null;
    ownerError = "";
    void confirm?.ask({
      title: "Transfer ownership?", text: TEXT, action: "Transfer ownership", pending: "Transferring…",
      phrase: detail.name, fields, run: handOver, done: () => ondone(said),
    });
  }

  async function handOver(): Promise<string | null> {
    if (!picker) return "";
    let choice: OwnerMatch;
    try {
      choice = await picker.chosen();
    } catch (e) {
      ownerError = describe(e);
      return "";
    }
    const outcome = { gone: null as string | null };
    await handing.attempt(async () => {
      await setCampaignOwner(session.signer, id, { owner: ownerOf(choice) });
      // `GET /api/v1/campaigns/{id}` applies the reach `/overview` does, without walking the plans.
      outcome.gone = await campaign(session.signer, id).then(() => null, (e) => handedAway(e, choice));
      if (outcome.gone) return;
      said = `Transferred to ${choice.label}.`;
      await onchange();
    });
    if (handing.error) return handing.error;
    if (outcome.gone) onhanded(outcome.gone);
    return null;
  }
</script>

{#snippet fields()}
  <div class="field">
    <span class="caption">Current owner</span>
    <OwnerCell owner={detail.owner} names={detail.names ?? {}} />
  </div>
  <div class="field">
    <span class="caption">New owner</span>
    <OwnerPicker bind:this={picker} label="new owner" lookup={(q) => campaignOwners(session.signer, id, q)}
                 invalid={!!ownerError} describedby="{uid}-owner-note"
                 onpick={(choice) => { to = choice; ownerError = ""; }} />
    <div id="{uid}-owner-note" class="hint" class:err={!!ownerError}>
      {#if ownerError}{ownerError}
      {:else if to}Goes to {to.label}, {to.type === "org" ? "an organization" : "a person"}.
      {:else}Yourself, an organization you belong to or administer, or a member of the organization you
        administer.{/if}
    </div>
  </div>
{/snippet}

<SettingRow title="Transfer ownership" text={TEXT}>
  {#snippet control(ids)}
    <button type="button" class="danger" aria-haspopup="dialog" aria-describedby={ids.text}
            aria-disabled={handing.busy} onclick={open}>{handing.busy ? "Transferring…" : "Transfer ownership"}</button>
  {/snippet}
</SettingRow>
<ConfirmDialog bind:this={confirm} />

<style>
  .field { margin: 0 0 12px; }
</style>
