<script module lang="ts">
  import type { NamedCampaignOverview } from "../lib/api-types";

  /**
   * The overview the campaign's address read before moving onto its plan's. The
   * move remounts the page, so it is kept here, where the plan's read takes it
   * rather than ask again.
   */
  let carried: NamedCampaignOverview | null = null;
</script>

<script lang="ts">
  // The classic console's campaign page (`static/templates/campaign.html`): the
  // campaign from `GET /api/v1/campaigns/{id}/overview` and a plan it ran, drawn by
  // `PlanTabs` with the campaign's details, history and Settings. Its own address
  // opens on the plan it runs, as the classic redirect did.
  import { tick, untrack } from "svelte";
  import { campaignOverview, campaigns, plan as planOf } from "../lib/api";
  import { auditAction } from "../lib/audit";
  import { catalogOrgOf } from "../lib/catalog-entry";
  import {
    CAMPAIGN_PAGES, campaignPlan, deviceTimeText, headAddress, labelPairs, labelScope, labelVocabulary,
    revertOffered, stateOf, unlessWithheld,
  } from "../lib/campaigns";
  import CampaignLabels from "../lib/components/CampaignLabels.svelte";
  import CampaignRename from "../lib/components/CampaignRename.svelte";
  import CampaignArchive from "../lib/components/CampaignArchive.svelte";
  import CampaignTransfer from "../lib/components/CampaignTransfer.svelte";
  import LabelsInline from "../lib/components/LabelsInline.svelte";
  import Loaded from "../lib/components/Loaded.svelte";
  import OwnerCell from "../lib/components/OwnerCell.svelte";
  import PlanTabs from "../lib/components/PlanTabs.svelte";
  import RestoreAction from "../lib/components/RestoreAction.svelte";
  import RevertAction from "../lib/components/RevertAction.svelte";
  import Revisions from "../lib/components/Revisions.svelte";
  import DangerZone from "../lib/components/DangerZone.svelte";
  import SettingsSection from "../lib/components/SettingsSection.svelte";
  import { Loader } from "../lib/components/work.svelte";
  import PageHead from "../lib/components/PageHead.svelte";
  import { when } from "../lib/format";
  import { ownedQuery, ownerBranch, underTrail } from "../lib/owned";
  import { here, redirect } from "../lib/route.svelte";
  import { campaignLink, campaignPlanLink, type RouteOf } from "../lib/routes";
  import { session } from "../lib/session.svelte";
  import { ROUTE_PAGE_MAX, everyPage } from "../lib/table";
  import { baseOf, legacyReports, tabLink, tabOf, type CampaignTabRoute } from "../lib/tabs";
  import type { NamedPlanView } from "../lib/api-types";

  let { route }: {
    route: RouteOf<"campaign"> | CampaignTabRoute | RouteOf<"campaign-plan-reports"> | RouteOf<"campaign-plan-report">;
  } = $props();

  const id = $derived(route.campaign_id);
  const asked = $derived(route.page === "campaign" ? null : route.plan_id);
  const tab = $derived(tabOf(route));
  // The Reports tab before Results moves on before anything is read.
  const legacy = $derived(route.page === "campaign-plan-reports" || route.page === "campaign-plan-report"
    ? legacyReports({ plan_id: route.plan_id, campaign_id: id }, "view" in route ? route.view : "", here.query)
    : null);
  $effect(() => { if (legacy) redirect(legacy); });

  interface Drawn {
    detail: NamedCampaignOverview;
    /** The plan the page draws: the one asked for, else the one the campaign runs. */
    shown: string | null;
    /** Its reads, or `null` where the reader's plan scope withholds it. */
    screen: { plan: NamedPlanView } | null;
  }

  const page = new Loader<Drawn>();
  const read = (shown: string | null) => async (): Promise<Drawn> => {
    // A reader without a plan grant is refused the read, so it is not asked; the jobs are `PlanTabs`' own read.
    const plan = shown && session.account!.may.read_plans !== "none"
      ? unlessWithheld(planOf(session.signer, shown).then((plan) => ({ plan })))
      : Promise.resolve(null);
    const kept = carried?.campaign_id === id && shown && carried.current_plan_id === shown ? carried : null;
    carried = null;
    const [detail, screen] = await Promise.all([kept ?? campaignOverview(session.signer, id), plan]);
    if (!shown && detail.current_plan_id) carried = detail;
    const drawn = shown ?? detail.current_plan_id ?? null;
    return { detail, shown: drawn, screen: shown ? campaignPlan(detail, shown, screen) : null };
  };
  // The campaign's own address moves onto its plan's, in an effect that dies with the page.
  const moved = $derived(!asked && page.data?.detail.current_plan_id
    ? headAddress(id, page.data.detail.current_plan_id) : null);
  $effect(() => { if (moved) redirect(moved); });
  // Read once per plan, as every tab draws from it; the page stays drawn while another plan's read comes in.
  $effect(() => {
    const shown = asked;
    if (!legacy) page.run(read(shown), true);
  });
  const reread = () => page.run(read(asked), true);
  let reverter = $state<ReturnType<typeof RevertAction>>();
  // Device time costs pipette-duckdb a query per total, so it is read only where the record shows it.
  const times = new Loader<NamedCampaignOverview>();
  const timed = $derived(!!page.data
    && ((!page.data.screen && !moved) || tab?.name === "revisions"));
  // Read again with each read of the page, as a write may have moved the campaign onto another plan.
  $effect(() => {
    void page.data;
    const campaign = id;
    if (!timed) return;
    // Untracked: each answer writes `times.data`, which would run this again.
    untrack(() => times.run(() => campaignOverview(session.signer, campaign, true),
                            times.data?.campaign_id === campaign));
  });

  const view = $derived(page.data?.detail ?? null);
  // The catalog organization's slug, read from the server for a reader that does not hold the organization already.
  let lookedUp = $state<{ id: string; slug: string } | null>(null);
  $effect(() => {
    const org_id = view?.catalog_org_id;
    if (!org_id || session.knownOrg(org_id)) return;
    untrack(() => void session.resolveOrg(org_id).catch(() => null)
      .then((org) => { if (org) lookedUp = { id: org_id, slug: org.slug }; }));
  });
  const slugOf = (org_id: string) =>
    session.knownOrg(org_id)?.slug ?? (lookedUp?.id === org_id ? lookedUp.slug : null);
  // Where the reader may name the plan's runtimes in the catalog; the server decides whether it may.
  const catalogOrg = $derived(view ? catalogOrgOf(view, slugOf) : null);
  // What the owner's campaigns already use, which a label's inputs offer: a whole listing, so read only once an
  // editor opens, and once per owner. A failed read offers nothing.
  const known = new Loader<Map<string, string[]>>();
  let knownFor = "";
  function suggest() {
    const scope = view && session.account ? labelScope(session.account, view.owner) : null;
    if (!scope || JSON.stringify(scope) === knownFor) return;
    knownFor = JSON.stringify(scope);
    const signer = session.signer;
    void known.run(async () => labelVocabulary((await everyPage((cursor) =>
      campaigns(signer, ownedQuery(scope, [], cursor, ROUTE_PAGE_MAX)))).items));
  }
  const vocabulary = $derived(known.data ?? new Map<string, string[]>());
  const under = $derived(view && session.account ? ownerBranch(session.account, view.owner, session.known, CAMPAIGN_PAGES) : null);
  // Set again on each tab, as every step to another address clears it.
  $effect(() => {
    void here.path;
    here.under = under?.href ?? null;
  });

  /** Whom a transfer that kept the campaign in reach handed it to, said atop the page, which takes the focus. */
  let transferred = $state("");
  let said = $state<HTMLElement>();
  async function transferDone(what: string) {
    transferred = what;
    await tick();
    said?.focus();
  }

  /** Why the page is gone after a transfer that left the campaign out of reach, drawn in its place. */
  let handed = $state<string | null>(null);
  let notice = $state<HTMLElement>();
  async function handedOver(why: string) {
    handed = why;
    // The form that held the focus is gone; the notice is what changed, so it takes it.
    await tick();
    notice?.focus();
  }
