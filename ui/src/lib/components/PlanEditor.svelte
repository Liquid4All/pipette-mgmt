<script lang="ts">
  // The plan document editor of the new-campaign and plan-document pages, checked
  // as typed (`POST /api/v1/plans/validate`).
  import { tick } from "svelte";
  import { validatePlan } from "../api";
  import { editorRows, firstError, isDirty, lineRange, readTextFile, sameDocument, validationNote } from "../document";
  import type { PlanExample } from "../plan-examples";
  import { session } from "../session.svelte";
  import type { Asked } from "./ConfirmDialog.svelte";
  import { describe, latest } from "./guard";
  import { DocumentValidator } from "./work.svelte";

  let { text = $bindable(), confirm, verb, original, examples, required = false, locked = false }: {
    text: string;
    /** The page's confirmation, outside its form: a dialog's form inside another would submit both. */
    confirm: (asked: Asked) => void;
    /** What the page does with the document, which is when gated access is checked. */
    verb: string;
    /** The document an edit started from, which Reset goes back to. */
    original?: string;
    examples?: readonly PlanExample[];
    /** A blank document was refused, so its status reads as an error. */
    required?: boolean;
    /** A write is checking or filing the text as it stood, which no edit may overtake. */
    locked?: boolean;
  } = $props();

  const uid = $props.id();
  let area = $state<HTMLTextAreaElement>();
  let picker = $state<HTMLInputElement>();
  /** What a file or an example last put in, which replacing loses nothing of. */
  let given = $state<string | null>(null);
  let fileError = $state("");
  const validator = new DocumentValidator((toml, signal) => validatePlan(session.signer, toml, signal));

  const note = $derived(validationNote({ text, original, verdict: validator.verdict, error: validator.error, required }));
  const issues = $derived(validator.verdict?.issues ?? []);
  const gated = $derived(validator.verdict?.gated_model_count ?? 0);
  const edited = $derived(original !== undefined && !sameDocument(text, original));

  $effect(() => {
    if (original !== undefined && sameDocument(text, original)) validator.clear();
    else validator.schedule(text);
  });
  $effect(() => () => validator.stop());
  // The page read the document again: what a file put in before is gone.
  $effect(() => {
    void original;
    given = null;
  });

  export function focus() {
    area?.focus();
  }

  /** Select 1-based `line` and centre it; a selection is not scrolled into view reliably. */
  export function goToLine(line: number) {
    if (!area) return;
    // The field's own value: it holds LF where `text` may still hold a document's CRLF.
    const [start, end] = lineRange(area.value, line);
    area.focus();
    area.setSelectionRange(start, end);
    // `line-height: normal` reads as NaN, which browsers draw near 1.2em.
    const style = getComputedStyle(area);
    const set = parseFloat(style.lineHeight);
    const lineHeight = Number.isFinite(set) ? set : parseFloat(style.fontSize) * 1.2;
    area.scrollTop = (line - 1) * lineHeight - area.clientHeight / 2;
    area.scrollLeft = 0;
  }

  /**
   * Check the text before a write and go to the first error; whether the write
   * may go on. A check that could not be made does not stop it: the route checks again.
   */
  export async function check(): Promise<boolean> {
    const fault = firstError(await validator.now(text));
    if (!fault) return true;
    if (fault.line === null) focus();
    else goToLine(fault.line);
    return false;
  }

  async function put(next: string, from: string | null, then?: () => void) {
    // As the field would make it, so what is checked and filed is what it shows.
    text = next.replace(/\r\n?/g, "\n");
    given = from;
    fileError = "";
    then?.();
    if (original === undefined || !sameDocument(text, original)) void validator.now(text);
    await tick();
    area?.focus();
    area?.setSelectionRange(0, 0);
    if (area) area.scrollTop = 0;
  }

  /**
   * Put `next` in, asking first where that loses an edit: setting the text wipes
   * the textarea's undo. `from` is what replacing it again loses nothing of.
   */
  export function replace(next: string, what: string, from: string | null = next, then?: () => void) {
    if (locked) return;
    if (!isDirty(text, given ?? original ?? "")) {
      void put(next, from, then);
      return;
    }
    confirm({
      title: "Replace the document?",
      text: `The editor's text is replaced by ${what}; your edits are lost.`,
      action: "Replace",
      pending: "Replacing…",
      run: async () => null,
      // After the dialog closes, which hands the focus back to the button that opened it.
      done: () => void put(next, from, then),
    });
  }

  // The file picked last wins, however long an earlier one takes to read.
  const picking = latest();
  async function open(event: Event & { currentTarget: HTMLInputElement }) {
    const input = event.currentTarget;
    const file = input.files?.[0];
    // So picking the same file again reads it again.
    input.value = "";
    if (!file) return;
    const mine = picking();
    try {
      const read = await readTextFile(file);
      if (mine()) replace(read, file.name);
    } catch (e) {
      if (mine()) fileError = describe(e);
    }
  }

  function reset() {
    if (original !== undefined) replace(original, "the document the campaign runs", null);
  }
