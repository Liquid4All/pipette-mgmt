<script lang="ts">
  // The global catalog, read page by page from its compact listing: one tab for
  // runtime entries and one for model entries.
  import { catalogImportRuns, globalCatalog } from "../lib/api";
  import { IMPORT_STALE_HOURS, importStatus } from "../lib/catalog-import-status";
  import {
    CATALOG_TABS, catalogKindOf, catalogListQuery, catalogTabs, flavors, offerWarning,
  } from "../lib/catalog";
  import DataTable from "../lib/components/DataTable.svelte";
  import Loaded from "../lib/components/Loaded.svelte";
  import PageHead from "../lib/components/PageHead.svelte";
  import TabBar from "../lib/components/TabBar.svelte";
  import { Loader } from "../lib/components/work.svelte";
  import { SERVER } from "../lib/crumbs";
  import { ago, when } from "../lib/format";
  import { here, redirect } from "../lib/route.svelte";
  import { pathOf, type RouteOf } from "../lib/routes";
  import { session } from "../lib/session.svelte";
  import { everyPage, withCursor } from "../lib/table";
  import type { CatalogImportRuns, DescriptorEntryListRow } from "../lib/api-types";
  import type { CursorPage } from "../lib/table";

  let { route }: { route: RouteOf<"server-catalog"> } = $props();
  const path = $derived(pathOf(route));
  const kind = $derived(catalogKindOf(here.query));
  const prefix = $derived(here.query.get("prefix") ?? "");
  const showHidden = $derived(here.query.get("hidden") === "1");
  const read = new Loader<CursorPage<DescriptorEntryListRow>>();
  const runs = new Loader<CatalogImportRuns>();
  let seen = $state(Date.now());
  $effect(() => {
    if (!session.account?.may.read_fleet) return;
    runs.run(async () => {
      const answer = await catalogImportRuns(session.signer);
      seen = Date.now();
      return answer;
    });
  });
  $effect(() => {
    const query = catalogListQuery(prefix, showHidden, kind);
    read.run(() => everyPage((cursor) => globalCatalog(session.signer, withCursor(query, cursor))));
  });

  function toggleHidden(checked: boolean) {
    const query = new URLSearchParams(here.query);
    if (checked) query.set("hidden", "1"); else query.delete("hidden");
    query.delete("offset");
    redirect(`${path}${query.size ? `?${query}` : ""}`);
  }
</script>

{#snippet flavorsCell(row: DescriptorEntryListRow)}
  <span class="flavors">{flavors(row)}</span>
{/snippet}
{#snippet offerCell(row: DescriptorEntryListRow)}
  {@const warning = offerWarning(row)}
  {#if warning}<span class="err">{warning}</span>{:else}—{/if}
{/snippet}

<PageHead trail={SERVER} title="Catalog" />
<TabBar label="Catalog" items={catalogTabs(kind)} />
<p class="muted">Global {kind} entries are managed with <code>pipette-mgmt catalog</code>.</p>
{#if session.account?.may.read_fleet}
<Loaded of={runs}>
  {#snippet children(history)}
    {@const shown = importStatus(history, seen)}
    <section class="import-status" class:notice={shown.attention} aria-label="Catalog importer status">
      {#if shown.latest && shown.look}
        {@const runAt = shown.latest.finished_at ?? shown.latest.started_at}
        <strong>Importer:</strong> {shown.latest.command === "refresh" ? "Refresh" : "Import"} {shown.look.label}
        <time datetime={runAt} title={when(runAt)}>{ago(runAt, seen)}</time>
        {#if shown.look.counts}
          <span>· {shown.latest.entries_created} created · {shown.latest.entries_hidden} hidden · {shown.latest.members_added} members added</span>
        {/if}
        {#if shown.result === "failed" && shown.latest.error_summary}
          <span class="err">· {shown.latest.error_summary}</span>
        {/if}
        {#if shown.latest.unclassified_assets.length}
          <details class="assets">
            <summary>{shown.latest.unclassified_assets.length} unclassified assets</summary>
            <ul>{#each shown.latest.unclassified_assets as asset}<li>{asset}</li>{/each}</ul>
          </details>
        {/if}
      {:else}
        <strong>Importer:</strong> no runs recorded.
      {/if}
      {#if shown.stale}
        <p class="stale">No successful import in the last {IMPORT_STALE_HOURS} hours{history.last_successful_import_at ? `; last success ${ago(history.last_successful_import_at, seen)}` : ""}.</p>
      {/if}
    </section>
  {/snippet}
</Loaded>
{/if}
<div class="bar">
  <label class="check"><input type="checkbox" checked={showHidden}
                            onchange={(event) => toggleHidden(event.currentTarget.checked)}>Show hidden</label>
</div>

<Loaded of={read}>
  {#snippet children(listed)}
    {#if listed.unreadable}
      <div class="bar"><span class="err">{listed.unreadable} unreadable catalog entry(s)</span></div>
    {/if}
    {#key kind}
      <DataTable spec={CATALOG_TABS[kind].table} rows={listed.items} id={(row) => row.name} noun="catalog entries"
                 {path} kept={[["kind", kind === "model" ? "model" : ""], ["hidden", showHidden ? "1" : ""]]}
                 columnsKey={CATALOG_TABS[kind].columnsKey} empty={`No global ${kind} entries.`}
                 cells={{ flavors: flavorsCell, offer_rule: offerCell }} />
    {/key}
  {/snippet}
</Loaded>

<style>
  .flavors { display: block; max-width: 32em; white-space: normal; overflow-wrap: anywhere; }
  .import-status { margin: 12px 0; padding: 10px 12px; border: 1px solid var(--border); border-radius: 6px; overflow-wrap: anywhere; }
  .import-status.notice { border-color: var(--amber-border); }
  .import-status time { margin-left: 5px; }
  .assets { margin: 8px 0 0; }
  .assets ul { margin: 6px 0 0; padding-left: 20px; overflow-wrap: anywhere; }
  .stale { margin: 6px 0 0; color: var(--amber); }
</style>
