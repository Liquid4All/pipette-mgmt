<script lang="ts">
  // The classic console's fleet (`static/templates/clients.html` over every
  // device): `GET /api/v1/clients`, every device the reader's fleet grant reads.
  import DeviceListing from "../lib/components/DeviceListing.svelte";
  import PageHead from "../lib/components/PageHead.svelte";
  import { pathOf, type RouteOf } from "../lib/routes";

  let { route }: { route: RouteOf<"clients"> } = $props();
</script>

<PageHead title="Fleet" />
<DeviceListing branch={{ kind: "all" }} path={pathOf(route)} />
