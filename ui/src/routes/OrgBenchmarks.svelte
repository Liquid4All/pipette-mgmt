<script lang="ts">
  // The classic console's private benchmarks of an organization
  // (`static/templates/benchmarks.html` under its branch):
  // `GET /api/v1/orgs/{org}/benchmarks`, once the address resolves as `OrgScope`
  // resolves it, so one outside the reader's reach is not found and not asked.
  import { orgBenchmarks } from "../lib/api";
  import { newBenchmarkAction, type BenchmarkCatalogs } from "../lib/benchmarks";
  import BenchmarkListing from "../lib/components/BenchmarkListing.svelte";
  import { scopedOrg } from "../lib/components/OrgScope.svelte";
  import PageHead from "../lib/components/PageHead.svelte";
  import { Loader } from "../lib/components/work.svelte";
  import { orgTrail } from "../lib/crumbs";
  import { ownOrg } from "../lib/orgs";
  import { pathOf, type RouteOf } from "../lib/routes";
  import { session } from "../lib/session.svelte";

  let { route }: { route: RouteOf<"org-benchmarks"> } = $props();

  const reader = session.account!;
  const page = new Loader<BenchmarkCatalogs>();
  page.run(async () => {
    const org = await scopedOrg(route.org);
    const catalog = await orgBenchmarks(session.signer, org.org_id);
    const own = ownOrg(reader);
    return { read: own ? [org, own] : [org], catalogs: [{ org, catalog }] };
  });
  const org = $derived(page.data?.catalogs[0]?.org ?? null);
</script>

<PageHead trail={orgTrail(org ?? route.org)} title="Benchmarks"
          action={page.data ? newBenchmarkAction(reader, page.data.read, org) : null} />
<BenchmarkListing branch={{ kind: "org", slug: org?.slug ?? route.org }} path={pathOf(route)} of={page} />
