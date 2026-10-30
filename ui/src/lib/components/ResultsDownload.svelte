<script lang="ts">
  // Every result row of a plan as one CSV file, as the warehouse holds it whatever
  // the Jobs filters.
  import { exportResults } from "../api";
  import { resultsFilename } from "../plans";
  import { session } from "../session.svelte";
  import { Busy } from "./work.svelte";

  let { planId }: { planId: string } = $props();

  const download = new Busy();
  // Signed like every call, so the file comes as text and is saved from memory.
  const save = () => download.attempt(async () => {
    const { csv, disposition } = await exportResults(session.signer, planId);
    const link = document.createElement("a");
    link.href = URL.createObjectURL(new Blob([csv], { type: "text/csv" }));
    link.download = resultsFilename(planId, disposition);
    link.click();
    setTimeout(() => URL.revokeObjectURL(link.href));
  });
</script>

<button type="button" class="primary" aria-disabled={download.busy} onclick={save}>{download.busy ? "Downloading…" : "Download CSV"}</button>
{#if download.error}<span class="err" role="alert">{download.error}</span>{/if}
