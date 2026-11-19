<script lang="ts">
  // A campaign group's Activity tab, from its policy preview: a row a member campaign, with what each check's policy
  // alone would do with the answered jobs of its plan and what the policies that are on would do together; the
  // policies' last revision of each campaign; the rows that lack model params; and the failed jobs below a clean larger
  // run.
  import { filingsTable, policyPreviewTable, recentFilings } from "../campaign-groups";
  import { when } from "../format";
  import { campaignLink } from "../routes";
  import type {
    CampaignGroupPolicy, CampaignGroupPolicyPreview, CampaignView, CreateModelParamsRequest, MemberPolicyPreview,
    OwnUserView, UserPermission,
  } from "../api-types";
  import DataTable from "./DataTable.svelte";
  import Loaded from "./Loaded.svelte";
  import ModelParamsGaps from "./ModelParamsGaps.svelte";
  import SmallerFailures from "./SmallerFailures.svelte";
  import StatusPill from "./StatusPill.svelte";
  import type { Loader } from "./work.svelte";

  let { of, lacks, members, policies, reader, add }: {
    of: Loader<CampaignGroupPolicyPreview>;
    /** The grants the route takes that the reader lacks; with any, `of` is never read. */
    lacks: UserPermission[];
    /** The group's member campaigns, which name the rows. */
    members: CampaignView[];
    policies: CampaignGroupPolicy[];
    reader: OwnUserView;
    /** Write a model params entry, then read the preview again. */
    add: (body: CreateModelParamsRequest) => Promise<void>;
  } = $props();

  const uid = $props.id();
  const names = $derived(Object.fromEntries(members.map((member) => [member.campaign_id, member.name])));
  const spec = $derived(policyPreviewTable(policies, names));
  const filings = $derived(filingsTable(names));
  const params = $derived(policies.find((policy) => policy.kind === "model_params"));
  const smaller = $derived(policies.find((policy) => policy.kind === "smaller_failed"));
  // The route lists only the members whose plans the reader reads.
  const empty = $derived(members.length ? "You may not read the plans of this group's campaigns."
    : "This group holds no campaign.");
</script>

{#snippet name(row: MemberPolicyPreview)}<a href={campaignLink(row.campaign_id)}>{names[row.campaign_id] ?? row.campaign_id}</a>{/snippet}
{#snippet deadline(row: MemberPolicyPreview)}
  {#if row.deadline_passed}<StatusPill status="expired" />{" "}{/if}{when(row.earliest_deadline)}
{/snippet}

{#if lacks.length}
  <p class="muted">Previewing the policies takes
    {#each lacks as grant, index (grant)}{#if index}{" and "}{/if}<code>{grant}</code>{/each}.</p>
{:else}
  <Loaded {of}>
    {#snippet children({ members: rows })}
      <section aria-labelledby="{uid}-title">
        <h2 id="{uid}-title">Preview</h2>
        <p class="muted">What the policies would do now with the answered jobs of each campaign's plan, its eval jobs
          left out: each check's policy alone, on or off, then the policies that are on together.</p>
        <DataTable {spec} {rows} id={(row) => row.campaign_id} noun="campaigns" {empty} cells={{ name, deadline }} />
        {#if rows.some((row) => row.deadline_passed)}
          <p class="muted">The server refuses a re-run of a plan whose document sets a deadline that has passed.</p>
        {/if}
      </section>
      {#if rows.length}
        <section aria-labelledby="{uid}-filings">
          <h2 id="{uid}-filings">Recent policy revisions</h2>
          <DataTable spec={filings} rows={recentFilings(rows)} id={(row) => row.campaign_id} noun="revisions"
                     empty="The policies have revised none of these campaigns." cells={{ name }} />
        </section>
        {#if params}<ModelParamsGaps {rows} policy={params} {names} {reader} {add} />{/if}
        {#if smaller}<SmallerFailures {rows} policy={smaller} {names} />{/if}
      {/if}
    {/snippet}
  </Loaded>
{/if}
