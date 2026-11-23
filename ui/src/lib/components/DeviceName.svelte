<script lang="ts">
  import SettingRow from "./SettingRow.svelte";

  let { name: current, fallback, busy, save }: {
    name: string | null;
    fallback: string;
    busy: boolean;
    /** Call `saved` after the write succeeds. An empty value clears the name. */
    save: (to: string, saved: () => void) => Promise<void>;
  } = $props();

  const uid = $props.id();
  const drawn = $derived(current ?? fallback);
  let name = $derived(drawn);
  const changed = $derived(name.trim() !== drawn);
  const canSave = $derived(current === null ? name.trim() !== "" : changed);
  /** What the last write did, until the name is typed again. */
  let done = $state("");

  function write(to: string) {
    if (busy) return;
    done = "";
    return save(to, () => { done = to ? `Named “${to}”.` : "Name cleared."; });
  }

  async function rename(event: SubmitEvent) {
    event.preventDefault();
    if (!canSave) return;
    await write(name.trim());
  }
</script>

<SettingRow title="Name"
            text="Shown in device listings and atop this page. The device cannot change it. If the device has no name, the console uses its client details, model, or ID.">
  {#snippet children(ids)}
    <form novalidate onsubmit={rename}>
      <div class="bar">
        <input type="text" value={name} disabled={busy} autocomplete="off" aria-labelledby={ids.title}
               aria-describedby="{ids.text} {uid}-note"
               oninput={(e) => { name = e.currentTarget.value; done = ""; }}>
        <button type="submit" class="primary" disabled={busy || !canSave}>Save name</button>
        {#if changed}<button type="button" disabled={busy} onclick={() => (name = drawn)}>Reset</button>{/if}
        {#if current}<button type="button" disabled={busy} onclick={() => write("")}>Clear name</button>{/if}
      </div>
      <div id="{uid}-note" class="hint ok" role="status" hidden={!done}>{done}</div>
    </form>
  {/snippet}
</SettingRow>
