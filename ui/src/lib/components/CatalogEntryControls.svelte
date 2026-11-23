<script lang="ts">
  import {
    changeOrgDescriptorEntryState, relabelOrgDescriptorMember, renameOrgDescriptorEntry, setOrgDescriptorEntryFamily,
    setOrgDescriptorFamilyLogo,
  } from "../api";
  import type { DescriptorEntryAction, DescriptorEntryListRow, ModelFormat } from "../api-types";
  import { catalogEntryPath, catalogPath } from "../catalog";
  import {
    LOGO_URI_PREFIX, familyError, logoUriError, relabelError, renameError, stateActions, writeFailure,
  } from "../catalog-entry";
  import { redirect } from "../route.svelte";
  import { session } from "../session.svelte";
  import ConfirmDialog from "./ConfirmDialog.svelte";

  let { entry, onchanged }: {
    entry: DescriptorEntryListRow;
    /** Read the entry again, after a write that keeps its name. */
    onchanged: () => Promise<void>;
  } = $props();

  let confirm = $state<ReturnType<typeof ConfirmDialog>>();
  let busy = $state(false);
  let error = $state("");
  let typed = $state("");
  let tried = $state(false);
  let member = $state<{ format: ModelFormat; quantization: string } | null>(null);
  const nameFault = $derived(renameError(typed, entry.name));
  const labelFault = $derived(member ? relabelError(typed, member.quantization) : null);
  const familyFault = $derived(familyError(typed));
  const logoFault = $derived(logoUriError(typed));
  const listPath = $derived(entry.descriptor_kind === "model" ? `${catalogPath}?kind=model` : catalogPath);

  async function change(action: Exclude<DescriptorEntryAction, "tombstone">) {
    if (busy) return;
    busy = true;
    error = "";
    try {
      await changeOrgDescriptorEntryState(session.signer, entry.scope, entry.name, { action });
    } catch (e) {
      error = writeFailure(e, "state");
      busy = false;
      return;
    }
    try {
      await onchanged();
    } catch {
      error = "The entry changed, but the page could not refresh. Reload the page.";
    } finally {
      busy = false;
    }
  }

  function rename() {
    if (busy) return;
    typed = entry.name;
    tried = false;
    let renamed = "";
    void confirm?.ask({
      title: "Rename this entry?",
      text: `The entry keeps its members. Another entry can use the old name ${entry.display_name} immediately. `
        + "If another entry of the organization has the new name, the server merges this entry into it. That entry "
        + "gets the members of this entry, and the server deletes this entry. Nobody can undo a merge.",
      action: "Rename",
      pending: "Wait for the server to rename the entry…",
      fields: nameField,
      run: async () => {
        tried = true;
        if (nameFault) return "";
        try {
          renamed = (await renameOrgDescriptorEntry(session.signer, entry.scope, entry.name, { name: typed }))
            .display_name;
        } catch (e) {
          return writeFailure(e, "name");
        }
        return null;
      },
      // The address holds the name, so the old address no longer finds the entry.
      done: () => redirect(catalogEntryPath(renamed)),
    });
  }

  function tombstone() {
    if (busy) return;
    void confirm?.ask({
      title: "Tombstone this entry?",
      text: `A tombstone is permanent. Plans cannot select ${entry.display_name} again, and no new entry of the `
        + "organization can take the name. Tombstone an entry with a wrong descriptor. To correct a typo in the name, "
        + "rename the entry.",
      action: "Tombstone",
      pending: "Wait for the server to tombstone the entry…",
      phrase: entry.name,
      run: async () => {
        try {
          await changeOrgDescriptorEntryState(session.signer, entry.scope, entry.name,
            { action: "tombstone", confirmation: entry.name });
        } catch (e) {
          return writeFailure(e, "state");
        }
        return null;
      },
      // The catalog pages do not show a tombstoned entry.
      done: () => redirect(listPath),
    });
  }

  function setFamily() {
    if (busy) return;
    typed = entry.family ?? "";
    tried = false;
    void confirm?.ask({
      title: "Change the family?",
      text: "The model pickers of the apps group entries by family. Leave the field empty to use the default family: "
        + "the text of the name before its first hyphen.",
      action: "Change family",
      pending: "Wait for the server to change the family…",
      fields: familyField,
      run: async () => {
        if (typed === (entry.family ?? "")) return null;
        tried = true;
        if (familyFault) return "";
        if (busy) return "An entry change is already in progress.";
        busy = true;
        try {
          await setOrgDescriptorEntryFamily(session.signer, entry.scope, entry.name, { family: typed || null });
          await onchanged();
        } catch (e) {
          return writeFailure(e, "family");
        } finally {
          busy = false;
        }
        return null;
      },
    });
  }

  function setLogo() {
    if (busy || !entry.family) return;
    const family = entry.family;
    // The shown logo can be the global one, so the field starts empty and every save is sent.
    typed = "";
    tried = false;
    void confirm?.ask({
      title: `Change the logo of the family ${family}?`,
      text: "The model pickers of the apps show the logo with the family. The logo applies to every entry of the "
        + `organization in this family. Give a URI under ${LOGO_URI_PREFIX}, or a data URI that holds a PNG, SVG `
        + "or WebP image. Leave the field empty to remove the logo of the organization, if it has one. "
        + "The family then shows the global logo, if it has one.",
      action: "Change logo",
      pending: "Wait for the server to change the logo…",
      fields: logoField,
      run: async () => {
        tried = true;
        if (logoFault) return "";
        if (busy) return "An entry change is already in progress.";
        busy = true;
        try {
          await setOrgDescriptorFamilyLogo(session.signer, entry.scope, family, { logo_uri: typed || null });
          await onchanged();
        } catch (e) {
          return writeFailure(e, "logo");
        } finally {
          busy = false;
        }
        return null;
      },
    });
  }

  export function relabel(format: ModelFormat, quantization: string) {
    if (busy) return;
    member = { format, quantization };
    typed = quantization;
    tried = false;
    void confirm?.ask({
      title: "Change the quantization label?",
      text: `The member ${entry.display_name}::${format}::${quantization} keeps its descriptor. `
        + "Another member can use the old label immediately.",
      action: "Change label",
      pending: "Wait for the server to change the label…",
      fields: labelField,
      run: async () => {
        tried = true;
        if (labelFault) return "";
        if (busy) return "An entry change is already in progress.";
        busy = true;
        try {
          await relabelOrgDescriptorMember(session.signer, entry.scope, entry.name,
            { format, from: quantization, to: typed });
          await onchanged();
        } catch (e) {
          return writeFailure(e, "label");
        } finally {
          busy = false;
        }
        return null;
      },
    });
  }
