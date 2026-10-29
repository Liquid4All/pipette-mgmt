<script lang="ts">
  // A danger zone's row for one destructive write: what it does, its button,
  // and the dialog that runs it once its phrase is typed.
  import type { Snippet } from "svelte";
  import ConfirmDialog from "./ConfirmDialog.svelte";
  import SettingRow from "./SettingRow.svelte";
  import { Busy } from "./work.svelte";

  let { title, text, ask = `${title}?`, pending, phrase, fields, run, done, locked = false }: {
    /** The row's heading and its button, such as "Delete this device". */
    title: string;
    text: string;
    /** The dialog's question. */
    ask?: string;
    /** The button's label while the write runs. */
    pending: string;
    phrase: string;
    /** The fields the write takes, drawn in the dialog above the phrase. */
    fields?: Snippet;
    /** The write; a failure is drawn in the dialog, which stays open. */
    run: () => Promise<void>;
    /** After the dialog has closed on a write that took, such as the step to the listing. */
    done?: () => void;
    /** Another write of the page is under way. */
    locked?: boolean;
  } = $props();

  const work = new Busy();
  let confirm = $state<ReturnType<typeof ConfirmDialog>>();

  const open = () => work.busy || locked || void confirm?.ask({
    title: ask, text, action: title, pending, phrase, fields, done,
    run: async () => {
      await work.attempt(run);
      return work.error || null;
    },
  });
</script>

<SettingRow {title} {text}>
  {#snippet control(ids)}
    <button type="button" class="danger" aria-haspopup="dialog" aria-describedby={ids.text}
            aria-disabled={work.busy || locked} onclick={open}>{work.busy ? pending : title}</button>
  {/snippet}
</SettingRow>
<ConfirmDialog bind:this={confirm} />
