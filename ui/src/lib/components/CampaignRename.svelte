<script lang="ts">
  // A campaign's or a group's name, a General row of its Settings: checked as typed where the route would refuse it.
  import { NAME_MAX_CHARS, nameProblem } from "../campaigns";
  import SettingRow from "./SettingRow.svelte";
  import { Busy } from "./work.svelte";

  let { title, name: current, save }: {
    title: string;
    name: string;
    /** Write the new name, then read the record again. */
    save: (to: string) => Promise<void>;
  } = $props();

  const uid = $props.id();
  // Taken again from each read, as a write reads the record again.
  let name = $derived(current);
  const changed = $derived(name.trim() !== current);
  const problem = $derived(nameProblem(name));
  const renaming = new Busy();
  /** What the last rename did, until the name is typed again. */
  let done = $state("");

  async function rename(event: SubmitEvent) {
    event.preventDefault();
    if (renaming.busy || problem || !changed) return;
    const to = name.trim();
    await renaming.attempt(async () => {
      await save(to);
      done = `Renamed to “${to}”.`;
    });
  }
</script>

<SettingRow {title}
            text={`Shown in listings and atop this page; up to ${NAME_MAX_CHARS.toLocaleString("en")} characters.`}>
  {#snippet children(ids)}
    <form novalidate onsubmit={rename}>
      <div class="bar">
        <input type="text" value={name} autocomplete="off" aria-labelledby={ids.title}
               aria-describedby="{ids.text} {uid}-note" aria-invalid={!!problem || undefined}
               oninput={(e) => { name = e.currentTarget.value; done = ""; }}>
        <button type="submit" class="primary" aria-disabled={renaming.busy || !changed || !!problem}>{
          renaming.busy ? "Saving…" : "Save changes"}</button>
        {#if changed}<button type="button" onclick={() => (name = current)}>Reset</button>{/if}
      </div>
      <div id="{uid}-note" class="hint" class:err={!!problem} class:ok={!problem && !!done} role="status"
           hidden={!problem && !done}>{problem || done}</div>
      {#if renaming.error}<p class="err" role="alert">{renaming.error}</p>{/if}
    </form>
  {/snippet}
</SettingRow>
