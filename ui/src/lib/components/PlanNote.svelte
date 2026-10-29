<script lang="ts">
  // The Note row in the Settings of a plan. The note is free text from the filer that says why the plan was
  // filed. The component saves the whole note. A blank note clears it.
  import { updatePlan } from "../api";
  import { NOTE_MAX_CHARS, noteProblem } from "../campaigns";
  import { session } from "../session.svelte";
  import type { NamedPlanView } from "../api-types";
  import SettingRow from "./SettingRow.svelte";
  import { Busy } from "./work.svelte";

  let { plan, onchange }: {
    plan: NamedPlanView;
    /** Read the plan again, after a write. */
    onchange: () => Promise<void>;
  } = $props();

  const uid = $props.id();
  // Taken again from each read, as a write reads the plan again.
  let note = $derived(plan.note ?? "");
  const changed = $derived(note.trim() !== (plan.note ?? ""));
  const problem = $derived(noteProblem(note));
  const saving = new Busy();
  /** What the last save did, until the note is typed again. */
  let done = $state("");

  async function save(event: SubmitEvent) {
    event.preventDefault();
    if (saving.busy || problem || !changed) return;
    const to = note.trim();
    await saving.attempt(async () => {
      await updatePlan(session.signer, plan.plan_id, { note: to });
      await onchange();
      done = to ? "Note saved." : "Note cleared.";
    });
  }
</script>

<SettingRow title="Note"
            text={`Why this plan was filed, shown on its Overview and in listings; up to ${
              NOTE_MAX_CHARS.toLocaleString("en")} characters.`}>
  {#snippet children(ids)}
    <form novalidate onsubmit={save}>
      <textarea rows="3" value={note} aria-labelledby={ids.title} aria-describedby="{ids.text} {uid}-note"
                aria-invalid={!!problem || undefined}
                oninput={(e) => { note = e.currentTarget.value; done = ""; }}></textarea>
      <div class="bar">
        <button type="submit" class="primary" aria-disabled={saving.busy || !changed || !!problem}>{
          saving.busy ? "Saving…" : "Save note"}</button>
        {#if changed}<button type="button" onclick={() => (note = plan.note ?? "")}>Reset</button>{/if}
      </div>
      <div id="{uid}-note" class="hint" class:err={!!problem} class:ok={!problem && !!done} role="status"
           hidden={!problem && !done}>{problem || done}</div>
      {#if saving.error}<p class="err" role="alert">{saving.error}</p>{/if}
    </form>
  {/snippet}
</SettingRow>