</script>

{#snippet nameField()}
  {@const shown = tried || typed !== entry.name}
  <label><span>New name</span>
    <input type="text" bind:value={typed} autocomplete="off" spellcheck="false"
           aria-invalid={shown && !!nameFault} /></label>
  {#if shown && nameFault}<p class="err" role="alert">{nameFault}</p>{/if}
{/snippet}

{#snippet familyField()}
  {@const shown = tried || typed !== (entry.family ?? "")}
  <label><span>Family</span>
    <input type="text" bind:value={typed} autocomplete="off" spellcheck="false" placeholder="LFM2.5"
           aria-invalid={shown && !!familyFault} /></label>
  {#if shown && familyFault}<p class="err" role="alert">{familyFault}</p>{/if}
{/snippet}

{#snippet logoField()}
  {@const shown = tried || typed !== ""}
  <label><span>Logo URI</span>
    <input type="url" bind:value={typed} autocomplete="off" spellcheck="false" placeholder={`${LOGO_URI_PREFIX}lfm.svg`}
           aria-invalid={shown && !!logoFault} /></label>
  {#if shown && logoFault}<p class="err" role="alert">{logoFault}</p>{/if}
{/snippet}

{#snippet labelField()}
  {@const shown = tried || typed !== member?.quantization}
  <label><span>New quantization label</span>
    <input type="text" bind:value={typed} autocomplete="off" spellcheck="false" placeholder="q4_0"
           aria-invalid={shown && !!labelFault} /></label>
  {#if shown && labelFault}<p class="err" role="alert">{labelFault}</p>{/if}
{/snippet}

<div class="bar" role="group" aria-label="Change this entry">
  {#each stateActions(entry.state) as { action, label } (action)}
    <button type="button" aria-disabled={busy} onclick={() => change(action)}>{label}</button>
  {/each}
  <button type="button" aria-disabled={busy} onclick={rename}>Rename</button>
  {#if entry.descriptor_kind === "model"}
    <button type="button" aria-disabled={busy} onclick={setFamily}>Set family</button>
    <button type="button" aria-disabled={busy} onclick={setLogo}>Set family logo</button>
  {/if}
  <button type="button" class="danger" aria-disabled={busy} onclick={tombstone}>Tombstone</button>
</div>
{#if error}<p class="err" role="alert">{error}</p>{/if}

<ConfirmDialog bind:this={confirm} />
