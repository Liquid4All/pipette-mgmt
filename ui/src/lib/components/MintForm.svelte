<script lang="ts">
  // A pre-auth key's mint form: its expiry, its note and, for a device's, whether it admits many.
  import { EXPIRY_DAYS, EXPIRY_DEFAULT, days } from "../preauth";

  let { action, noteLabel, placeholder, multi = false, busy, onmint }: {
    action: string;
    noteLabel: string;
    placeholder: string;
    /** Offer a key that registers devices until it expires. */
    multi?: boolean;
    busy: boolean;
    /** Resolves whether the mint took, which clears the form. */
    onmint: (form: { days: number; note: string; multiUse: boolean }) => Promise<boolean>;
  } = $props();

  let expiresInDays = $state(EXPIRY_DEFAULT);
  let note = $state("");
  let multiUse = $state(false);

  async function submit(event: SubmitEvent) {
    event.preventDefault();
    if (busy || !(await onmint({ days: expiresInDays, note, multiUse }))) return;
    [expiresInDays, note, multiUse] = [EXPIRY_DEFAULT, "", false];
  }
</script>

<form onsubmit={submit}>
  <div class="bar">
    <label><span>expires in</span>
      <select bind:value={expiresInDays}>
        {#each EXPIRY_DAYS as n (n)}<option value={n}>{days(n)}</option>{/each}
      </select>
    </label>
    {#if multi}
      <label class="check"><input type="checkbox" bind:checked={multiUse}>
        Multi-use: one key for a rack, until it expires or is revoked</label>
    {/if}
  </div>
  <label><span>{noteLabel}</span>
    <input type="text" bind:value={note} autocomplete="off" {placeholder}>
  </label>
  <button type="submit" aria-disabled={busy}>{action}</button>
</form>
