<script lang="ts">
  // The classic console's devices under **Me** (`static/templates/clients.html`
  // under the reader's branch): `GET /api/v1/clients?owner=mine`, the devices the
  // reader owns as a person.
  import DeviceListing from "../lib/components/DeviceListing.svelte";
  import PageHead from "../lib/components/PageHead.svelte";
  import { ME } from "../lib/crumbs";
  import { pathOf, type RouteOf } from "../lib/routes";

  let { route }: { route: RouteOf<"me-devices"> } = $props();
</script>

<PageHead trail={ME} title="Devices" />
<DeviceListing branch={{ kind: "me" }} path={pathOf(route)} />