</script>

<div class="bar editor-bar">
  <label class="caption" for="{uid}-text">plan document</label>
  {#if edited}<span class="dirty">Edited</span><button type="button" aria-disabled={locked} onclick={reset}>Reset</button>{/if}
  <button type="button" aria-disabled={locked} onclick={() => locked || picker?.click()}>Open file…</button>
  <input type="file" hidden accept=".toml,application/toml,text/plain" bind:this={picker} onchange={open}>
</div>
{#if examples?.length}
  <div class="bar examples">
    <span class="muted">Start from an example:</span>
    {#each examples as example (example.id)}
      <button type="button" aria-disabled={locked} onclick={() => replace(example.text, example.title)}>{
        example.title}</button>
    {/each}
  </div>
{/if}
<textarea id="{uid}-text" class="code" rows={editorRows(text)} spellcheck="false" autocapitalize="off"
          autocomplete="off" aria-describedby="{uid}-status {uid}-issues" readonly={locked}
          aria-invalid={note.tone === "err" || undefined} bind:value={text} bind:this={area}
          oninput={() => (fileError = "")}></textarea>
<div id="{uid}-status" class="hint" class:ok={note.tone === "ok"} class:err={note.tone === "err"} role="status"
     aria-busy={validator.pending}>{note.text}</div>
{#if fileError}<div class="hint err" role="alert">{fileError}</div>{/if}
{#if gated && validator.verdict}
  <div class="hint">{gated} gated {gated === 1 ? "model" : "models"}: Hugging Face access is checked when you {verb}.</div>
{/if}
{#if issues.length}
  <ul id="{uid}-issues" class="issues" class:stale={!validator.isFor(text)}>
    {#each issues as issue, i (i)}
      <li class:warning={issue.severity === "warning"}>
        <span class="tag" class:error={issue.severity === "error"}
              class:warning={issue.severity === "warning"}>{issue.severity}</span>
        {#if issue.line !== null}
          {@const line = issue.line}
          <button type="button" class="again" onclick={() => goToLine(line)}>Line {line}{
            issue.column === null ? "" : `, col ${issue.column}`}</button>
        {/if}
        <code class="msg">{issue.message}</code>
      </li>
    {/each}
  </ul>
{/if}

<style>
  .editor-bar { align-items: baseline; margin: 10px 0 4px; }
  .editor-bar label { margin: 0 auto 0 0; }
  .examples { align-items: baseline; margin: 0 0 6px; font-size: 12px; }
  .examples button { padding: 2px 10px; }
  .issues { margin: 8px 0 0; padding: 0; list-style: none; }
  .issues.stale { opacity: .6; }
  .issues li { display: flex; flex-wrap: wrap; align-items: baseline; gap: 4px 10px; margin: 4px 0;
               padding: 4px 0 4px 10px; border-left: 3px solid var(--red-border); }
  .issues li.warning { border-left-color: var(--amber-border); }
  .issues .again { font-size: 12px; }
  .msg { flex-basis: 100%; white-space: pre-wrap; overflow-wrap: anywhere; }
</style>
