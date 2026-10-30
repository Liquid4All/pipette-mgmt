<script lang="ts">
  // A command to run in a terminal: shown whole, selected whole by one click,
  // with a button that copies it.
  let { command, copyLabel = "Copy" }: { command: string; copyLabel?: string } = $props();
  let copied = $state(false);

  async function copy() {
    await navigator.clipboard.writeText(command);
    copied = true;
    setTimeout(() => (copied = false), 1500);
  }
</script>

<pre><code>{command}</code></pre>
<div class="bar">
  <button type="button" onclick={copy}>{copied ? "Copied" : copyLabel}</button>
</div>

<style>
  /* Read before it is copied, so it wraps rather than scrolls. */
  pre { white-space: pre-wrap; overflow-wrap: anywhere; user-select: all; }
</style>
