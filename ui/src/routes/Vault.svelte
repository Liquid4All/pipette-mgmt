<script lang="ts">
  // The classic console's vault listing (`static/templates/vault.html` under no
  // branch): `GET /api/v1/vault/resources`, every resource in the reader's reach, each
  // owner named. A link carrying the old owner filter lands on its branch, as
  // the classic `vault` redirected it.
  import PageHead from "../lib/components/PageHead.svelte";
  import VaultListing from "../lib/components/VaultListing.svelte";
  import { here, redirect } from "../lib/route.svelte";
  import { legacyOwnerFilter, pathOf, type RouteOf } from "../lib/routes";
  import { session } from "../lib/session.svelte";
  import { VAULT_PAGES, kindOfPage } from "../lib/vault";

  let { route }: { route: RouteOf<"vault" | "vault-runtimes"> } = $props();

  // Derived: `owner=org` with no organization lands on this same address, filter dropped.
  const moved = $derived(
    legacyOwnerFilter(here.query, VAULT_PAGES, session.account!));
  $effect(() => { if (moved) redirect(moved); });
</script>

{#if !moved}
  <PageHead title="Vault" action={{ href: "/app/vault/new", label: "Upload a resource" }} />
  <VaultListing branch={{ kind: "all" }} base={pathOf({ page: "vault" })} kind={kindOfPage(route.page)} />
{/if}
