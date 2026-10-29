<script lang="ts">
  // The classic console's vault under an organization
  // (`static/templates/vault.html` under its branch):
  // `GET /api/v1/vault/resources?owner=org&org_id=`, its models or its runtimes.
  // The listing answers an empty page for an organization outside the roster
  // read, so the address resolves first, and one the reader may not read is not
  // found, as the classic `org_vault` refused it.
  import OrgScope from "../lib/components/OrgScope.svelte";
  import VaultListing from "../lib/components/VaultListing.svelte";
  import { orgLink, type RouteOf } from "../lib/routes";
  import { kindOfPage } from "../lib/vault";

  let { route }: { route: RouteOf<"org-vault" | "org-vault-runtimes"> } = $props();
</script>

<OrgScope reference={route.org} title="Vault" action={{ href: "/app/vault/new", label: "Upload a resource" }}>
  {#snippet children(org)}
    <VaultListing branch={{ kind: "org", org_id: org.org_id, slug: org.slug }} base={orgLink(org, "/vault")}
                  kind={kindOfPage(route.page)} />
  {/snippet}
</OrgScope>
