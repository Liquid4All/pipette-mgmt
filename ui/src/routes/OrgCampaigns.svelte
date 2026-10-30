<script lang="ts">
  // The classic console's campaigns under an organization
  // (`static/templates/campaigns.html` under its branch):
  // `GET /api/v1/campaigns?owner=org&org_id=`. The listing answers an empty page for an
  // organization outside the reach, so the address resolves first, and one the
  // reader may not read is not found, as the classic `org_campaigns` refused it.
  import CampaignListing from "../lib/components/CampaignListing.svelte";
  import OrgScope from "../lib/components/OrgScope.svelte";
  import { newCampaignAction } from "../lib/campaigns";
  import { pathOf, type RouteOf } from "../lib/routes";

  let { route }: { route: RouteOf<"org-campaigns"> } = $props();
</script>

<OrgScope reference={route.org} title="Campaigns" inline
          action={(org) => newCampaignAction({ kind: "org", org_id: org.org_id, slug: org.slug })}>
  {#snippet children(org)}
    <CampaignListing branch={{ kind: "org", org_id: org.org_id, slug: org.slug }} path={pathOf(route)} />
  {/snippet}
</OrgScope>
