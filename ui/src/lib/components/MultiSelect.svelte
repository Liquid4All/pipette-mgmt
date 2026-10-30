<script lang="ts">
  // A pick of several choices: a box naming what is chosen, opening on a
  // checkbox per option with how many have it, under a search box that keeps
  // the options holding its text. Escape or a click elsewhere closes it.
  import type { Attachment } from "svelte/attachments";
  import { choiceMatches, type Choice } from "../table";

  let { label, options, chosen, toggle, none = "any", searchable = false }: {
    label: string;
    options: Choice[];
    chosen: string[];
    toggle: (value: string, on: boolean) => void;
    /** The box's text with nothing chosen. */
    none?: string;
    /** The search box shows whatever the options' count, as long ones such as descriptors need it. */
    searchable?: boolean;
  } = $props();

  let open = $state(false);
  let menu = $state<HTMLElement>();
  // A menu that would run past the window's right edge opens leftward from the box's right edge instead.
  let leftward = $state(false);
  $effect(() => {
    if (!open || !menu) return;
    leftward = false;
    const box = menu.getBoundingClientRect();
    leftward = box.right > document.documentElement.clientWidth;
  });
  let search = $state("");
  const listed = $derived(options.filter((o) => choiceMatches(o, search)));
  const named = $derived(chosen.length === 0 ? none
    : chosen.length === 1 ? (options.find((o) => o.value === chosen[0])?.label ?? chosen[0]) : `${chosen.length} chosen`);

  const closing: Attachment<HTMLElement> = (node) => {
    const away = (e: Event) => { if (!node.contains(e.target as Node)) open = false; };
    const escape = (e: KeyboardEvent) => {
      if (e.key !== "Escape" || !open) return;
      // Handled, so a dialog around the pick stays open.
      e.preventDefault();
      open = false;
      node.querySelector("summary")?.focus();
    };
    document.addEventListener("pointerdown", away);
    document.addEventListener("keydown", escape);
    return () => {
      document.removeEventListener("pointerdown", away);
      document.removeEventListener("keydown", escape);
    };
  };
</script>

<div class="field">
  <span>{label}</span>
  <details bind:open {@attach closing}>
    <summary class:set={chosen.length}>{named}</summary>
    <div class="menu" class:leftward bind:this={menu} role="group" aria-label={label}>
      {#if searchable || options.length > 6}
        <input type="text" class="search" bind:value={search} placeholder="search" autocomplete="off"
               spellcheck="false" aria-label="search {label}" />
      {/if}
      {#each listed as option (option.value)}
        <label class="check"><input type="checkbox" checked={chosen.includes(option.value)}
                                    onchange={(e) => toggle(option.value, e.currentTarget.checked)} />
          <span class="name">{option.label}</span>{#if option.count !== undefined}<span
            class="count">{option.count}</span>{/if}</label>
      {:else}
        <p class="muted none">{search ? `Nothing holds “${search}”.` : "Nothing to choose."}</p>
      {/each}
    </div>
  </details>
</div>

<style>
  .field > span { display: block; color: var(--text-muted); font-size: 12px; margin-bottom: 3px; }
  details { position: relative; margin: 0; }
  summary { box-sizing: border-box; width: 11em; padding: 5px 24px 5px 8px; overflow: hidden; text-overflow: ellipsis;
            white-space: nowrap; list-style: none; cursor: pointer; border: 1px solid var(--border);
            border-radius: var(--radius); background: var(--bg); }
  summary::-webkit-details-marker { display: none; }
  summary::after { content: "▾"; position: absolute; right: 8px; color: var(--text-muted); }
  summary.set { border-color: var(--accent); }
  .menu { position: absolute; z-index: 20; top: calc(100% + 4px); left: 0; min-width: 100%; max-height: 22em;
          width: max-content; max-width: min(48em, calc(100vw - 32px)); overflow-y: auto; padding: 4px 0;
          background: var(--bg); border: 1px solid var(--border-strong);
          border-radius: var(--radius); box-shadow: 0 8px 24px #0000001f; }
  label.check { display: flex; gap: 8px; align-items: baseline; margin: 0; padding: 4px 10px; white-space: nowrap;
                cursor: pointer; }
  label.check:hover { background: var(--hover); }
  .menu.leftward { left: auto; right: 0; }
  /* A long choice, such as a descriptor's JSON, wraps rather than widening the menu off the window. */
  .name { flex: 1; white-space: normal; overflow-wrap: anywhere; }
  .search { display: block; box-sizing: border-box; width: calc(100% - 16px); margin: 4px 8px 6px; }
  .none { margin: 4px 10px; }
  .count { color: var(--text-muted); font-variant-numeric: tabular-nums; }
</style>
