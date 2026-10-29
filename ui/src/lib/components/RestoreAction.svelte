<script lang="ts">
  // The restore of one variant of an earlier plan into the plan that a campaign
  // runs, over `POST /api/v1/campaigns/{id}/variants?restore=<plan_id>:<index>`.
  // It lists the variants of the earlier plan. The reader picks one, and chooses
  // to append it or to put it in place of a variant of the current plan. A dry
  // run comes first, then a confirmation that says what filing would do.
  import { campaignVariants, editVariants, planVariantList, previewVariantEdit } from "../api";
  import { REVISION_EFFECT, repeatedText, restoreEdit, revisionSummary } from "../campaigns";
  import { filingFailure } from "../rerun";
  import { here, navigate } from "../route.svelte";
  import { campaignLink } from "../routes";
  import { session } from "../session.svelte";
  import type { CampaignVariants, NamedCampaignOverview } from "../api-types";
  import ConfirmDialog from "./ConfirmDialog.svelte";
  import { Busy, Loader } from "./work.svelte";

  let { detail, plan_id, onchange }: {
    detail: Pick<NamedCampaignOverview, "campaign_id">;
    /** The earlier plan whose variants the reader may restore. */
    plan_id: string;
    /** Read the page again, after a restore filed while the reader moved on. */
    onchange?: () => Promise<void>;
  } = $props();

  const source = new Loader<string[]>();
  const head = new Loader<CampaignVariants>();
  const previewing = new Busy();
  let confirm = $state<ReturnType<typeof ConfirmDialog>>();
  /** The variant of the earlier plan to restore, once the reader picks one. */
  let chosen = $state<number | null>(null);
  /** The variant of the current plan that the restored one replaces; `null` appends it. */
  let replace = $state<number | null>(null);

  // Read when the reader opens the list, so a page view costs no extra read.
  const opened = (event: Event) => {
    if ((event.currentTarget as HTMLDetailsElement).open && !source.data) {
      void source.run(async () => (await planVariantList(session.signer, plan_id)).variants);
    }
  };

  const choose = (index: number) => {
    chosen = index;
    replace = null;
    void head.run(() => campaignVariants(session.signer, detail.campaign_id));
  };

  /** Preview the restore of `chosen`, then ask before filing it. */
  async function check() {
    const base = head.data?.base.plan_id;
    const index = chosen;
    const campaign = detail.campaign_id;
    if (!base || index === null || previewing.busy) return;
    const { query, toml } = restoreEdit(base, plan_id, index, replace);
    await previewing.attempt(async () => {
      const preview = await previewVariantEdit(session.signer, campaign, query, toml);
      const repeated = repeatedText(preview.repeated_variants);
      const from = here.path;
      void confirm?.ask({
        title: `Restore variant ${index} of ${plan_id}?`,
        text: [revisionSummary(preview), repeated && `Warning: ${repeated}`, REVISION_EFFECT].filter(Boolean).join(" "),
        action: "Restore",
        pending: "Filing…",
        run: async () => {
          try {
            await editVariants(session.signer, campaign, query, toml);
          } catch (e) {
            return filingFailure("restore", e);
          }
          // A reader who moved to another page stays there, and sees the campaign moved.
          if (here.path === from) navigate(campaignLink(campaign));
          else await onchange?.();
          return null;
        },
      });
    });
  }
</script>

<details class="restore" ontoggle={opened}>
  <summary>Restore a variant of this plan into the current plan</summary>
  {#if source.error}<p class="err" role="alert">Could not read the variants of this plan: {source.error}</p>{/if}
  {#if source.data}
    <ol start="0">
      {#each source.data as block, index (index)}
        <li>
          <pre>{block}</pre>
          <button type="button" class="again" aria-pressed={chosen === index} onclick={() => choose(index)}>Restore
            variant {index} into the current plan</button>
        </li>
      {/each}
    </ol>
  {/if}
  {#if chosen !== null}
    {#if head.error}<p class="err" role="alert">Could not read the variants of the current plan: {head.error}</p>{/if}
    {#if head.data}
      <fieldset>
        <legend>Where variant {chosen} goes in the current plan, {head.data.base.plan_id}</legend>
        <label><input type="radio" name="restore-place" value={null} bind:group={replace} /> Append it after the
          variants of the current plan</label>
        {#each head.data.variants as block, index (index)}
          <label><input type="radio" name="restore-place" value={index} bind:group={replace} /> Replace variant
            {index}</label>
          <pre>{block}</pre>
        {/each}
        <div class="bar">
          <button type="button" class="primary" aria-haspopup="dialog" disabled={previewing.busy}
                  onclick={check}>Check the restore</button>
          <button type="button" onclick={() => (chosen = null)}>Cancel</button>
        </div>
      </fieldset>
    {/if}
  {/if}
  {#if previewing.busy}<p class="muted" role="status">Checking what the restore would file…</p>{/if}
  {#if previewing.error}<p class="err" role="alert">Could not check the restore: {previewing.error}</p>{/if}
</details>
<ConfirmDialog bind:this={confirm} />
