<script lang="ts">
  // The classic console's devices under an organization
  // (`static/templates/clients.html` under its branch): `GET /api/v1/clients?org_id=`,
  // what it owns directly or through a member, on its roster read, and its
  // device pre-auth keys for a reader who may mint one.
  import DeviceKeys from "../lib/components/DeviceKeys.svelte";
  import DeviceListing from "../lib/components/DeviceListing.svelte";
  import OrgScope from "../lib/components/OrgScope.svelte";
  import { mayMintFor } from "../lib/devices";
  import { pathOf, type RouteOf } from "../lib/routes";
  import { session } from "../lib/session.svelte";

  let { route }: { route: RouteOf<"org-devices"> } = $props();
</script>

<!-- Fresh, for "Registered clients", which a claim elsewhere in the app changes. -->
<OrgScope reference={route.org} title="Devices" fresh>
  {#snippet children(org)}
    <DeviceListing
      branch={{ kind: "org", org_id: org.org_id, slug: org.slug,
                count: "client_count" in org ? org.client_count ?? null : null }}
      path={pathOf(route)} />
    {#if session.account && mayMintFor(session.account, org.org_id)}
      <DeviceKeys {org} />
    {/if}
  {/snippet}
</OrgScope>
