<script lang="ts">
  // A long line, such as a starved group's twenty clause sets, held to two lines with a toggle where it runs past them.
  let { text }: { text: string } = $props();
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

<span class="text" class:open bind:this={box}>{text}</span>
{#if long}<button type="button" class="again" aria-expanded={open} onclick={() => (open = !open)}>{
  open ? "Show less" : "Show all"}</button>{/if}

<style>
  .text { display: -webkit-box; overflow: hidden; -webkit-box-orient: vertical; -webkit-line-clamp: 2; line-clamp: 2; }
  .text.open { display: block; -webkit-line-clamp: none; line-clamp: none; }
</style>
