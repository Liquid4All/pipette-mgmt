<script lang="ts">
  // The classic console's campaigns under an organization
  // (`static/templates/campaigns.html` under its branch):
  // `GET /api/v1/campaigns?owner=org&org_id=`. The listing answers an empty page for an
  // organization outside the reach, so the address resolves first, and one the
  // reader may not read is not found, as the classic `org_campaigns` refused it. Its
  // Groups tab lists the organization's campaign groups.
  import CampaignGroups from "../lib/components/CampaignGroups.svelte";
  import CampaignListing from "../lib/components/CampaignListing.svelte";
  import OrgScope from "../lib/components/OrgScope.svelte";
  import TabBar from "../lib/components/TabBar.svelte";
  import { listingTabs } from "../lib/campaign-groups";
  import { newCampaignAction } from "../lib/campaigns";
  import { orgLink, pathOf, type RouteOf } from "../lib/routes";

  let { route }: { route: RouteOf<"org-campaigns" | "org-campaign-groups"> } = $props();

  const groups = $derived(route.page === "org-campaign-groups");
</script>

<OrgScope reference={route.org} title="Campaigns" inline
          action={(org) => newCampaignAction({ kind: "org", org_id: org.org_id, slug: org.slug })}>
  {#snippet children(org)}
    {@const branch = { kind: "org" as const, org_id: org.org_id, slug: org.slug }}
    {@const base = orgLink(org, "/campaigns")}
    <TabBar label="Campaigns" items={listingTabs(base, groups)} />
    {#if groups}
      <CampaignGroups {branch} {base} />
    {:else}
      <CampaignListing {branch} path={pathOf(route)} />
    {/if}
  {/snippet}
</OrgScope>
