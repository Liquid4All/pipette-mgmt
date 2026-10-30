<script lang="ts">
  // The classic console's private benchmarks of every organization the reader
  // reads (`static/templates/benchmarks.html` under no branch): `GET /api/v1/orgs` for
  // a reader of every organization, else its own, then one
  // `GET /api/v1/orgs/{org}/benchmarks` each. A link carrying the old `org` filter
  // lands on that organization's listing, as the classic `benchmarks` redirected it.
  import { orgBenchmarks, orgs } from "../lib/api";
  import { legacyCatalogFilter, newBenchmarkAction, readsEveryCatalog, type BenchmarkCatalogs } from "../lib/benchmarks";
  import BenchmarkListing from "../lib/components/BenchmarkListing.svelte";
  import PageHead from "../lib/components/PageHead.svelte";
  import { Loader } from "../lib/components/work.svelte";
  import { ownOrg, type OrgRef } from "../lib/orgs";
  import { here, redirect } from "../lib/route.svelte";
  import { pathOf, type RouteOf } from "../lib/routes";
  import { session } from "../lib/session.svelte";

  let { route }: { route: RouteOf<"benchmarks"> } = $props();

  const moved = legacyCatalogFilter(here.query);
  $effect(() => { if (moved) redirect(moved); });

  const reader = session.account!;
  const page = new Loader<BenchmarkCatalogs>();
  if (!moved) {
    page.run(async () => {
      const signer = session.signer;
      const own = ownOrg(reader);
      const read: OrgRef[] = readsEveryCatalog(reader) ? await orgs(signer, { counts: false }) : own ? [own] : [];
      const catalogs = await Promise.all(read.map(async (org) =>
        ({ org, catalog: await orgBenchmarks(signer, org.org_id) })));
      return { read, catalogs };
    });
  }
</script>

{#if !moved}
  <PageHead title="Benchmarks" action={page.data ? newBenchmarkAction(reader, page.data.read, null) : null} />
  <BenchmarkListing branch={{ kind: "all" }} path={pathOf(route)} of={page} />
{/if}
