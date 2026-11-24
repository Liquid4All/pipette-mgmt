<script lang="ts">
  // `/app` is the reader's organization's campaigns, which only the session knows, so the page sends the
  // reader there once drawn.
  import { redirect } from "../lib/route.svelte";
  import { homePath, type RouteOf } from "../lib/routes";
  import { session } from "../lib/session.svelte";

  let { route: _ }: { route: RouteOf<"home"> } = $props();

  $effect(() => redirect(homePath(session.account!)));
</script>
