<script lang="ts">
  // The classic console's organizations (`static/templates/orgs.html`), the
  // whole list filtered, sorted and paged in `DataTable`.
  import { orgs } from "../lib/api";
  import DataTable from "../lib/components/DataTable.svelte";
  import Loaded from "../lib/components/Loaded.svelte";
  import PageHead from "../lib/components/PageHead.svelte";
  import { Loader } from "../lib/components/work.svelte";
  import { ORG_TABLE, bySlug } from "../lib/orgs";
  import { pathOf, type RouteOf } from "../lib/routes";
  import { session } from "../lib/session.svelte";
  import type { OrgView } from "../lib/api-types";

  let { route }: { route: RouteOf<"orgs"> } = $props();

  const read = new Loader<OrgView[]>();
  read.run(async () => (await orgs(session.signer)).sort(bySlug));
</script>

<PageHead title="Organizations" action={session.account?.may.create_orgs
  ? { href: "/app/orgs/new", label: "New organization" } : null} />

<Loaded of={read}>
  {#snippet children(listed)}
    <DataTable spec={ORG_TABLE} rows={listed} id={(row) => row.org_id} noun="organizations" path={pathOf(route)}
               empty="No organizations you may see." columnsKey="pipette.orgs.columns" />
  {/snippet}
</Loaded>
