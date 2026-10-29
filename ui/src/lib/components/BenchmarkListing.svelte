<script lang="ts">
  // The classic console's private benchmark listing (`benchmarks.html`) under
  // either branch. Each `GET /api/v1/orgs/{org}/benchmarks` answers a whole catalog,
  // so `DataTable` filters, sorts and pages the rows here.
  import {
    benchmarkRows, benchmarkTable, benchmarksEmpty, type BenchmarkBranch, type BenchmarkCatalogs, type BenchmarkRow,
  } from "../benchmarks";
  import { orgLink } from "../routes";
  import DataTable from "./DataTable.svelte";
  import Loaded from "./Loaded.svelte";
  import type { Loader } from "./work.svelte";

  let { branch, path, of }: {
    branch: BenchmarkBranch;
    /** The page's own address, which the filters, the sort and the pager keep. */
    path: string;
    /** The organizations the reader reads, and the catalogs the listing draws. */
    of: Loader<BenchmarkCatalogs>;
  } = $props();

  // A branch's rows share one organization, so only everything's names it per row.
  const spec = $derived(benchmarkTable(branch.kind === "all"));
  const listed = $derived(benchmarkRows(of.data?.catalogs ?? []));
</script>

{#snippet organization(row: BenchmarkRow)}<a href={orgLink(row.org)} title={row.org.org_id}>{row.org.slug}</a>{/snippet}
{#snippet benchmark_type(row: BenchmarkRow)}
  {#if row.benchmark_type}{row.benchmark_type}{:else}<span class="err">unreadable</span>{/if}
{/snippet}

<Loaded {of}>
  {#snippet children({ read })}
    <DataTable {spec} rows={listed} id={(row) => `${row.org.org_id}/${row.benchmark_id}`} noun="benchmarks" {path}
               empty={benchmarksEmpty(branch, read.length)} columnsKey="pipette.benchmarks.columns"
               cells={{ organization, benchmark_type }} />
  {/snippet}
</Loaded>
