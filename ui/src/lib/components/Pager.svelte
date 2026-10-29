<script lang="ts">
  // A listing's paging links. A cursor listing goes to its first page or the
  // next one, as the JSON routes page; an offset one goes either way and says
  // which rows it shows (`pager.html`).
  import { offsetPager } from "../pager";

  type Props =
    | { cursor: string | null; first: string; next: string | null }
    | { offset: number; shown: number; size: number; total?: number | null; more?: boolean;
        at: (offset: number) => string };

  let props: Props = $props();

  const paged = $derived("offset" in props
    ? offsetPager(props.offset, props.shown, props.size, props.total ?? null, props.more) : null);
</script>

{#if "cursor" in props}
  {#if props.cursor || props.next}
    <div class="bar">
      {#if props.cursor}<a href={props.first}>← First page</a>{/if}
      {#if props.next}<a href={props.next}>Next →</a>{/if}
    </div>
  {/if}
{:else if paged && (paged.previous !== null || paged.next !== null || paged.range)}
  <div class="bar">
    {#if paged.previous !== null}<a href={props.at(paged.previous)}>← Previous</a>{/if}
    {#if paged.next !== null}<a href={props.at(paged.next)}>Next →</a>{/if}
    {#if paged.range}<span class="muted">{paged.range}</span>{/if}
  </div>
{/if}
