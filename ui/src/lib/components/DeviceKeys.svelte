<script lang="ts">
  // An organization's device pre-auth keys (`clients.html`'s `#device-keys`,
  // and `client_preauth_minted.html` for the one just minted): the list, the
  // mint form and each key's revoke. A minted token lives in this component
  // alone, never in the address, the history or storage.
  import { clientPreauthKeys, mintClientPreauthKey, revokeClientPreauthKey } from "../api";
  import { handOver, mintRequest, usage } from "../devices";
  import { when } from "../format";
  import type { OrgRef } from "../orgs";
  import { session } from "../session.svelte";
  import type { ClientPreauthKeyMinted, ClientPreauthKeyView, OrgView } from "../api-types";
  import Loaded from "./Loaded.svelte";
  import MintForm from "./MintForm.svelte";
  import MintedToken from "./MintedToken.svelte";
  import PreauthKeyTable from "./PreauthKeyTable.svelte";
  import { Busy, Loader } from "./work.svelte";

  let { org }: { org: OrgRef | OrgView } = $props();

  const keys = new Loader<ClientPreauthKeyView[]>();
  const work = new Busy();
  const list = (keep = false) => keys.run(() => clientPreauthKeys(session.signer, org.org_id), keep);
  list();

  let minted = $state<ClientPreauthKeyMinted | null>(null);

  async function mint(form: { days: number; note: string; multiUse: boolean }) {
    await work.attempt(async () => {
      minted = await mintClientPreauthKey(session.signer, org.org_id, mintRequest(form.days, form.multiUse, form.note));
      await list(true);
    });
    return !work.error;
  }

  async function revoke(key_id: string) {
    await revokeClientPreauthKey(session.signer, org.org_id, key_id);
    if (minted?.key_id === key_id) minted = null;
    await list(true);
  }
</script>

<section id="device-keys">
  <h2>Device pre-auth keys</h2>
  {#if work.error}<p class="err" role="alert">{work.error}</p>{/if}

  {#if minted}
    {@const shown = minted}
    <MintedToken token={shown.token} ondone={() => (minted = null)}>
      {#snippet facts()}
        <dt>organization</dt>
        <dd>{"display_name" in org ? org.display_name : org.slug} <span class="muted id">{org.org_id}</span></dd>
        <dt>key_id</dt><dd class="id">{shown.key_id}</dd>
        <dt>usage</dt><dd>{usage(shown.usage)}</dd>
        <dt>expires</dt><dd>{when(shown.expires_at)}</dd>
        {#if shown.note}<dt>note</dt><dd>{shown.note}</dd>{/if}
      {/snippet}
      {#snippet handover()}
        <p>
          Give it to whoever is bringing the device online, over a channel only they
          read. The device sends it as <code>client_preauth_key</code> when it registers
          (<code>POST /clients/register</code>), and arrives approved and owned by this
          organization.
        </p>
        <p class="muted">
          {handOver(shown.usage)}
          It admits nothing once you no longer administer this organization's devices.
        </p>
      {/snippet}
    </MintedToken>
  {/if}

  <p class="muted">
    A device that registers with one arrives approved and owned by this
    organization, with no approval or assignment after it. The key seeds the
    owner and nothing else: grant <code>perform_work</code> on the page of a
    device that claims queued work, and tags are set on the host.
  </p>
  <Loaded of={keys}>
    {#snippet children(listed)}
      <PreauthKeyTable keys={listed} usage={(key) => usage(key.usage)} busy={work.busy} onrevoke={revoke}
                       empty="None outstanding. A single-use key is gone once it registers a device." />
    {/snippet}
  </Loaded>

  <MintForm action="Mint a device pre-auth key" noteLabel="note: what it is for, shown in this listing"
            placeholder="rack 3" multi busy={work.busy} onmint={mint} />
</section>
