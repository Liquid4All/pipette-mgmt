<script lang="ts">
  // A campaign group's failed jobs whose cell ran with no failure at larger sizes, such as a longer context, over its
  // member campaigns.
  import { smallerFailedRows, smallerFailedTable, stateTitle, type SmallerFailedRow } from "../campaign-groups";
  import { campaignLink } from "../routes";
  import type { CampaignGroupPolicy, MemberPolicyPreview } from "../api-types";
  import DataTable from "./DataTable.svelte";

  let { rows, policy, names }: {
    /** The preview's member campaigns. */
    rows: MemberPolicyPreview[];
    /** The smaller_failed policy, whose state the heading says. */
    policy: CampaignGroupPolicy;
    names: Record<string, string>;
  } = $props();

  const uid = $props.id();
  const found = $derived(smallerFailedRows(rows));
  const spec = $derived(smallerFailedTable(names));
</script>

{#snippet name(row: SmallerFailedRow)}<a href={campaignLink(row.campaign_id)}>{names[row.campaign_id] ?? row.campaign_id}</a>{/snippet}

<section aria-labelledby="{uid}-title">
  <h2 id="{uid}-title">{stateTitle(policy)}</h2>
  <p class="muted">The answered jobs of each campaign that failed while a job of the same cell at larger sizes ran with
    no failure: the same benchmark, model, runtime and flags, with no size smaller and at least one larger, such as a
    longer context. Each is shown with the smallest such clean job. Every failure counts, a device's, a mark, a
    deadline or the queue's for a smaller run that failed.</p>
  <DataTable {spec} rows={found} id={(row) => `${row.campaign_id}/${row.job_id}`} noun="jobs" name="smaller" cells={{ name }}
             empty="No failed job has a clean larger run." />
</section>
