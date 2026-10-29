<script lang="ts">
  // The classic console's vault resource (`static/templates/vault_resource.html`):
  // `GET /vault/resources/{id}` with its owner's name and whether its file is
  // still stored, and the delete for a reader who may administer it. A model's
  // parameter counts sit beside it, for an administrator of its owner to set.
  import {
    createModelParams, deleteModelParams, deleteVaultResource, updateModelParams, vaultResource,
  } from "../lib/api";
  import Command from "../lib/components/Command.svelte";
  import DangerRow from "../lib/components/DangerRow.svelte";
  import Loaded from "../lib/components/Loaded.svelte";
  import OwnerCell from "../lib/components/OwnerCell.svelte";
  import PageHead from "../lib/components/PageHead.svelte";
  import SettingRow from "../lib/components/SettingRow.svelte";
  import DangerZone from "../lib/components/DangerZone.svelte";
  import SettingsSection from "../lib/components/SettingsSection.svelte";
  import { Busy, Loader } from "../lib/components/work.svelte";
  import { DELETES } from "../lib/danger";
  import { when } from "../lib/format";
  import { counts, noted } from "../lib/model-params";
  import { bytesText } from "../lib/plans";
  import { here, navigateFrom } from "../lib/route.svelte";
  import { vaultLink, type RouteOf } from "../lib/routes";
  import { session } from "../lib/session.svelte";
  import { ownerBranch, principalOf, underTrail } from "../lib/owned";
  import {
    VAULT_PAGES, artifactFiles, entryDescriptor, entryKind, jsonText, listedUnder, runtimesPath,
  } from "../lib/vault";
  import type { ModelParamsDetail, NamedVaultResourceDetail } from "../lib/api-types";

  let { route }: { route: RouteOf<"vault-resource"> } = $props();

  const id = $derived(route.resource_id);
  const record = new Loader<NamedVaultResourceDetail>();
  const read = () => vaultResource(session.signer, id);
  record.run(read);

  const view = $derived(record.data);
  const under = $derived(view && session.account ? ownerBranch(session.account, view.owner, session.known, VAULT_PAGES) : null);
  // The tab that lists it, where its delete lands: a runtime's is under the vault's own address.
  const runtimes = $derived(under && view && listedUnder(view) === "runtime"
    ? { label: "Runtimes", href: runtimesPath(under.href) } : null);
  const listing = $derived(runtimes?.href ?? under?.href ?? null);
  // The tree marks the listing the page sits under, as `vault_resource.html` sets its branch.
  $effect(() => { here.under = under?.href ?? null; });

  // As the classic `delete_vault_resource`, to the listing the page sat under, once its phrase is typed.
  const DELETES_TEXT = "Deletes this resource and the rest of its upload, whose plan entry needs every file. The files "
    + "stay stored, since another resource may name them; every device loses access, and a plan still naming these "
    + "resources is refused at ingestion. A record is never edited: to change anything, delete it and upload again.";
  let from = "";
  const remove = async () => {
    from = here.path;
    await deleteVaultResource(session.signer, id);
  };

  // The counts as the form holds them: the row's, once read.
  const work = new Busy();
  let total = $state<number | null>(null);
  let active = $state<number | null>(null);
  let note = $state("");
  $effect(() => {
    const row = record.data?.model_params?.row;
    // Left empty for a dense model, so a new total stays the active count too.
    [total, active, note] = row
      ? [row.total_millions, row.active_millions === row.total_millions ? null : row.active_millions, row.note ?? ""]
      : [null, null, ""];
  });

  async function saveParams(event: SubmitEvent, params: ModelParamsDetail) {
    event.preventDefault();
    await work.attempt(async () => {
      const body = { ...counts(total, active), ...noted(note) };
      if (params.row) await updateModelParams(session.signer, params.model_key, body);
      else await createModelParams(session.signer, { model_key: params.model_key, ...body });
      await record.run(read, true);
    });
  }

  const clearParams = (params: ModelParamsDetail) => work.attempt(async () => {
    await deleteModelParams(session.signer, params.model_key);
    await record.run(read, true);
  });
</script>

<PageHead trail={under ? [...underTrail(under, "Vault"), ...(runtimes ? [runtimes] : [])] : []} title={view?.name ?? id} />

