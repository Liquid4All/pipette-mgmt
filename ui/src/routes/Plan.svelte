<script lang="ts">
  // The classic console's plan page (`static/templates/plan.html`), now a plan's
  // screen: the plan from `GET /api/v1/plans/{id}`, drawn by `PlanTabs`, and on
  // Revisions its campaign's history from `GET /api/v1/campaigns/{id}/overview`.
  import { untrack } from "svelte";
  import { campaignOverview, plan } from "../lib/api";
  import { planTrail } from "../lib/campaigns";
  import Loaded from "../lib/components/Loaded.svelte";
  import PlanTabs from "../lib/components/PlanTabs.svelte";
  import Revisions from "../lib/components/Revisions.svelte";
  import { Loader } from "../lib/components/work.svelte";
  import PageHead from "../lib/components/PageHead.svelte";
  import { here, redirect } from "../lib/route.svelte";
  import { planLink, type RouteOf } from "../lib/routes";
  import { session } from "../lib/session.svelte";
  import { baseOf, legacyReports, legacyResults, tabOf, type PlanTabRoute } from "../lib/tabs";
  import type { NamedCampaignOverview, NamedPlanView } from "../lib/api-types";

  let { route }: {
    route: PlanTabRoute | RouteOf<"plan-results"> | RouteOf<"plan-reports"> | RouteOf<"plan-report">;
  } = $props();

  const id = $derived(route.plan_id);
  const tab = $derived(tabOf(route));
  // The results page before the tabs, and the Reports tab before Results, move on before anything is read.
  const moved = $derived(route.page === "plan-results"
    ? legacyResults({ plan_id: id, campaign_id: null }, here.query, location.hash)
    : route.page === "plan-reports" || route.page === "plan-report"
      ? legacyReports({ plan_id: id, campaign_id: null }, "view" in route ? route.view : "", here.query) : null);
  $effect(() => { if (moved) redirect(moved); });

  const page = new Loader<{ plan: NamedPlanView }>();
  const read = (of: string) => async () => ({ plan: await plan(session.signer, of) });
  // Read once per plan: every tab draws from it and the jobs `PlanTabs` reads, so a filter reads nothing.
  $effect(() => {
    const of = id;
    if (!moved) page.run(read(of), true);
  });
  const reread = () => page.run(read(id), true);

  // Its campaign's history, read once per screen for the History tab, which a reader refused it does not see.
  const campaign = $derived(page.data?.plan.campaign_id ?? null);
  const lineage = new Loader<NamedCampaignOverview>();
  $effect(() => {
    const of = campaign;
    if (of) untrack(() => lineage.run(() => campaignOverview(session.signer, of)));
  });
  const refused = $derived(lineage.status === 403 || lineage.status === 404);

  const trail = $derived(page.data || page.error
    ? planTrail(id, campaign, lineage.data, session.account!, session.known) : []);
</script>

{#snippet revisions()}
  <Loaded of={lineage}>
    {#snippet children(detail)}<Revisions {detail} shown={id} link={planLink} onchange={reread} />{/snippet}
  </Loaded>
{/snippet}

{#if !moved && tab}
  <!-- Drawn once the plan is read, so a campaign's plan never flashes the wrong link. -->
  <!-- The id at once, so the heading keeps the focus navigation gave it. -->
  <PageHead {trail} title={id} />

  <Loaded of={page}>
    {#snippet children(drawn)}
      <PlanTabs base={baseOf(route as PlanTabRoute)} {tab} plan={drawn.plan} onchange={reread}
                revisions={campaign && !refused ? revisions : undefined} />
    {/snippet}
  </Loaded>
{/if}
