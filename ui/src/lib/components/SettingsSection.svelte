<script lang="ts">
  // A Settings section: its heading over a box of `SettingRow`s, the danger zone's in red.
  import type { Snippet } from "svelte";

  let { title, danger = false, children }: { title: string; danger?: boolean; children: Snippet } = $props();
  const uid = $props.id();
</script>

<section aria-labelledby="{uid}-title">
  <h2 id="{uid}-title">{title}</h2>
  <div class="settings-box" class:danger>{@render children()}</div>
</section>
