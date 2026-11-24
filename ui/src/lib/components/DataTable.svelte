<script module lang="ts">
  /** The action column's key among the widths kept, which no field's name takes. */
  const ACTION = "#action";
</script>

<script lang="ts" generics="R">
  // A listing's table, as the Jobs tab draws one: its filters, the columns the
  // reader picks and the widths dragged to, a header that sorts per column, a
  // row a line, and the pager.
  // Its filters, sort and page live in the address, under `name` where the page
  // draws another table too. A listing gives its `path`; a table without one
  // sits on a page about something else and offers its filters only once it is long.
  // An address that names no sort is replaced by one that names the table's own.
  import { untrack, type Snippet } from "svelte";
  import { when } from "../format";
  import { offsetPaging } from "../pager";
  import { PickedColumns } from "../picked.svelte";
  import { here, href, redirect } from "../route.svelte";
  import {
    FILTERS_FROM, choicesOf, drawnColumns, filtering, ownQuery, statePairs, tableState, tableView, withOthers,
    type TableSpec, type TableState,
  } from "../table";
  import { ColumnWidths, headerWidths } from "../widths.svelte";
  import ColumnPicker from "./ColumnPicker.svelte";
  import Pager from "./Pager.svelte";
  import SortTh from "./SortTh.svelte";
  import StatusPill from "./StatusPill.svelte";
  import TableBar from "./TableBar.svelte";

  let { spec, rows, id, noun, empty, path, name = "", kept = [], keep = [], columnsKey, cells = {}, heads = {}, action,
        actionHead = "", marked }: {
    spec: TableSpec<R>;
    /** Every row read, before the filters. */
    rows: readonly R[];
    /** A row's key. */
    id: (row: R) => string;
    /** What a row is, plural: "campaigns". */
    noun: string;
    /** What the table says with no row to filter. */
    empty: string;
    /** The listing's own address, which the filters, the sort and the pager keep; the page's without one. */
    path?: string;
    /** What leads this table's keys in the address, as `<name>.sort`, on a page that draws another table. */
    name?: string;
    /** The address pairs the server's filters read, which every link keeps, and the keys of those kept empty. */
    kept?: [string, string][];
    keep?: string[];
    /** Where this browser keeps the columns picked; without one the table offers no pick. */
    columnsKey?: string;
    /** A column's own markup, by its key; any other is drawn from its `raw`. */
    cells?: Record<string, Snippet<[R]>>;
    /** Markup a column's header draws after its label, by its key. */
    heads?: Record<string, Snippet>;
    /** A row's own control, in a last column under `actionHead`. */
    action?: Snippet<[R]>;
    actionHead?: string;
    /** The row the page is about, tinted. */
    marked?: (row: R) => boolean;
  } = $props();

  const own = $derived(ownQuery(here.query, name));
  const table = $derived(tableState(spec, own));
  const paging = $derived(offsetPaging(own));
  const listed = $derived(tableView(spec, table, rows));
  const shown = $derived(listed.slice(paging.offset, paging.offset + paging.limit));
  const link = (to: TableState, offset = 0) => href(path ?? here.path, [...kept, ...withOthers(here.query, name,
    [...statePairs(to), ["limit", paging.asked], ["offset", offset ? String(offset) : ""]])], keep);
  // The hash is kept, as the reader may have followed a link to it.
  $effect(() => {
    if (table.sort && own.get("sort") !== table.sort) redirect(`${link(table, paging.offset)}${location.hash}`);
  });

  const picked = $derived(columnsKey ? new PickedColumns(columnsKey, untrack(() => spec.columns)) : null);
  const columns = $derived(drawnColumns(spec.columns, picked?.picks ?? null));
  const widths = $derived(columnsKey ? new ColumnWidths(`${columnsKey}.widths`) : null);
  let head = $state<HTMLTableRowElement>();
</script>

{#if path !== undefined || rows.length > FILTERS_FROM || filtering(table)}
  <TableBar {spec} state={table} choices={choicesOf(spec, rows, table)}
            kept={listed.length} total={rows.length} {noun} {link} />
{/if}
{#if picked}
  <div class="bar"><ColumnPicker columns={spec.columns} drawn={columns.map((column) => column.key)} {picked}
                                 {widths} /></div>
{/if}
<div class="wide">
  <table class:sized={widths?.sized}>
    {#if widths?.sized}
      <colgroup>
        {#each columns as column (column.key)}<col style:width="{widths.of(column.key)}px" />{/each}
        {#if action}<col style:width="{widths.of(ACTION)}px" />{/if}
      </colgroup>
    {/if}
    <thead><tr bind:this={head}>
      {#each columns as column (column.key)}
        <SortTh key={column.key} label={column.label} sort={table.sort} num={column.num}
                link={(sort) => link({ ...table, sort })}
                resize={widths?.grip(column.key, () => headerWidths(head))} extra={heads[column.key]} />
      {/each}
      {#if action}<th aria-label={actionHead ? undefined : "actions"} data-key={ACTION}>{actionHead}</th>{/if}
    </tr></thead>
    <tbody>
      {#each shown as row (id(row))}
        <tr class:shown={marked?.(row)}>
          {#each columns as column (column.key)}
            {@const own = cells[column.key]}
            {@const raw = column.raw(row)}
            {@const to = column.href?.(row)}
            <td class:num={column.num} class:id={column.as === "id" && !own && !to}>
              {#if own}{@render own(row)}
              {:else if to}<a class="id" href={to}>{raw}</a>
              {:else if column.as === "json"}{#if raw !== null}<code class="descriptor">{raw}</code>{/if}
              {:else if column.as === "time"}{when(raw === null ? null : String(raw))}
              {:else if column.as === "state" && raw !== null}<StatusPill status={String(raw)} />
              {:else}{raw ?? "—"}{/if}
            </td>
          {/each}
          {#if action}<td class="claim">{@render action(row)}</td>{/if}
        </tr>
      {:else}
        <!-- An offset past the last row keeps rows it does not draw. -->
        <tr><td colspan={columns.length + (action ? 1 : 0)} class="muted">{listed.length
          ? `No ${noun} at this position.` : rows.length ? `No ${noun} match these filters.` : empty}</td></tr>
      {/each}
    </tbody>
  </table>
</div>
<Pager offset={paging.offset} shown={shown.length} size={paging.limit} total={listed.length}
       at={(offset) => link(table, offset)} />

<style>
  /* A descriptor wraps in its own width, as on the Jobs tab; the table scrolls sideways. */
  .wide td:has(:global(code.descriptor)) { white-space: normal; }
</style>
