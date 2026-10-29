<script module lang="ts">
  import type { Snippet } from "svelte";

  /** A text of the dialog: fixed, or a getter whose answer follows the state of the fields. */
  export type Wording = string | (() => string);

  /** A write the dialog confirms. */
  export interface Asked {
    title: Wording;
    text: Wording;
    /** The button that runs it, and its label while it runs. */
    action: Wording;
    pending: Wording;
    /** What the reader types to unlock it: a danger-zone write's. */
    phrase?: string;
    /** The fields the write takes, above the phrase. */
    fields?: Snippet;
    /** Resolves `null` once done, else why not (`""` where a field already says). */
    run: () => Promise<string | null>;
    /** After a done write's dialog has closed, where its opener may be gone: say what changed. */
    done?: () => void;
  }
</script>

<script lang="ts">
  // The plan and campaign writes' one confirmation: a modal saying what a write
  // does, with the fields it takes and, for a danger-zone write, a phrase to type.
  // It stays open while the write runs, and keeps why it failed.
  import { tick } from "svelte";
  import { unlocks } from "../danger";

  const say = (wording: Wording) => (typeof wording === "function" ? wording() : wording);

  let { status = "" }: { /** How long the write has run, while it runs. */ status?: string } = $props();

  const uid = $props.id();
  let dialog = $state<HTMLDialogElement>();
  // Raw, so the fields snippet is not proxied.
  let asked = $state.raw<Asked | null>(null);
  let typed = $state("");
  let pending = $state(false);
  let error = $state("");
  const ready = $derived(!!asked && !pending && (!asked.phrase || unlocks(typed, asked.phrase)));

  export async function ask(question: Asked) {
    if (pending) return;
    asked = question;
    typed = "";
    error = "";
    pending = false;
    dialog?.showModal();
    await tick();
    // The first field, else the way out, so Enter never runs a write by accident.
    (dialog?.querySelector<HTMLElement>("input") ?? dialog?.querySelector<HTMLElement>("button.dismiss"))?.focus();
  }

  // `aria-disabled` keeps the focus but not the click from a script, so the lock is checked here too.
  async function submit(event: SubmitEvent) {
    event.preventDefault();
    if (!asked || !ready) return;
    pending = true;
    error = "";
    const question = asked;
    const failed = await question.run();
    pending = false;
    if (failed !== null) {
      error = failed;
      return;
    }
    dialog?.close();
    question.done?.();
  }

  // A backdrop click closes it; a press that began inside, or lands on its own padding, does not.
  let pressedOutside = false;
  const outside = (e: MouseEvent) => {
    const box = dialog?.getBoundingClientRect();
    return !!box && (e.clientX < box.left || e.clientX > box.right || e.clientY < box.top || e.clientY > box.bottom);
  };
</script>

<dialog bind:this={dialog} aria-labelledby="{uid}-title" aria-describedby="{uid}-text"
        oncancel={(e) => { if (pending) e.preventDefault(); }}
        onclose={() => {
          // A second Esc closes it whatever `cancel` says: a write under way keeps it open.
          if (pending) dialog?.showModal();
          else { asked = null; typed = ""; error = ""; }
        }}
        onpointerdown={(e) => { pressedOutside = e.target === dialog && outside(e); }}
        onclick={(e) => {
          if (pressedOutside && e.target === dialog && outside(e) && !pending) dialog?.close();
          pressedOutside = false;
        }}>
  {#if asked}
    <form novalidate onsubmit={submit}>
      <h2 id="{uid}-title">{say(asked.title)}</h2>
      <p id="{uid}-text">{say(asked.text)}</p>
      {@render asked.fields?.()}
      {#if asked.phrase}
        <label><span>Type <code class="phrase">{asked.phrase}</code> to confirm</span>
          <input type="text" bind:value={typed} autocomplete="off" spellcheck="false" /></label>
      {/if}
      {#if pending && status}<p class="muted" role="status">{status}</p>{/if}
      {#if error}<p class="err" role="alert">{error}</p>{/if}
      <div class="bar">
        <button type="submit" class:primary={!asked.phrase} class:danger={!!asked.phrase} aria-disabled={!ready}>{
          say(pending ? asked.pending : asked.action)}</button>
        <button type="button" class="dismiss" aria-disabled={pending}
                onclick={() => pending || dialog?.close()}>Never mind</button>
      </div>
    </form>
  {/if}
</dialog>

<style>
  /* Scrolls where it outgrows the window; an owner field's list scrolls with it. */
  dialog { width: 36em; max-width: calc(100vw - 32px); max-height: calc(100vh - 32px); box-sizing: border-box;
           overflow: auto;
           padding: 18px 20px 14px; border: 1px solid var(--border-strong); border-radius: 8px;
           box-shadow: 0 12px 32px var(--shadow); color: var(--text); background: var(--bg); }
  dialog::backdrop { background: var(--backdrop); }
  h2 { margin: 0 0 8px; padding: 0; border: 0; overflow-wrap: anywhere; }
  p { margin: 0 0 12px; }
  .bar { margin: 14px 0 0; }
  /* Boxed, so a phrase with spaces shows where it starts and ends. */
  .phrase { padding: 0 4px; border: 1px solid var(--border-strong); border-radius: 4px; background: var(--surface-2);
            color: var(--text); white-space: pre-wrap; }
</style>
