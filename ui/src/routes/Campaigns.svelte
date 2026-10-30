<script lang="ts">
  // The classic console's campaign listing (`static/templates/campaigns.html`
  // under no branch): `GET /api/v1/campaigns`, every campaign in the reader's reach,
  // each owner named. A link carrying the old owner filter lands on its branch,
  // as the classic `campaigns` redirected it.
  import CampaignListing from "../lib/components/CampaignListing.svelte";
  import PageHead from "../lib/components/PageHead.svelte";
  import { CAMPAIGN_PAGES, newCampaignAction } from "../lib/campaigns";
  import { here, redirect } from "../lib/route.svelte";
  import { legacyOwnerFilter, pathOf, type RouteOf } from "../lib/routes";
  import { session } from "../lib/session.svelte";

  let { route }: { route: RouteOf<"campaigns"> } = $props();

  // Derived: `owner=org` with no organization lands on this same address, filter dropped.
  const moved = $derived(legacyOwnerFilter(here.query, CAMPAIGN_PAGES, session.account!));
  $effect(() => { if (moved) redirect(moved); });
</script>

{#if !moved}
  <PageHead title="Campaigns" action={newCampaignAction({ kind: "all" })} inline />
  <CampaignListing branch={{ kind: "all" }} path={pathOf(route)} />
{/if}