<Loaded of={record}>
  {#snippet children(resource)}
    {@const descriptor = jsonText(entryDescriptor(resource.entry))}
    <dl>
      <dt>resource_id</dt><dd class="id">{resource.resource_id}</dd>
      <dt>owner</dt><dd><OwnerCell owner={resource.owner} names={resource.names ?? {}} /></dd>
      {#if resource.artifact}
        <dt>kind</dt><dd>{entryKind(resource.entry)}</dd>
        <dt>type</dt><dd class="id">{resource.artifact.type}</dd>
        {#if resource.artifact.flavor}<dt>flavor</dt><dd class="id">{resource.artifact.flavor}</dd>{/if}
        <dt>files</dt><dd>
          <!-- Every file of the upload by role, the others linking to their own resources. -->
          {#each artifactFiles(resource.artifact) as { role, file } (role)}
            <div>
              {#if file.resource_id === resource.resource_id}<span class="id">{file.filename}</span>
              {:else}<a class="id" href={vaultLink(file.resource_id)}>{file.filename}</a>{/if}
              <span class="muted">{role}</span>
            </div>
          {/each}
        </dd>
      {/if}
      <dt>sha256</dt><dd class="id">{resource.sha256}</dd>
      <dt>size</dt><dd>{bytesText(resource.bytes)} <span class="muted">({resource.bytes} bytes)</span></dd>
      <dt>created_at</dt><dd>{when(resource.created_at)}</dd>
      <!-- The one thing a record cannot say about itself: its file can go missing
           from the store without mgmt hearing of it. -->
      <dt>object.state</dt><dd>
        {#if resource.object?.state === "resized"}
          <span class="notice">resized: {resource.object.bytes} bytes where the record says
          {resource.bytes}, replaced behind the record. An upload keeps a file already stored, so
          an operator removes this one first; uploading the file then restores this resource.</span>
        {:else if resource.object?.state === "missing"}
          <span class="notice">missing: the file is gone from the store, and devices get a 404.
          Uploading the file again restores this resource.</span>
        {:else}<span class="id">{resource.object?.state ?? "—"}</span>{/if}
      </dd>
      <dt>origin</dt><dd>{#if resource.origin}<code>{jsonText(resource.origin)}</code>
        {:else}<span class="muted">none: a user uploaded it</span>{/if}</dd>
      {#if descriptor}<dt>descriptor</dt><dd><code>{descriptor}</code></dd>{/if}
      {#if resource.alias}
        <dt>alias</dt><dd><code>{jsonText(resource.alias)}</code>
          <div class="muted">The HuggingFace model its results are recorded under, beside its runs from there.</div></dd>
      {/if}
      <dt>in a plan</dt><dd>
        {#if resource.toml}
          <Command command={resource.toml} />
          <div class="muted">The entry for a plan's <code>models</code> or <code>runtimes</code>,
          naming every file uploaded with this one.</div>
        {:else}
          <code>/vault/resources/{resource.resource_id}/content</code>
          <div class="muted">The <code>url</code> a <code>vault_*</code> source names, beside a
          <code>principal_id</code> of <span class="id">{principalOf(resource.owner)}</span>.</div>
        {/if}
      </dd>
    </dl>

    {#if work.error}<p class="err" role="alert">{work.error}</p>{/if}
    <!-- As the writes decide: the server says so on the record. -->
    {#if resource.model_params}
      {@const params = resource.model_params}
      <div class="settings">
        <SettingsSection title="Model params">
          <SettingRow title="Parameter counts"
                      text="In millions, stored on this model's results from the next scoring pass. Set by an administrator of the organization that owns the model.">
            {#snippet children()}
              {#if params.model_key !== resource.resource_id}
                <p class="muted">Held under the model's main file,
                  <a class="id" href={vaultLink(params.model_key)}>{params.model_key}</a>.</p>
              {/if}
              {#if params.may_set}
                <form onsubmit={(event) => saveParams(event, params)}>
                  <label><span>total_millions</span><input type="number" min="1" bind:value={total} required></label>
                  <label><span>active_millions, the total when left empty</span>
                    <input type="number" min="1" bind:value={active}>
                  </label>
                  <label><span>note, where the counts come from</span>
                    <input type="text" maxlength="500" bind:value={note} autocomplete="off">
                  </label>
                  <button type="submit" aria-disabled={work.busy}>Save</button>
                  {#if params.row}
                    <button type="button" aria-disabled={work.busy} onclick={() => clearParams(params)}>Clear</button>
                  {/if}
                </form>
              {:else if params.row}
                <dl>
                  <dt>total_millions</dt><dd>{params.row.total_millions}</dd>
                  <dt>active_millions</dt><dd>{params.row.active_millions}</dd>
                  <dt>note</dt><dd>{params.row.note ?? "—"}</dd>
                </dl>
              {:else}
                <p class="muted">None set.</p>
              {/if}
            {/snippet}
          </SettingRow>
        </SettingsSection>
      </div>
    {/if}

    {#if resource.may_administer}
      <div class="settings">
        <DangerZone>
          <DangerRow title="Delete this resource" ask={`Delete ${resource.name}?`} text={DELETES_TEXT}
                     pending="Deleting…" phrase={DELETES.resource} run={remove}
                     done={() => { if (listing) navigateFrom(from, listing); }} />
        </DangerZone>
      </div>
    {:else}
      <p class="muted">You may read this resource and not delete it: deleting your organization's takes
        <code>manage_orgs</code>.</p>
    {/if}
  {/snippet}
</Loaded>
