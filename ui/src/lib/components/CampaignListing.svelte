<script lang="ts">
  // The classic console's campaign listing (`campaigns.html`) under any branch:
  // every page of `GET /api/v1/campaigns` narrowed to the branch, newest first, each row
  // with what the plan it runs is doing where the reader may read that plan,
  // filtered, sorted and paged in `DataTable`, the plan id and the state by the server.
  import { campaigns } from "../api";
  import { campaignQuery, campaignTable, labelVocabulary, mayLabel, progressText } from "../campaigns";
  import { ownedEmpty, type OwnedBranch } from "../owned";
  import { shownStatus } from "../plans";
  import { here, href } from "../route.svelte";
  import { campaignLink } from "../routes";
  import { session } from "../session.svelte";
  import { ROUTE_PAGE_MAX, everyPage, withCursor, type CursorPage } from "../table";
  import type { CampaignView } from "../api-types";
  import DataTable from "./DataTable.svelte";
  import LabelsInline from "./LabelsInline.svelte";
  import Loaded from "./Loaded.svelte";
  import OwnerCell from "./OwnerCell.svelte";
  import StatusPill from "./StatusPill.svelte";
  import { Loader } from "./work.svelte";

  let { branch, path }: {
    branch: OwnedBranch;
    /** The page's own address, which the filters, the sort and the pager keep. */
    path: string;
  } = $props();

  const planId = $derived(here.query.get("plan_id")?.trim() ?? "");

  // `""` for the active campaigns, `archived` or `all`; any other value is the default.
  const stateAsked = $derived((["archived", "all"] as const).find((value) => value === here.query.get("state")) ?? "");

  // Read again whenever the query it sends changes, not on a filter, which applies here.
  const asked = $derived(campaignQuery(branch, null, ROUTE_PAGE_MAX, planId, stateAsked || "active"));
  const read = new Loader<CursorPage<CampaignView>>();
  const load = (query: string, keep = false) =>
    read.run(() => everyPage((cursor) => campaigns(session.signer, withCursor(query, cursor))), keep);
  $effect(() => { void load(asked); });
  // The labels the branch's campaigns use, which a row's add offers.
  const vocabulary = $derived(labelVocabulary(read.data?.items ?? []));

  // A branch's rows share one owner, so only everything's names it per row.
  const spec = $derived(campaignTable(read.data?.names ?? {}, branch.kind === "all"));
</script>

<!-- The name opens the campaign, as its id does where the reader shows that column. -->
{#snippet name(row: CampaignView)}<a href={campaignLink(row.campaign_id)}>{row.name}</a>{/snippet}
{#snippet owner(row: CampaignView)}<OwnerCell owner={row.owner} names={read.data?.names ?? {}} />{/snippet}
<!-- A plan the reader may not read is its id alone, as in the campaign's history. -->
{#snippet status(row: CampaignView)}
  {#if row.state === "archived"}<StatusPill status="archived" />
  {:else if row.plan}<StatusPill status={shownStatus(row.plan.status, row.plan.counts)} />{#if row.plan.cancel_requested}{" "}(cancel requested){/if}
  {:else}—{/if}
{/snippet}
{#snippet progress(row: CampaignView)}{progressText(row)}{/snippet}
<!-- The rows stay drawn while a new label is read back. -->
{#snippet labels(row: CampaignView)}
  <LabelsInline campaign={row} listing={path} {vocabulary} onchange={() => load(asked, true)}
                editable={!!session.account && mayLabel(session.account, row.owner)} />
{/snippet}

<Loaded of={read}>
  {#snippet children(listed)}
    {#if listed.unreadable}
      <div class="bar"><span class="err">{listed.unreadable} unreadable campaign record(s)</span></div>
    {/if}
    <DataTable {spec} rows={listed.items} id={(row) => row.campaign_id} noun="campaigns" {path}
               empty={planId ? `No campaign you may see ran ${planId}.`
                 : ownedEmpty(branch, stateAsked === "all" ? "campaigns" : `${stateAsked || "active"} campaigns`)}
               columnsKey="pipette.campaigns.columns" cells={{ name, owner, status, progress, labels }} />
  {/snippet}
</Loaded>
<!-- A refused id draws no table, so no box to correct it in. -->
{#if read.error && planId}
  <p><a href={href(path, [...here.query].filter(([key]) => key !== "plan_id" && key !== "offset"))}>Read the
    listing without this plan id</a></p>
{/if}
