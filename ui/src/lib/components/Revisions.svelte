<script lang="ts">
  // What a campaign has run, newest first, the plan shown marked: a plan's
  // History tab, on its own pages and its campaign's, sorted and filtered in
  // the page, which draws it in more than one place. A row for an earlier plan
  // offers a revert if the reader may read that plan and may revise the campaign.
  import { revertOffered } from "../campaigns";
  import { progressOf } from "../format";
  import { PLAN_STATUSES, changeText, doneShare } from "../plans";
  import type { TableSpec } from "../table";
  import type { CampaignPlanRow, NamedCampaignOverview } from "../api-types";
  import Clamped from "./Clamped.svelte";
  import DataTable from "./DataTable.svelte";
  import PlanTagChips from "./PlanTagChips.svelte";
  import PlanTagForm from "./PlanTagForm.svelte";
  import RevertAction from "./RevertAction.svelte";

  let { detail, shown, link, onchange }: {
    detail: NamedCampaignOverview;
    shown: string | null;
    /** A listed plan's address. */
    link: (planId: string) => string;
    /** Read the page again, after a revert. */
    onchange?: () => Promise<void>;
  } = $props();

  let reverter = $state<ReturnType<typeof RevertAction>>();
  /** The plan the tag form is open for. */
  let tagging = $state<string | null>(null);

  const counts = (row: CampaignPlanRow) => row.plan?.counts;
  // A plan the reader may not read is its id alone.
  const spec: TableSpec<CampaignPlanRow> = $derived({
    columns: [
      { key: "plan_id", label: "plan_id", raw: (row) => row.plan_id },
      { key: "tags", label: "tags", raw: (row) => row.tags?.map((tag) => tag.name).join(", ") || null },
      { key: "note", label: "note", raw: (row) => row.plan?.note ?? null },
      { key: "change", label: "change", as: "json", raw: (row) => (row.plan?.change ? JSON.stringify(row.plan.change) : null) },
      { key: "status", label: "status", raw: (row) => row.plan?.status ?? null, as: "state" },
      { key: "created_at", label: "created_at", raw: (row) => row.plan?.created_at ?? null, as: "time" },
      { key: "progress", label: "progress", raw: (row) => doneShare(counts(row)) },
    ],
    texts: [{ key: "q", label: "plan_id, tag, note or change",
              texts: (row) => [row.plan_id, ...(row.tags ?? []).map((tag) => tag.name), row.plan?.note,
                               row.plan?.change && changeText(row.plan.change)] }],
    choices: [{ key: "status", label: "status", values: (row) => [row.plan?.status ?? ""],
                name: (value) => value.replaceAll("_", " "), order: PLAN_STATUSES }],
  });
  const current = $derived(detail.history[0]?.plan_id);
</script>

{#snippet plan_id(row: CampaignPlanRow)}
  <a class="id" href={link(row.plan_id)}>{row.plan_id}</a>
  {#if row.plan_id === current}<span class="tag current">current</span>{/if}
  {#if row.plan && revertOffered(detail, row.plan_id)}
    <button type="button" class="pill" aria-haspopup="dialog" aria-label="Revert to this plan"
            onclick={() => reverter?.revert(row.plan_id)}><span aria-hidden="true">↺</span> Revert to this</button>
  {/if}
  {#if detail.may_tag}
    <button type="button" class="pill" aria-label="Tag plan {row.plan_id}" onclick={() => (tagging = row.plan_id)}>Tag</button>
  {/if}
{/snippet}
{#snippet tags(row: CampaignPlanRow)}{#if row.tags?.length}<PlanTagChips tags={row.tags} />{:else}—{/if}{/snippet}
{#snippet note(row: CampaignPlanRow)}{#if row.plan?.note}<Clamped text={row.plan.note} note />{:else}—{/if}{/snippet}
{#snippet progressCell(row: CampaignPlanRow)}{row.plan ? progressOf(row.plan.counts) : "—"}{/snippet}

{#if detail.history_truncated}<p class="muted">Older revisions than these are not walked.</p>{/if}
<RevertAction {detail} {onchange} bind:this={reverter} />
{#if tagging}
  <PlanTagForm campaignId={detail.campaign_id} planId={tagging} onclose={() => (tagging = null)} {onchange} />
{/if}
<DataTable {spec} rows={detail.history} id={(row) => row.plan_id} noun="revisions" empty="No revision is listed."
           cells={{ plan_id, tags, note, progress: progressCell }} marked={(row) => row.plan_id === shown} />
