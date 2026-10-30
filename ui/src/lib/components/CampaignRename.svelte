<script lang="ts">
  // A campaign's name, the General row of its Settings: checked as typed where the route would refuse it.
  import { updateCampaign } from "../api";
  import { NAME_MAX_CHARS, nameProblem, renameBody } from "../campaigns";
  import { session } from "../session.svelte";
  import type { NamedCampaignOverview } from "../api-types";
  import SettingRow from "./SettingRow.svelte";
  import { Busy } from "./work.svelte";

  let { detail, onchange }: {
    detail: NamedCampaignOverview;
    /** Read the campaign again, after a write. */
    onchange: () => Promise<void>;
  } = $props();

  const uid = $props.id();
  // Taken again from each read, as a write reads the campaign again.
  let name = $derived(detail.name);
  const changed = $derived(name.trim() !== detail.name);
  const problem = $derived(nameProblem(name));
  const renaming = new Busy();
  /** What the last rename did, until the name is typed again. */
  let done = $state("");

  async function rename(event: SubmitEvent) {
    event.preventDefault();
    if (renaming.busy || problem || !changed) return;
    const to = name.trim();
    await renaming.attempt(async () => {
      await updateCampaign(session.signer, detail.campaign_id, renameBody(to));
      await onchange();
      done = `Renamed to “${to}”.`;
    });
  }
</script>

<SettingRow title="Campaign name"
            text={`Shown in listings and atop this page; up to ${NAME_MAX_CHARS.toLocaleString("en")} characters.`}>
  {#snippet children(ids)}
    <form novalidate onsubmit={rename}>
      <div class="bar">
        <input type="text" value={name} autocomplete="off" aria-labelledby={ids.title}
               aria-describedby="{ids.text} {uid}-note" aria-invalid={!!problem || undefined}
               oninput={(e) => { name = e.currentTarget.value; done = ""; }}>
        <button type="submit" class="primary" aria-disabled={renaming.busy || !changed || !!problem}>{
          renaming.busy ? "Saving…" : "Save changes"}</button>
        {#if changed}<button type="button" onclick={() => (name = detail.name)}>Reset</button>{/if}
      </div>
      <div id="{uid}-note" class="hint" class:err={!!problem} class:ok={!problem && !!done} role="status"
           hidden={!problem && !done}>{problem || done}</div>
      {#if renaming.error}<p class="err" role="alert">{renaming.error}</p>{/if}
    </form>
  {/snippet}
</SettingRow>
