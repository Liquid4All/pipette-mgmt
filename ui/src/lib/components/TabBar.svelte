<script lang="ts">
  // A bar of a plan's tabs: plain links, the open one marked. A sub-bar sits
  // under the tab it belongs to, smaller and lighter.
  import type { BarTab } from "../bar";

  let { label, items, sub = false }: {
    label: string;
    items: BarTab[];
    sub?: boolean;
  } = $props();
</script>

<nav class="tabs" class:sub aria-label={label}>
  <ul>
    {#each items as item (item.key)}
      <li class:start={item.start}>
        <a href={item.href} title={item.title} aria-current={item.current ? "page" : undefined}>{item.label}</a>
      </li>
    {/each}
  </ul>
</nav>

<style>
  .tabs { margin: 14px 0 16px; border-bottom: 1px solid var(--border); }
  /* Wraps rather than truncates, so a plan of many types keeps every tab in reach. */
  ul { display: flex; flex-wrap: wrap; margin: 0; padding: 0; list-style: none; }
  li { display: flex; }
  li.start:not(:first-child)::before { content: ""; align-self: center; height: 14px; margin: 0 6px;
                                       border-left: 1px solid var(--border-strong); }
  a { padding: 7px 12px 6px; margin-bottom: -1px; border-bottom: 2px solid transparent; color: var(--text-muted);
      font-weight: 500; white-space: nowrap; }
  a:hover { color: var(--text); text-decoration: none; }
  a[aria-current="page"] { color: var(--accent); border-bottom-color: var(--accent); }
  .sub { margin: -6px 0 14px; border-bottom: 0; }
  .sub ul { row-gap: 4px; }
  /* A label longer than the bar, such as a device type, breaks inside its tab. */
  .sub a { padding: 2px 8px; margin: 0; border: 0; border-radius: var(--radius); font-size: 12px; font-weight: 400;
           white-space: normal; }
  .sub a[aria-current="page"] { background: var(--accent-soft); }
</style>
