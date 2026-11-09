<script lang="ts">
  // A long line, such as a starved group's twenty clause sets, held to two lines with a toggle where it runs past them.
  // A `note` keeps its newlines and stops at four lines, with the full text in a balloon.
  import Balloon from "./Balloon.svelte";

  let { text, note = false }: { text: string; note?: boolean } = $props();
  let open = $state(false);
  let long = $state(false);
  let box = $state<HTMLElement>();
  // Bumped by a resize, so a width that refolds the line measures it again.
  let resized = $state(0);

  $effect(() => {
    if (!box) return;
    const observer = new ResizeObserver(() => resized++);
    observer.observe(box);
    return () => observer.disconnect();
  });
  // Measured folded only, after the DOM has the text: an open line always fits its box.
  $effect(() => {
    void [text, resized];
    if (box && !open) long = box.scrollHeight > box.clientHeight + 1;
  });
</script>

<span class="text" class:open class:note bind:this={box}>{text}</span>
{#if long && note}
  <Balloon>
    {#snippet face()}Show all{/snippet}
    <div class="full">{text}</div>
  </Balloon>
{:else if long}<button type="button" class="again" aria-expanded={open} onclick={() => (open = !open)}>{
  open ? "Show less" : "Show all"}</button>{/if}

<style>
  .text { display: -webkit-box; overflow: hidden; -webkit-box-orient: vertical; -webkit-line-clamp: 2; line-clamp: 2; }
  .text.open { display: block; -webkit-line-clamp: none; line-clamp: none; }
  .text.note { width: max-content; max-width: 36em; -webkit-line-clamp: 4; line-clamp: 4; white-space: pre-wrap; overflow-wrap: break-word; }
  :global(table.sized) .text.note { width: auto; max-width: none; }
  .full { white-space: pre-wrap; overflow-wrap: anywhere; }
</style>
