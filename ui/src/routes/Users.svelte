<script lang="ts">
  // The classic console's roster (`static/templates/users.html` over every
  // account the reader reads). A link carrying the old `org` filter lands on
  // that organization's members, as the classic `users` redirected it.
  import { legacyRosterFilter, newAccountAction } from "../lib/accounts";
  import PageHead from "../lib/components/PageHead.svelte";
  import Roster from "../lib/components/Roster.svelte";
  import { here, redirect } from "../lib/route.svelte";
  import { pathOf, type RouteOf } from "../lib/routes";
  import { session } from "../lib/session.svelte";

  let { route }: { route: RouteOf<"users"> } = $props();

  const moved = legacyRosterFilter(here.query);
  $effect(() => { if (moved) redirect(moved); });
</script>

{#if !moved}
  <PageHead title="Users" action={newAccountAction(session.account!)} />
  <Roster branch={{ kind: "all" }} path={pathOf(route)} />
{/if}