</script>

<!-- A transfer out of reach reads nothing again, so this still names the listing the campaign sat under. -->
<!-- The id until the name is read: drawn at once, the heading keeps the focus navigation gave it. -->
<PageHead trail={under ? underTrail(under, "Campaigns") : []} title={view?.name ?? id} inline
          action={session.account ? auditAction(session.account, id) : null} />

{#if handed}
  <p class="notice" role="status" tabindex="-1" bind:this={notice}>{handed}</p>
{:else if !moved && !legacy}
{#if transferred}<p class="ok" role="status" tabindex="-1" bind:this={said}>{transferred}</p>{/if}
<Loaded of={page}>
  {#snippet children({ detail, shown, screen })}
    {@const current = shown === (detail.current_plan_id ?? null)}
    <!-- The campaign's record: atop Revisions, or the page where no plan is drawn. -->
    {#snippet facts()}
      <div><dt>state</dt><dd>{stateOf(detail)}</dd></div>
      <div><dt>owner</dt><dd><OwnerCell owner={detail.owner} names={detail.names ?? {}} /></dd></div>
      <div><dt>created_at</dt><dd>{when(detail.created_at)}</dd></div>
      <div><dt>updated_at</dt><dd>{when(detail.updated_at)}</dd></div>
      {#if times.data?.device_time_current_plan}
        <div><dt>device time, current plan</dt><dd>{deviceTimeText(times.data.device_time_current_plan)}</dd></div>
      {/if}
      {#if times.data?.device_time_all_plans}
        <div><dt>device time, all plans</dt><dd>{deviceTimeText(times.data.device_time_all_plans)}</dd></div>
      {/if}
      <div><dt>campaign_id</dt><dd class="id">{detail.campaign_id}</dd></div>
    {/snippet}
    <!-- A revision is filed from the list it joins. -->
    {#snippet history()}
      {#if detail.may_edit_variants && detail.current_plan_id}
        <div class="bar"><a class="primary" href={campaignLink(id, "/document")}>Edit plan document</a></div>
        <p class="muted">Filing a revision creates a new plan that keeps unchanged jobs and their results, moves the
          campaign onto it, and cancels the current one.</p>
      {/if}
      <Revisions {detail} {shown} link={(planId) => campaignPlanLink(id, planId)} onchange={reread} />
    {/snippet}
    {#snippet lineage()}
      <dl class="summary">{@render facts()}</dl>
      {@render history()}
    {/snippet}
    {#snippet general()}
      <CampaignRename {detail} onchange={reread} />
      <CampaignLabels {detail} {vocabulary} onopen={suggest} onchange={reread} />
      <CampaignArchive {detail} onchange={reread} />
    {/snippet}
    {#snippet transfer()}
      <CampaignTransfer {detail} onchange={reread} onhanded={handedOver} ondone={transferDone} />
    {/snippet}

    {#if stateOf(detail) === "archived"}
      <p class="notice">This campaign is archived, so the listings hide it.</p>
    {/if}
    <!-- On every tab; nothing where there is no label and the reader may add none. -->
    {#if detail.may_administer || labelPairs(detail.labels).length}
      <p><LabelsInline campaign={detail} listing={under?.href} {vocabulary} onopen={suggest} onchange={reread}
                                      editable={detail.may_administer} /></p>
    {/if}

    <!-- Every plan the campaign ran has an address under it; the campaign's own opens what it runs now. -->
    {#if !current}
      <p class="notice">You're viewing an earlier plan. <a class="pill" href={campaignLink(id)}><span
        aria-hidden="true">→</span> View current plan</a>
        {#if shown && revertOffered(detail, shown)}
          <button type="button" class="pill" aria-haspopup="dialog" aria-label="Revert to this plan"
                  onclick={() => reverter?.revert(shown)}><span aria-hidden="true">↺</span> Revert to this</button>
        {/if}</p>
      <RevertAction {detail} onchange={reread} bind:this={reverter} />
      {#if shown && revertOffered(detail, shown)}
        <RestoreAction {detail} plan_id={shown} onchange={reread} />
      {/if}
    {/if}
    {#if screen && shown && tab}
      {@const base = baseOf(route as CampaignTabRoute)}
      <PlanTabs {base} {tab} plan={screen.plan} onchange={reread}
                record={`${current ? "Current plan" : "Earlier plan"} · ${screen.plan.plan_id}`}
                revisions={lineage} campaign={detail.may_administer ? { general, transfer } : undefined}
                {catalogOrg} />
    {:else}
      <!-- No plan drawn, so no tabs: the campaign's details, its history and its writes stand alone. -->
      <dl class="summary">
        <div><dt>plan</dt><dd>
          {#if shown}
            <!-- Named but not drawn: "not started" here would call a running team campaign idle. -->
            <span class="muted">withheld: reading a campaign's plans needs read_plans</span>
            <div class="muted id">{shown}</div>
          {:else}<span class="muted">none yet</span>{/if}
        </dd></div>
        {@render facts()}
      </dl>
      {#if detail.history.length}
        <h2>History</h2>
        {@render history()}
      {/if}
      {#if detail.may_administer}
        <div class="settings">
          <SettingsSection title="General">{@render general()}</SettingsSection>
          <DangerZone>{@render transfer()}</DangerZone>
        </div>
      {/if}
    {/if}
  {/snippet}
</Loaded>
{/if}
