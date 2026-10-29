<script lang="ts">
  // Every audit event: the host's commands and the refusals no organization's
  // log lists included.
  import AuditLog from "../lib/components/AuditLog.svelte";
  import PageHead from "../lib/components/PageHead.svelte";
  import { SERVER } from "../lib/crumbs";
  import { pathOf, type RouteOf } from "../lib/routes";

  let { route }: { route: RouteOf<"server-audit"> } = $props();
</script>

<PageHead trail={SERVER} title="Audit log" />
<AuditLog path={pathOf(route)} />
