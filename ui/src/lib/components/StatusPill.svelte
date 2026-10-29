<script lang="ts">
  // A record's status, tinted by `app.css`'s `[data-state]` rules.
  let { status }: { status: string } = $props();
</script>

<span data-state={status}>{status}</span>
