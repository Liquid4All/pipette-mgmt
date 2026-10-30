<script lang="ts">
  // The classic console's vault under **Me** (`static/templates/vault.html`
  // under the reader's branch): `GET /api/v1/vault/resources?owner=mine`, its
  // models, or its runtimes.
  import PageHead from "../lib/components/PageHead.svelte";
  import VaultListing from "../lib/components/VaultListing.svelte";
  import { ME } from "../lib/crumbs";
  import { pathOf, type RouteOf } from "../lib/routes";
  import { kindOfPage } from "../lib/vault";

  let { route }: { route: RouteOf<"me-vault" | "me-vault-runtimes"> } = $props();
</script>

<PageHead trail={ME} title="Vault" action={{ href: "/app/vault/new", label: "Upload a resource" }} />
<VaultListing branch={{ kind: "me" }} base={pathOf({ page: "me-vault" })} kind={kindOfPage(route.page)} />
