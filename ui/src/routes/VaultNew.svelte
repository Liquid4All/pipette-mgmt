<script lang="ts">
  // The classic console's upload page (`static/templates/new_vault_resource.html`).
  import { vaultTypes } from "../lib/api";
  import Loaded from "../lib/components/Loaded.svelte";
  import { Busy, Loader } from "../lib/components/work.svelte";
  import PageHead from "../lib/components/PageHead.svelte";
  import { navigate } from "../lib/route.svelte";
  import { vaultLink, type RouteOf } from "../lib/routes";
  import { session } from "../lib/session.svelte";
  import { accept, offered, signedTransport, takes, upload, uploadOwners, type Chosen } from "../lib/upload";
  import type { VaultFileRole, VaultTypes } from "../lib/api-types";

  let { route: _ }: { route: RouteOf<"vault-new"> } = $props();

  const account = $derived(session.account!);
  const me = $derived(account.user_id);
  const owners = $derived(uploadOwners(account));
  let name = $state("");
  let owner = $state(session.account!.user_id);
  let type = $state("");
  let flavor = $state("");
  /** The file chosen for each role of the type shown. */
  let files = $state<Partial<Record<VaultFileRole, File>>>({});
  /** One line per file, which its progress rewrites. */
  let lines = $state<string[]>([]);
  const work = new Busy();
  // Leaving the page stops its upload, which would otherwise run on unseen and
  // then pull the reader to the resource from wherever they went.
  const gone = new AbortController();
  $effect(() => () => gone.abort());

  // The first model is chosen, as the classic select's first option is.
  const types = new Loader<VaultTypes>();
  types.run(async () => {
    const listed = await vaultTypes(session.signer);
    type ||= listed.models[0]?.type ?? "";
    return listed;
  });

  const shown = $derived(types.data ? offered(types.data, type) : null);

  // Another type draws its own fields, empty.
  function choose(next: string) {
    type = next;
    flavor = "";
    files = {};
  }

  function pick(role: VaultFileRole, input: HTMLInputElement) {
    const file = input.files?.[0];
    if (file) files[role] = file;
    else delete files[role];
  }

  async function submit(event: SubmitEvent) {
    event.preventDefault();
    const picked = shown;
    if (!picked) return;
    const chosen: Chosen[] = picked.vaultType.files.flatMap(({ role }) => {
      const file = files[role];
      return file ? [{ role, file }] : [];
    });
    await work.attempt(async () => {
      lines = [];
      const id = await upload(signedTransport(session.signer, gone.signal), { name, ...picked, owner, me, flavor },
                              chosen, (index, line) => { lines[index] = line; });
      if (!gone.signal.aborted) navigate(vaultLink(id));
    });
  }
</script>

<PageHead trail={[{ label: "Vault", href: "/app/vault" }]} title="Upload to the vault" />
<p class="muted">
  Each file is hashed in this browser, then streamed to mgmt, which keeps it only
  if it hashes the same; a file already uploaded is not sent again. The files
  are then described as the plan variant they make up, and the resource's page
  shows the entry a plan lists. A model directory goes up as one tar, tar.gz or
  zip whose root is the directory itself.
</p>
{#if work.error}<p class="err" role="alert">{work.error}</p>{/if}

<Loaded of={types}>
  {#snippet children(listed)}
    <form onsubmit={submit}>
      <label><span>name</span>
        <input type="text" required placeholder="acme LFM2-VL-3B q4" bind:value={name}>
      </label>
      <label><span>owner</span>
        <select bind:value={owner}>
          {#each owners as choice (choice.value)}
            <option value={choice.value}>{choice.label}</option>
          {/each}
        </select>
      </label>
      <label><span>type</span>
        <select value={type} onchange={(event) => choose(event.currentTarget.value)}>
          <optgroup label="models">
            {#each listed.models as vaultType (vaultType.type)}
              <option value={vaultType.type}>{vaultType.type}</option>
            {/each}
          </optgroup>
          <optgroup label="runtimes">
            {#each listed.runtimes as vaultType (vaultType.type)}
              <option value={vaultType.type}>{vaultType.type}</option>
            {/each}
          </optgroup>
        </select>
      </label>
      {#if shown}
        {@const { vaultType, runtime } = shown}
        {#key vaultType.type}
          <fieldset>
            <p class="muted">{vaultType.description}</p>
            {#if runtime}
              <label><span>flavor</span>
                <input type="text" list="flavors-{vaultType.type}" required autocomplete="off" spellcheck="false"
                       bind:value={flavor}>
              </label>
              <datalist id="flavors-{vaultType.type}">
                {#each vaultType.flavors as known (known)}<option value={known}></option>{/each}
              </datalist>
            {/if}
            {#each vaultType.files as field (field.role)}
              <label><span>{field.role}{field.optional ? " (optional)" : ""}: {takes(field.format)}</span>
                <input type="file" accept={accept(field.format)} required={!field.optional}
                       onchange={(event) => pick(field.role, event.currentTarget)}>
              </label>
            {/each}
          </fieldset>
        {/key}
      {/if}
      <button type="submit" class="primary" aria-disabled={work.busy}>Upload</button>
    </form>
    <ol>
      {#each lines as line, index (index)}<li>{line}</li>{/each}
    </ol>
  {/snippet}
</Loaded>
