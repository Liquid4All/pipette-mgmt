<script module lang="ts">
  /** The ids of a row's title and description, which its control is named and described by. */
  export interface RowIds {
    title: string;
    text: string;
  }
</script>

<script lang="ts">
  // One write of a Settings section: what it does and what it last did, then
  // its control, or the form it takes, under them.
  import type { Snippet } from "svelte";

  let { title, text, status = "", control, children }: {
    title: string;
    text: string;
    /** What the last write did. */
    status?: string;
    control?: Snippet<[RowIds]>;
    children?: Snippet<[RowIds]>;
  } = $props();

  const uid = $props.id();
  const ids: RowIds = { title: `${uid}-title`, text: `${uid}-text` };
  let said = $state<HTMLElement>();

  /** Move the focus to what the row's last write did. */
  export function focusStatus() {
    said?.focus();
  }
</script>

<div class="setting">
  <div class="what">
    <h3 id={ids.title}>{title}</h3>
    <p class="muted" id={ids.text}>{text}</p>
    <!-- Not a `p`, which would take `p.ok`'s banner; focusable, for a write that removed its own control. -->
    <div class="hint ok" role="status" tabindex="-1" hidden={!status} bind:this={said}>{status}</div>
  </div>
  {#if control}<div class="control">{@render control(ids)}</div>{/if}
  {#if children}<div class="body">{@render children(ids)}</div>{/if}
</div>
