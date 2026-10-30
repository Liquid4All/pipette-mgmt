<script lang="ts">
  // A descriptor under its catalog name (`descriptor_names`), the member key quieter. A click shows the descriptor
  // in a balloon by the name, over the cells near it, with a tail that points at the name; a click outside or Esc
  // closes it. The tooltip lists the other names. A descriptor with no name shows as it is.
  import type { Snippet } from "svelte";
  import type { DescriptorDisplay } from "../api-types";
  import { type Tail, tailOf } from "../balloon";
  import { memberKeyOf, synonymsTitle } from "../descriptor-names";

  let { name, children }: {
    /** The descriptor's catalog names, or null for a descriptor with none. */
    name: DescriptorDisplay | null;
    /** The descriptor, as drawn without a name. */
    children: Snippet;
  } = $props();

  const uid = $props.id();
  let open = $state(false);
  let button = $state<HTMLButtonElement>();
  let balloon = $state<HTMLDivElement>();
  let tail = $state<Tail | null>(null);

  // The browser places the balloon, and flips it above the name where the viewport ends: the tail follows.
  function aim() {
    if (button && balloon) tail = tailOf(button.getBoundingClientRect(), balloon.getBoundingClientRect());
  }

  $effect(() => {
    if (!open) return;
    aim();
    const options = { capture: true, passive: true };
    addEventListener("scroll", aim, options);
    addEventListener("resize", aim, options);
    return () => {
      removeEventListener("scroll", aim, options);
      removeEventListener("resize", aim, options);
    };
  });
</script>

{#if name}
  <button bind:this={button} type="button" class="again catalog-name" aria-expanded={open}
          title={synonymsTitle(name) || undefined} popovertarget="{uid}-descriptor"
          style:anchor-name="--{uid}">{name.name}<span class="member">{memberKeyOf(name)}</span></button>
  <!-- The descriptor renders only while the balloon is open, so a closed row holds the name alone. -->
  <div bind:this={balloon} id="{uid}-descriptor" popover="auto" class="balloon" style:position-anchor="--{uid}"
       data-tail={tail?.edge} style:--tail-x={tail ? `${tail.x}px` : undefined}
       ontoggle={(e) => (open = e.newState === "open")}>
    <div class="scroll">{#if open}{@render children()}{/if}</div></div>
{:else}{@render children()}{/if}

<style>
  .catalog-name { font-family: var(--mono); font-size: 12px; text-align: left; white-space: nowrap; }
  .member { color: var(--text-muted); }
  /* Under the name, flipped where the viewport ends. A browser without anchor positioning centers it.
     The balloon shows its tail outside its edge, so its contents scroll inside it. */
  .balloon { position-area: block-end span-inline-end; position-try-fallbacks: flip-block, flip-inline;
             margin: 8px 0 0; padding: 0; max-width: min(60em, calc(100vw - 32px)); overflow: visible;
             box-sizing: border-box; color: var(--text); background: var(--bg);
             border: 1px solid var(--border-strong); border-radius: var(--radius); box-shadow: 0 2px 8px #0000001f;
             font-size: 12px; white-space: normal; text-align: left; }
  .scroll { max-height: 50vh; overflow: auto; padding: 8px 10px; }
  /* A square on its corner, half outside the edge, with the balloon's border on its two outer sides. */
  .balloon[data-tail]::before { content: ""; position: absolute; left: calc(var(--tail-x) - 5px); width: 10px;
                                height: 10px; box-sizing: border-box; background: var(--bg);
                                border: solid var(--border-strong); transform: rotate(45deg); }
  .balloon[data-tail="top"]::before { top: -6px; border-width: 1px 0 0 1px; }
  .balloon[data-tail="bottom"]::before { bottom: -6px; border-width: 0 1px 1px 0; }
</style>
