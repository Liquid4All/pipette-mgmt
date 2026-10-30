<script lang="ts">
  // A campaign's revert to an earlier plan of its history, over
  // `POST /api/v1/campaigns/{id}/revert`: a dry run first, then a confirmation that
  // says what filing would do. One per group of controls that offer a revert,
  // such as the rows of a History table; each control calls `revert` with its plan.
  import { previewRevert, revertCampaign } from "../api";
  import { REVISION_EFFECT, revisionSummary } from "../campaigns";
  import { filingFailure } from "../rerun";
  import { here, navigate } from "../route.svelte";
  import { campaignLink } from "../routes";
  import { session } from "../session.svelte";
  import type { NamedCampaignOverview } from "../api-types";
  import ConfirmDialog from "./ConfirmDialog.svelte";
  import { Busy } from "./work.svelte";

  let { detail, onchange }: {
    detail: Pick<NamedCampaignOverview, "campaign_id" | "current_plan_id">;
    /** Read the page again, after a revert filed while the reader moved on. */
    onchange?: () => Promise<void>;
  } = $props();

  const previewing = new Busy();
  let confirm = $state<ReturnType<typeof ConfirmDialog>>();
  let said = $state("");

  /** Preview a revert to `target`, then ask before filing it. */
  export async function revert(target: string) {
    const base = detail.current_plan_id;
    const campaign = detail.campaign_id;
    if (!base || previewing.busy) return;
    said = "";
    await previewing.attempt(async () => {
      const preview = await previewRevert(session.signer, campaign, base, { plan_id: target });
      if (preview.unchanged) {
        said = "The campaign already runs the document of this plan. Nothing is filed.";
        return;
      }
      const from = here.path;
      void confirm?.ask({
        title: `Revert to ${target}?`,
        text: `${revisionSummary(preview)} ${REVISION_EFFECT}`,
        action: "Revert",
        pending: "Filing…",
        run: async () => {
          try {
            await revertCampaign(session.signer, campaign, base, { plan_id: target });
          } catch (e) {
            return filingFailure("revert", e);
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

{#if previewing.busy}<p class="muted" role="status">Checking what the revert would file…</p>{/if}
{#if previewing.error}<p class="err" role="alert">Could not check the revert: {previewing.error}</p>{/if}
{#if said}<p class="muted" role="status">{said}</p>{/if}
<ConfirmDialog bind:this={confirm} />
