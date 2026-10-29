<script lang="ts">
  // A new catalog entry of one organization, with one runtime or one model as its one member, over
  // `POST /api/v1/orgs/{org_id}/descriptor-catalog`. A name that an entry of the same kind holds adds the runtime or the
  // model to that entry as a new member, over `POST /api/v1/orgs/{org_id}/descriptor-catalog/{name}/members`. A runtime
  // joins only an entry of its own build.
  // One per progress table; each runtime cell's or model cell's button calls `create` with its descriptor as the table
  // groups it.
  import { addOrgDescriptorMember, createOrgDescriptorEntry, orgDescriptorEntries } from "../api";
  import type {
    CreateDescriptorEntryRequest, DescriptorEntryListRow, DescriptorKind, Model, Runtime,
  } from "../api-types";
  import {
    catalogNameError, entryTarget, memberTree, quantizationError, quantizationHints, storedName, writeFailure,
    type CatalogOrg, type MemberNode,
  } from "../catalog-entry";
  import { session } from "../session.svelte";
  import ConfirmDialog from "./ConfirmDialog.svelte";

  let { org, onnamed }: {
    org: CatalogOrg;
    /** Read the names again, after the server created an entry or added a member. */
    onnamed: () => Promise<void>;
  } = $props();

  let confirm = $state<ReturnType<typeof ConfirmDialog>>();
  let descriptor = $state("");
  const parsedDescriptor = $derived(JSON.parse(descriptor || "null") as unknown);
  let kind = $state<DescriptorKind>("runtime");
  const runtimeFlavor = $derived(kind === "runtime" && parsedDescriptor && typeof parsedDescriptor === "object"
    && "flavor" in parsedDescriptor && typeof parsedDescriptor.flavor === "string" ? parsedDescriptor.flavor : null);
  let name = $state("");
  let nameInput = $state<HTMLInputElement>();
  let quantization = $state("");
  // The quantization labels that the strings of a model descriptor show. A click on one fills the field.
  const hints = $derived(kind === "model" ? quantizationHints(parsedDescriptor) : []);
  // The live entries of the organization, read each time the dialog opens. For a runtime, the read also gives the
  // descriptors of the members. Until the read ends, and after it fails, every name creates an entry.
  let entries = $state.raw<DescriptorEntryListRow[]>([]);
  let entriesFailed = $state(false);
  // The dialog that the last read belongs to, so that a late answer for an earlier dialog changes nothing.
  let opened = 0;
  // A field's fault shows once the reader types in it, or tries to write.
  let tried = $state(false);
  const fault = $derived(catalogNameError(name));
  const quantizationFault = $derived(kind === "model" ? quantizationError(quantization) : null);
  const target = $derived(fault ? { to: "create" as const }
                                : entryTarget(name, entries, { kind, value: parsedDescriptor }));
  const adds = $derived(target.to === "add");

  function body(): CreateDescriptorEntryRequest {
    return kind === "model"
      ? { name, models: [{ quantization, model: JSON.parse(descriptor) as Model }] }
      : { name, runtimes: [JSON.parse(descriptor) as Runtime] };
  }

  async function readEntries(dialog: number, members: boolean) {
    try {
      const read = await orgDescriptorEntries(session.signer, org.id, members);
      if (dialog !== opened) return;
      entries = read;
      // A name that the reader typed before the read ended takes the case of its entry. A name that the reader is
      // still typing changes when the field loses focus.
      if (document.activeElement !== nameInput) name = storedName(name, read);
    } catch {
      if (dialog === opened) entriesFailed = true;
    }
  }

  async function write(): Promise<string | null> {
    tried = true;
    if (fault || quantizationFault || target.to === "taken") return "";
    const submittedTarget = target;
    try {
      if (submittedTarget.to === "add") {
        await addOrgDescriptorMember(session.signer, org.id, submittedTarget.entry.name, kind === "model"
          ? { model: parsedDescriptor as Model, quantization }
          : { runtime: parsedDescriptor as Runtime });
      } else {
        await createOrgDescriptorEntry(session.signer, org.id, body());
      }
    } catch (e) {
      return writeFailure(e, submittedTarget.to === "add" ? "member" : "entry");
    }
    await onnamed();
    return null;
  }

  /** Ask for a name for the descriptor `text` of kind `of`.
   * For a model, also ask for its quantization label. Add the descriptor to the entry that holds the name, if any.
   * Then create the entry or add the member. */
  export function create(text: string, of: DescriptorKind) {
    descriptor = text;
    kind = of;
    name = "";
    quantization = "";
    tried = false;
    entries = [];
    entriesFailed = false;
    opened += 1;
    void readEntries(opened, of === "runtime");
    void confirm?.ask({
      title: () => adds ? "Add a member to a catalog entry?" : "Create a catalog entry?",
      text: () => adds
        ? `The ${of} becomes a new member of an entry of ${org.label}. The members of ${org.label} see the name in `
          + `place of the ${of}.`
        : `The entry belongs to ${org.label}, and this ${of} is its one member. The members of ${org.label} see `
          + `the name in place of the ${of}.`,
      action: () => adds ? "Add member" : "Create",
      pending: () => adds ? "Adding…" : "Creating…",
      fields,
      run: write,
    });
  }
