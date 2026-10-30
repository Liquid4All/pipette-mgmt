<script lang="ts">
  // The classic console's vault listing (`vault.html`) under any branch: every
  // page of `GET /api/v1/vault/resources` narrowed to the branch, newest first, each row
  // with its files, its descriptor and whether its file is still stored, filtered,
  // sorted and paged in `DataTable`.
  import { vaultResources } from "../api";
  import { bytesText } from "../plans";
  import { here } from "../route.svelte";
  import { session } from "../session.svelte";
  import { ownedEmpty, type OwnedBranch } from "../owned";
  import { ROUTE_PAGE_MAX, everyPage, withCursor, type CursorPage } from "../table";
  import {
    VAULT_COLUMNS, listedUnder, runtimesPath, storedCell, vaultQuery, vaultTable, vaultTabs, type VaultKind,
  } from "../vault";
  import type { VaultResourceDetail } from "../api-types";
  import DataTable from "./DataTable.svelte";
  import Loaded from "./Loaded.svelte";
  import OwnerCell from "./OwnerCell.svelte";
  import TabBar from "./TabBar.svelte";
  import { Loader } from "./work.svelte";

  let { branch, base, kind }: {
    branch: OwnedBranch;
    /** The vault's address, as the tree names it: its models are there, its runtimes under it. */
    base: string;
    /** The tab drawn. */
    kind: VaultKind;
  } = $props();

  // The page's own address, which the filters, the sort and the pager keep.
  const path = $derived(kind === "runtime" ? runtimesPath(base) : base);
  // The runtimes sit under the vault's leaf of the tree.
  $effect(() => { if (kind === "runtime") here.under = base; });

  // Read again whenever the query it sends changes, not on a filter, which applies here.
  const asked = $derived(vaultQuery(branch, null, ROUTE_PAGE_MAX));
  const read = new Loader<CursorPage<VaultResourceDetail>>();
  $effect(() => {
    const query = asked;
    read.run(() => everyPage((cursor) => vaultResources(session.signer, withCursor(query, cursor))));
  });

  // A branch's rows share one owner, so only everything's offers the owners to pick among.
  const spec = $derived(vaultTable(read.data?.names ?? {}, kind, branch.kind === "all"));
  const all = $derived((read.data?.items ?? []).filter((row) => listedUnder(row) === kind));
  const noun = $derived(kind === "runtime" ? "vault runtimes" : "vault models");
</script>

{#snippet owner(row: VaultResourceDetail)}<OwnerCell owner={row.owner} names={read.data?.names ?? {}} />{/snippet}
{#snippet bytes(row: VaultResourceDetail)}<span title="{row.bytes} bytes">{bytesText(row.bytes)}</span>{/snippet}
{#snippet object(row: VaultResourceDetail)}
  {@const stored = storedCell(row.object)}
  <span class:muted={stored.muted} class:err={stored.fault}>{row.object?.state ?? ""}</span>
{/snippet}

<TabBar label="Vault" items={vaultTabs(base, kind)} />
<Loaded of={read}>
  {#snippet children(listed)}
    {#if listed.unreadable}
      <div class="bar"><span class="err">{listed.unreadable} unreadable vault record(s)</span></div>
    {/if}
    <!-- Each tab keeps its picked columns apart, since a pick on one would hide what only the other has. -->
    <DataTable {spec} rows={all} id={(row) => row.resource_id} {noun} {path} empty={ownedEmpty(branch, noun)}
               columnsKey={VAULT_COLUMNS[kind]} cells={{ owner, bytes, object }} />
  {/snippet}
</Loaded>
