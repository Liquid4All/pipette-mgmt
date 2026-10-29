<script lang="ts">
  // An address under `/app/` that names no page.
  import type { Route } from "../lib/routes";

  let { route }: { route: Extract<Route, { page: "missing" }> } = $props();
</script>

<h1>Not found</h1>
<p>This app has no page at <code>{route.path}</code>. <a href="/app/account">Your account</a> is.</p>
