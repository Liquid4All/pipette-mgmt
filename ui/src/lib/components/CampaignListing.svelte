<script lang="ts">
  // The classic console's campaign listing (`campaigns.html`) under any branch:
  // every page of `GET /api/v1/campaigns` narrowed to the branch, newest first, each row
  // with what the plan it runs is doing where the reader may read that plan,
  // filtered, sorted and paged in `DataTable`, the plan id and the state by the server,
  // a job id by the campaigns of the plans that asked for it.
  import { campaigns, jobPlans } from "../api";
  import { campaignQuery, campaignTable, jobCampaigns, labelVocabulary, mayLabel, outsideCount, progressFill,
    progressText, soughtEmpty } from "../campaigns";
  import { ownedEmpty, type OwnedBranch } from "../owned";
  import { shownStatus } from "../plans";
  import { here, href } from "../route.svelte";
  import { campaignLink, pathOf } from "../routes";
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
  const jobId = $derived(here.query.get("job_id")?.trim() ?? "");
  const sought = $derived([planId, jobId].filter(Boolean).join(" and "));

  // `""` for the active campaigns, `archived` or `all`; any other value is the default.
  const stateAsked = $derived((["archived", "all"] as const).find((value) => value === here.query.get("state")) ?? "");

  // Read again whenever what it sends changes, not on a filter, which applies here. A job id, as a plan id does,
  // finds its campaigns in any state.
  const asked = $derived(campaignQuery(branch, null, ROUTE_PAGE_MAX, planId, jobId ? "all" : stateAsked || "active"));
  const read = new Loader<CursorPage<CampaignView> & { ran?: { job: string; campaigns: Set<string> } }>();
  const load = (query: string, job: string, keep = false) => read.run(async () => {
    // A label edit (`keep`) reads the listing again but not the job's plans, which no label changes.
    const known = keep && read.data?.ran?.job === job ? read.data.ran.campaigns : null;
    const [listed, ran] = await Promise.all([
      everyPage((cursor) => campaigns(session.signer, withCursor(query, cursor))),
      job ? known ?? jobCampaigns(jobPlans(session.signer, job)) : null,
    ]);
    if (!ran) return listed;
    return { ...listed, items: listed.items.filter((row) => ran.has(row.campaign_id)), ran: { job, campaigns: ran } };
  }, keep);
  $effect(() => { void load(asked, jobId); });
  const outside = $derived(outsideCount(branch, planId, read.data?.ran?.campaigns, read.data?.items.length ?? 0));
  // The labels the branch's campaigns use, which a row's add offers.
  const vocabulary = $derived(labelVocabulary(read.data?.items ?? []));

  // A branch's rows share one owner, so only everything's names it per row.
  const spec = $derived(campaignTable(read.data?.names ?? {}, branch.kind === "all", !!sought,
                                      session.account?.may.read_plans !== "none"));
  const without = (key: string) => href(path, [...here.query].filter(([held]) => held !== key && held !== "offset"));
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
<!-- A bar fills the cell behind the text: the finished jobs, then the failed ones, in the colors of the plan's phase.
     The text's tooltip names the failed count. -->
{#snippet progress(row: CampaignView)}
  {@const fill = progressFill(row)}
  {@const failed = row.plan?.counts?.failed}
  {#if fill}<span class="fill" data-phase={fill.phase} data-failed={fill.done > fill.finished || undefined}
                  style:--finished="{fill.finished * 100}%"
                  style:--done="{fill.done * 100}%"></span>{/if}<span
    title={failed ? `${failed} failed` : undefined}>{progressText(row)}</span>
{/snippet}
<!-- The rows stay drawn while a new label is read back. -->
{#snippet labels(row: CampaignView)}
  <LabelsInline campaign={row} listing={path} {vocabulary} onchange={() => load(asked, jobId, true)}
                editable={!!session.account && mayLabel(session.account, row.owner)} />
{/snippet}

<Loaded of={read}>
  {#snippet children(listed)}
    {#if listed.unreadable}
      <div class="bar"><span class="err">{listed.unreadable} unreadable campaign record(s)</span></div>
    {/if}
    {#if outside}
      <p class="notice">{outside === 1 ? "1 campaign" : `${outside} campaigns`} outside this listing
        {outside === 1 ? "holds" : "hold"} this job:
        <a href={href(pathOf({ page: "campaigns" }), [["job_id", jobId]])}>see every campaign that holds it</a>.</p>
    {/if}
    <DataTable {spec} rows={listed.items} id={(row) => row.campaign_id} noun="campaigns" {path}
               empty={sought ? soughtEmpty(branch, sought)
                 : ownedEmpty(branch, stateAsked === "all" ? "campaigns" : `${stateAsked || "active"} campaigns`)}
               columnsKey="pipette.campaigns.columns" cells={{ name, owner, status, progress, labels }} />
  {/snippet}
</Loaded>
<!-- A refused id draws no table, so no box to correct it in. -->
{#if read.error && planId}
  <p><a href={without("plan_id")}>Read the listing without this plan id</a></p>
{/if}
{#if read.error && jobId}
  <p><a href={without("job_id")}>Read the listing without this job id</a></p>
{/if}