</script>

{#snippet fields()}
  <pre class="descriptor">{descriptor}</pre>
  <label><span>Name</span>
    <input type="text" bind:value={name} bind:this={nameInput} autocomplete="off" spellcheck="false"
           onblur={() => (name = storedName(name, entries))}
           aria-invalid={(tried || !!name) && (!!fault || target.to === "taken")} /></label>
  {#if kind === "runtime"}
    <p><span class="caption">Flavor</span><code>{runtimeFlavor ?? "None"}</code></p>
  {/if}
  {#if (tried || name) && fault}<p class="err" role="alert">{fault}</p>{/if}
  {#if target.to === "add"}
    <div class="muted" role="status">
      <p>{target.entry.display_name} already names a {kind} in {org.label}. This {kind} becomes a new member. Its
        members:</p>
      <ul class="members"><li>{target.entry.display_name}{@render members(memberTree(target.entry))}</li></ul>
    </div>
  {:else if target.to === "taken" && target.why === "build"}
    <p class="err" role="alert">{target.entry.display_name} names another build in {org.label}. A new member differs
      from the members of the entry only in flavor. Type another name.</p>
  {:else if target.to === "taken"}
    <p class="err" role="alert">{target.entry.display_name} names a {target.entry.descriptor_kind} entry in
      {org.label}. Type another name.</p>
  {/if}
  {#if entriesFailed}
    <p class="muted" role="status">Could not read the entries of {org.label}. The dialog can only create an
      entry.</p>
  {/if}
  {#if kind === "model"}
    <label><span>Quantization</span>
      <input type="text" bind:value={quantization} autocomplete="off" spellcheck="false" placeholder="q4_0"
             aria-invalid={(tried || !!quantization) && !!quantizationFault} /></label>
    {#if hints.length}
      <p class="muted hints">Found in the descriptor:
        {#each hints as hint (hint)}
          <button type="button" aria-pressed={quantization === hint} onclick={() => (quantization = hint)}>{hint}</button>
        {/each}</p>
    {/if}
    {#if (tried || quantization) && quantizationFault}<p class="err" role="alert">{quantizationFault}</p>{/if}
  {/if}
{/snippet}

{#snippet members(nodes: MemberNode[])}
  {#if nodes.length}
    <ul>{#each nodes as node}<li>{node.label}{@render members(node.children)}</li>{/each}</ul>
  {/if}
{/snippet}

<ConfirmDialog bind:this={confirm} />

<style>
  .descriptor { margin: 0 0 8px; padding: 6px 8px; max-height: 8em; overflow: auto; white-space: pre-wrap;
                word-break: break-all; font-size: 11px; border: 1px solid var(--border); border-radius: 4px;
                background: var(--surface); }
  .members, .members ul { margin: 2px 0; padding-left: 1.5em; font-family: var(--mono); font-size: 11px; }
  .members { padding-left: 0; list-style: none; }
  .hints button { margin-left: 4px; padding: 2px 8px; font-family: var(--mono); font-size: 11px; }
  .hints button[aria-pressed="true"] { border-color: var(--accent); }
</style>
