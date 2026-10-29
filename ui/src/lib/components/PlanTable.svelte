<script lang="ts">
  // The plan table of `plans.html` and `user.html`. The columns are:
  // - the id of the plan
  // - the note
  // - the status
  // - the time of filing
  // - the submitter, where the listing names submitters
  // - the progress
  // `DataTable` filters, sorts and pages the rows.
  import { progress } from "../format";
  import { planTable, shownStatus } from "../plans";
  import { userLink } from "../routes";
  import type { PlanView } from "../api-types";
  import DataTable from "./DataTable.svelte";
  import StatusPill from "./StatusPill.svelte";

  let { plans, names = {}, submitters, empty, path, kept = [] }: {
    plans: PlanView[];
    names?: Record<string, string>;
    submitters: boolean;
    empty: string;
    /** The page's own address, which the filters, the sort and the pager keep. */
    path: string;
    /** The address pairs the server's filters read, which every link keeps. */
    kept?: [string, string][];
  } = $props();

  const spec = $derived(planTable(names, submitters));
</script>

{#snippet status(plan: PlanView)}
  <StatusPill status={shownStatus(plan.status, plan.progress_snapshot?.counts)} />{#if plan.cancel_requested}{" "}(cancel requested){/if}
{/snippet}
<!-- An account since deleted has no address, and the id stands in. -->
{#snippet submitted_by(plan: PlanView)}
  {#if plan.submitted_by}<a href={userLink(plan.submitted_by)}>{names[plan.submitted_by] ?? plan.submitted_by}</a>
  {:else}<span class="muted">operator</span>{/if}
{/snippet}
{#snippet progressCell(plan: PlanView)}{progress(plan)}{/snippet}

<DataTable {spec} rows={plans} id={(plan) => plan.plan_id} noun="plans" {path} {kept} {empty}
           columnsKey="pipette.plans.columns" cells={{ status, submitted_by, progress: progressCell }} />
