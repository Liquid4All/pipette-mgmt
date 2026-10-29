<script lang="ts">
  // The classic console's members of an organization (`static/templates/users.html`
  // under its branch): `GET /api/v1/users`, narrowed to the organization.
  import { newAccountAction } from "../lib/accounts";
  import OrgScope from "../lib/components/OrgScope.svelte";
  import Roster from "../lib/components/Roster.svelte";
  import { pathOf, type RouteOf } from "../lib/routes";
  import { session } from "../lib/session.svelte";

  let { route }: { route: RouteOf<"org-members"> } = $props();
</script>

<OrgScope reference={route.org} title="Members" action={newAccountAction(session.account!)}>
  {#snippet children(org)}
    <Roster branch={{ kind: "org", org_id: org.org_id, slug: org.slug }} path={pathOf(route)} />
  {/snippet}
</OrgScope>
