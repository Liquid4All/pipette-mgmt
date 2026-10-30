<script lang="ts">
  // The classic console's campaigns under **Me** (`static/templates/campaigns.html`
  // under the reader's branch): `GET /api/v1/campaigns?owner=mine`. The app's root
  // draws it too, where a reader starts.
  import CampaignListing from "../lib/components/CampaignListing.svelte";
  import PageHead from "../lib/components/PageHead.svelte";
  import { newCampaignAction } from "../lib/campaigns";
  import { ME } from "../lib/crumbs";
  import { pathOf, type RouteOf } from "../lib/routes";

  let { route }: { route: RouteOf<"me-campaigns"> } = $props();
</script>

<PageHead trail={ME} title="Campaigns" action={newCampaignAction({ kind: "me" })} inline />
<CampaignListing branch={{ kind: "me" }} path={pathOf(route)} />
