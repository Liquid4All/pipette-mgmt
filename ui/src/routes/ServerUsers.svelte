<script lang="ts">
  // The classic console's unaffiliated users (`static/templates/users.html`
  // under the server's branch): the roster narrowed to accounts in no organization.
  import { newAccountAction } from "../lib/accounts";
  import PageHead from "../lib/components/PageHead.svelte";
  import Roster from "../lib/components/Roster.svelte";
  import { SERVER } from "../lib/crumbs";
  import { pathOf, type RouteOf } from "../lib/routes";
  import { session } from "../lib/session.svelte";

  let { route }: { route: RouteOf<"server-users"> } = $props();
</script>

<PageHead trail={SERVER} title="Unaffiliated users" action={newAccountAction(session.account!)} />
<Roster branch={{ kind: "server" }} path={pathOf(route)} />
