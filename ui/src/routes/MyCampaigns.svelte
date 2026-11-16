<script lang="ts">
  // The classic console's campaigns under **Me** (`static/templates/campaigns.html`
  // under the reader's branch): `GET /api/v1/campaigns?owner=mine`. The app's root
  // draws it too, where a reader starts. Its Groups tab lists the reader's campaign groups.
  import CampaignGroups from "../lib/components/CampaignGroups.svelte";
  import CampaignListing from "../lib/components/CampaignListing.svelte";
  import PageHead from "../lib/components/PageHead.svelte";
  import TabBar from "../lib/components/TabBar.svelte";
  import { listingTabs } from "../lib/campaign-groups";
  import { newCampaignAction } from "../lib/campaigns";
  import { ME } from "../lib/crumbs";
  import { pathOf, type RouteOf } from "../lib/routes";

  let { route }: { route: RouteOf<"me-campaigns" | "me-campaign-groups"> } = $props();

  const base = pathOf({ page: "me-campaigns" });
  const groups = $derived(route.page === "me-campaign-groups");
</script>

<PageHead trail={ME} title="Campaigns" action={newCampaignAction({ kind: "me" })} inline />
<TabBar label="Campaigns" items={listingTabs(base, groups)} />
{#if groups}
  <CampaignGroups branch={{ kind: "me" }} {base} />
{:else}
  <CampaignListing branch={{ kind: "me" }} path={pathOf(route)} />
{/if}
