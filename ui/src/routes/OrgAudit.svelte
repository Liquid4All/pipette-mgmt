<script lang="ts">
  // What changed in an organization, and who changed it, for its administrator.
  import AuditLog from "../lib/components/AuditLog.svelte";
  import OrgScope from "../lib/components/OrgScope.svelte";
  import { pathOf, type RouteOf } from "../lib/routes";

  let { route }: { route: RouteOf<"org-audit"> } = $props();
</script>

<OrgScope reference={route.org} title="Audit">
  {#snippet children(org)}
    <AuditLog org_id={org.org_id} path={pathOf(route)} />
  {/snippet}
</OrgScope>
