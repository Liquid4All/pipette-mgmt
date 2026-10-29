<script lang="ts">
  // The columns a table draws, picked among all it has and kept in this browser, with the way back to its defaults,
  // and back to its own column widths where the reader set any.
  import type { PickedColumns } from "../picked.svelte";
  import type { ColumnWidths } from "../widths.svelte";
  import MultiSelect from "./MultiSelect.svelte";

  let { columns, drawn, picked, widths = null }: {
    /** Every column the table has. */
    columns: readonly { key: string; label: string }[];
    /** The keys drawn now. */
    drawn: readonly string[];
    picked: PickedColumns;
    widths?: ColumnWidths | null;
  } = $props();
</script>

<MultiSelect label="columns" options={columns.map((column) => ({ value: column.key, label: column.label }))}
             chosen={[...drawn]} toggle={(key, on) => picked.pick(key, on)} none="none" />
{#if picked.picks}<button type="button" class="again" onclick={() => picked.reset()}>Default columns</button>{/if}
{#if widths?.sized}<button type="button" class="again" onclick={() => widths.reset()}>Default widths</button>{/if}
