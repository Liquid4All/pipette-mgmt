<script lang="ts">
  // A campaign group's re-runs that its policies filed, as queue-maintenance recorded them: what each policy's re-runs
  // came to, and a row per re-run with its check's value before and after, which also download as one CSV file.
  import { changeText, rerunSummaryText, rerunTitle, rerunsCsv, rerunsTable } from "../campaign-groups";
  import { campaignLink, pathOf } from "../routes";
  import type {
    CampaignGroupPolicy, CampaignGroupRerunResults, CampaignView, PolicyRerun, UserPermission,
  } from "../api-types";
  import DataTable from "./DataTable.svelte";
  import Loaded from "./Loaded.svelte";
  import type { Loader } from "./work.svelte";

  let { of, lacks, groupId, members, policies }: {
    of: Loader<CampaignGroupRerunResults>;
    /** The grants the route takes that the reader lacks; with any, `of` is never read. */
    lacks: UserPermission[];
    /** Names the CSV file and the address that keeps the table's filters. */
    groupId: string;
    /** The group's member campaigns, which name the rows. */
    members: CampaignView[];
    policies: CampaignGroupPolicy[];
  } = $props();

  const uid = $props.id();
  const names = $derived(Object.fromEntries(members.map((member) => [member.campaign_id, member.name])));
  const spec = $derived(rerunsTable(names));
  // The route lists only the members whose plans the reader reads.
  const empty = $derived(members.length ? "The policies have filed no re-run in the campaigns whose plans you may read."
    : "This group holds no campaign.");

  // Every row read, whatever the filters, saved from memory as the Jobs tab saves its rows.
  function download(rows: PolicyRerun[]) {
    const file = document.createElement("a");
    file.href = URL.createObjectURL(new Blob([rerunsCsv(rows, names)], { type: "text/csv" }));
    file.download = `${groupId}-rerun-results.csv`;
    file.click();
    setTimeout(() => URL.revokeObjectURL(file.href));
  }
</script>

{#snippet name(row: PolicyRerun)}<a href={campaignLink(row.campaign_id)}>{names[row.campaign_id] ?? row.campaign_id}</a>{/snippet}
{#snippet change(row: PolicyRerun)}{changeText(row)}{/snippet}

<section aria-labelledby="{uid}-title">
  <h2 id="{uid}-title">Re-run results</h2>
  {#if lacks.length}
    <p class="muted">Reading the re-run results takes
      {#each lacks as grant, index (grant)}{#if index}{" and "}{/if}<code>{grant}</code>{/each}.</p>
  {:else}
    <Loaded {of}>
      {#snippet children({ summary, rows })}
        <p class="muted">Each re-run the policies filed, newest first, with its check's value on the run it replaces and,
          once its rows are read, on the re-run: still flagged where the check, at the settings it was filed under, flags
          it again, failed where it does not but the re-run failed, and cleared otherwise. A re-run the campaign no longer
          runs that has neither rows nor a recorded result is dropped. The share cleared and the medians count the re-runs
          read.</p>
        {#if summary.length}
          <dl class="facts">
            {#each summary as entry (entry.kind)}
              <!-- Spread's at the percents its re-runs were judged at, not the one stored now. -->
              {@const titles = new Set(rows.filter((row) => row.kind === entry.kind).map(rerunTitle))}
              {@const off = policies.find((stored) => stored.kind === entry.kind)?.enabled === false}
              <div><dt>{[...titles].join(", ")}{off ? " (off)" : ""}</dt><dd>{rerunSummaryText(entry)}</dd></div>
            {/each}
          </dl>
        {/if}
        {#if rows.length}
          <div class="bar"><button type="button" class="primary" title="Every re-run read, whatever the filters"
                                  onclick={() => download(rows)}>Download CSV</button></div>
        {/if}
        <DataTable {spec} {rows} id={(row) => `${row.rerun_job_id} ${row.kind}`} noun="re-runs" {empty}
                   path={pathOf({ page: "campaign-group-activity", group_id: groupId })} cells={{ name, change }} />
      {/snippet}
    </Loaded>
  {/if}
</section>
